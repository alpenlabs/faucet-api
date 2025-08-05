use std::{
    cmp,
    collections::BinaryHeap,
    net::Ipv4Addr,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, OnceLock,
    },
    time::{Duration, Instant},
};

use bdk_wallet::bitcoin::Amount;
use concurrent_map::{CasFailure, ConcurrentMap};
use kanal::Sender;
use parking_lot::{Mutex, MutexGuard};
use rand::{rng, Rng};
use sha2::{Digest, Sha256};
use terrors::OneOf;
use tokio::{select, time::sleep};
use tracing::debug;

use crate::{display_err, err, Chain};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    nonce: Nonce,
    claimed: bool,
    expires_at: Instant,
    difficulty: u8,
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
    pub fn get(
        chain: Chain,
        ip: &Ipv4Addr,
        difficulty_if_not_present: u8,
        challenge_duration: Duration,
    ) -> Self {
        let challenge = Self {
            nonce: rng().random(),
            claimed: false,
            expires_at: Instant::now() + challenge_duration,
            difficulty: difficulty_if_not_present,
        };
        match challenge_set().cas((ip.to_bits(), chain), None, Some(challenge.clone())) {
            Ok(None) => {
                EVICTION_Q.add_challenge(&challenge, *ip, chain);
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
        chain: Chain,
        ip: &Ipv4Addr,
        solution: Solution,
    ) -> Result<(), OneOf<(NonceNotFound, BadProofOfWork, AlreadyClaimed)>> {
        let challenge_set = challenge_set();
        let raw_ip = ip.to_bits();

        let Some(old_challenge) = challenge_set.get(&(raw_ip, chain)) else {
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
            (ip.to_bits(), chain),
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
pub type ChallengeSet = ConcurrentMap<(u32, Chain), Challenge>;

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
/// challenge set efficiently and automatically using a [`BinaryHeap`]
/// and a background task.
pub struct EvictionQueue {
    q: Mutex<BinaryHeap<EvictionEntry>>,

    next_wakeup_millis: AtomicU64,
    /// notifies the background task that the wakeup time has changed
    i_changed_the_wakeup: Sender<Instant>,
    start: Instant,
}

static EVICTION_Q: LazyLock<Arc<EvictionQueue>> = LazyLock::new(EvictionQueue::new);

impl EvictionQueue {
    /// Creates a new [`EvictionQueue`] and spawns a background task
    /// to perform evictions every 500ms.
    fn new() -> Arc<Self> {
        let (i_changed_the_wakeup, someone_changed_the_wakeup) = kanal::unbounded_async();
        let eq = Arc::new(EvictionQueue {
            q: Default::default(),
            next_wakeup_millis: AtomicU64::new(u64::MAX),
            i_changed_the_wakeup: i_changed_the_wakeup.to_sync(),
            start: Instant::now(),
        });
        let eq2 = eq.clone();
        tokio::spawn(async move {
            fn time_until(next_wakeup: Instant) -> Duration {
                next_wakeup.saturating_duration_since(Instant::now())
            }

            // the next time we're gonna wake up to perform evictions
            // default to 10000 years from now
            let mut next_wakeup = eq2.start + Duration::from_secs(60 * 60 * 24 * 365 * 10_000);

            // a future to sleep until the next wakeup time, updated whenever the wakeup time changes
            let mut honk_shoo = Box::pin(sleep(time_until(next_wakeup)));

            loop {
                select! {
                    Ok(new_wakeup) = someone_changed_the_wakeup.recv() => {
                        debug!("new wakeup time received");
                        if new_wakeup < next_wakeup {
                            debug!("changing sleep duration from {:?} to {:?}", time_until(next_wakeup), time_until(new_wakeup));
                            next_wakeup = new_wakeup;
                            eq2.next_wakeup_millis.store(next_wakeup.duration_since(eq2.start).as_millis() as u64, Ordering::Relaxed);
                            honk_shoo = Box::pin(sleep(time_until(next_wakeup)));
                        }
                    },
                    // wakey wakey
                    _ = &mut honk_shoo => {
                        debug!("i am awake, evicting");
                        if let Some(wakeup_time) = Self::remove_expired(eq2.q.lock()) {
                            next_wakeup = wakeup_time;
                            eq2.next_wakeup_millis.store(next_wakeup.duration_since(eq2.start).as_millis() as u64, Ordering::Relaxed);
                            honk_shoo = Box::pin(sleep(time_until(next_wakeup)));
                        } else {
                            // default to 10000 years from now
                            next_wakeup = eq2.start + Duration::from_secs(60 * 60 * 24 * 365 * 10_000);
                            eq2.next_wakeup_millis.store(next_wakeup.duration_since(eq2.start).as_millis() as u64, Ordering::Relaxed);
                            honk_shoo = Box::pin(sleep(time_until(next_wakeup)));
                        }
                    }
                };
            }
        });
        eq
    }

    /// Adds a challenge to the eviction queue to be removed TTL in the future
    pub fn add_challenge(&self, challenge: &Challenge, ip: Ipv4Addr, chain: Chain) {
        self.q.lock().push(EvictionEntry {
            ip,
            chain,
            expires_at: challenge.expires_at,
        });
        let expires_at_millies_from_start =
            challenge.expires_at.duration_since(self.start).as_millis() as u64;
        let next_wakeup = self.next_wakeup_millis.load(Ordering::Relaxed);
        if expires_at_millies_from_start < next_wakeup {
            self.i_changed_the_wakeup
                .send(challenge.expires_at)
                .expect("Failed to send wakeup time");
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
    fn remove_expired(mut heap: HeapGuard) -> Option<Instant> {
        let mut to_expire = Vec::new();
        let now = Instant::now();
        let next_wakeup = loop {
            match heap.peek() {
                Some(entry) if entry.expires_at <= now => {
                    to_expire.push(heap.pop().unwrap());
                }
                Some(entry) => break Some(entry.expires_at),
                None => break None,
            }
        };
        let cs = challenge_set();
        for EvictionEntry { ip, chain, .. } in to_expire {
            cs.remove(&(ip.to_bits(), chain));
        }
        next_wakeup
    }
}

type HeapGuard<'a> = MutexGuard<'a, BinaryHeap<EvictionEntry>>;

#[derive(Debug)]
pub struct EvictionEntry {
    ip: Ipv4Addr,
    chain: Chain,
    expires_at: Instant,
}

impl PartialEq for EvictionEntry {
    fn eq(&self, other: &Self) -> bool {
        self.expires_at == other.expires_at
    }
}

impl Eq for EvictionEntry {}

impl PartialOrd for EvictionEntry {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EvictionEntry {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        // invert so we can use a min heap
        other.expires_at.cmp(&self.expires_at)
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
    /// Optimization for when x >= b+Lq, which should be the majority of the time
    min_diff_start: f32,
    /// Optimization for the linear function. This is the gradient of the linear function.
    precompute_big_a: f32,
    /// Optimization for the linear function. This is the y-intercept of the linear function.
    precompute_big_b: f32,
}

impl DifficultyConfig {
    pub fn new(
        max_diff: u8,
        min_diff: u8,
        min_balance: Amount,
        per_claim: Amount,
        difficulty_increase_coeff: f32,
    ) -> Result<Self, DifficultyConfigError> {
        if max_diff < min_diff {
            return Err(DifficultyConfigError::MaxDiffMustBeGreaterThanMinDiff);
        }
        if per_claim == Amount::ZERO {
            return Err(DifficultyConfigError::PerClaimMustBeGreaterThanZero);
        }
        if difficulty_increase_coeff <= 0.0 {
            return Err(DifficultyConfigError::DifficultyIncreaseCoefficientMustBeGreaterThanZero);
        }

        let big_m = max_diff as f32;
        let m = min_diff as f32;
        let b = min_balance.to_sat() as f32;
        let q = per_claim.to_sat() as f32;
        let big_l = difficulty_increase_coeff;

        // Check for potential overflow in big_l * q
        let lq_product = big_l * q;
        if !lq_product.is_finite() {
            return Err(DifficultyConfigError::ArithmeticOverflow);
        }

        // Check for potential overflow in b + big_l * q
        let min_diff_start = b + lq_product;
        if !min_diff_start.is_finite() {
            return Err(DifficultyConfigError::ArithmeticOverflow);
        }

        // Check for division by zero or very small values that could cause issues
        if lq_product.abs() < f32::EPSILON {
            return Err(DifficultyConfigError::InvalidCalculation);
        }

        // Check for potential overflow in (m - big_m) / (big_l * q)
        let numerator = m - big_m;
        let precompute_big_a = numerator / lq_product;
        if !precompute_big_a.is_finite() {
            return Err(DifficultyConfigError::ArithmeticOverflow);
        }

        // Check for potential overflow in precompute_big_a * b
        let ab_product = precompute_big_a * b;
        if !ab_product.is_finite() {
            return Err(DifficultyConfigError::ArithmeticOverflow);
        }

        // Check for potential overflow in big_m - precompute_big_a * b
        let precompute_big_b = big_m - ab_product;
        if !precompute_big_b.is_finite() {
            return Err(DifficultyConfigError::ArithmeticOverflow);
        }

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
    PerClaimMustBeGreaterThanZero,
    DifficultyIncreaseCoefficientMustBeGreaterThanZero,
    ArithmeticOverflow,
    InvalidCalculation,
}

impl std::fmt::Display for DifficultyConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DifficultyConfigError::MaxDiffMustBeGreaterThanMinDiff => {
                write!(
                    f,
                    "Maximum difficulty must be greater than minimum difficulty"
                )
            }
            DifficultyConfigError::PerClaimMustBeGreaterThanZero => {
                write!(f, "Per claim amount must be greater than zero")
            }
            DifficultyConfigError::DifficultyIncreaseCoefficientMustBeGreaterThanZero => {
                write!(
                    f,
                    "Difficulty increase coefficient must be greater than zero"
                )
            }
            DifficultyConfigError::ArithmeticOverflow => {
                write!(
                    f,
                    "Arithmetic overflow occurred during difficulty configuration calculation"
                )
            }
            DifficultyConfigError::InvalidCalculation => {
                write!(f, "Invalid calculation parameters resulted in division by zero or near-zero values")
            }
        }
    }
}

impl std::error::Error for DifficultyConfigError {}

/// Calculates dynamic difficulty for a given challenge. Read docs/pow.md for more information.
pub fn calculate_difficulty(config: &DifficultyConfig, x: Amount) -> u8 {
    match x.to_sat() as f32 {
        // Most expected path optimization, return min difficulty
        x if x >= config.min_diff_start => config.m,
        // Least expected path optimization, return max difficulty
        x if x <= config.b => config.big_m,
        // Optimised calculation for the gradient
        // Safety: guaranteed within 0..=255 due to the nature of the linear function and the bounds of x
        // the cast performs a truncation of the decimal part, so we round prior
        x => (config.precompute_big_a * x + config.precompute_big_b).round() as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_config_valid() {
        let config =
            DifficultyConfig::new(255, 20, Amount::ZERO, Amount::from_sat(10000), 10.).unwrap();

        assert_eq!(config.big_m, 255);
        assert_eq!(config.m, 20);
        assert_eq!(config.b, 0.0);
        assert_eq!(config.min_diff_start, 100_000.0); // b + L*q = 0 + 10*10000

        // Verify precomputed values
        let expected_a = (20.0 - 255.0) / (10.0 * 10_000.0);
        let expected_b = 255.0 - expected_a * 0.0;
        assert_eq!(config.precompute_big_a, expected_a);
        assert_eq!(config.precompute_big_b, expected_b);
    }

    #[test]
    fn test_new_config_invalid() {
        let result = DifficultyConfig::new(10, 20, Amount::ZERO, Amount::from_sat(10000), 10.);
        assert!(matches!(
            result,
            Err(DifficultyConfigError::MaxDiffMustBeGreaterThanMinDiff)
        ));
    }

    #[test]
    fn test_calculate_difficulty_high_balance() {
        let config =
            DifficultyConfig::new(255, 20, Amount::ZERO, Amount::from_sat(10000), 10.).unwrap();

        // When x >= min_diff_start, should return minimum difficulty
        assert_eq!(calculate_difficulty(&config, Amount::from_sat(100_000)), 20);
        assert_eq!(calculate_difficulty(&config, Amount::from_sat(150_000)), 20);
        assert_eq!(
            calculate_difficulty(&config, Amount::from_sat(1_000_000)),
            20
        );
    }

    #[test]
    fn test_calculate_difficulty_low_balance() {
        let config =
            DifficultyConfig::new(255, 20, Amount::ZERO, Amount::from_sat(10_000), 10.).unwrap();

        // When x <= b, should return maximum difficulty
        assert_eq!(calculate_difficulty(&config, Amount::ZERO), 255);
    }

    #[test]
    fn test_calculate_difficulty_with_min_balance() {
        let config = DifficultyConfig::new(
            255,
            20,
            Amount::from_sat(5000),
            Amount::from_sat(10_000),
            10.,
        )
        .unwrap();

        // When x <= b (5000), should return maximum difficulty
        assert_eq!(calculate_difficulty(&config, Amount::ZERO), 255);
        assert_eq!(calculate_difficulty(&config, Amount::from_sat(5000)), 255);
        assert_eq!(calculate_difficulty(&config, Amount::from_sat(4999)), 255);
    }

    #[test]
    fn test_arithmetic_overflow_error() {
        // Test with very large values that could cause overflow
        let result = DifficultyConfig::new(
            255,
            20,
            Amount::from_sat(u64::MAX),
            Amount::from_sat(u64::MAX),
            f32::MAX,
        );
        assert!(matches!(
            result,
            Err(DifficultyConfigError::ArithmeticOverflow)
        ));
    }

    #[test]
    fn test_invalid_calculation_error() {
        // Test with very small difficulty_increase_coeff that results in effective zero
        let result = DifficultyConfig::new(
            255,
            20,
            Amount::ZERO,
            Amount::from_sat(1),
            f32::EPSILON / 10.0, // Much smaller than EPSILON
        );
        assert!(matches!(
            result,
            Err(DifficultyConfigError::InvalidCalculation)
        ));
    }

    #[test]
    fn test_per_claim_zero_error() {
        let result = DifficultyConfig::new(255, 20, Amount::ZERO, Amount::ZERO, 10.0);
        assert!(matches!(
            result,
            Err(DifficultyConfigError::PerClaimMustBeGreaterThanZero)
        ));
    }

    #[test]
    fn test_negative_difficulty_coeff_error() {
        let result = DifficultyConfig::new(255, 20, Amount::ZERO, Amount::from_sat(10000), -1.0);
        assert!(matches!(
            result,
            Err(DifficultyConfigError::DifficultyIncreaseCoefficientMustBeGreaterThanZero)
        ));
    }

    #[test]
    fn test_calculate_difficulty_linear_region() {
        let config =
            DifficultyConfig::new(255, 20, Amount::ZERO, Amount::from_sat(10_000), 10.).unwrap();

        // Test points in the linear region (0 < x < 100000)
        // At x = 50000 (halfway), difficulty should be roughly halfway between 20 and 255
        let mid_diff = calculate_difficulty(&config, Amount::from_sat(50_000));
        assert!(mid_diff > 20 && mid_diff < 255);

        // Verify the linear progression
        let diff_25k = calculate_difficulty(&config, Amount::from_sat(25_000));
        let diff_75k = calculate_difficulty(&config, Amount::from_sat(75_000));
        assert!(diff_25k > mid_diff); // Lower balance = higher difficulty
        assert!(diff_75k < mid_diff); // Higher balance = lower difficulty
    }

    #[test]
    fn test_boundary_conditions() {
        let config =
            DifficultyConfig::new(255, 20, Amount::ZERO, Amount::from_sat(10_000), 10.).unwrap();

        // Test right at the boundary of min_diff_start
        assert_eq!(calculate_difficulty(&config, Amount::from_sat(100_000)), 20);
        assert_eq!(calculate_difficulty(&config, Amount::from_sat(99_999)), 20); // Should round to 20

        // Test just above minimum balance
        let just_above_min = calculate_difficulty(&config, Amount::from_sat(1));
        assert!(just_above_min > 20);
    }

    #[test]
    fn test_different_parameters() {
        // Test with different L value
        let config =
            DifficultyConfig::new(255, 17, Amount::ZERO, Amount::from_sat(5000), 25.).unwrap();
        assert_eq!(config.min_diff_start, 125000.0); // 0 + 25*5000

        // High balance should give min difficulty
        assert_eq!(calculate_difficulty(&config, Amount::from_sat(200_000)), 17);

        // Low balance should give max difficulty
        assert_eq!(calculate_difficulty(&config, Amount::ZERO), 255);
    }

    #[test]
    fn test_exact_linear_calculation() {
        let config =
            DifficultyConfig::new(255, 20, Amount::ZERO, Amount::from_sat(10_000), 10.).unwrap();

        // Manually calculate expected difficulty for x = 50000
        let x = 50000.0;
        let expected = config.precompute_big_a * x + config.precompute_big_b;
        let calculated = calculate_difficulty(&config, Amount::from_sat(50_000));

        assert_eq!(calculated, expected.round() as u8);
    }

    #[test]
    fn test_edge_case_equal_difficulties() {
        // Test when min and max difficulty are equal
        let config =
            DifficultyConfig::new(100, 100, Amount::ZERO, Amount::from_sat(10_000), 10.).unwrap();

        // Should always return 100
        assert_eq!(calculate_difficulty(&config, Amount::ZERO), 100);
        assert_eq!(calculate_difficulty(&config, Amount::from_sat(50_000)), 100);
        assert_eq!(
            calculate_difficulty(&config, Amount::from_sat(100_000)),
            100
        );
    }

    #[test]
    fn test_large_values() {
        let config = DifficultyConfig::new(
            255,
            20,
            Amount::from_sat(1000000),
            Amount::from_sat(100000),
            50.,
        )
        .unwrap();

        // Test with large balance values
        assert_eq!(
            calculate_difficulty(&config, Amount::from_sat(10_000_000)),
            20
        ); // Very high balance
        assert_eq!(
            calculate_difficulty(&config, Amount::from_sat(500_000)),
            255
        ); // Below min balance

        // Test in linear region
        let mid_balance = 3500000; // Roughly in the middle of linear region
        let diff = calculate_difficulty(&config, Amount::from_sat(mid_balance));
        assert!(diff > 20 && diff < 255);
    }
}
