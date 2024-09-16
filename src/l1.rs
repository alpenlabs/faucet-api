use std::{
    cell::RefCell,
    io,
    ops::{Deref, DerefMut},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        LazyLock,
    },
    time::Duration,
};

use bdk_esplora::esplora_client::{self, AsyncClient};
use bdk_wallet::{
    bitcoin::{
        bip32::{Xpriv, Xpub},
        key::Secp256k1,
        FeeRate, Network,
    },
    descriptor::calc_checksum,
    rusqlite::{self, Connection},
    ChangeSet, KeychainKind, PersistedWallet, Wallet, WalletPersister,
};
use colored::Colorize;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::{seed::SavableSeed, ESPLORA_URL};

const DB_PATH: &str = "faucet.sqlite";

thread_local! {
    static DB: Rc<RefCell<Connection>> = RefCell::new(Connection::open(DB_PATH).unwrap()).into();
}

/// Live updating fee rate in sat/kwu
static FEE_RATE: AtomicU64 = AtomicU64::new(250);

pub fn fee_rate() -> FeeRate {
    FeeRate::from_sat_per_kwu(FEE_RATE.load(Ordering::Relaxed))
}

/// Shared async client for esplora
pub static ESPLORA_CLIENT: LazyLock<AsyncClient> = LazyLock::new(|| {
    esplora_client::Builder::new(ESPLORA_URL)
        .build_async()
        .expect("valid esplora config")
});

/// Wrapper around the built-in rusqlite db that allows PersistedWallet to be shared across multiple threads by
/// lazily initializing per core connections to the sqlite db and keeping them in local thread storage instead of
/// sharing the connection across cores
#[derive(Debug)]
pub struct Persister;

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

impl Persister {
    fn db() -> Rc<RefCell<Connection>> {
        DB.with(|db| db.clone())
    }
}

#[derive(Debug)]
/// A wrapper around BDK's wallet that has the logic for loading the single sig taproot wallet
pub struct L1Wallet(PersistedWallet<Persister>);

impl L1Wallet {
    pub fn load_or_create(network: Network) -> io::Result<Self> {
        let seed = SavableSeed::load_or_create()?;
        let rootpriv = Xpriv::new_master(Network::Signet, &seed).expect("valid xpriv");
        let secp = Secp256k1::new();
        let rootpub = Xpub::from_priv(&secp, &rootpriv);
        let base_desc = format!("tr({}/86h/0h/0h", rootpriv);
        let base_pub_desc = format!("tr({}/86h/0h/0h)", rootpub);
        info!(
            "public descriptor: {}",
            format!(
                "{}#{}",
                base_pub_desc,
                calc_checksum(&base_pub_desc).expect("valid descriptor")
            )
            .green()
        );

        let external_desc = format!("{base_desc}/0)");
        let internal_desc = format!("{base_desc}/1)");

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
                        info!("updated fee rate from {prev} to {new} sat/kwu")
                    }
                    Ok(None) => error!("failed to fetch latest fee rates"),
                    Err(e) => error!("failed to fetch latest fee rates: {e:?}"),
                }
                sleep(Duration::from_secs(20)).await;
            }
        });

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
