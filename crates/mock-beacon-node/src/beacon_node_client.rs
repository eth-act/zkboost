//! Client of the beacon API of the CL. It reads the chain spec, the genesis validators root, and
//! the public key of a validator. It streams head notifications and forwards every other request.
//! It decodes a Gloas block with its execution payload envelope into the `NewPayloadRequest`.

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use axum::{
    body::Bytes,
    http::{HeaderValue, Method, StatusCode, header::CONTENT_TYPE},
};
use lighthouse_bls::PublicKey;
use lighthouse_types::{
    BeaconBlockRef, BuilderDepositRequests as LighthouseBuilderDepositRequests,
    BuilderExitRequests as LighthouseBuilderExitRequests, ChainSpec,
    ConsolidationRequests as LighthouseConsolidationRequests,
    DepositRequests as LighthouseDepositRequests,
    ExecutionRequestsGloas as LighthouseExecutionRequestsGloas, ForkName, ForkVersionDecode,
    Hash256, KzgCommitment, MainnetEthSpec, SignedBeaconBlock, SignedExecutionPayloadEnvelope,
    Slot, WithdrawalRequests as LighthouseWithdrawalRequests,
};
use reqwest_eventsource::{Event, EventSource, retry::Constant};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::time::{Instant, sleep, timeout_at};
use tokio_stream::{Stream, StreamExt};
use tracing::warn;
use url::Url;
use zkboost_types::{
    BuilderDepositRequest, BuilderDepositRequests, BuilderExitRequest, BuilderExitRequests,
    ConsolidationRequest, ConsolidationRequests, DepositRequest, DepositRequests,
    ExecutionPayloadV4, ExecutionRequestsGloas, NewPayloadRequestGloas, ProgressiveList, SszList,
    VersionedHashes, Withdrawal, WithdrawalRequest, WithdrawalRequests,
};

/// Delay between two connection attempts of the event stream.
const BEACON_NODE_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Interval between execution payload envelope fetches.
const ENVELOPE_RETRY_INTERVAL: Duration = Duration::from_millis(200);
/// Budget for fetching an execution payload envelope. The builder reveals the payload after the
/// block, so a whole mainnet slot bounds the wait.
const ENVELOPE_TIMEOUT: Duration = Duration::from_secs(12);

/// Client of the beacon API of the CL.
#[derive(Debug)]
pub(crate) struct BeaconNodeClient {
    endpoint: Url,
    http_client: reqwest::Client,
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

#[derive(Deserialize)]
struct Genesis {
    genesis_validators_root: Hash256,
}

#[derive(Deserialize)]
struct ValidatorData {
    validator: Validator,
}

#[derive(Deserialize)]
struct Validator {
    #[serde(with = "serde_utils::hex_vec")]
    pubkey: Vec<u8>,
}

#[derive(Deserialize)]
struct HeaderData {
    root: Hash256,
}

#[derive(Deserialize)]
struct Checkpoint {
    root: Hash256,
}

#[derive(Deserialize)]
struct FinalityCheckpoints {
    current_justified: Checkpoint,
    finalized: Checkpoint,
}

impl BeaconNodeClient {
    /// Creates the client of the beacon API at the endpoint.
    pub(crate) fn new(endpoint: Url) -> Self {
        Self {
            endpoint,
            http_client: reqwest::Client::new(),
        }
    }

