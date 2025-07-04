//! A simple faucet server that uses [`axum`] and [`bdk_wallet`]
//! to generate and dispense bitcoin.

mod batcher;
pub mod l1;
pub mod l2;
pub mod macros;
pub mod pow;
pub mod seed;
pub mod settings;

use std::{
    env,
    net::{IpAddr, SocketAddr},
    sync::{Arc, LazyLock},
};

use alloy::{
    consensus::constants::ETH_TO_WEI,
    network::TransactionBuilder,
    primitives::{Address as L2Address, U256},
    providers::Provider,
    rpc::types::TransactionRequest,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use axum_client_ip::ClientIp;
use batcher::{Batcher, L1PayoutRequest, PayoutRequest};
use bdk_wallet::{
    bitcoin::{address::NetworkUnchecked, Address as L1Address, Amount},
    KeychainKind,
};
use l1::{L1Wallet, Persister};
use l2::L2Wallet;
use parking_lot::RwLock;
use pow::{Challenge, Nonce, Solution};
use seed::SavableSeed;
use serde::{Deserialize, Serialize};
use settings::SETTINGS;
use shrex::Hex;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

pub struct AppState {
    l1_wallet: Arc<RwLock<L1Wallet>>,
    l2_wallet: L2Wallet,
    batcher: Batcher,
}

pub static CRATE_NAME: LazyLock<String> =
    LazyLock::new(|| env!("CARGO_PKG_NAME").replace("-", "_"));

const BTC_TO_SATS: u64 = 100_000_000;
const BTC_TO_WEI: u128 = ETH_TO_WEI;
const SATS_TO_WEI: u64 = (BTC_TO_WEI / BTC_TO_SATS as u128) as u64;

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

    let seed = SavableSeed::load_or_create().expect("seed load should work");

    let mut l1_wallet =
        L1Wallet::new(SETTINGS.network, &seed).expect("l1 wallet creation to succeed");
    let l1_address = l1_wallet.reveal_next_address(KeychainKind::External);
    l1_wallet
        .persist(&mut Persister)
        .expect("successful persist");
    info!("L1 address: {}", l1_address.address);
    l1::spawn_fee_rate_task();

    let l2_wallet = L2Wallet::new(&seed).expect("l2 wallet creation to succeed");
    let l1_wallet = Arc::new(RwLock::new(l1_wallet));
    let mut batcher = Batcher::new(SETTINGS.batcher.clone());
    batcher.start(l1_wallet.clone());

    L1Wallet::spawn_syncer(l1_wallet.clone());

    let state = Arc::new(AppState {
        l1_wallet,
        l2_wallet,
        batcher,
    });

    let app = Router::new()
        .route("/pow_challenge/{chain}", get(get_pow_challenge))
        .route("/claim_l1/{solution}/{address}", get(claim_l1))
        .route("/claim_l2/{solution}/{address}", get(claim_l2))
        .route("/balance/{chain}", get(get_balance))
        .route("/sats_to_claim/{chain}", get(get_sats_per_claim))
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
pub struct ProvidedChallenge {
    nonce: Hex<Nonce>,
    difficulty: u8,
}

/// Which chain the faucet is reasoning about.
#[derive(Debug)]
enum Chain {
    L1,
    L2,
}

impl TryFrom<&str> for Chain {
    type Error = (StatusCode, String);

    fn try_from(level: &str) -> Result<Self, Self::Error> {
        match level {
            "l1" => Ok(Chain::L1),
            "l2" => Ok(Chain::L2),
            _ => Err((
                StatusCode::BAD_REQUEST,
                "Invalid chain. Must be 'l1' or 'l2'".to_string(),
            )),
        }
    }
}

async fn get_pow_challenge(
    ClientIp(ip): ClientIp,
    Path(chain): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ProvidedChallenge>, (StatusCode, String)> {
    let chain = Chain::try_from(chain.as_str())?;

    let (need, balance) = match chain {
        Chain::L1 => {
            let bal = state.l1_wallet.read().balance().trusted_spendable();
            (SETTINGS.l1_sats_per_claim, bal)
        }
        Chain::L2 => {
            let wei_bal = state
                .l2_wallet
                .get_default_signer_balance()
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            let sats_bal = (wei_bal / (SATS_TO_WEI as u128)) as u64;
            let bal = Amount::from_sat(sats_bal);
            (SETTINGS.l2_sats_per_claim, bal)
        }
    };

    if balance < need {
        let error_string = format!("Insufficient {chain:?} funds. Has {balance}, needs {need}.");
        return Err((StatusCode::SERVICE_UNAVAILABLE, error_string));
    };

    if let IpAddr::V4(ip) = ip {
        let difficulty = pow::calculate_difficulty(
            balance.to_btc() as f32,
            u8::MAX as f32,
            SETTINGS.pow.min_difficulty as f32,
            SETTINGS.pow.min_balance.to_btc() as f32,
            need.to_btc() as f32,
        ) as u8;
        let challenge = Challenge::get(&ip, difficulty);
        Ok(Json(ProvidedChallenge {
            nonce: Hex(challenge.nonce()),
            difficulty: challenge.difficulty(),
        }))
    } else {
        Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "IPV6 is not supported at the moment".to_string(),
        ))
    }
}

