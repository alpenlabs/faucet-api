//! A simple faucet server that uses [`axum`] and [`bdk_wallet`]
//! to generate and dispense bitcoin.

pub mod hex;
pub mod l1;
pub mod macros;
pub mod pow;
pub mod seed;
pub mod settings;

use std::{
    env,
    net::{IpAddr, SocketAddr},
    sync::{Arc, LazyLock},
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use axum_client_ip::SecureClientIp;
use bdk_wallet::bitcoin::{address::NetworkUnchecked, Address};
use hex::Hex;
use l1::{fee_rate, L1Wallet, ESPLORA_CLIENT};
use parking_lot::{RwLock, RwLockWriteGuard};
use pow::{Challenge, Nonce, Solution};
use serde::{Deserialize, Serialize};
use settings::SETTINGS;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

pub struct AppState {
    l1_wallet: RwLock<L1Wallet>,
}

pub static CRATE_NAME: LazyLock<String> =
    LazyLock::new(|| env!("CARGO_PKG_NAME").replace("-", "_"));

#[tokio::main]
async fn main() {
    let builder = tracing_subscriber::fmt();
    if let Ok(level) = std::env::var("RUST_LOG") {
        builder
            .with_env_filter(EnvFilter::new(format!("{}={level}", *CRATE_NAME,)))
            .init();
    } else {
        builder.init();
    }

    let (host, port) = (SETTINGS.host, SETTINGS.port);

    let l1_wallet =
        L1Wallet::load_or_create(SETTINGS.network).expect("l1 wallet creation to succeed");
    l1::spawn_fee_rate_task();

    let state = Arc::new(AppState {
        l1_wallet: l1_wallet.into(),
    });

    L1Wallet::spawn_syncer(state.clone());

    let app = Router::new()
        .route("/pow_challenge", get(get_pow_challenge))
        .route("/claim_l1/:solution/:address", get(claim_l1))
        .layer(SETTINGS.ip_src.clone().into_extension())
        .with_state(state);

    let listener = TcpListener::bind((host, port)).await.unwrap();
    info!("listening on http://{host}:{port}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PowChallenge {
    nonce: Hex<Nonce>,
    difficulty: u8,
}

async fn get_pow_challenge(
    SecureClientIp(ip): SecureClientIp,
) -> Result<Json<PowChallenge>, (StatusCode, &'static str)> {
    if let IpAddr::V4(ip) = ip {
        Ok(Json(PowChallenge {
            nonce: Hex(Challenge::get(&ip).nonce()),
            difficulty: SETTINGS.pow_difficulty,
        }))
    } else {
        Err((StatusCode::SERVICE_UNAVAILABLE, "IPV6 is not unavailable"))
    }
}

async fn claim_l1(
    SecureClientIp(ip): SecureClientIp,
    Path((solution, address)): Path<(Hex<Solution>, Address<NetworkUnchecked>)>,
    State(state): State<Arc<AppState>>,
) -> Result<(), (StatusCode, String)> {
    let IpAddr::V4(ip) = ip else {
        return Err((
            StatusCode::BAD_REQUEST,
            "IPV6 is not unavailable".to_string(),
        ));
    };

    // num hashes on average to solve challenge: 2^15
    if !Challenge::valid(&ip, SETTINGS.pow_difficulty, solution.0) {
        return Err((StatusCode::BAD_REQUEST, "Bad solution".to_string()));
    }

    let address = address.require_network(SETTINGS.network).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "wrong address network type".to_string(),
        )
    })?;

    let psbt = {
        let mut l1w = state.l1_wallet.write();
        let balance = l1w.balance();
        if balance.trusted_spendable() < SETTINGS.sats_per_claim {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "not enough bitcoin in the faucet".to_owned(),
            ));
        }
        let mut psbt = l1w
            .build_tx()
            .fee_rate(fee_rate())
            .enable_rbf()
            .add_recipient(address.script_pubkey(), SETTINGS.sats_per_claim)
            .clone()
            .finish()
            .expect("transaction to be constructed");
        let l1w = RwLockWriteGuard::downgrade(l1w);
        l1w.sign(&mut psbt, Default::default())
            .expect("signing should not fail");
        psbt
    };

    let tx = psbt.extract_tx().map_err(|e| {
        error!("error extracting tx: {e:?}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "error extracting tx".to_owned(),
        )
    })?;

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
