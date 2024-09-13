pub mod hex;
pub mod macros;

use bdk_wallet::rusqlite;
use axum::{routing::get, Router};
use bdk_wallet::{
    bitcoin::{
        bip32::{ChildNumber, Xpriv},
        Network,
    }, rusqlite::Connection, ChangeSet, KeychainKind, Wallet, WalletPersister
};
use hex::{decode, encode};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    cell::RefCell, cmp::Ordering, collections::BinaryHeap, io, net::Ipv4Addr, rc::Rc, sync::{Arc, OnceLock}, time::{Duration, Instant}
};
use tokio::time::sleep;

use arrayvec::ArrayVec;
use concurrent_map::{CasFailure, ConcurrentMap};
use parking_lot::{lock_api::MutexGuard, Mutex, RawMutex};
use rand::{thread_rng, Rng};
use sha2::Sha256;

const DB_PATH: &str = "faucet.sqlite";
const KEY_PATH: &str = "faucet.key";
const NETWORK: Network = Network::Signet;
const ESPLORA_URL: &str = "https://mutinynet.com/api";

#[derive(Serialize, Deserialize)]
pub struct SavableSeed {
    seed: String,
    l1_descriptor: String,
}

impl SavableSeed {
    fn save(&self) -> io::Result<()> {
        std::fs::write(KEY_PATH, serde_json::to_string_pretty(self)?)
    }

    fn read() -> io::Result<Option<Self>> {
        if std::path::Path::new(KEY_PATH).exists() {
            let content = std::fs::read_to_string(KEY_PATH)?;
            Ok(serde_json::from_str(&content)?)
        } else {
            Ok(None)
        }
    }
}

thread_local! {
    static DB: Rc<RefCell<Connection>> = RefCell::new(Connection::open(DB_PATH).unwrap()).into();
}