async fn claim_l1(
    ClientIp(ip): ClientIp,
    Path((solution, address)): Path<(Hex<Solution>, L1Address<NetworkUnchecked>)>,
    State(state): State<Arc<AppState>>,
) -> Result<(), (StatusCode, String)> {
    let IpAddr::V4(ip) = ip else {
        return Err((
            StatusCode::BAD_REQUEST,
            "IPV6 is not supported at this time".to_string(),
        ));
    };

    // num hashes on average to solve challenge: 2^15
    if let Err(e) = Challenge::check_solution(&ip, solution.0) {
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }

    let address = address.require_network(SETTINGS.network).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "wrong address network type".to_string(),
        )
    })?;

    state
        .batcher
        .queue_payout_request(PayoutRequest::L1(L1PayoutRequest {
            address,
            amount: SETTINGS.l1_sats_per_claim,
        }))
        .await
        .expect("successful queuing");

    Ok(())
}

async fn claim_l2(
    ClientIp(ip): ClientIp,
    Path((solution, address)): Path<(Hex<Solution>, L2Address)>,
    State(state): State<Arc<AppState>>,
) -> Result<String, (StatusCode, String)> {
    let IpAddr::V4(ip) = ip else {
        return Err((
            StatusCode::BAD_REQUEST,
            "IPV6 is not unavailable".to_string(),
        ));
    };

    // num hashes on average to solve challenge: 2^15
    if let Err(e) = Challenge::check_solution(&ip, solution.0) {
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }

    let tx = TransactionRequest::default()
        .with_to(address)
        // 1 btc == 1 "eth" => 1 sat = 1e10 "wei"
        .with_value(U256::from(
            SETTINGS.l2_sats_per_claim.to_sat() * SATS_TO_WEI,
        ));

    let txid = match state.l2_wallet.send_transaction(tx).await {
        Ok(r) => *r.tx_hash(),
        Err(e) => {
            error!("error sending transaction: {e:?}");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "error sending tx".to_owned(),
            ));
        }
    };

    info!("l2 claim to {address} via tx {}", txid);

    Ok(txid.to_string())
}

async fn get_balance(
    State(state): State<Arc<AppState>>,
    Path(chain): Path<String>,
) -> Result<String, (StatusCode, String)> {
    let bal = match Chain::try_from(chain.as_str())? {
        Chain::L1 => state
            .l1_wallet
            .read()
            .balance()
            .trusted_spendable()
            .to_sat()
            .to_string(),
        Chain::L2 => state
            .l2_wallet
            .get_default_signer_balance()
            .await
            .map(|x| x.to_string())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?,
    };
    Ok(bal)
}

async fn get_sats_per_claim(Path(chain): Path<String>) -> Result<String, (StatusCode, String)> {
    let claim_level = Chain::try_from(chain.as_str())?;

    let sats = match claim_level {
        Chain::L1 => SETTINGS.l1_sats_per_claim.to_sat(),
        Chain::L2 => SETTINGS.l2_sats_per_claim.to_sat(),
    };

    Ok(sats.to_string())
}

#[cfg(test)]
mod tests {
    use tokio::test;

    use super::*;

    #[test]
    async fn test_sats_to_claim_l1() {
        let result = get_sats_per_claim(Path("l1".to_string())).await;
        assert_eq!(result, Ok(SETTINGS.l1_sats_per_claim.to_sat().to_string()));
    }

    #[test]
    async fn test_sats_to_claim_l2() {
        let result = get_sats_per_claim(Path("l2".to_string())).await;
        assert_eq!(result, Ok(SETTINGS.l2_sats_per_claim.to_sat().to_string()));
    }

    #[test]
    async fn test_sats_to_claim_invalid() {
        let result = get_sats_per_claim(Path("invalid".to_string())).await;
        assert_eq!(
            result,
            Err((
                StatusCode::BAD_REQUEST,
                "Invalid chain. Must be 'l1' or 'l2'".to_string()
            ))
        );
    }
}
