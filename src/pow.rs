use std::{
    cmp::Ordering,
    collections::VecDeque,
    net::Ipv4Addr,
    rc::Rc,
    sync::{Arc, LazyLock, OnceLock},
    time::{Duration, Instant},
};

use arrayvec::ArrayVec;
use bdk_wallet::bitcoin::Amount;
use concurrent_map::{CasFailure, ConcurrentMap};
use parking_lot::{Mutex, MutexGuard};
use rand::{rng, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use terrors::OneOf;
use tokio::time::sleep;

use crate::{display_err, err, settings::SETTINGS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    nonce: Nonce,
    claimed: bool,
    expires_at: Instant,
    difficulty: u8,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct PowConfig {
    /// Minimum balance required for a user to claim funds.
    ///
    /// Defaults to `500` BTC, or `50_000_000_000` sats.
    /// When configuring in the config file, this value
    /// should be in sats as a number.
    pub min_balance: Amount,
    /// Minimum difficulty required for a user to claim funds.
    ///
    /// Defaults to `17`.
    ///
    /// Users will have to solve a POW challenge with a chance of finding of
    /// `1 / 2^min_difficulty` per random guess. The faucet will dynamically adjust
    /// the actual difficulty given to the user based on the current balance,
    /// `min_balance` and `sats_per_claim`.
    pub min_difficulty: u8,
    /// How long a challenge is valid for.
    ///
    /// Defaults to `120` seconds.
    ///
    /// In config, this should be provided as an object with fields `secs` and `nanos` with integers.
    /// For example:
    ///
    /// ```toml
    /// [pow]
    /// challenge_duration = { secs = 120, nanos = 0 }
    /// ```
    pub challenge_duration: Duration,
}

impl PowConfig {
    pub fn validate(&self) -> Result<(), InvalidPowConfig> {
        // min_balance >= 0 as u64
        // 0 <= min_difficulty <= 255 because u8 so valid
        if self.min_balance == Amount::ZERO {
            return Err(InvalidPowConfig::MinBalance("min_balance isn't positive"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum InvalidPowConfig {
    MinBalance(&'static str),
    MinDifficulty(&'static str),
}

impl Default for PowConfig {
    fn default() -> Self {
        Self {
            min_balance: Amount::from_int_btc(500),
            min_difficulty: 17,
            challenge_duration: Duration::from_secs(120),
        }
    }
}

/// Tokens already claimed within the challenge duration.
#[derive(Debug)]
pub struct AlreadyClaimed;
display_err!(
    AlreadyClaimed,
<<<<<<< HEAD
    "You have already claimed tokens. Please wait and try again."
=======
    "You have already claimed tokens from the faucet. Please wait and try again."
>>>>>>> main
);

/// Proof of Work is invalid.
#[derive(Debug)]
pub struct BadProofOfWork;
display_err!(
    BadProofOfWork,
    "Proof of Work is invalid. Please try again."
);

/// Nonce or POW challenge is no longer valid.
#[derive(Debug)]
pub struct NonceNotFound;
display_err!(
    NonceNotFound,
    "Proof of Work took too long. The challenge is no longer valid."
);

impl Challenge {
    /// Retrieves a proof-of-work challenge for the given Ipv4 address.
    ///
    /// Note that this doesn't support IPv6 yet because those IPs are a lot
    /// easier to get.
    pub fn get(ip: &Ipv4Addr, difficulty_if_not_present: u8) -> Self {
        let challenge = Self {
            nonce: rng().random(),
            claimed: false,
            expires_at: Instant::now() + SETTINGS.pow.challenge_duration,
            difficulty: difficulty_if_not_present,
        };
        match challenge_set().cas(ip.to_bits(), None, Some(challenge.clone())) {
            Ok(None) => {
                EvictionQueue::add_challenge(&challenge, *ip);
                challenge
            }
            Err(CasFailure {
                actual: Some(challenge),
                ..
            }) => challenge,
            // Unreachable as this CAS will return a Some(..) only
            // in an Err.
            Ok(Some(_)) => unreachable!(),
            // Unreachable for same reason as above
            Err(CasFailure { actual: None, .. }) => unreachable!(),
        }
    }

    /// Validates the proof of work solution by the client.
    pub fn check_solution(
        ip: &Ipv4Addr,
        solution: Solution,
    ) -> Result<(), OneOf<(NonceNotFound, BadProofOfWork, AlreadyClaimed)>> {
        let ns = challenge_set();
        let raw_ip = ip.to_bits();
        let mut challenge = match ns.get(&raw_ip) {
            Some(nonce_data) if !nonce_data.claimed => nonce_data,
            Some(_) => return err!(AlreadyClaimed),
            None => return err!(NonceNotFound),
        };

        let mut hasher = Sha256::new();
        hasher.update(b"strata faucet 2024");
        hasher.update(challenge.nonce);
        hasher.update(solution);

        // note, we mark the challenge as claimed here whether or not the
        // proof of work is valid. This is because this effectively ratelimits
        // the number of times a client can try to solve a challenge and waste
        // our server resources.
        challenge.claimed = true;
        let required_difficulty = challenge.difficulty;
        ns.insert(raw_ip, challenge);

        if count_leading_zeros(&hasher.finalize()) >= required_difficulty {
            Ok(())
        } else {
            err!(BadProofOfWork)
        }
    }

    pub fn nonce(&self) -> [u8; 16] {
        self.nonce
    }

    pub fn difficulty(&self) -> u8 {
        self.difficulty
    }
}

pub type Solution = [u8; 8];
pub type Nonce = [u8; 16];
/// IP set is used to check if an IPV4 address already
/// has a nonce present. IPs stored as u32 form for
/// compatibility with concurrent map. IPs are big endian
/// but these are notably using platform endianness.
pub type ChallengeSet = ConcurrentMap<u32, Challenge>;

static CELL: OnceLock<Mutex<ChallengeSet>> = OnceLock::new();

thread_local! {
    static CHALLENGE_SET: Rc<ChallengeSet> = Rc::new(
        // ensure CELL is initialised with the empty ChallengeSet
        // lock it to this thread
        CELL.get_or_init(Default::default).lock()
            // clone and store a copy thread local
            .clone()
        // release lock
    );
}

/// Helper function to retrieve the thread local instantiation of the
/// [`ChallengeSet`]
pub fn challenge_set() -> Rc<ChallengeSet> {
    CHALLENGE_SET.with(|ns| ns.clone())
}

/// A queue for evicting old challenges from the
/// challenge set efficiently and automatically using a [`VecDeque`]
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

    /// Adds a challenge to the eviction queue to be removed TTL in the future
    pub fn add_challenge(challenge: &Challenge, ip: Ipv4Addr) {
        let mut q = EVICTION_Q.q.lock();
        q.push_back(EvictionEntry {
            ip,
            expires_at: challenge.expires_at,
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
    let ns = challenge_set();
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

/// The faucet will dynamically adjust the difficulty based on the
/// current balance of the faucet to make it increasingly difficult
/// to retrieve funds from the faucet. The actual equation for this
/// is:
///
/// ```math
/// f_unclamped(x)=(M-m)(1-\log_{q}\frac{x}{b})+m
/// f_clamped(x) = \max(m,\min(M,f_unclamped(x)))
/// f(x) = f_clamped(x)
/// ```
///
/// where:
///
/// - `x` is the current balance in BTC
/// - `M` is the maximum difficulty
/// - `m` is the minimum difficulty
/// - `b` is the minimum balance in BTC
/// - `q` is the amount emitted per request in BTC
///
/// # Guarantees
///
/// This function guarantees that the difficulty will be between `min_difficulty`
/// and `max_difficulty` given that the correctness assumptions are met.
///
/// # Correctness
///
/// For this function correctly output, you must ensure:
///
/// - `per_emission` > `Amount::ONE_BTC`, ideally >2 BTC due to the way the curve
///   functions
/// - `min_difficulty` <= `max_difficulty`
/// - `max_difficulty`, `min_difficulty`, `min_balance`, `q` and `x` all > 0
///
/// # Expected clamping points
///
/// f(x) = M at x = b
/// f(x) = m at x = bq.
pub fn calculate_difficulty(x: f32, big_m: f32, m: f32, b: f32, q: f32) -> f32 {
    // optimisation for when the balance is less than or equal to the min balance
    if x <= b {
        return big_m;
    }

    let fval = (big_m - m) * (1.0 - (x / b).log(q)) + m;
    fval.clamp(m, big_m)
}

#[cfg(test)]
mod tests {
    use approx::assert_relative_eq;

    use super::*;

    /// Test for expected function values at sampled points for given parameters.
    /// The points are sampled such that it covers the clamped range as well as the
    /// range where the function's behavior is as expected.
    #[test]
    fn test_function() {
        let big_m = 255.0;
        let q = 15.0;
        let b = 70.0;
        let m = 16.0;

        let expected_points = [
            // At min balance and lesser, difficulty should be max.
            (45.0, big_m),
            (60.03, big_m),
            (70.0, big_m),
            // Between the clamped points the function should behave as expected.
            (71.0, 253.74813),
            (103.0, 220.9128),
            (201.0, 161.90737),
            (327.0, 118.95743),
            (600.0, 65.38911),
            (800.0, 39.99963),
            (960.0, 23.908766),
            (1043.0, 16.590347),
            (1049.96, 16.00336),
            // Beyond certain point, the difficulty should always be minimum.
            (1050.0, m),
            (1050.0001, m),
            (1080.0, m),
            (1950.0, m),
            (99950.0, m),
        ];

        for (x, exp_y) in expected_points {
            let y = calculate_difficulty(x, big_m, m, b, q);
            assert_relative_eq!(y, exp_y, epsilon = f32::EPSILON);
        }
    }

    // More general tests

    /// Tests the calculation at a reasonable balance value.
    #[test]
    fn test_calculate_difficulty_expected() {
        let balance = 5.0;
        let big_m = 256.0;
        let m = 2.0;
        let min_bal = 1.0;
        let q = 2.0;

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert!(difficulty >= m && difficulty <= big_m);
    }

    /// Tests when the balance is less than or equal to the minimum balance. The pow should be max.
    #[test]
    fn test_calculate_difficulty_min_balance() {
        let min_bal = 1.0;
        let balance = min_bal; // balance equals min_balance
        let big_m = 256.0;
        let m = 2.0;
        let q = 2.0;

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert_eq!(difficulty, big_m);

        let balance = min_bal - 0.1; // balance less than min_balance
        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert_eq!(difficulty, big_m);
    }

    /// Tests when the balance is zero. The pow should be max.
    #[test]
    fn test_calculate_difficulty_zero_balance() {
        let min_bal = 1.0;
        let balance = 0.0; // zero balance
        let big_m = 256.0;
        let m = 2.0;
        let q = 2.0;

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert_eq!(difficulty, big_m);
    }

    /// Tests when the balance is slightly greater than the minimum balance.
    #[test]
    fn test_calculate_difficulty_slightly_above_min_balance() {
        let min_bal = 1.0;
        let balance = 1.1;
        let big_m = 256.0;
        let m = 2.0;
        let q = 2.0;

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert!(difficulty < big_m && difficulty >= m);
    }

    /// Tests when the balance is much larger than the minimum balance.
    #[test]
    fn test_calculate_difficulty_large_balance() {
        let balance = 100.0;
        let big_m = 256.0;
        let m = 2.0;
        let min_bal = 1.0;
        let q = 2.0;

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert!(difficulty >= m && difficulty <= big_m);
    }

    /// Tests when the balance is extremely large, ensuring difficulty is still clamped correctly.
    #[test]
    fn test_calculate_difficulty_extreme_balance() {
        let balance = 1_000_000.0; // Extremely high balance
        let big_m = 256.0;
        let m = 2.0;
        let min_bal = 1.0;
        let q = 2.0;

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert!(difficulty >= m && difficulty <= big_m);
    }

    /// Tests when `min_difficulty` and `max_difficulty` are equal.
    #[test]
    fn test_calculate_difficulty_same_min_max() {
        let balance = 10.0;
        let big_m = 5.0;
        let m = 5.0; // min_difficulty == max_difficulty
        let min_bal = 1.0;
        let q = 2.0;

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert_eq!(difficulty, 5.0);
    }

    /// Tests when the emission per request is large.
    #[test]
    fn test_calculate_difficulty_large_emission() {
        let balance = 50.0;
        let big_m = 256.0;
        let m = 2.0;
        let min_bal = 1.0;
        let q = 10.0; // Large emission

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert!(difficulty >= m && difficulty <= big_m);
    }

    /// Test zero minimum balance(b).
    #[test]
    fn test_calculate_difficulty_zero_min_bal() {
        let balance = 50.0;
        let big_m = 256.0;
        let m = 2.0;
        let min_bal = 0.0;
        let q = 10.0;

        let difficulty = calculate_difficulty(balance, big_m, m, min_bal, q);
        assert_eq!(difficulty, m);
    }
}