    /// Signals when forkchoice may have changed, including after reconnecting. Events are hints:
    /// callers must read the current head rather than replay potentially stale event roots.
    pub(crate) fn subscribe_heads(&self) -> impl Stream<Item = ()> + '_ {
        let mut url = self
            .endpoint
            .join("eth/v1/events")
            .expect("the beacon endpoint accepts a relative path");
        url.query_pairs_mut()
            .append_pair("topics", "head,chain_reorg,finalized_checkpoint");
        let mut event_source = EventSource::new(self.http_client.get(url))
            .expect("the stream request carries no body");
        event_source.set_retry_policy(Box::new(Constant::new(BEACON_NODE_RETRY_DELAY, None)));
        event_source.filter_map(|event| match event {
            Ok(Event::Open) => Some(()),
            Ok(Event::Message(message))
                if matches!(
                    message.event.as_str(),
                    "head" | "chain_reorg" | "finalized_checkpoint"
                ) =>
            {
                Some(())
            }
            Ok(_) => None,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "head stream lost");
                None
            }
        })
    }

    /// Returns the current canonical head, also used to recover missed SSE events.
    pub(crate) async fn head_root(&self) -> anyhow::Result<Hash256> {
        let header: HeaderData = self
            .get("eth/v1/beacon/headers/head")
            .await?
            .context("head not found")?;
        Ok(header.root)
    }

    /// Returns the justified and finalized execution hashes from the same head state.
    pub(crate) async fn finality_hashes(
        &self,
        state_root: Hash256,
    ) -> anyhow::Result<(Hash256, Hash256)> {
        let checkpoints: FinalityCheckpoints = self
            .get(&format!(
                "eth/v1/beacon/states/{state_root}/finality_checkpoints"
            ))
            .await?
            .context("finality checkpoints not found")?;
        Ok((
            self.execution_hash(checkpoints.current_justified.root)
                .await?,
            self.execution_hash(checkpoints.finalized.root).await?,
        ))
    }

    /// Finds the last revealed payload at a checkpoint, walking past empty Gloas payloads.
    async fn execution_hash(&self, mut root: Hash256) -> anyhow::Result<Hash256> {
        while root != Hash256::ZERO {
            let block = self.block(root).await?;
            if block.slot() == Slot::new(0) {
                break;
            }
            let envelope: Option<SignedExecutionPayloadEnvelope<MainnetEthSpec>> = self
                .get(&format!("eth/v1/beacon/execution_payload_envelopes/{root}"))
                .await?;
            if let Some(envelope) = envelope {
                return Ok(envelope.message.payload.block_hash.0);
            }
            root = block.parent_root();
        }
        // Zero means no execution checkpoint is available yet, as at genesis.
        Ok(Hash256::ZERO)
    }

    /// Finds the payload of an execution parent along the canonical beacon ancestry.
    pub(crate) async fn parent_payload(
        &self,
        mut root: Hash256,
        hash: Hash256,
    ) -> anyhow::Result<NewPayloadRequestGloas> {
        while root != Hash256::ZERO {
            let block = self.block(root).await?;
            if block.slot() == Slot::new(0) {
                break;
            }
            let envelope: Option<SignedExecutionPayloadEnvelope<MainnetEthSpec>> = self
                .get(&format!("eth/v1/beacon/execution_payload_envelopes/{root}"))
                .await?;
            if let Some(envelope) = envelope
                && envelope.message.payload.block_hash.0 == hash
            {
                return new_payload_request_gloas(&block, &envelope);
            }
            root = block.parent_root();
        }
        bail!("execution parent {hash} not found in beacon ancestry")
    }

    /// Returns the signed beacon block of a root, decoded from SSZ by its fork.
    pub(crate) async fn block(
        &self,
        root: Hash256,
    ) -> anyhow::Result<SignedBeaconBlock<MainnetEthSpec>> {
        let url = self
            .endpoint
            .join(&format!("eth/v2/beacon/blocks/{root}"))?;
        let response = self
            .http_client
            .get(url.clone())
            .timeout(ENVELOPE_TIMEOUT)
            .header("Accept", "application/octet-stream")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("GET {url}: {status}: {body}");
        }
        let fork_name: ForkName = response
            .headers()
            .get("Eth-Consensus-Version")
            .context("missing Eth-Consensus-Version")?
            .to_str()?
            .parse()
            .map_err(|error: String| anyhow!("{error}"))?;
        let bytes = response.bytes().await?;
        SignedBeaconBlock::from_ssz_bytes_by_fork(&bytes, fork_name)
            .map_err(|error| anyhow!("{error:?}"))
    }

    /// Returns the execution payload envelope of a Gloas block. It retries until the builder
    /// publishes the envelope or the budget expires.
    pub(crate) async fn execution_payload_envelope(
        &self,
        root: Hash256,
    ) -> anyhow::Result<SignedExecutionPayloadEnvelope<MainnetEthSpec>> {
        let path = format!("eth/v1/beacon/execution_payload_envelopes/{root}");
        let deadline = Instant::now() + ENVELOPE_TIMEOUT;
        let mut last_error = None;
        loop {
            match timeout_at(deadline, self.get(&path)).await {
                Ok(Ok(Some(envelope))) => return Ok(envelope),
                Ok(Ok(None)) => last_error = None,
                Ok(Err(error)) => last_error = Some(error),
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                let context =
                    format!("execution payload envelope for {root} not published in time");
                return Err(match last_error {
                    Some(error) => error.context(context),
                    None => anyhow!(context),
                });
            }
            sleep(ENVELOPE_RETRY_INTERVAL).await;
        }
    }

    /// Returns the chain spec of the beacon node.
    pub(crate) async fn spec(&self) -> anyhow::Result<ChainSpec> {
        let config = self
            .get("eth/v1/config/spec")
            .await?
            .context("spec not found")?;
        ChainSpec::from_config::<MainnetEthSpec>(&config)
            .context("beacon node spec is not the mainnet preset")
    }

    /// Returns the public key of a validator in the head state.
    pub(crate) async fn validator_pubkey(&self, validator_index: u64) -> anyhow::Result<PublicKey> {
        let path = format!("eth/v1/beacon/states/head/validators/{validator_index}");
        let validator: ValidatorData = self
            .get(&path)
            .await?
            .with_context(|| format!("validator {validator_index} not found"))?;
        PublicKey::deserialize(&validator.validator.pubkey)
            .map_err(|error| anyhow!("validator {validator_index} pubkey: {error:?}"))
    }

    /// Returns the genesis validators root of the beacon node.
    pub(crate) async fn genesis_validators_root(&self) -> anyhow::Result<Hash256> {
        let genesis: Genesis = self
            .get("eth/v1/beacon/genesis")
            .await?
            .context("genesis not found")?;
        Ok(genesis.genesis_validators_root)
    }

    /// Forwards a request to the CL and returns the response status, content type, and the
    /// response itself, whose body streams to the caller.
    pub(crate) async fn forward(
        &self,
        method: Method,
        path_and_query: &str,
        body: Bytes,
    ) -> anyhow::Result<(StatusCode, Option<HeaderValue>, reqwest::Response)> {
        let url = self.endpoint.join(path_and_query)?;
        let response = self
            .http_client
            .request(method, url)
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let content_type = response.headers().get(CONTENT_TYPE).cloned();
        Ok((status, content_type, response))
    }

    /// Fetches the `data` of a beacon API resource, or `None` when it does not exist.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Option<T>> {
        let url = self.endpoint.join(path)?;
        let response = self
            .http_client
            .get(url.clone())
            .timeout(ENVELOPE_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("GET {url}: {status}: {body}");
        }
        Ok(Some(response.json::<Data<T>>().await?.data))
    }
}

