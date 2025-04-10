use std::{collections::VecDeque, sync::Arc, time::Duration};

use bdk_wallet::bitcoin::{self, Amount};
use chrono::Utc;
use kanal::{unbounded_async, AsyncSender, SendError};
use parking_lot::{RwLock, RwLockWriteGuard};
use serde::{Deserialize, Serialize};
use terrors::OneOf;
use tokio::{
    select, spawn,
    task::{spawn_blocking, JoinHandle},
    time::interval,
};
use tracing::{error, info, info_span, Instrument};

use crate::l1::{fee_rate, L1Wallet, Persister, ESPLORA_CLIENT};

pub enum PayoutRequest {
    L1(L1PayoutRequest),
}

pub struct L1PayoutRequest {
    pub address: bitcoin::Address,
    pub amount: Amount,
}

pub struct Batcher {
    task: Option<JoinHandle<()>>,
    payout_sender: Option<AsyncSender<PayoutRequest>>,
    cfg: BatcherConfig,
}

#[derive(Debug)]
pub struct BatcherNotStarted;

#[derive(Debug)]
#[allow(dead_code)]
pub struct BatcherNotAvailable(SendError);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BatcherConfig {
    /// How long the period for transaction batching is.
    ///
    /// Defaults to `180` seconds.
    pub period: Duration,

    /// Maximum number of transactions to batch per batching period.
    ///
    /// Defaults to `250`.
    pub max_per_tx: usize,

    /// Maximum number of requests to allow in memory at a time.
    ///
    /// Defaults to `2_500`.
    pub max_in_flight: usize,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            period: Duration::from_secs(180),
            max_per_tx: 250,
            max_in_flight: 2500,
        }
    }
}

impl Batcher {
    /// Creates a new `Batcher`.
    /// You should call `Batcher::start` after this to start the batcher task,
    /// otherwise the batcher won't do anything.
    pub fn new(cfg: BatcherConfig) -> Self {
        Self {
            task: None,
            payout_sender: None,
            cfg,
        }
    }

    pub fn start(&mut self, l1_wallet: Arc<RwLock<L1Wallet>>) {
        let (tx, rx) = unbounded_async();

        let cfg = self.cfg.clone();

        let span = info_span!("batcher");
        let batcher_task = spawn(async move {
            let mut batch_interval = interval(cfg.period);
            let mut l1_payout_queue: VecDeque<L1PayoutRequest> = VecDeque::new();

            loop {
                select! {
                    // biased to ensure that even if we have incoming requests, they don't block
                    // each batch from being built when it's scheduled
                    biased;
                    instant = batch_interval.tick() => {
                        if l1_payout_queue.is_empty() {
                            continue
                        }
                        let span = info_span!("batch processing", batch = ?instant);
                        let _guard = span.enter();

                        let mut l1w = l1_wallet.write();

                        let mut psbt = l1w.build_tx();
                        psbt.fee_rate(fee_rate());
                        let num_to_deque = cfg.max_per_tx.min(l1_payout_queue.len());
                        let mut total_sent = Amount::ZERO;
                        for req in l1_payout_queue.drain(..num_to_deque) {
                            psbt.add_recipient(req.address.script_pubkey(), req.amount);
                            total_sent += req.amount;
                        }
                        let mut psbt = match psbt.finish() {
                            Ok(psbt) => psbt,
                            Err(e) => {
                                error!("failed finalizing tx: {e:?}");
                                continue;
                            }
                        };

                        let l1w = RwLockWriteGuard::downgrade(l1w);

                        l1w.sign(&mut psbt, Default::default())
                            .expect("signing should not fail");
                        let tx = psbt.extract_tx().expect("fully signed psbt");

                        let l1_wallet = l1_wallet.clone();
                        let span = info_span!("broadcast l1 tx", batch = ?instant);
                        spawn(async move {
                            if let Err(e) = ESPLORA_CLIENT.broadcast(&tx).await {
                                error!("error broadcasting tx: {e:?}");
                            }
                            info!("sent {total_sent} to {num_to_deque} requestors");
                            // triple nested spawn!
                            spawn_blocking(move || {
                                let mut l1w = l1_wallet.write();
                                l1w.apply_unconfirmed_txs([(tx, Utc::now().timestamp() as u64)]);
                                l1w.persist(&mut Persister).expect("persist should work");
                            })
                            .await
                            .expect("successful blocking update");
                        }.instrument(span));
                    }
                    req = rx.recv() => match req {
                        Ok(req) => match req {
                            PayoutRequest::L1(req) => if l1_payout_queue.len() < cfg.max_in_flight {
                                l1_payout_queue.push_back(req)
                            }
                        },
                        Err(e) => error!("error receiving PayoutRequest: {e:?}")
                    }
                }
            }
        }.instrument(span));

        self.task = Some(batcher_task);
        self.payout_sender = Some(tx);
    }

    pub async fn queue_payout_request(
        &self,
        req: PayoutRequest,
    ) -> Result<(), OneOf<(BatcherNotStarted, BatcherNotAvailable)>> {
        let tx = self
            .payout_sender
            .as_ref()
            .ok_or(OneOf::new(BatcherNotStarted))?
            .clone();

        tx.send(req)
            .await
            .map_err(|e| OneOf::new(BatcherNotAvailable(e)))?;

        Ok(())
    }
}
