//! Resolution of the active protocol fork from the execution layer's chain config.
//!
//! A clientless requester derives its fork label from the Beacon API, whose
//! `BLOB_SCHEDULE` reports the effective per-epoch parameters and collapses
//! same-epoch entries into the winning one. On networks that activate several
//! BPO forks at the same epoch (a common devnet shape) the requester cannot
//! recover the execution layer's fork naming from that view and may mislabel
//! e.g. BPO2 as BPO1. The guest maps the fork label to that fork's blob
//! parameters, so a mislabel would prove the block under the wrong blob rules.
//!
//! This module resolves the fork actually active on the execution layer at a
//! payload timestamp so the request handler can correct such labels against
//! EL ground truth before proving.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Context;
use serde::Deserialize;
use tokio::sync::OnceCell;
use tracing::warn;
use zkboost_types::ProtocolFork;

use crate::el_client::ElClient;

/// Upper bound on the lazy chain-config fetch, so an unresponsive EL degrades
/// to the warn-and-skip path instead of stalling proof requests.
const EL_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Chain identity and fork activation times extracted from the EL's
/// `debug_chainConfig` response.
///
/// Only the timestamp-scheduled forks a stateless input can name are modeled;
/// forks the EL does not schedule are absent. Unrecognized fields are kept
/// aside so a fork-time key newer than this model can be detected.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct ElChainConfig {
    chain_id: Option<u64>,
    shanghai_time: Option<u64>,
    cancun_time: Option<u64>,
    prague_time: Option<u64>,
    osaka_time: Option<u64>,
    bpo1_time: Option<u64>,
    bpo2_time: Option<u64>,
    amsterdam_time: Option<u64>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// Returns the EL activation time for a fork, or `None` for forks scheduled by
/// block number or total difficulty rather than timestamp.
///
/// Deliberately wildcard-free: when the upstream [`ProtocolFork`] enum gains a
/// variant (a new fork or BPO), this match stops compiling, forcing an
/// explicit arm. The schedule is built by enumerating every discriminant
/// through the upstream `ProtocolFork::from_u8` decoder and this match, so no
/// fork list exists in this crate to fall out of sync. An arm added without a
/// matching [`ElChainConfig`] field leaves that fork's `*Time` key
/// unrecognized, which the unknown-key guard turns into disabled resolution
/// rather than a wrong schedule.
fn activation_time(config: &ElChainConfig, fork: ProtocolFork) -> Option<u64> {
    use ProtocolFork::*;
    match fork {
        Frontier | Homestead | DAOFork | TangerineWhistle | SpuriousDragon | Byzantium
        | StPetersburg | Istanbul | MuirGlacier | Berlin | London | ArrowGlacier | GrayGlacier
        | Paris => None,
        Shanghai => config.shanghai_time,
        Cancun => config.cancun_time,
        Prague => config.prague_time,
        Osaka => config.osaka_time,
        BPO1 => config.bpo1_time,
        BPO2 => config.bpo2_time,
        Amsterdam => config.amsterdam_time,
    }
}

/// The EL's chain id and fork schedule as `(activation timestamp, fork)` pairs.
#[derive(Debug, Clone)]
pub(crate) struct ForkSchedule {
    chain_id: Option<u64>,
    forks: Vec<(u64, ProtocolFork)>,
    /// Fork-time keys in the EL config that this model does not recognize
    /// (e.g. `bpo3Time` from an EL newer than this binary). A non-empty list
    /// means the schedule is incomplete and resolution is disabled.
    unknown_fork_times: Vec<String>,
}

impl ForkSchedule {
    /// Builds the schedule from the EL chain config's activation times.
    pub(crate) fn new(config: &ElChainConfig) -> Self {
        let forks = (u8::MIN..=u8::MAX)
            .filter_map(ProtocolFork::from_u8)
            .filter_map(|fork| activation_time(config, fork).map(|time| (time, fork)))
            .collect();
        let unknown_fork_times: Vec<String> = config
            .extra
            .keys()
            .filter(|key| key.ends_with("Time"))
            .cloned()
            .collect();
        if !unknown_fork_times.is_empty() {
            warn!(
                keys = ?unknown_fork_times,
                "EL chain config schedules forks unknown to this build; fork normalization disabled"
            );
        }
        Self {
            chain_id: config.chain_id,
            forks,
            unknown_fork_times,
        }
    }

    /// Returns the EL's chain id when the config reported one.
    pub(crate) fn chain_id(&self) -> Option<u64> {
        self.chain_id
    }

