use std::{
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    str::FromStr,
    sync::LazyLock,
    time::Duration,
};

use axum_client_ip::SecureClientIpSource;
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
        .try_deserialize::<InternalSettings>()
        .expect("a valid config")
        .try_into()
        .expect("invalid config")
});

#[derive(Serialize, Deserialize)]
pub struct InternalSettings {
    pub host: Option<IpAddr>,
    pub port: Option<u16>,
    pub ip_src: SecureClientIpSource,
    pub seed_file: Option<String>,
    pub sqlite_file: Option<String>,
    pub network: Option<Network>,
    pub esplora: String,
    pub l2_http_endpoint: String,
    pub sats_per_claim: Amount,
    pub pow_difficulty: u8,
    pub batcher_period: Option<u64>,
    pub batcher_max_per_batch: Option<usize>,
    pub batcher_max_in_flight: Option<usize>,
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
    pub sats_per_claim: Amount,
    pub pow_difficulty: u8,
    pub batcher: BatcherConfig,
}

// on L2, we represent 1 btc as 1 "eth" on the rollup
// that means 1 sat = 1e10 "wei"
// we have to store the amount we send in wei as a u64,
// so this is a safety check.
const MAX_SATS_PER_CLAIM: Amount = Amount::from_sat(u64::MAX / 10u64.pow(10));

impl TryFrom<InternalSettings> for Settings {
    type Error = <PathBuf as FromStr>::Err;

    fn try_from(internal: InternalSettings) -> Result<Self, Self::Error> {
        if internal.sats_per_claim > MAX_SATS_PER_CLAIM {
            panic!("sats per claim is too high, max is {MAX_SATS_PER_CLAIM}");
        }
        Ok(Self {
            host: internal.host.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            port: internal.port.unwrap_or(3000),
            ip_src: internal.ip_src,
            seed_file: PathBuf::from_str(&internal.seed_file.unwrap_or("faucet.seed".to_owned()))?,
            sqlite_file: PathBuf::from_str(
                &internal.sqlite_file.unwrap_or("faucet.sqlite".to_owned()),
            )?,
            network: internal.network.unwrap_or(Network::Signet),
            esplora: internal.esplora,
            l2_http_endpoint: internal.l2_http_endpoint,
            sats_per_claim: internal.sats_per_claim,
            pow_difficulty: internal.pow_difficulty,
            batcher: BatcherConfig {
                period: Duration::from_secs(internal.batcher_period.unwrap_or(30)),
                max_per_tx: internal.batcher_max_per_batch.unwrap_or(250),
                max_in_flight: internal.batcher_max_in_flight.unwrap_or(2500),
            },
        })
    }
}
