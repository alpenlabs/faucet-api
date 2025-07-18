use std::{
    cmp::Ordering,
    collections::VecDeque,
    net::Ipv4Addr,
    rc::Rc,
    sync::{Arc, LazyLock, OnceLock},
    time::{Duration, Instant},
    u8,
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
    "You have already claimed tokens from the faucet. Please wait and try again."
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
        let challenge_set = challenge_set();
        let raw_ip = ip.to_bits();

        let Some(old_challenge) = challenge_set.get(&raw_ip) else {
            return err!(NonceNotFound);
        };

        if old_challenge.claimed {
            return err!(AlreadyClaimed);
        }

        let mut replacement_challenge = old_challenge.clone();
        replacement_challenge.claimed = true;

        // note, we mark the challenge as claimed here whether or not the
        // proof of work is valid. This is because this effectively ratelimits
        // the number of times a client can try to solve a challenge and waste
        // our server resources.
        //
        // This also acts as a gate against race conditions and ensures that
        // only one client can claim a nonce at a time.
        match challenge_set.cas(
            ip.to_bits(),
            Some(&old_challenge),
            Some(replacement_challenge),
        ) {
            // successfully marked the unclaimed challenge as claimed, we can
            // proceed with the proof of work check
            Ok(_old_challenge) => (),
            // the challenge was already claimed by another client
            Err(_) => return err!(AlreadyClaimed),
        }

        let mut hasher = Sha256::new();
        hasher.update(b"alpen faucet 2024");
        hasher.update(old_challenge.nonce);
        hasher.update(solution);

        if count_leading_zeros(&hasher.finalize()) >= old_challenge.difficulty {
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
    let cs = challenge_set();
    for ip in to_expire {
        cs.remove(ip);
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

pub struct DifficultyConfig {
    big_m: u8,
    m: u8,
    b: f32,
    /// Optimisation for when x >= b+Lq, which should be the majority of the time
    min_diff_start: f32,
    /// Optimisation for the linear function. This is the gradient of the linear function.
    precompute_big_a: f32,
    /// Optimisation for the linear function. This is the y-intercept of the linear function.
    precompute_big_b: f32,
}

impl DifficultyConfig {
    pub fn new(
        max_diff: u8,
        min_diff: u8,
        min_balance: u64,
        sats_per_emission: u64,
        difficulty_increase_coeff: u64,
    ) -> Result<Self, DifficultyConfigError> {
        if max_diff < min_diff {
            return Err(DifficultyConfigError::MaxDiffMustBeGreaterThanMinDiff);
        }

        let big_m = max_diff as f32;
        let m = min_diff as f32;
        let b = min_balance as f32;
        let q = sats_per_emission as f32;
        let big_l = difficulty_increase_coeff as f32;

        let min_diff_start = b + big_l * q;

        let precompute_big_a = (m - big_m) / (big_l * q);
        let precompute_big_b = big_m - precompute_big_a * b;

        Ok(DifficultyConfig {
            big_m: max_diff,
            m: min_diff,
            b,
            min_diff_start,
            precompute_big_a,
            precompute_big_b,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DifficultyConfigError {
    MaxDiffMustBeGreaterThanMinDiff,
}

/// Calculates dynamic difficulty for a given challenge. Read docs/pow.md for more information.
pub fn calculate_difficulty(config: &DifficultyConfig, x: u64) -> u8 {
    match x as f32 {
        // Most expected path optimisation, return min difficulty
        x if x >= config.min_diff_start => config.m,
        // Least expected path optimisation, return max difficulty
        x if x <= config.b => config.big_m,
        // Optimised calculation for the gradient
        x => (config.precompute_big_a * x + config.precompute_big_b).round() as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_config_valid() {
        let config = DifficultyConfig::new(255, 20, 0, 10000, 10).unwrap();

        assert_eq!(config.big_m, 255);
        assert_eq!(config.m, 20);
        assert_eq!(config.b, 0.0);
        assert_eq!(config.min_diff_start, 100000.0); // b + L*q = 0 + 10*10000

        // Verify precomputed values
        let expected_a = (20.0 - 255.0) / (10.0 * 10000.0);
        let expected_b = 255.0 - expected_a * 0.0;
        assert_eq!(config.precompute_big_a, expected_a);
        assert_eq!(config.precompute_big_b, expected_b);
    }

    #[test]
    fn test_new_config_invalid() {
        let result = DifficultyConfig::new(10, 20, 0, 10000, 10);
        assert!(matches!(
            result,
            Err(DifficultyConfigError::MaxDiffMustBeGreaterThanMinDiff)
        ));
    }

    #[test]
    fn test_calculate_difficulty_high_balance() {
        let config = DifficultyConfig::new(255, 20, 0, 10000, 10).unwrap();

        // When x >= min_diff_start, should return minimum difficulty
        assert_eq!(calculate_difficulty(&config, 100000), 20);
        assert_eq!(calculate_difficulty(&config, 150000), 20);
        assert_eq!(calculate_difficulty(&config, 1000000), 20);
    }

    #[test]
    fn test_calculate_difficulty_low_balance() {
        let config = DifficultyConfig::new(255, 20, 0, 10000, 10).unwrap();

        // When x <= b, should return maximum difficulty
        assert_eq!(calculate_difficulty(&config, 0), 255);
    }

    #[test]
    fn test_calculate_difficulty_with_min_balance() {
        let config = DifficultyConfig::new(255, 20, 5000, 10000, 10).unwrap();

        // When x <= b (5000), should return maximum difficulty
        assert_eq!(calculate_difficulty(&config, 0), 255);
        assert_eq!(calculate_difficulty(&config, 5000), 255);
        assert_eq!(calculate_difficulty(&config, 4999), 255);
    }

    #[test]
    fn test_calculate_difficulty_linear_region() {
        let config = DifficultyConfig::new(255, 20, 0, 10000, 10).unwrap();

        // Test points in the linear region (0 < x < 100000)
        // At x = 50000 (halfway), difficulty should be roughly halfway between 20 and 255
        let mid_diff = calculate_difficulty(&config, 50000);
        assert!(mid_diff > 20 && mid_diff < 255);

        // Verify the linear progression
        let diff_25k = calculate_difficulty(&config, 25000);
        let diff_75k = calculate_difficulty(&config, 75000);
        assert!(diff_25k > mid_diff); // Lower balance = higher difficulty
        assert!(diff_75k < mid_diff); // Higher balance = lower difficulty
    }

    #[test]
    fn test_boundary_conditions() {
        let config = DifficultyConfig::new(255, 20, 0, 10000, 10).unwrap();

        // Test right at the boundary of min_diff_start
        assert_eq!(calculate_difficulty(&config, 100000), 20);
        assert_eq!(calculate_difficulty(&config, 99999), 20); // Should round to 20

        // Test just above minimum balance
        let just_above_min = calculate_difficulty(&config, 1);
        assert!(just_above_min > 20);
    }

    #[test]
    fn test_different_parameters() {
        // Test with different L value
        let config = DifficultyConfig::new(255, 17, 0, 5000, 25).unwrap();
        assert_eq!(config.min_diff_start, 125000.0); // 0 + 25*5000

        // High balance should give min difficulty
        assert_eq!(calculate_difficulty(&config, 200000), 17);

        // Low balance should give max difficulty
        assert_eq!(calculate_difficulty(&config, 0), 255);
    }

    #[test]
    fn test_exact_linear_calculation() {
        let config = DifficultyConfig::new(255, 20, 0, 10000, 10).unwrap();

        // Manually calculate expected difficulty for x = 50000
        let x = 50000.0;
        let expected = config.precompute_big_a * x + config.precompute_big_b;
        let calculated = calculate_difficulty(&config, 50000);

        assert_eq!(calculated, expected.round() as u8);
    }

    #[test]
    fn test_edge_case_equal_difficulties() {
        // Test when min and max difficulty are equal
        let config = DifficultyConfig::new(100, 100, 0, 10000, 10).unwrap();

        // Should always return 100
        assert_eq!(calculate_difficulty(&config, 0), 100);
        assert_eq!(calculate_difficulty(&config, 50000), 100);
        assert_eq!(calculate_difficulty(&config, 100000), 100);
    }

    #[test]
    fn test_large_values() {
        let config = DifficultyConfig::new(255, 20, 1000000, 100000, 50).unwrap();

        // Test with large balance values
        assert_eq!(calculate_difficulty(&config, 10000000), 20); // Very high balance
        assert_eq!(calculate_difficulty(&config, 500000), 255); // Below min balance

        // Test in linear region
        let mid_balance = 3500000; // Roughly in the middle of linear region
        let diff = calculate_difficulty(&config, mid_balance);
        assert!(diff > 20 && diff < 255);
    }
}
