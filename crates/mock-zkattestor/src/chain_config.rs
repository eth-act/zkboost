//! Resolves the chain config for a block from CL configuration.
//!
//! The CL exposes every field of the chain config except the blob schedule `target`
//! and `base_fee_update_fraction`, which are execution-layer genesis values. This resolver reads
//! the fork schedule from `/eth/v1/config/spec` and `/eth/v1/beacon/genesis` and emits a config
//! with those two fields zeroed. The zkboost server fills them from its own execution-layer genesis
//! and validates the pinned `max`.

use anyhow::Context;
use zkboost_types::{BlobSchedule, ChainConfig, ForkActivation, ForkConfig, ProtocolFork};

use crate::cl_client::{Genesis, Spec};

/// Sentinel fork epoch used by CL for a fork that is not scheduled.
const NOT_SCHEDULED: u64 = u64::MAX;

/// Resolves the active fork for a block from a precomputed list of fork activations.
#[derive(Debug, Clone)]
pub(crate) struct ChainConfigResolver {
    chain_id: u64,
    forks: Vec<ScheduledFork>,
}

/// A resolved fork activation with its blob schedule maximum, when the fork carries blobs.
#[derive(Debug, Clone)]
struct ScheduledFork {
    timestamp: u64,
    fork: ProtocolFork,
    blob_max: Option<u64>,
}

impl ChainConfigResolver {
    /// Builds the resolver from the consensus spec and genesis time.
    pub(crate) fn new(spec: Spec, genesis: Genesis) -> anyhow::Result<Self> {
        let epoch_seconds = spec.seconds_per_slot.saturating_mul(spec.slots_per_epoch);
        let activation = |epoch: u64| {
            genesis
                .genesis_time
                .saturating_add(epoch.saturating_mul(epoch_seconds))
        };

        let mut forks = Vec::new();

        let base_forks = [
            (spec.capella_fork_epoch, ProtocolFork::Shanghai, None),
            (
                spec.deneb_fork_epoch,
                ProtocolFork::Cancun,
                Some(spec.max_blobs_per_block),
            ),
            (
                spec.electra_fork_epoch,
                ProtocolFork::Prague,
                Some(spec.max_blobs_per_block_electra),
            ),
            (
                spec.fulu_fork_epoch,
                ProtocolFork::Osaka,
                Some(spec.max_blobs_per_block_electra),
            ),
        ];
        for (epoch, fork, blob_max) in base_forks {
            if epoch != NOT_SCHEDULED {
                forks.push(ScheduledFork {
                    timestamp: activation(epoch),
                    fork,
                    blob_max,
                });
            }
        }

        for (index, entry) in spec.blob_schedule.iter().enumerate() {
            if let Some(fork) = bpo_fork(index) {
                forks.push(ScheduledFork {
                    timestamp: activation(entry.epoch),
                    fork,
                    blob_max: Some(entry.max_blobs_per_block),
                });
            };
        }

        anyhow::ensure!(
            !forks.is_empty(),
            "consensus spec schedules no supported fork"
        );

        Ok(Self {
            chain_id: spec.deposit_chain_id,
            forks,
        })
    }

    /// Resolves the chain config for a block at the given execution timestamp. The blob schedule
    /// carries the consensus `max` with `target` and `base_fee_update_fraction` zeroed for the
    /// zkboost server to fill from its execution-layer genesis.
    pub(crate) fn resolve(&self, timestamp: u64) -> anyhow::Result<ChainConfig> {
        let active = self
            .forks
            .iter()
            .filter(|fork| fork.timestamp <= timestamp)
            .max_by_key(|fork| (fork.timestamp, fork.fork))
            .context("no fork active at block timestamp")?;

        Ok(ChainConfig {
            chain_id: self.chain_id,
            active_fork: ForkConfig::new(
                active.fork,
                ForkActivation::new(None, Some(active.timestamp)),
                active.blob_max.map(|max| BlobSchedule {
                    target: 0,
                    max,
                    base_fee_update_fraction: 0,
                }),
            ),
        })
    }
}

/// Maps a zero-based blob schedule index to its blob-parameter-only fork. Only BPO1 and BPO2 are
/// handled.
fn bpo_fork(index: usize) -> Option<ProtocolFork> {
    match index {
        0 => Some(ProtocolFork::BPO1),
        1 => Some(ProtocolFork::BPO2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use zkboost_types::ProtocolFork;

    use crate::{
        chain_config::{ChainConfigResolver, Spec},
        cl_client::Genesis,
    };

    fn spec_from(value: serde_json::Value) -> Spec {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn resolves_bpo1_when_all_forks_share_genesis() {
        let spec = spec_from(serde_json::json!({
            "DEPOSIT_CHAIN_ID": "3151908",
            "SECONDS_PER_SLOT": "12",
            "SLOTS_PER_EPOCH": "32",
            "CAPELLA_FORK_EPOCH": "0",
            "DENEB_FORK_EPOCH": "0",
            "ELECTRA_FORK_EPOCH": "0",
            "FULU_FORK_EPOCH": "0",
            "GLOAS_FORK_EPOCH": "18446744073709551615",
            "MAX_BLOBS_PER_BLOCK": "6",
            "MAX_BLOBS_PER_BLOCK_ELECTRA": "9",
            "BLOB_SCHEDULE": [{ "EPOCH": "0", "MAX_BLOBS_PER_BLOCK": "15" }],
        }));
        let genesis = Genesis {
            genesis_time: 1_000,
        };
        let resolver = ChainConfigResolver::new(spec, genesis).unwrap();
        let config = resolver.resolve(50_000).unwrap();

        assert_eq!(config.chain_id, 3151908);
        assert_eq!(config.active_fork.fork, ProtocolFork::BPO1);
        assert_eq!(config.active_fork.activation.timestamp(), Some(1_000));
        let blob = config.active_fork.blob_schedule().unwrap();
        assert_eq!(
            (blob.target, blob.max, blob.base_fee_update_fraction),
            (0, 15, 0)
        );
    }

    #[test]
    fn resolves_active_fork_by_timestamp() {
        // Capella at genesis, Deneb one epoch later (12 * 32 = 384 seconds after genesis).
        let spec = spec_from(serde_json::json!({
            "DEPOSIT_CHAIN_ID": "1",
            "SECONDS_PER_SLOT": "12",
            "SLOTS_PER_EPOCH": "32",
            "CAPELLA_FORK_EPOCH": "0",
            "DENEB_FORK_EPOCH": "1",
            "ELECTRA_FORK_EPOCH": "18446744073709551615",
            "FULU_FORK_EPOCH": "18446744073709551615",
            "GLOAS_FORK_EPOCH": "18446744073709551615",
            "MAX_BLOBS_PER_BLOCK": "6",
            "MAX_BLOBS_PER_BLOCK_ELECTRA": "9",
            "BLOB_SCHEDULE": [],
        }));
        let genesis = Genesis { genesis_time: 0 };
        let resolver = ChainConfigResolver::new(spec, genesis).unwrap();

        assert_eq!(
            resolver.resolve(383).unwrap().active_fork.fork,
            ProtocolFork::Shanghai
        );
        let after = resolver.resolve(384).unwrap();
        assert_eq!(after.active_fork.fork, ProtocolFork::Cancun);
        assert_eq!(after.active_fork.blob_schedule().unwrap().max, 6);
    }
}
