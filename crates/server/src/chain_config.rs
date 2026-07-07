//! Completion of a CL sent chain config with the EL blob fee parameters that the CL cannot supply.
//!
//! A CL resolves every field of the [`ChainConfig`] except the blob schedule `target` and
//! `base_fee_update_fraction`, which live only in the EL genesis config (EIP-7840).
//!
//! CL sends `target` and `base_fee_update_fraction` as zero and pin `max`. This module validates
//! `max` against the EL genesis and fills the two execution-only values so the completed config
//! matches the network the guest re-executes against.

use std::collections::BTreeMap;

use alloy_genesis::ChainConfig as AlloyChainConfig;
use anyhow::Context;
use zkboost_types::{BlobSchedule, ChainConfig, ForkConfig, ProtocolFork};

/// Per-fork blob schedules extracted from the EL genesis config.
pub(crate) type BlobParams = BTreeMap<ProtocolFork, BlobSchedule>;

/// Extracts the per-fork blob schedules from an EL genesis config, keyed by the fork
/// that owns each entry. Forks absent from the genesis blob schedule are omitted.
pub(crate) fn blob_params_from_el_chain_config(chain_config: &AlloyChainConfig) -> BlobParams {
    [
        ProtocolFork::Cancun,
        ProtocolFork::Prague,
        ProtocolFork::Osaka,
        ProtocolFork::BPO1,
        ProtocolFork::BPO2,
        ProtocolFork::Amsterdam,
    ]
    .into_iter()
    .filter_map(|fork| {
        let blob_params = chain_config.blob_schedule.get(schedule_key(fork)?)?;
        Some((
            fork,
            BlobSchedule {
                target: blob_params.target_blob_count,
                max: blob_params.max_blob_count,
                base_fee_update_fraction: blob_params.update_fraction as u64,
            },
        ))
    })
    .collect()
}

/// Completes a chain config with the EL blob fee parameters.
///
/// For a blob-bearing active fork the config pins `max`. This validates it against the EL genesis
/// and returns a config carrying the genesis `target` and `base_fee_update_fraction`. A config
/// whose active fork has no blob schedule is returned unchanged.
pub(crate) fn complete_chain_config(
    chain_config: &ChainConfig,
    el_blob_params: &BlobParams,
) -> anyhow::Result<ChainConfig> {
    let Some(schedule) = chain_config.active_fork.blob_schedule() else {
        return Ok(chain_config.clone());
    };
    let fork = chain_config.active_fork.fork;
    let el_schedule = el_blob_params.get(&fork).with_context(|| {
        format!("execution layer has no blob schedule for active fork {fork:?}")
    })?;
    anyhow::ensure!(
        schedule.max == el_schedule.max,
        "blob max mismatch for {fork:?}: chain_config sent {}, execution layer has {}",
        schedule.max,
        el_schedule.max
    );
    Ok(ChainConfig {
        chain_id: chain_config.chain_id,
        active_fork: ForkConfig::new(
            fork,
            chain_config.active_fork.activation.clone(),
            Some(BlobSchedule {
                target: el_schedule.target,
                max: schedule.max,
                base_fee_update_fraction: el_schedule.base_fee_update_fraction,
            }),
        ),
    })
}

/// Maps a fork to the key its blob schedule uses in an alloy genesis config.
fn schedule_key(fork: ProtocolFork) -> Option<&'static str> {
    Some(match fork {
        // Amsterdam is capitalized in alloy-genesis where every other fork key is lowercase.
        ProtocolFork::Amsterdam => "Amsterdam",
        ProtocolFork::BPO2 => "bpo2",
        ProtocolFork::BPO1 => "bpo1",
        ProtocolFork::Osaka => "osaka",
        ProtocolFork::Prague => "prague",
        ProtocolFork::Cancun => "cancun",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use alloy_genesis::ChainConfig as AlloyChainConfig;
    use zkboost_types::{BlobSchedule, ChainConfig, ForkActivation, ForkConfig, ProtocolFork};

    use crate::chain_config::{
        BlobParams, blob_params_from_el_chain_config, complete_chain_config,
    };

    /// Current mainnet BPO2 blob schedule (EIP-7892), derived from alloy's `BlobParams::bpo2`.
    const BPO2_SCHEDULE: BlobSchedule = {
        let params = alloy_eips::eip7840::BlobParams::bpo2();
        BlobSchedule {
            target: params.target_blob_count,
            max: params.max_blob_count,
            base_fee_update_fraction: params.update_fraction as u64,
        }
    };

    /// EL blob params exposing only the BPO2 schedule.
    fn blob_params() -> BlobParams {
        BlobParams::from([(ProtocolFork::BPO2, BPO2_SCHEDULE)])
    }

    /// Builds a CL sent config whose active fork pins `max` with the zeroed target and update
    /// fraction a CL sends, or carries no blob schedule when `max` is `None`.
    fn chain_config(fork: ProtocolFork, max: Option<u64>) -> ChainConfig {
        ChainConfig {
            chain_id: 1,
            active_fork: ForkConfig::new(
                fork,
                ForkActivation::new(None, Some(0)),
                max.map(|max| BlobSchedule {
                    target: 0,
                    max,
                    base_fee_update_fraction: 0,
                }),
            ),
        }
    }

    #[test]
    fn fills_target_and_update_fraction_from_el() {
        let config_config = chain_config(ProtocolFork::BPO2, Some(BPO2_SCHEDULE.max));
        let completed = complete_chain_config(&config_config, &blob_params()).unwrap();
        assert_eq!(completed.chain_id, config_config.chain_id);
        assert_eq!(completed.active_fork.fork, ProtocolFork::BPO2);
        assert_eq!(completed.active_fork.blob_schedule(), Some(&BPO2_SCHEDULE));
    }

    #[test]
    fn passes_through_pre_blob_fork() {
        let config_config = chain_config(ProtocolFork::Shanghai, None);
        let completed = complete_chain_config(&config_config, &blob_params()).unwrap();
        assert_eq!(completed, config_config);
    }

    #[test]
    fn rejects_configs_that_cannot_complete() {
        let uncompletable = [
            (ProtocolFork::BPO2, BPO2_SCHEDULE.max + 1),
            (ProtocolFork::Prague, BPO2_SCHEDULE.max),
        ];
        for (fork, max) in uncompletable {
            let config_config = chain_config(fork, Some(max));
            assert!(complete_chain_config(&config_config, &blob_params()).is_err());
        }
    }

    #[test]
    fn blob_params_from_alloy_extracts_by_fork() {
        let config: AlloyChainConfig = serde_json::from_value(serde_json::json!({
            "chainId": 1,
            "blobSchedule": {
                "bpo2": {
                    "target": BPO2_SCHEDULE.target,
                    "max": BPO2_SCHEDULE.max,
                    "baseFeeUpdateFraction": BPO2_SCHEDULE.base_fee_update_fraction
                }
            }
        }))
        .unwrap();
        let blob_params = blob_params_from_el_chain_config(&config);
        assert_eq!(blob_params.get(&ProtocolFork::BPO2), Some(&BPO2_SCHEDULE));
        assert!(!blob_params.contains_key(&ProtocolFork::Prague));
    }
}
