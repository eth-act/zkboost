//! The submission of the proofs to the beacon node. The submitter makes the validator ready,
//! follows the beacon blocks that carry the payloads, and signs and posts each proof.

use std::{
    num::NonZeroUsize,
    pin::pin,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use ::metrics::Gauge;
use alloy_primitives::B256;
use anyhow::Context;
use bytes::Bytes;
use lighthouse_bls::Keypair;
use lru::LruCache;
use tokio::{task::spawn_blocking, time::sleep};
use tokio_stream::StreamExt;
use tracing::{error, info, warn};
use url::Url;

use crate::{
    beacon::{beacon_client::BeaconClient, validator::Validator},
    engine::payload::PayloadHeader,
    observability::metrics::{self, SubmissionStatus},
    proof::prover::Prover,
    util::RETRY_DELAY,
};

/// Slots of beacon block roots that are kept, two epochs, for a payload that is imported again
/// after a reorg.
const BLOCK_ROOT_SLOTS: usize = 64;

/// Attempts to submit a proof, `RETRY_DELAY` apart, which span less than the slot.
const SUBMISSION_ATTEMPTS: u32 = 5;

pub(crate) struct ProofSubmitter {
    beacon: BeaconClient,
    /// Set once the beacon node responds to the startup reads.
    validator: OnceLock<Validator>,
    validator_ready: Gauge,
    /// The root of the beacon block of a payload by its block hash and slot, from the events.
    block_roots: Mutex<LruCache<(B256, u64), B256>>,
}

impl ProofSubmitter {
    pub(crate) fn new(beacon_endpoint: Url) -> Self {
        Self {
            beacon: BeaconClient::new(beacon_endpoint),
            validator: OnceLock::new(),
            validator_ready: metrics::validator_ready(),
            block_roots: Mutex::new(LruCache::new(NonZeroUsize::new(BLOCK_ROOT_SLOTS).unwrap())),
        }
    }

    pub(crate) fn validator(&self) -> Option<&Validator> {
        self.validator.get()
    }

    /// Makes the validator of `keypair` ready, then follows the `execution_payload` events.
    pub(crate) async fn run(self: Arc<Self>, keypair: Keypair) {
        let validator = loop {
            match Validator::fetch(&self.beacon, keypair.clone()).await {
                Ok(validator) => break validator,
                Err(error) => {
                    warn!(error = %format!("{error:#}"), "waiting for the beacon node");
                    sleep(RETRY_DELAY).await;
                }
            }
        };
        self.validator.get_or_init(|| validator);
        self.validator_ready.set(1.0);
        info!("validator ready");

        loop {
            let mut events = pin!(self.beacon.execution_payload_events());
            while let Some(event) = events.next().await {
                let mut block_roots = self.block_roots.lock().unwrap();
                let key = (event.block_hash, event.slot);
                // The first beacon block of a payload is kept.
                if *block_roots.get_or_insert(key, || event.block_root) != event.block_root {
                    warn!(block_hash = %event.block_hash, slot = event.slot, "second beacon block of the payload ignored");
                }
            }
            // The event source ends its stream at a response other than an event stream.
            sleep(RETRY_DELAY).await;
        }
    }

    /// Submits a proof to the beacon node with `SUBMISSION_ATTEMPTS` attempts, and returns the
    /// duration of an accepted submission.
    pub(crate) async fn submit(
        self: &Arc<Self>,
        prover: &Prover,
        header: PayloadHeader,
        proof: Bytes,
    ) -> Option<Duration> {
        let proof_type = prover.proof_type;
        let started = Instant::now();
        for attempt in 1..=SUBMISSION_ATTEMPTS {
            match self.try_submit(prover, header, proof.clone()).await {
                Ok(()) => {
                    let duration = started.elapsed();
                    info!(%proof_type, "proof submitted");
                    prover
                        .metrics
                        .submitted(SubmissionStatus::Success, duration);
                    return Some(duration);
                }
                Err(error) if attempt < SUBMISSION_ATTEMPTS => {
                    warn!(%proof_type, attempt, error = %format!("{error:#}"), "proof submission retried");
                    sleep(RETRY_DELAY).await;
                }
                Err(error) => {
                    error!(%proof_type, error = %format!("{error:#}"), "proof submission failed");
                    prover
                        .metrics
                        .submitted(SubmissionStatus::Error, started.elapsed());
                }
            }
        }
        None
    }

    async fn try_submit(
        self: &Arc<Self>,
        prover: &Prover,
        header: PayloadHeader,
        proof: Bytes,
    ) -> anyhow::Result<()> {
        let key = (header.block_hash, header.slot);
        let announced = self.block_roots.lock().unwrap().get(&key).copied();
        let beacon_block_root = match announced {
            Some(beacon_block_root) => beacon_block_root,
            None => {
                let parent = header.parent_beacon_block_root;
                let listed = (self.beacon)
                    .beacon_block_root(header.block_hash, parent, header.slot)
                    .await?;
                // An event during the read comes first, so every proof of the payload names
                // one beacon block.
                *(self.block_roots.lock().unwrap()).get_or_insert(key, || listed)
            }
        };
        let (submitter, proof_type) = (self.clone(), prover.proof_type);
        let envelopes = spawn_blocking(move || {
            let validator = submitter.validator().context("validator not ready")?;
            validator.sign(beacon_block_root, header.slot, proof_type, &proof)
        });
        self.beacon.post_execution_proofs(envelopes.await??).await
    }
}
