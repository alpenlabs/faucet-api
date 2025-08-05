use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    str::FromStr,
    sync::LazyLock,
    time::Duration,
};

use axum_client_ip::ClientIpSource;
use bdk_wallet::bitcoin::{Amount, Network};
use config::Config;
use serde::{Deserialize, Serialize};

use crate::{batcher::BatcherConfig, CRATE_NAME};

pub static SETTINGS: LazyLock<Settings> = LazyLock::new(|| {
    let args = std::env::args().collect::<Vec<_>>();

    let settings_path = match (args.get(1), args.get(2)) {
        (Some(a1), Some(a2)) if a1 == "--config" || a1 == "-c" => Some(PathBuf::from(a2)),
        _ => None,
    };

    let mut builder = Config::builder();
    if let Some(path) = settings_path {
        builder = builder.add_source(config::File::from(path));
    } else {
        builder = builder.add_source(config::File::with_name("faucet.toml"))
    }
    builder
        // Add in settings from the environment (with a prefix of CRATE_NAME)
        .add_source(config::Environment::with_prefix(&CRATE_NAME.to_uppercase()))
        .build()
        .expect("a valid config")
        .try_deserialize::<ReadableSettings>()
        .expect("a valid config")
        .try_into()
        .expect("invalid config")
});

#[derive(Serialize, Deserialize)]
pub struct ReadableSettings {
    /// Host to listen for HTTP requests on
    pub host: Option<IpAddr>,
    /// Port to listen for HTTP requests on
    pub port: Option<u16>,
    /// How the server should determine the client's IP address
    pub ip_src: ClientIpSource,
    /// Path to the seed file which stores the wallet's seed/master bytes
    pub seed_file: Option<String>,
    /// Path to the SQLite database file which stores the wallet's data
    pub sqlite_file: Option<String>,
    /// Network to use for the wallet. Defaults to [`Network::Signet`]
    pub network: Option<Network>,
    /// URL of the esplora API to use for the wallet. Should not have a trailing slash
    pub esplora: String,
    /// URL of the EVM L2 HTTP endpoint to use for the wallet. Should not have a trailing slash
    pub l2_http_endpoint: String,
    /// Transaction batching configuration
    pub batcher: Option<BatcherConfig>,
    pub l1: ReadableLayerConfig,
    pub l2: ReadableLayerConfig,
}

#[derive(Debug)]
/// Settings struct filled with either config values or
/// opinionated defaults
pub struct Settings {
    pub host: IpAddr,
    pub port: u16,
    pub ip_src: ClientIpSource,
    pub seed_file: PathBuf,
    pub sqlite_file: PathBuf,
    pub network: Network,
    pub esplora: String,
    pub l2_http_endpoint: String,
    pub batcher: BatcherConfig,
    pub l1: LayerConfig,
    pub l2: LayerConfig,
}

// on L2, we represent 1 btc as 1 "eth" on the rollup
// that means 1 sat = 1e10 "wei"
// we have to store the amount we send in wei as a u64,
// so this is a safety check.
const MAX_SATS_PER_CLAIM: Amount = Amount::from_sat(u64::MAX / 10u64.pow(10));

#[derive(Debug)]
pub enum SettingsError {
    /// `sats_per_claim` is too high.
    TooHighSatsPerClaim,
    /// `sats_per_claim` is too low.
    TooLowSatsPerClaim,
    /// Invalid seed path.
    InvalidSeedPath(String),
    /// Invalid database path.
    InvalidDatabasePath(String),
}

impl TryFrom<ReadableSettings> for Settings {
    type Error = SettingsError;

