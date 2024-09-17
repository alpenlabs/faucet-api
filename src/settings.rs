use std::{net::{IpAddr, Ipv4Addr}, path::PathBuf, sync::LazyLock};

use axum_client_ip::SecureClientIpSource;
use bdk_wallet::bitcoin::{Amount, Network};
use config::Config;
use serde::{Deserialize, Serialize};

use crate::CRATE_NAME;

pub static SETTINGS: LazyLock<Settings> = LazyLock::new(|| {
    let args = std::env::args().collect::<Vec<_>>();

    let settings_path = match (args.get(1), args.get(2)) {
        (Some(a1), Some(a2)) if a1 == "--config" || a1 == "-c" => Some(PathBuf::try_from(a2).expect("failed to parse config path")),
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
        .into()
});

#[derive(Serialize, Deserialize)]
pub struct InternalSettings {
    pub host: Option<IpAddr>,
    pub port: Option<u16>,
    pub ip_src: SecureClientIpSource,
    pub seed_file: Option<String>,
    pub sqlite_file: Option<String>,
    pub network: Option<Network>,
    pub esplora: Option<String>,
    pub sats_per_claim: Option<Amount>,
    pub pow_difficulty: Option<u8>,
}

#[derive(Serialize, Deserialize, Debug)]
/// Settings struct filled with either config values or
/// opinionated defaults
pub struct Settings {
    pub host: IpAddr,
    pub port: u16,
    pub ip_src: SecureClientIpSource,
    pub seed_file: String,
    pub sqlite_file: String,
    pub network: Network,
    pub esplora: String,
    pub sats_per_claim: Amount,
    pub pow_difficulty: u8,
}

impl From<InternalSettings> for Settings {
    fn from(internal: InternalSettings) -> Self {
        Self {
            host: internal.host.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            port: internal.port.unwrap_or(3000),
            ip_src: internal.ip_src,
            seed_file: internal.seed_file.unwrap_or("faucet.seed".to_owned()),
            sqlite_file: internal.sqlite_file.unwrap_or("faucet.sqlite".to_owned()),
            network: internal.network.unwrap_or(Network::Signet),
            esplora: internal
                .esplora
                .unwrap_or("https://explorer.bc-2.jp/api".to_owned()),
            sats_per_claim: internal
                .sats_per_claim
                .unwrap_or(Amount::from_sat(10_000_000)),
            pow_difficulty: internal.pow_difficulty.unwrap_or(17),
        }
    }
}