/// Wrapper around the built-in rusqlite db that allows PersistedWallet to be shared across multiple threads by
/// lazily initializing per core connections to the sqlite db and keeping them in local thread storage instead of
/// sharing the connection across cores
struct Persister;

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

    fn persist(_persister: &mut Self, changeset: &bdk_wallet::ChangeSet) -> Result<(), Self::Error> {
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();

    let (seed, loaded_seed) = match SavableSeed::read() {
        Ok(Some(SavableSeed { seed, .. })) => {
            let mut buf = [0u8; 32];
            if let Err(e) = decode(&seed, &mut buf) {
                panic!("seed failed to decode: {e:?}")
            }
            (buf, true)
        }
        _ => (thread_rng().gen(), false),
    };

    let rootpriv = Xpriv::new_master(Network::Signet, &seed).expect("valid xpriv");
    let purpose = ChildNumber::from_hardened_idx(86).unwrap();
    let coin_type = ChildNumber::from_hardened_idx(0).unwrap();
    let account = ChildNumber::from_hardened_idx(0).unwrap();
    let base_desc = format!("tr({}/{}/{}/{}", rootpriv, purpose, coin_type, account);

    if !loaded_seed {
        SavableSeed {
            seed: encode(&seed),
            l1_descriptor: format!("{base_desc})"),
        }
        .save()
        .expect("should be able to save");
    }

    let external_desc = format!("{base_desc}/0)");
    let internal_desc = format!("{base_desc}/1)");

    let mut l1_wallet = Wallet::load()
        .descriptor(KeychainKind::External, Some(external_desc.clone()))
        .descriptor(KeychainKind::Internal, Some(internal_desc.clone()))
        .extract_keys()
        .check_network(NETWORK)
        .load_wallet(&mut Persister)
        .unwrap()
        .unwrap_or_else(|| {
            Wallet::create(external_desc, internal_desc)
                .network(NETWORK)
                .create_wallet(&mut Persister)
                .expect("wallet creation to succeed")
        });

    let address = l1_wallet.next_unused_address(KeychainKind::External);
    dbg!(address);

    let app = Router::new().route("/", get(|| async { "Hello, World!" }))
        .with_state(Arc::new(l1_wallet));

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

pub struct Challenge {
    nonce: Nonce,
    expires_at: Instant,
}

const TTL: Duration = Duration::from_secs(20);

impl Challenge {
    pub fn get(ip: &Ipv4Addr, eviction_q: &EvictionQueue) -> Self {
        let nonce = thread_rng().gen();
        let expires_at = Instant::now() + TTL;
        match nonce_set().cas(ip.to_bits(), None, Some((nonce, false))) {
            Ok(None) => {
                let nonce = Self { nonce, expires_at };
                eviction_q.add_nonce(&nonce, *ip);
                nonce
            }
            // Unreachable as this CAS will return a Some(..) only
            // as a failure.
            Ok(Some(_)) => unreachable!(),
            Err(CasFailure { actual, .. }) => Self {
                // safety: safe to unwrap actual as if it were None
                // we would've got an Ok
                nonce: actual.unwrap().0,
                expires_at,
            },
        }
    }
}

pub struct ProofOfWorkChallenge {
    target_prefixed_zeros: u8,
    ip: Ipv4Addr,
    nonce: Nonce,
}

#[derive(Debug)]
pub struct NonceDoesNotExist;

impl ProofOfWorkChallenge {
    pub fn new(
        target_prefixed_zeros: u8,
        nonce: Nonce,
        ip: Ipv4Addr,
    ) -> Result<Self, NonceDoesNotExist> {
        match nonce_set().contains_key(&ip.to_bits()) {
            true => Ok(Self {
                target_prefixed_zeros,
                nonce,
                ip,
            }),
            false => Err(NonceDoesNotExist),
        }
    }

    /// Validates the proof of work solution by the client.
    /// Can only be called once per (IP,nonce) combination
    pub fn validate(&self, solution: u64) -> bool {
        // nonce_set().remove(&self.ip.to_bits());
        let mut hasher = Sha256::new();
        hasher.update(b"alpen labs faucet 2024");
        hasher.update(self.nonce);
        hasher.update(solution.to_le_bytes());
        count_leading_zeros(&hasher.finalize()) >= self.target_prefixed_zeros
    }
}

pub type Nonce = [u8; 16];
/// IP set is used to check if an IPV4 address already
/// has a nonce present. IPs stored as u32 form for
/// compatibility with concurrent map. IPs are big endian
/// but these are notably using platform endianness.
pub type NonceSet = ConcurrentMap<u32, (Nonce, bool)>;

/// A queue for evicting old challenges' nonces from the
/// nonce set efficiently and automatically using a [`BinaryHeap`] priority queue
/// and background task.
pub struct EvictionQueue {
    q: Mutex<BinaryHeap<EvictionEntry>>,
}

static CELL: OnceLock<Mutex<NonceSet>> = OnceLock::new();

thread_local! {
    static NONCE_SET: Rc<NonceSet> = Rc::new(
        // ensure CELL is initialised with the empty NonceSet
        // lock it to this thread
        CELL.get_or_init(Default::default).lock()
            // clone and store a copy thread local
            .clone()
    );
}

/// Helper function to retrieve the thread local instantiation of the [`NonceSet`]
pub fn nonce_set() -> Rc<NonceSet> {
    NONCE_SET.with(|ns| ns.clone())
}

impl EvictionQueue {
    /// Creates a new [`EvictionQueue`] and spawns a background task
    /// to perform evictions every 500ms.
    pub fn new() -> Arc<Self> {
        let eq = Arc::new(EvictionQueue {
            q: Default::default(),
        });
        let eq2 = eq.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(500)).await;
            eq2.remove_expired();
        });
        eq
    }

    /// Adds a nonce to the eviction queue to be removed TTL in the future
    pub fn add_nonce(&self, nonce: &Challenge, ip: Ipv4Addr) {
        let mut q = self.q.lock();
        q.push(EvictionEntry {
            ip,
            expires_at: nonce.expires_at,
        });
        self.remove_expired_internal(q)
    }

    /// Attempts to run the expiry routine. If not successful, it means that the routine is already running.
    /// In this case, there's no need to block and redo as it will be handled by the currently executing instance.
    fn remove_expired(&self) {
        if let Some(guard) = self.q.try_lock() {
            self.remove_expired_internal(guard);
        }
    }

    /// Removes expired entries from the heap and deletes them from the nonce set.
    /// This function is called internally by `remove_expired` and `add_nonce`. It handles two cases:
    /// - When the heap has less than 100 items, it creates an `ArrayVec` of size 100 to store expired entries.
    ///   It then pulls expired entries from the heap and adds them to the `ArrayVec`, up to a limit of 100.
    /// - When the heap has 100 or more items, it creates an `ArrayVec` of size 1000 to store expired entries.
    ///   It then pulls expired entries from the heap and adds them to the `ArrayVec`, up to a limit of 1000.
    ///   If there are still more expired entries in the heap, it calls `remove_expired` recursively.
    /// Finally, it deletes the expired entries from the nonce set using the `delete_expired` function.
    /// This means the function does not heap allocate and it doesn't hold the lock while it's deleting
    /// pulled, expired items.
    fn remove_expired_internal(&self, heap: HeapGuard) {
        if heap.is_empty() {
            return;
        }

        if heap.len() < 100 {
            let mut expired = ArrayVec::<_, 100>::new();
            // heap lock is auto dropped because moved into the heap
            pull_expired(heap, &mut expired, 100);
            delete_expired(&expired);
            return;
        }

        let mut expired = ArrayVec::<_, 1000>::new();
        // heap lock is auto dropped because moved into the heap
        let more_to_expire = pull_expired(heap, &mut expired, 1000);
        delete_expired(&expired);
        if more_to_expire {
            return self.remove_expired();
        }
    }
}

fn delete_expired(to_expire: &[u32]) {
    let ns = nonce_set();
    for k in to_expire {
        ns.remove(k);
    }
}

type HeapGuard<'a> = MutexGuard<'a, RawMutex, BinaryHeap<EvictionEntry>>;

/// Pulls expired entries from the eviction's priority queue and pushes their raw IPs onto
/// a generic [`Extend`]able list
fn pull_expired(mut from: HeapGuard, add_to: &mut impl Extend<u32>, limit: usize) -> bool {
    let now = Instant::now();
    let mut left = limit;
    loop {
        match from.peek() {
            Some(entry) if left > 0 => {
                if entry.expires_at >= now {
                    add_to.extend([from.pop().unwrap().ip.to_bits()]);
                    left -= 1;
                }
            }
            Some(entry) => break entry.expires_at >= now,
            None => break false,
        }
    }
}

pub struct EvictionEntry {
    ip: Ipv4Addr,
    expires_at: Instant,
}

impl PartialEq for EvictionEntry {
    fn eq(&self, other: &Self) -> bool {
        self.expires_at == other.expires_at
    }
}

impl Eq for EvictionEntry {}

impl PartialOrd for EvictionEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.expires_at.cmp(&other.expires_at))
    }
}

impl Ord for EvictionEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.expires_at.cmp(&other.expires_at)
    }
}

/// Counts the number of leading 0 bits in a `&[u8]`
/// with up to 255 leading 0 bits
fn count_leading_zeros(data: &[u8]) -> u8 {
    let mut leading_zeros = 0;
    for byte in data {
        if *byte == 0 {
            leading_zeros += 8;
        } else {
            leading_zeros += byte.leading_zeros() as u8;
            break;
        }
    }

    leading_zeros
}