    /// Resolves the fork active at an execution timestamp, returning it with
    /// its activation timestamp.
    ///
    /// Forks activating at the same instant shadow each other and the latest
    /// one wins, matching how the execution layer applies its schedule.
    /// Returns `None` when the EL schedules forks this build does not know:
    /// resolving against an incomplete model could normalize a correct label
    /// to a stale fork.
    pub(crate) fn resolve(&self, timestamp: u64) -> Option<(ProtocolFork, u64)> {
        if !self.unknown_fork_times.is_empty() {
            return None;
        }
        self.forks
            .iter()
            .filter(|(activation, _)| *activation <= timestamp)
            .max_by_key(|(activation, fork)| (*activation, *fork))
            .map(|(activation, fork)| (*fork, *activation))
    }
}

/// Lazily fetched, cached [`ForkSchedule`].
///
/// The schedule is fetched from the EL on first use so the server does not
/// depend on the EL being reachable at startup. A failed fetch is logged,
/// resolution is skipped for that request, and the fetch is retried on the
/// next one.
#[derive(Debug)]
pub(crate) struct ForkScheduleCache {
    el_client: Arc<ElClient>,
    schedule: OnceCell<ForkSchedule>,
}

impl ForkScheduleCache {
    /// Creates an empty cache backed by the given EL client.
    pub(crate) fn new(el_client: Arc<ElClient>) -> Self {
        Self {
            el_client,
            schedule: OnceCell::new(),
        }
    }

    /// Creates a cache pre-populated with a schedule, bypassing the EL fetch.
    #[cfg(test)]
    pub(crate) fn preset(schedule: ForkSchedule) -> Self {
        let el_client = ElClient::new(
            url::Url::parse("http://127.0.0.1:0/").unwrap(),
            reqwest::header::HeaderMap::new(),
        )
        .unwrap();
        Self {
            el_client: Arc::new(el_client),
            schedule: OnceCell::new_with(Some(schedule)),
        }
    }

    /// Returns the schedule, fetching it from the EL on first use.
    pub(crate) async fn get(&self) -> Option<&ForkSchedule> {
        self.schedule
            .get_or_try_init(|| async {
                let config = tokio::time::timeout(EL_FETCH_TIMEOUT, self.el_client.get_chain_config())
                    .await
                    .context("fetch EL chain config timed out")?
                    .context("fetch EL chain config")?
                    .context("EL returned no chain config")?;
                Ok::<_, anyhow::Error>(ForkSchedule::new(&config))
            })
            .await
            .map_err(
                |error| warn!(%error, "failed to fetch EL chain config; skipping fork normalization"),
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use zkboost_types::ProtocolFork;

    use super::{ElChainConfig, ForkSchedule};

    fn config_from(value: serde_json::Value) -> ElChainConfig {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn resolves_latest_fork_when_activations_share_a_timestamp() {
        // Devnet shape: every fork including both BPOs activates at genesis.
        let config = config_from(serde_json::json!({
            "chainId": 3151908,
            "shanghaiTime": 0,
            "cancunTime": 0,
            "pragueTime": 0,
            "osakaTime": 0,
            "bpo1Time": 0,
            "bpo2Time": 0,
        }));
        let schedule = ForkSchedule::new(&config);
        assert_eq!(schedule.resolve(1_000), Some((ProtocolFork::BPO2, 0)));
        assert_eq!(schedule.chain_id(), Some(3151908));
    }

    #[test]
    fn resolves_by_activation_time() {
        let config = config_from(serde_json::json!({
            "cancunTime": 0,
            "pragueTime": 100,
            "osakaTime": 200,
            "bpo1Time": 300,
            "bpo2Time": 400,
        }));
        let schedule = ForkSchedule::new(&config);
        assert_eq!(schedule.resolve(0), Some((ProtocolFork::Cancun, 0)));
        assert_eq!(schedule.resolve(299), Some((ProtocolFork::Osaka, 200)));
        assert_eq!(schedule.resolve(300), Some((ProtocolFork::BPO1, 300)));
        assert_eq!(schedule.resolve(1_000), Some((ProtocolFork::BPO2, 400)));
    }

    #[test]
    fn resolves_none_before_first_activation() {
        let config = config_from(serde_json::json!({ "osakaTime": 100 }));
        assert_eq!(ForkSchedule::new(&config).resolve(99), None);
    }

    #[test]
    fn resolves_none_when_no_fork_is_scheduled() {
        let config = config_from(serde_json::json!({}));
        assert_eq!(ForkSchedule::new(&config).resolve(u64::MAX), None);
    }

    #[test]
    fn disables_resolution_when_el_schedules_unknown_forks() {
        // An EL newer than this build scheduling e.g. bpo3: resolving against
        // the incomplete model could normalize a correct BPO3 label back to
        // BPO2, so resolution is disabled entirely (chain id checks remain).
        let config = config_from(serde_json::json!({
            "chainId": 1,
            "bpo1Time": 0,
            "bpo2Time": 0,
            "bpo3Time": 0,
        }));
        let schedule = ForkSchedule::new(&config);
        assert_eq!(schedule.resolve(1_000), None);
        assert_eq!(schedule.chain_id(), Some(1));
    }
}
