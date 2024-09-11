use sha2::Digest;
use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    net::Ipv4Addr,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::time::sleep;

use arrayvec::ArrayVec;
use concurrent_map::{CasFailure, ConcurrentMap};
use parking_lot::{lock_api::MutexGuard, Mutex, RawMutex};
use rand::{thread_rng, Rng};
use sha2::Sha256;

fn main() {
    tracing_subscriber::fmt().init();
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
        match nonce_set().cas(ip.to_bits(), None::<&[u8]>, Some(nonce)) {
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
                nonce: actual.unwrap(),
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
    pub fn new(target_prefixed_zeros: u8, nonce: Nonce, ip: Ipv4Addr) -> Result<Self, NonceDoesNotExist> {
        match nonce_set().contains_key(&ip.to_bits()) {
            true => Ok(Self {
                target_prefixed_zeros,
                nonce,
                ip
            }),
            false => Err(NonceDoesNotExist)
        }
    }

    /// Validates the proof of work solution by the client.
    /// Can only be called once per (IP,nonce) combination
    pub fn validate(&self, solution: u64) -> bool {
        nonce_set().remove(&self.ip.to_bits());
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
pub type NonceSet = ConcurrentMap<u32, Nonce>;

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
        } else if heap.len() < 100 {
            let mut expired = ArrayVec::<_, 100>::new();
            pull_expired(heap, &mut expired, 100);
            delete_expired(&expired);
        } else {
            let mut expired = ArrayVec::<_, 1000>::new();
            let more_to_expire = pull_expired(heap, &mut expired, 1000);
            delete_expired(&expired);
            if more_to_expire {
                return self.remove_expired();
            }
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