    fn try_from(read_settings: ReadableSettings) -> Result<Self, Self::Error> {
        if read_settings.l1.amount_per_claim > MAX_SATS_PER_CLAIM {
            panic!("L1 sats per claim is too high, max is {MAX_SATS_PER_CLAIM}");
        }
        if read_settings.l2.amount_per_claim > MAX_SATS_PER_CLAIM {
            panic!("L2 sats per claim is too high, max is {MAX_SATS_PER_CLAIM}");
        }

        Ok(Self {
            host: read_settings
                .host
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            port: read_settings.port.unwrap_or(3000),
            ip_src: read_settings.ip_src,
            seed_file: PathBuf::from_str(
                &read_settings.seed_file.unwrap_or("faucet.seed".to_owned()),
            )
            .map_err(|e| SettingsError::InvalidSeedPath(e.to_string()))?,
            sqlite_file: PathBuf::from_str(
                &read_settings
                    .sqlite_file
                    .unwrap_or("faucet.sqlite".to_owned()),
            )
            .map_err(|e| SettingsError::InvalidDatabasePath(e.to_string()))?,
            network: read_settings.network.unwrap_or(Network::Signet),
            esplora: read_settings.esplora,
            l2_http_endpoint: read_settings.l2_http_endpoint,
            batcher: read_settings.batcher.unwrap_or_default(),
            l1: read_settings.l1.into(),
            l2: read_settings.l2.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadableLayerConfig {
    /// Minimum difficulty required for a user to claim funds.
    ///
    /// Defaults to `18`.
    ///
    /// Users will have to solve a POW challenge with a chance of finding of
    /// `1 / 2^min_difficulty` per random guess. The faucet will dynamically adjust
    /// the actual difficulty given to the user based on the current balance,
    /// `min_balance` and `sats_per_claim`.
    pub min_difficulty: Option<u8>,

    /// Maximum difficulty cap that the faucet will adjust to.
    ///
    /// Defaults to 64.
    pub max_difficulty: Option<u8>,

    /// Minimum balance to keep in the faucet
    ///
    /// Defaults to `0` BTC.
    /// When configuring in the config file, this value
    /// should be in sats as a number.
    pub min_balance: Option<Amount>,

    /// Amount of sats release per claim to the user.
    pub amount_per_claim: Amount,

    /// Adjusts how aggressive the faucet is at ramping POW difficulty when getting close
    /// to the minimum balance. See docs/pow.md to see how this works.
    ///
    /// Defaults to `20`.
    pub difficulty_increase_coeff: Option<f32>,

    /// How long a challenge is valid for.
    ///
    /// Defaults to `120` seconds.
    ///
    /// In config, this should be provided as an object with fields `secs` and `nanos` with integers.
    /// For example:
    ///
    /// ```toml
    /// challenge_duration = { secs = 120, nanos = 0 }
    /// ```
    pub challenge_duration: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayerConfig {
    /// Minimum difficulty required for a user to claim funds.
    ///
    /// Users will have to solve a POW challenge with a chance of finding of
    /// `1 / 2^min_difficulty` per random guess. The faucet will dynamically adjust
    /// the actual difficulty given to the user based on the current balance,
    /// `min_balance` and `sats_per_claim`.
    pub min_difficulty: u8,

    /// Maximum difficulty cap that the faucet will adjust to.
    ///
    /// Defaults to 64.
    pub max_difficulty: u8,

    /// Minimum balance to keep in the faucet
    pub min_balance: Amount,

    /// Amount of sats release per claim to the user.
    pub amount_per_claim: Amount,

    /// Adjusts how aggressive the faucet is at ramping POW difficulty when getting close
    /// to the minimum balance. See docs/pow.md to see how this works.
    pub difficulty_increase_coeff: f32,

    /// How long a challenge is valid for.
    pub challenge_duration: Duration,
}

impl From<ReadableLayerConfig> for LayerConfig {
    fn from(value: ReadableLayerConfig) -> Self {
        Self {
            min_difficulty: value.min_difficulty.unwrap_or(18),
            max_difficulty: value.max_difficulty.unwrap_or(64),
            min_balance: value.min_balance.unwrap_or(Amount::ZERO),
            amount_per_claim: value.amount_per_claim,
            difficulty_increase_coeff: value.difficulty_increase_coeff.unwrap_or(20.),
            challenge_duration: value.challenge_duration.unwrap_or(Duration::from_secs(120)),
        }
    }
}
