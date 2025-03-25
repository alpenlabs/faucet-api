use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    str::FromStr,
    sync::LazyLock,
};

use axum_client_ip::SecureClientIpSource;
use bdk_wallet::bitcoin::{Amount, Network};
use config::Config;
use serde::{Deserialize, Serialize};

use crate::{batcher::BatcherConfig, pow::PowConfig, CRATE_NAME};

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
        .try_deserialize::<InternalSettings>()
        .expect("a valid config")
        .try_into()
        .expect("invalid config")
});

#[derive(Serialize, Deserialize)]
pub struct InternalSettings {
    /// Host to listen for HTTP requests on
    pub host: Option<IpAddr>,
    /// Port to listen for HTTP requests on
    pub port: Option<u16>,
    /// How the server should determine the client's IP address
    pub ip_src: SecureClientIpSource,
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
    pub l1_sats_per_claim: Amount,
    pub l2_sats_per_claim: Amount,
    /// Transaction batching configuration
    pub batcher: Option<BatcherConfig>,
    /// POW configuration
    pub pow: Option<PowConfig>,
}

#[derive(Debug)]
/// Settings struct filled with either config values or
/// opinionated defaults
pub struct Settings {
    pub host: IpAddr,
    pub port: u16,
    pub ip_src: SecureClientIpSource,
    pub seed_file: PathBuf,
    pub sqlite_file: PathBuf,
    pub network: Network,
    pub esplora: String,
    pub l2_http_endpoint: String,
    pub l1_sats_per_claim: Amount,
    pub l2_sats_per_claim: Amount,
    pub batcher: BatcherConfig,
    pub pow: PowConfig,
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

impl TryFrom<InternalSettings> for Settings {
    type Error = SettingsError;

    fn try_from(internal: InternalSettings) -> Result<Self, Self::Error> {
        if internal.l1_sats_per_claim > MAX_SATS_PER_CLAIM {
            panic!("L1 sats per claim is too high, max is {MAX_SATS_PER_CLAIM}");
        }
        if internal.l2_sats_per_claim > MAX_SATS_PER_CLAIM {
            panic!("L2 sats per claim is too high, max is {MAX_SATS_PER_CLAIM}");
        }

        Ok(Self {
            host: internal.host.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            port: internal.port.unwrap_or(3000),
            ip_src: internal.ip_src,
            seed_file: PathBuf::from_str(&internal.seed_file.unwrap_or("faucet.seed".to_owned()))
                .map_err(|e| SettingsError::InvalidSeedPath(e.to_string()))?,
            sqlite_file: PathBuf::from_str(
                &internal.sqlite_file.unwrap_or("faucet.sqlite".to_owned()),
            )
            .map_err(|e| SettingsError::InvalidDatabasePath(e.to_string()))?,
            network: internal.network.unwrap_or(Network::Signet),
            esplora: internal.esplora,
            l2_http_endpoint: internal.l2_http_endpoint,
            l1_sats_per_claim: internal.l1_sats_per_claim,
            l2_sats_per_claim: internal.l2_sats_per_claim,
            batcher: internal.batcher.unwrap_or_default(),
            pow: internal
                .pow
                .inspect(|c| c.validate().unwrap())
                .unwrap_or_default(),
        })
    }
}