/// Converts a lighthouse Gloas beacon block and its envelope into `NewPayloadRequestGloas`.
/// The block carries only a payload bid, so the envelope supplies the payload and the requests.
pub(crate) fn new_payload_request_gloas(
    block: &SignedBeaconBlock<MainnetEthSpec>,
    envelope: &SignedExecutionPayloadEnvelope<MainnetEthSpec>,
) -> anyhow::Result<NewPayloadRequestGloas> {
    let BeaconBlockRef::Gloas(block) = block.message() else {
        bail!("block is not a gloas block");
    };
    let payload = &envelope.message.payload;
    let bid = &block.body.signed_execution_payload_bid.message;
    Ok(NewPayloadRequestGloas {
        execution_payload: ExecutionPayloadV4 {
            parent_hash: payload.parent_hash.0.into(),
            fee_recipient: payload.fee_recipient.into(),
            state_root: payload.state_root.into(),
            receipts_root: payload.receipts_root.into(),
            logs_bloom: fixed(&payload.logs_bloom)?,
            prev_randao: payload.prev_randao.into(),
            block_number: payload.block_number,
            gas_limit: payload.gas_limit,
            gas_used: payload.gas_used,
            timestamp: payload.timestamp,
            extra_data: SszList::try_from(payload.extra_data.to_vec())
                .map_err(|error| anyhow!("extra data exceeds bound: {error:?}"))?,
            base_fee_per_gas: payload.base_fee_per_gas.to_le_bytes::<32>(),
            block_hash: payload.block_hash.0.into(),
            transactions: payload
                .transactions
                .iter()
                .map(|transaction| ProgressiveList::from(transaction.to_vec()))
                .collect::<Vec<_>>()
                .into(),
            withdrawals: payload
                .withdrawals
                .iter()
                .map(|withdrawal| Withdrawal {
                    index: withdrawal.index,
                    validator_index: withdrawal.validator_index,
                    address: withdrawal.address.into(),
                    amount: withdrawal.amount,
                })
                .collect::<Vec<_>>()
                .into(),
            blob_gas_used: payload.blob_gas_used,
            excess_blob_gas: payload.excess_blob_gas,
            block_access_list: payload.block_access_list.to_vec().into(),
            slot_number: payload.slot_number.as_u64(),
        },
        versioned_hashes: versioned_hashes(&bid.blob_kzg_commitments),
        parent_beacon_block_root: block.parent_root.0,
        execution_requests: execution_requests_gloas(&envelope.message.execution_requests),
    })
}

