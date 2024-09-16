//! A simple faucet server that uses [`axum`] and [`bdk_wallet`]
//! to generate and dispense bitcoin.

pub mod hex;
pub mod l1;
pub mod macros;
pub mod pow;
pub mod seed;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Router,
};
use axum_client_ip::{SecureClientIp, SecureClientIpSource};
use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::{
    bitcoin::{address::NetworkUnchecked, Address, Amount, Network},
    KeychainKind,
};
use hex::Hex;
use l1::{fee_rate, L1Wallet, ESPLORA_CLIENT};
use parking_lot::{RwLock, RwLockWriteGuard};
use pow::{Challenge, Nonce};
use std::{
    collections::BTreeSet,
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::time::sleep;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

const NETWORK: Network = Network::Signet;
const ESPLORA_URL: &str = "https://explorer.bc-2.jp/api";
const SATS_PER_CLAIM: Amount = Amount::from_sat(1_000_000);

#[tokio::main]
async fn main() {
    let builder = tracing_subscriber::fmt();
    if let Ok(level) = std::env::var("RUST_LOG") {
        builder
            .with_env_filter(EnvFilter::new(&format!(
                "{}={level}",
                env!("CARGO_PKG_NAME").replace("-", "_"),
            )))
            .init();
    } else {
        builder.init();
    }

    let l1_wallet = L1Wallet::load_or_create(NETWORK).expect("l1 wallet creation to succeed");

    let (ip, port) = (
        env::var("HOST")
            .ok()
            .and_then(|h| IpAddr::from_str(&h).ok())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        env::var("PORT")
            .ok()
            .and_then(|h| u16::from_str(&h).ok())
            .unwrap_or(3000),
    );

    let app_state = AppState {
        l1_wallet: l1_wallet.into(),
    };

    let ip_source = env::var("IP_SRC")
        .ok()
        .and_then(|src| SecureClientIpSource::from_str(&src).ok())
        .unwrap_or(SecureClientIpSource::ConnectInfo);

    let state = Arc::new(app_state);
    let s2 = state.clone();
    tokio::spawn(async move {
        loop {
            let req = s2
                .l1_wallet
                .read()
                .start_full_scan()
                .inspect({
                    let mut once = BTreeSet::<KeychainKind>::new();
                    move |keychain, spk_i, _| {
                        if once.insert(keychain) {
                            debug!("Scanning keychain [{:?}]", keychain);
                        }
                        debug!(" {:<3}", spk_i);
                    }
                })
                .build();
            // do full scans because the miner is sending to our public descriptor
            let update = match ESPLORA_CLIENT.full_scan(req, 3, 10).await {
                Ok(u) => u,
                Err(e) => {
                    error!("{e:?}");
                    continue;
                }
            };
            {
                // in a separate block otherwise compiler gets upset that we're holding
                // this over the await point
                let mut l1w = s2.l1_wallet.write();
                l1w.apply_update(update)
                    .expect("should be able to connect to db");
                l1w.persist(&mut l1::Persister)
                    .expect("persist should work");
            }
            sleep(Duration::from_secs(30)).await;
        }
    });

    let app = Router::new()
        .route("/pow_challenge", get(get_pow_challenge))
        .route("/claim_l1", get(claim_l1))
        .layer(ip_source.into_extension())
        .with_state(state);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind((ip, port)).await.unwrap();
    info!("listening on http://{ip}:{port}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

pub struct AppState {
    l1_wallet: RwLock<L1Wallet>,
}

async fn get_pow_challenge(SecureClientIp(ip): SecureClientIp) -> Result<Hex<Nonce>, &'static str> {
    if let IpAddr::V4(ip) = ip {
        Ok(Hex(Challenge::get(&ip).nonce()))
    } else {
        Err("IPV6 is not unavailable")
    }
}

async fn claim_l1(
    SecureClientIp(ip): SecureClientIp,
    Path((solution, address)): Path<(u64, Address<NetworkUnchecked>)>,
    State(state): State<Arc<AppState>>,
) -> Result<(), (StatusCode, String)> {
    let IpAddr::V4(ip) = ip else {
        return Err((
            StatusCode::BAD_REQUEST,
            "IPV6 is not unavailable".to_string(),
        ));
    };

    // num hashes on average to solve challenge: 2^15
    if !Challenge::valid(&ip, 15, solution) {
        return Err((StatusCode::BAD_REQUEST, "Bad solution".to_string()));
    }

    let address = match address.require_network(NETWORK) {
        Ok(a) => a,
        Err(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "wrong address network type".to_string(),
            ))
        }
    };
    let psbt = {
        let mut l1w = state.l1_wallet.write();
        let balance = l1w.balance();
        if balance.trusted_spendable() < SATS_PER_CLAIM {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "not enough bitcoin in the faucet".to_owned(),
            ));
        }
        let mut psbt = l1w
            .build_tx()
            .fee_rate(fee_rate())
            .enable_rbf()
            .add_recipient(address.script_pubkey(), SATS_PER_CLAIM)
            .clone()
            .finish()
            .expect("transaction to be constructed");
        let l1w = RwLockWriteGuard::downgrade(l1w);
        l1w.sign(&mut psbt, Default::default())
            .expect("signing should not fail");
        psbt
    };
    let tx = match psbt.extract_tx() {
        Ok(tx) => tx,
        Err(e) => {
            error!("error extracting tx: {e:?}");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "error extracting tx".to_owned(),
            ));
        }
    };

    if let Err(e) = ESPLORA_CLIENT.broadcast(&tx).await {
        error!("error broadcasting tx: {e:?}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "error broadcasting".to_owned(),
        ));
    }

    info!("l1 claim to {address} via tx {}", tx.compute_txid());

    Ok(())
}
