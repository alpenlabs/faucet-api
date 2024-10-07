use std::{
    cell::RefCell,
    collections::BTreeSet,
    io,
    ops::{Deref, DerefMut},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock,
    },
    time::Duration,
};

use bdk_esplora::{
    esplora_client::{self, AsyncClient},
    EsploraAsyncExt,
};
use bdk_wallet::{
    bitcoin::{
        bip32::{Xpriv, Xpub},
        key::Secp256k1,
        FeeRate, Network,
    },
    miniscript::descriptor::checksum::desc_checksum,
    rusqlite::{self, Connection},
    ChangeSet, KeychainKind, PersistedWallet, Wallet, WalletPersister,
};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::{seed::Seed, AppState, SETTINGS};

/// Live updating fee rate in sat/kwu
static FEE_RATE: AtomicU64 = AtomicU64::new(250);

/// Spawns a tokio task that updates the FEE_RATE every 20 seconds
pub fn spawn_fee_rate_task() {
    tokio::spawn(async move {
        loop {
            match ESPLORA_CLIENT
                .get_fee_estimates()
                .await
                .map(|frs| frs.get(&1).cloned())
            {
                Ok(Some(fr)) => {
                    let Some(new) = (fr as u64).checked_mul(1000 / 4) else {
                        warn!("got bad fee rate from esplora: {fr}");
                        return;
                    };
                    let prev = FEE_RATE.swap(new, Ordering::Relaxed);
                    if new != prev {
                        info!("updated fee rate from {prev} to {new} sat/kwu")
                    }
                }
                Ok(None) => error!("failed to fetch latest fee rates - got none back"),
                Err(e) => error!("failed to fetch latest fee rates: {e:?}"),
            }
            sleep(Duration::from_secs(20)).await;
        }
    });
}

/// Read-only public getter for the live updating fee rate
pub fn fee_rate() -> FeeRate {
    FeeRate::from_sat_per_kwu(FEE_RATE.load(Ordering::Relaxed))
}

/// Shared async client for esplora
pub static ESPLORA_CLIENT: LazyLock<AsyncClient> = LazyLock::new(|| {
    esplora_client::Builder::new(&SETTINGS.esplora)
        .build_async()
        .expect("valid esplora config")
});

/// Wrapper around the built-in rusqlite db that allows PersistedWallet to be
/// shared across multiple threads by lazily initializing per core connections
/// to the sqlite db and keeping them in local thread storage instead of sharing
/// the connection across cores
#[derive(Debug)]
pub struct Persister;

thread_local! {
    static DB: Rc<RefCell<Connection>> = RefCell::new(Connection::open(&SETTINGS.sqlite_file).unwrap()).into();
}

impl Persister {
    fn db() -> Rc<RefCell<Connection>> {
        DB.with(|db| db.clone())
    }
}

impl WalletPersister for Persister {
    type Error = rusqlite::Error;

    fn initialize(_persister: &mut Self) -> Result<bdk_wallet::ChangeSet, Self::Error> {
        let db = Self::db();
        let mut db_ref = db.borrow_mut();
        let db_tx = db_ref.transaction()?;
        ChangeSet::init_sqlite_tables(&db_tx)?;
        let changeset = ChangeSet::from_sqlite(&db_tx)?;
        db_tx.commit()?;
        Ok(changeset)
    }

    fn persist(
        _persister: &mut Self,
        changeset: &bdk_wallet::ChangeSet,
    ) -> Result<(), Self::Error> {
        let db = Self::db();
        let mut db_ref = db.borrow_mut();
        let db_tx = db_ref.transaction()?;
        changeset.persist_to_sqlite(&db_tx)?;
        db_tx.commit()
    }
}

#[derive(Debug)]
/// A wrapper around BDK's wallet with some custom logic
pub struct L1Wallet(PersistedWallet<Persister>);

impl L1Wallet {
    /// Create a wallet using the seed file and sqlite database.
    pub fn new(network: Network, seed: &Seed) -> io::Result<Self> {
        let rootpriv = Xpriv::new_master(Network::Signet, seed).expect("valid xpriv");
        let rootpub = Xpub::from_priv(&Secp256k1::new(), &rootpriv);
        let base_desc = format!("tr({}/86h/0h/0h", rootpriv);
        let pub_desc = format!("tr({}/86h/0h/0h/0/*)", rootpub);

        info!(
            "L1 descriptor: {pub_desc}#{}",
            desc_checksum(&pub_desc).expect("valid desc")
        );
        let external_desc = format!("{base_desc}/0/*)");
        let internal_desc = format!("{base_desc}/1/*)");

        Ok(Self(
            Wallet::load()
                .descriptor(KeychainKind::External, Some(external_desc.clone()))
                .descriptor(KeychainKind::Internal, Some(internal_desc.clone()))
                .extract_keys()
                .check_network(network)
                .load_wallet(&mut Persister)
                .unwrap()
                .unwrap_or_else(|| {
                    Wallet::create(external_desc, internal_desc)
                        .network(network)
                        .create_wallet(&mut Persister)
                        .expect("wallet creation to succeed")
                }),
        ))
    }

    /// Spawns a tokio task that scans the chain for the wallet's outputs
    /// every 30 secs.
    pub fn spawn_syncer(state: Arc<AppState>) {
        tokio::spawn(async move {
            loop {
                let req = state
                    .l1_wallet
                    .read()
                    .start_full_scan()
                    .inspect({
                        let mut once = BTreeSet::<KeychainKind>::new();
                        move |keychain, spk_i, _| {
                            if once.insert(keychain) {
                                debug!("Scanning keychain [{:?}]", keychain);
                            }
                            debug!(" {:<3}", spk_i);
                        }
                    })
                    .build();
                // do full scans because the miner is sending to our public descriptor
                let update = match ESPLORA_CLIENT.full_scan(req, 3, 10).await {
                    Ok(u) => u,
                    Err(e) => {
                        error!("{e:?}");
                        continue;
                    }
                };
                {
                    // in a separate block otherwise compiler gets upset that we're holding
                    // this over the await point
                    let mut l1w = state.l1_wallet.write();
                    l1w.apply_update(update)
                        .expect("should be able to connect to db");
                    l1w.persist(&mut Persister).expect("persist should work");
                }
                sleep(Duration::from_secs(30)).await;
            }
        });
    }
}

impl Deref for L1Wallet {
    type Target = PersistedWallet<Persister>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for L1Wallet {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