/// Converts a byte slice into a fixed-size array, erroring on a length mismatch.
fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    Ok(bytes.try_into()?)
}

/// Builds the versioned hashes list from the block's blob KZG commitments.
fn versioned_hashes(commitments: &[KzgCommitment]) -> VersionedHashes {
    commitments
        .iter()
        .map(kzg_commitment_to_versioned_hash)
        .collect::<Vec<_>>()
        .into()
}

/// Computes the EIP-4844 versioned hash for a KZG commitment.
fn kzg_commitment_to_versioned_hash(commitment: &KzgCommitment) -> [u8; 32] {
    let mut hash: [u8; 32] = Sha256::digest(commitment.0).into();
    hash[0] = 0x01;
    hash
}

/// Converts the lighthouse deposit request list into the SSZ deposit request list.
fn deposit_requests(value: &LighthouseDepositRequests<MainnetEthSpec>) -> DepositRequests {
    value
        .iter()
        .map(|deposit| DepositRequest {
            pubkey: deposit.pubkey.serialize(),
            withdrawal_credentials: deposit.withdrawal_credentials.into(),
            amount: deposit.amount,
            signature: deposit.signature.serialize(),
            index: deposit.index,
        })
        .collect::<Vec<_>>()
        .into()
}

/// Converts the lighthouse withdrawal request list into the SSZ withdrawal request list.
fn withdrawal_requests(value: &LighthouseWithdrawalRequests<MainnetEthSpec>) -> WithdrawalRequests {
    value
        .iter()
        .map(|withdrawal| WithdrawalRequest {
            source_address: withdrawal.source_address.into(),
            validator_pubkey: withdrawal.validator_pubkey.serialize(),
            amount: withdrawal.amount,
        })
        .collect::<Vec<_>>()
        .into()
}

/// Converts the lighthouse consolidation request list into the SSZ consolidation request list.
fn consolidation_requests(
    value: &LighthouseConsolidationRequests<MainnetEthSpec>,
) -> ConsolidationRequests {
    value
        .iter()
        .map(|consolidation| ConsolidationRequest {
            source_address: consolidation.source_address.into(),
            source_pubkey: consolidation.source_pubkey.serialize(),
            target_pubkey: consolidation.target_pubkey.serialize(),
        })
        .collect::<Vec<_>>()
        .into()
}

/// Converts the lighthouse EIP-8282 builder deposit request list into the SSZ list.
fn builder_deposit_requests(
    value: &LighthouseBuilderDepositRequests<MainnetEthSpec>,
) -> BuilderDepositRequests {
    value
        .iter()
        .map(|deposit| BuilderDepositRequest {
            pubkey: deposit.pubkey.serialize(),
            withdrawal_credentials: deposit.withdrawal_credentials.into(),
            amount: deposit.amount,
            signature: deposit.signature.serialize(),
        })
        .collect::<Vec<_>>()
        .into()
}

/// Converts the lighthouse EIP-8282 builder exit request list into the SSZ list.
fn builder_exit_requests(
    value: &LighthouseBuilderExitRequests<MainnetEthSpec>,
) -> BuilderExitRequests {
    value
        .iter()
        .map(|exit| BuilderExitRequest {
            source_address: exit.source_address.into(),
            pubkey: exit.pubkey.serialize(),
        })
        .collect::<Vec<_>>()
        .into()
}

/// Builds SSZ `ExecutionRequestsGloas` from the lighthouse container, which EIP-8282 extends with
/// the builder deposit and builder exit lists.
fn execution_requests_gloas(
    value: &LighthouseExecutionRequestsGloas<MainnetEthSpec>,
) -> ExecutionRequestsGloas {
    ExecutionRequestsGloas {
        deposits: deposit_requests(&value.deposits),
        withdrawals: withdrawal_requests(&value.withdrawals),
        consolidations: consolidation_requests(&value.consolidations),
        builder_deposits: builder_deposit_requests(&value.builder_deposits),
        builder_exits: builder_exit_requests(&value.builder_exits),
    }
}
