use std::{
    cmp::Ordering,
    collections::VecDeque,
    net::Ipv4Addr,
    rc::Rc,
    sync::{Arc, LazyLock, OnceLock},
    time::{Duration, Instant},
};

use arrayvec::ArrayVec;
use concurrent_map::{CasFailure, ConcurrentMap};
use parking_lot::{Mutex, MutexGuard};
use rand::{thread_rng, Rng};
use sha2::{Digest, Sha256};
use tokio::time::sleep;

pub struct Challenge {
    nonce: Nonce,
    expires_at: Instant,
}

const TTL: Duration = Duration::from_secs(20);

impl Challenge {
    /// Retrieves a proof-of-work challenge for the given Ipv4 address.
    ///
    /// Note that this doesn't support IPv6 yet because those IPs are a lot
    /// easier to get.
    pub fn get(ip: &Ipv4Addr) -> Self {
        let nonce = thread_rng().gen();
        let expires_at = Instant::now() + TTL;
        match nonce_set().cas(ip.to_bits(), None, Some((nonce, false))) {
            Ok(None) => {
                let nonce = Self { nonce, expires_at };
                EvictionQueue::add_nonce(&nonce, *ip);
                nonce
            }
            // Unreachable as this CAS will return a Some(..) only
            // in an Err.
            Ok(Some(_)) => unreachable!(),
            Err(CasFailure { actual, .. }) => Self {
                // safety: safe to unwrap actual as if it were None
                // we would've got an Ok
                nonce: actual.unwrap().0,
                expires_at,
            },
        }
    }

    /// Validates the proof of work solution by the client.
    pub fn valid(ip: &Ipv4Addr, difficulty: u8, solution: Solution) -> bool {
        let ns = nonce_set();
        let raw_ip = ip.to_bits();
        let nonce = match ns.get(&raw_ip) {
            Some((nonce, claimed)) if !claimed => nonce,
            _ => return false,
        };
        let mut hasher = Sha256::new();
        hasher.update(b"alpen labs faucet 2024");
        hasher.update(nonce);
        hasher.update(solution);
        let pow_valid = count_leading_zeros(&hasher.finalize()) >= difficulty;
        if pow_valid {
            ns.insert(raw_ip, (nonce, true));
        }
        pow_valid
    }

    pub fn nonce(&self) -> [u8; 16] {
        self.nonce
    }
}

pub type Solution = [u8; 8];
pub type Nonce = [u8; 16];
/// IP set is used to check if an IPV4 address already
/// has a nonce present. IPs stored as u32 form for
/// compatibility with concurrent map. IPs are big endian
/// but these are notably using platform endianness.
pub type NonceSet = ConcurrentMap<u32, (Nonce, bool)>;

static CELL: OnceLock<Mutex<NonceSet>> = OnceLock::new();

thread_local! {
    static NONCE_SET: Rc<NonceSet> = Rc::new(
        // ensure CELL is initialised with the empty NonceSet
        // lock it to this thread
        CELL.get_or_init(Default::default).lock()
            // clone and store a copy thread local
            .clone()
        // release lock
    );
}

/// Helper function to retrieve the thread local instantiation of the
/// [`NonceSet`]
pub fn nonce_set() -> Rc<NonceSet> {
    NONCE_SET.with(|ns| ns.clone())
}

/// A queue for evicting old challenges' nonces from the
/// nonce set efficiently and automatically using a [`VecDeque`]
/// and a background task.
pub struct EvictionQueue {
    q: Mutex<VecDeque<EvictionEntry>>,
}

static EVICTION_Q: LazyLock<Arc<EvictionQueue>> = LazyLock::new(EvictionQueue::new);

impl EvictionQueue {
    /// Creates a new [`EvictionQueue`] and spawns a background task
    /// to perform evictions every 500ms.
    fn new() -> Arc<Self> {
        let eq = Arc::new(EvictionQueue {
            q: Default::default(),
        });
        let eq2 = eq.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_millis(500)).await;
                eq2.remove_expired();
            }
        });
        eq
    }

    /// Adds a nonce to the eviction queue to be removed TTL in the future
    pub fn add_nonce(nonce: &Challenge, ip: Ipv4Addr) {
        let mut q = EVICTION_Q.q.lock();
        q.push_back(EvictionEntry {
            ip,
            expires_at: nonce.expires_at,
        });
        EVICTION_Q.remove_expired_internal(q)
    }

    /// Attempts to run the expiry routine. If not successful, it means that the
    /// routine is already running. In this case, there's no need to block
    /// and redo as it will be handled by the currently executing instance.
    fn remove_expired(&self) {
        if let Some(guard) = self.q.try_lock() {
            self.remove_expired_internal(guard);
        }
    }

    /// Removes expired entries from the heap and deletes them from the nonce
    /// set. This function is called internally by `remove_expired` and
    /// `add_nonce`. It handles two cases:
    ///
    /// - When the heap has less than 100 items, it creates an `ArrayVec` of
    ///   size 100 to store expired entries. It then pulls expired entries from
    ///   the heap and adds them to the `ArrayVec`, up to a limit of 100.
    /// - When the heap has 100 or more items, it creates an `ArrayVec` of size
    ///   1000 to store expired entries. It then pulls expired entries from the
    ///   heap and adds them to the `ArrayVec`, up to a limit of 1000. If there
    ///   are still more expired entries in the heap, it calls `remove_expired`
    ///   recursively.
    ///
    /// Finally, it deletes the expired entries from the nonce set using the
    /// `delete_expired` function. This means the function does not heap
    /// allocate and it doesn't hold the lock while it's deleting
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
            self.remove_expired()
        }
    }
}

fn delete_expired(to_expire: &[u32]) {
    let ns = nonce_set();
    for ip in to_expire {
        ns.remove(ip);
    }
}

type HeapGuard<'a> = MutexGuard<'a, VecDeque<EvictionEntry>>;

/// Pulls expired entries from the eviction's queue and pushes their raw IPs
/// onto a generic [`Extend`]able list
fn pull_expired(mut from: HeapGuard, add_to: &mut impl Extend<u32>, limit: usize) -> bool {
    let now = Instant::now();
    let mut left = limit;
    let mut i = 0;
    loop {
        match from.get(i) {
            Some(entry) if left > 0 => {
                if entry.expires_at <= now {
                    add_to.extend([from.pop_front().unwrap().ip.to_bits()]);
                    left -= 1;
                }
            }
            Some(entry) => break entry.expires_at <= now,
            None => break false,
        }
        i += 1;
    }
}

#[derive(Debug)]
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
