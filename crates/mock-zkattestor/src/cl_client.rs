//! CL beacon API client and beacon block decoding.

use std::time::Duration;

use anyhow::{anyhow, bail};
use futures::{Stream, StreamExt};
use lighthouse_types::{
    BeaconBlockRef, BuilderDepositRequests as LighthouseBuilderDepositRequests,
    BuilderExitRequests as LighthouseBuilderExitRequests,
    ConsolidationRequests as LighthouseConsolidationRequests,
    DepositRequests as LighthouseDepositRequests,
    ExecutionRequestsElectra as LighthouseExecutionRequestsElectra,
    ExecutionRequestsGloas as LighthouseExecutionRequestsGloas, ForkName, ForkVersionDecode,
    Hash256, KzgCommitment, MainnetEthSpec, SignedBeaconBlock, SignedExecutionPayloadEnvelope,
    Transactions as LighthouseTransactions, WithdrawalRequests as LighthouseWithdrawalRequests,
    Withdrawals as LighthouseWithdrawals,
};
use reqwest_eventsource::{Event as SseEvent, EventSource};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;
use zkboost_types::{
    BuilderDepositRequest, BuilderDepositRequests, BuilderExitRequest, BuilderExitRequests,
    ConsolidationRequest, ConsolidationRequests, DepositRequest, DepositRequests,
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, ExecutionPayloadV4,
    ExecutionRequestsElectraFulu, ExecutionRequestsGloas, NewPayloadRequest,
    NewPayloadRequestBellatrix, NewPayloadRequestCapella, NewPayloadRequestDeneb,
    NewPayloadRequestElectraFulu, NewPayloadRequestGloas, SszList, Transaction, Transactions,
    VersionedHashes, Withdrawal, WithdrawalRequest, WithdrawalRequests, Withdrawals,
};

/// Interval between execution payload envelope fetches.
const ENVELOPE_RETRY_INTERVAL: Duration = Duration::from_millis(200);

/// Budget for fetching an execution payload envelope. Under ePBS the builder reveals the payload
/// after the block itself and may do so as late as the payload deadline three quarters into the
/// slot, so a whole mainnet slot bounds the wait.
const ENVELOPE_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Block {
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) slot: u64,
    pub(crate) block: Hash256,
}

/// CL config.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) struct Spec {
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) deposit_chain_id: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) seconds_per_slot: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) slots_per_epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) capella_fork_epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) deneb_fork_epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) electra_fork_epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) fulu_fork_epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) gloas_fork_epoch: u64,
    #[serde(default)]
    pub(crate) blob_schedule: Vec<BlobParameters>,
}

/// A single EIP-7892 blob-parameter-only fork entry from the consensus spec. Only the activation
/// epoch is read, since it is the sole source of the BPO fork schedule.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) struct BlobParameters {
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) epoch: u64,
}

/// Genesis details from `/eth/v1/beacon/genesis`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Genesis {
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) genesis_time: u64,
}

#[derive(Clone)]
pub(crate) struct ClClient {
    base_url: Url,
    http: reqwest::Client,
}

impl ClClient {
    pub(crate) fn new(base_url: Url) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
        }
    }

    pub(crate) fn subscribe_block_events(
        &self,
    ) -> impl Stream<Item = Result<Block, anyhow::Error>> + Send + '_ {
        async_stream::try_stream! {
            let mut url = self.base_url.join("/eth/v1/events")?;
            url.query_pairs_mut().append_pair("topics", "block");
            let mut es = EventSource::new(self.http.get(url))?;
            while let Some(event) = es.next().await {
                match event {
                    Ok(SseEvent::Open) => {}
                    Ok(SseEvent::Message(message)) if message.event == "block" => {
                        let block_event: Block = serde_json::from_str(&message.data)?;
                        yield block_event
                    }
                    Ok(SseEvent::Message(_)) => {}
                    Err(error) => {
                        es.close();
                        Err(anyhow!("{error}"))?;
                    }
                }
            }
        }
    }

    pub(crate) async fn get_beacon_block(
        &self,
        block_root: Hash256,
    ) -> anyhow::Result<SignedBeaconBlock<MainnetEthSpec>> {
        let url = self
            .base_url
            .join(&format!("/eth/v2/beacon/blocks/{block_root}"))?;
        let resp = self
            .http
            .get(url)
            .header("Accept", "application/octet-stream")
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("{status}: {body}");
        }
        let fork_name: ForkName = resp
            .headers()
            .get("Eth-Consensus-Version")
            .ok_or_else(|| anyhow!("missing Eth-Consensus-Version"))?
            .to_str()?
            .parse()
            .map_err(|error: String| anyhow!("{error}"))?;
        let bytes = resp.bytes().await?;
        SignedBeaconBlock::from_ssz_bytes_by_fork(&bytes, fork_name).map_err(|e| anyhow!("{e:?}"))
    }

    /// Fetches the execution payload envelope carrying a Gloas block's execution payload and
    /// execution requests, retrying until the builder publishes it or the budget expires.
    pub(crate) async fn get_execution_payload_envelope(
        &self,
        block_root: Hash256,
    ) -> anyhow::Result<SignedExecutionPayloadEnvelope<MainnetEthSpec>> {
        let path = format!("/eth/v1/beacon/execution_payload_envelopes/{block_root}");
        let deadline = Instant::now() + ENVELOPE_TIMEOUT;
        let mut last_error = None;
        loop {
            match timeout_at(deadline, self.get_json(&path)).await {
                Ok(Ok(envelope)) => return Ok(envelope),
                Ok(Err(error)) => last_error = Some(error),
                Err(_) => {}
            }
            if Instant::now() >= deadline {
                let context =
                    format!("execution payload envelope for {block_root} not published in time");
                return Err(match last_error {
                    Some(error) => error.context(context),
                    None => anyhow!(context),
                });
            }
            sleep(ENVELOPE_RETRY_INTERVAL).await;
        }
    }

    /// Fetches the spec.
    pub(crate) async fn get_spec(&self) -> anyhow::Result<Spec> {
        self.get_json("/eth/v1/config/spec").await
    }

    /// Fetches the genesis.
    pub(crate) async fn get_genesis(&self) -> anyhow::Result<Genesis> {
        self.get_json("/eth/v1/beacon/genesis").await
    }

    /// Fetches a beacon-API endpoint and returns its `data` payload.
    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        #[derive(Deserialize)]
        struct Data<T> {
            data: T,
        }

        let url = self.base_url.join(path)?;
        let response = self.http.get(url).send().await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("{path}: {status}: {body}");
        }
        Ok(response.json::<Data<T>>().await?.data)
    }
}

/// Builds an SSZ `ExecutionPayloadV1` from a lighthouse Bellatrix execution payload.
macro_rules! execution_payload_v1 {
    ($payload:expr) => {{
        let payload = $payload;
        ExecutionPayloadV1 {
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
            extra_data: byte_list(&payload.extra_data)?,
            base_fee_per_gas: payload.base_fee_per_gas.to_le_bytes::<32>(),
            block_hash: payload.block_hash.0.into(),
            transactions: transactions(&payload.transactions)?,
        }
    }};
}

/// Builds an SSZ `ExecutionPayloadV2` from a lighthouse Capella execution payload.
macro_rules! execution_payload_v2 {
    ($payload:expr) => {{
        let payload = $payload;
        ExecutionPayloadV2 {
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
            extra_data: byte_list(&payload.extra_data)?,
            base_fee_per_gas: payload.base_fee_per_gas.to_le_bytes::<32>(),
            block_hash: payload.block_hash.0.into(),
            transactions: transactions(&payload.transactions)?,
            withdrawals: withdrawals(&payload.withdrawals)?,
        }
    }};
}

/// Builds an SSZ `ExecutionPayloadV3` from a lighthouse Deneb, Electra, or Fulu execution payload,
/// which share an identical layout.
macro_rules! execution_payload_v3 {
    ($payload:expr) => {{
        let payload = $payload;
        ExecutionPayloadV3 {
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
            extra_data: byte_list(&payload.extra_data)?,
            base_fee_per_gas: payload.base_fee_per_gas.to_le_bytes::<32>(),
            block_hash: payload.block_hash.0.into(),
            transactions: transactions(&payload.transactions)?,
            withdrawals: withdrawals(&payload.withdrawals)?,
            blob_gas_used: payload.blob_gas_used,
            excess_blob_gas: payload.excess_blob_gas,
        }
    }};
}

/// Builds an SSZ `ExecutionPayloadV4` from a lighthouse Gloas execution payload, which extends the
/// V3 layout with the EIP-7928 block access list and the slot number.
macro_rules! execution_payload_v4 {
    ($payload:expr) => {{
        let payload = $payload;
        ExecutionPayloadV4 {
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
            extra_data: byte_list(&payload.extra_data)?,
            base_fee_per_gas: payload.base_fee_per_gas.to_le_bytes::<32>(),
            block_hash: payload.block_hash.0.into(),
            transactions: transactions(&payload.transactions)?,
            withdrawals: withdrawals(&payload.withdrawals)?,
            blob_gas_used: payload.blob_gas_used,
            excess_blob_gas: payload.excess_blob_gas,
            block_access_list: byte_list(&payload.block_access_list)?,
            slot_number: payload.slot_number.as_u64(),
        }
    }};
}

/// Converts a lighthouse beacon block into the `NewPayloadRequest`.
///
/// The execution payload and execution requests are converted field by field into the SSZ types,
/// so a lighthouse layout change surfaces as a compile error rather than a silent mismatch. The
/// lighthouse Electra and Fulu forks both map to the `ElectraFulu` variant.
///
/// From Gloas onward the block carries only a payload bid, so `envelope` supplies the execution
/// payload and this block's execution requests. It is required for Gloas and later, and ignored
/// for earlier forks.
pub(crate) fn new_payload_request_from_beacon_block(
    block: &SignedBeaconBlock<MainnetEthSpec>,
    envelope: Option<&SignedExecutionPayloadEnvelope<MainnetEthSpec>>,
) -> anyhow::Result<NewPayloadRequest> {
    match block.message() {
        BeaconBlockRef::Base(_) | BeaconBlockRef::Altair(_) => unreachable!(),
        BeaconBlockRef::Bellatrix(b) => {
            Ok(NewPayloadRequest::Bellatrix(NewPayloadRequestBellatrix {
                execution_payload: execution_payload_v1!(
                    &b.body.execution_payload.execution_payload
                ),
            }))
        }
        BeaconBlockRef::Capella(b) => Ok(NewPayloadRequest::Capella(NewPayloadRequestCapella {
            execution_payload: execution_payload_v2!(&b.body.execution_payload.execution_payload),
        })),
        BeaconBlockRef::Deneb(b) => Ok(NewPayloadRequest::Deneb(NewPayloadRequestDeneb {
            execution_payload: execution_payload_v3!(&b.body.execution_payload.execution_payload),
            versioned_hashes: versioned_hashes(&b.body.blob_kzg_commitments)?,
            parent_beacon_block_root: b.parent_root.0,
        })),
        BeaconBlockRef::Electra(b) => Ok(NewPayloadRequest::ElectraFulu(
            NewPayloadRequestElectraFulu {
                execution_payload: execution_payload_v3!(
                    &b.body.execution_payload.execution_payload
                ),
                versioned_hashes: versioned_hashes(&b.body.blob_kzg_commitments)?,
                parent_beacon_block_root: b.parent_root.0,
                execution_requests: execution_requests_electra_fulu(&b.body.execution_requests)?,
            },
        )),
        BeaconBlockRef::Fulu(b) => Ok(NewPayloadRequest::ElectraFulu(
            NewPayloadRequestElectraFulu {
                execution_payload: execution_payload_v3!(
                    &b.body.execution_payload.execution_payload
                ),
                versioned_hashes: versioned_hashes(&b.body.blob_kzg_commitments)?,
                parent_beacon_block_root: b.parent_root.0,
                execution_requests: execution_requests_electra_fulu(&b.body.execution_requests)?,
            },
        )),
        BeaconBlockRef::Gloas(b) => {
            let envelope = envelope
                .ok_or_else(|| anyhow!("gloas block requires an execution payload envelope"))?;
            let bid = &b.body.signed_execution_payload_bid.message;
            Ok(NewPayloadRequest::Gloas(NewPayloadRequestGloas {
                execution_payload: execution_payload_v4!(&envelope.message.payload),
                versioned_hashes: versioned_hashes(&bid.blob_kzg_commitments)?,
                parent_beacon_block_root: b.parent_root.0,
                execution_requests: execution_requests_gloas(&envelope.message.execution_requests)?,
            }))
        }
    }
}

/// Converts a byte slice into a fixed-size array, erroring on a length mismatch.
fn fixed<const N: usize>(bytes: &[u8]) -> anyhow::Result<[u8; N]> {
    Ok(bytes.try_into()?)
}

/// Converts a byte slice into a bounded SSZ byte list.
fn byte_list<const N: usize>(bytes: &[u8]) -> anyhow::Result<SszList<u8, N>> {
    SszList::try_from(bytes.to_vec()).map_err(|error| anyhow!("byte list exceeds bound: {error:?}"))
}

/// Converts the lighthouse transaction list into the SSZ transaction list.
fn transactions(value: &LighthouseTransactions<MainnetEthSpec>) -> anyhow::Result<Transactions> {
    let transactions: Vec<Transaction> = value
        .iter()
        .map(|transaction| byte_list(transaction))
        .collect::<anyhow::Result<_>>()?;
    SszList::try_from(transactions).map_err(|error| anyhow!("transactions exceed bound: {error:?}"))
}

/// Converts the lighthouse withdrawal list into the SSZ withdrawal list.
fn withdrawals(value: &LighthouseWithdrawals<MainnetEthSpec>) -> anyhow::Result<Withdrawals> {
    let withdrawals: Vec<Withdrawal> = value
        .iter()
        .map(|withdrawal| Withdrawal {
            index: withdrawal.index,
            validator_index: withdrawal.validator_index,
            address: withdrawal.address.into(),
            amount: withdrawal.amount,
        })
        .collect();
    SszList::try_from(withdrawals).map_err(|error| anyhow!("withdrawals exceed bound: {error:?}"))
}

/// Builds the versioned hashes list from the block's blob KZG commitments.
fn versioned_hashes(commitments: &[KzgCommitment]) -> anyhow::Result<VersionedHashes> {
    let hashes: Vec<[u8; 32]> = commitments
        .iter()
        .map(kzg_commitment_to_versioned_hash)
        .collect();
    SszList::try_from(hashes).map_err(|e| anyhow!("versioned hashes exceed bound: {e:?}"))
}

/// Computes the EIP-4844 versioned hash for a KZG commitment.
fn kzg_commitment_to_versioned_hash(commitment: &KzgCommitment) -> [u8; 32] {
    let mut hash: [u8; 32] = Sha256::digest(commitment.0).into();
    hash[0] = 0x01;
    hash
}

/// Converts the lighthouse deposit request list into the SSZ deposit request list.
fn deposit_requests(
    value: &LighthouseDepositRequests<MainnetEthSpec>,
) -> anyhow::Result<DepositRequests> {
    let requests: Vec<DepositRequest> = value
        .iter()
        .map(|deposit| DepositRequest {
            pubkey: deposit.pubkey.serialize(),
            withdrawal_credentials: deposit.withdrawal_credentials.into(),
            amount: deposit.amount,
            signature: deposit.signature.serialize(),
            index: deposit.index,
        })
        .collect();
    SszList::try_from(requests).map_err(|error| anyhow!("deposit requests exceed bound: {error:?}"))
}

/// Converts the lighthouse withdrawal request list into the SSZ withdrawal request list.
fn withdrawal_requests(
    value: &LighthouseWithdrawalRequests<MainnetEthSpec>,
) -> anyhow::Result<WithdrawalRequests> {
    let requests: Vec<WithdrawalRequest> = value
        .iter()
        .map(|withdrawal| WithdrawalRequest {
            source_address: withdrawal.source_address.into(),
            validator_pubkey: withdrawal.validator_pubkey.serialize(),
            amount: withdrawal.amount,
        })
        .collect();
    SszList::try_from(requests)
        .map_err(|error| anyhow!("withdrawal requests exceed bound: {error:?}"))
}

/// Converts the lighthouse consolidation request list into the SSZ consolidation request list.
fn consolidation_requests(
    value: &LighthouseConsolidationRequests<MainnetEthSpec>,
) -> anyhow::Result<ConsolidationRequests> {
    let requests: Vec<ConsolidationRequest> = value
        .iter()
        .map(|consolidation| ConsolidationRequest {
            source_address: consolidation.source_address.into(),
            source_pubkey: consolidation.source_pubkey.serialize(),
            target_pubkey: consolidation.target_pubkey.serialize(),
        })
        .collect();
    SszList::try_from(requests)
        .map_err(|error| anyhow!("consolidation requests exceed bound: {error:?}"))
}

/// Converts the lighthouse EIP-8282 builder deposit request list into the SSZ list.
fn builder_deposit_requests(
    value: &LighthouseBuilderDepositRequests<MainnetEthSpec>,
) -> anyhow::Result<BuilderDepositRequests> {
    let requests: Vec<BuilderDepositRequest> = value
        .iter()
        .map(|deposit| BuilderDepositRequest {
            pubkey: deposit.pubkey.serialize(),
            withdrawal_credentials: deposit.withdrawal_credentials.into(),
            amount: deposit.amount,
            signature: deposit.signature.serialize(),
        })
        .collect();
    SszList::try_from(requests)
        .map_err(|error| anyhow!("builder deposit requests exceed bound: {error:?}"))
}

/// Converts the lighthouse EIP-8282 builder exit request list into the SSZ list.
fn builder_exit_requests(
    value: &LighthouseBuilderExitRequests<MainnetEthSpec>,
) -> anyhow::Result<BuilderExitRequests> {
    let requests: Vec<BuilderExitRequest> = value
        .iter()
        .map(|exit| BuilderExitRequest {
            source_address: exit.source_address.into(),
            pubkey: exit.pubkey.serialize(),
        })
        .collect();
    SszList::try_from(requests)
        .map_err(|error| anyhow!("builder exit requests exceed bound: {error:?}"))
}

/// Builds SSZ `ExecutionRequestsElectraFulu` from a lighthouse Electra or Fulu execution requests
/// container, which share the identical `ExecutionRequestsElectra` type.
fn execution_requests_electra_fulu(
    value: &LighthouseExecutionRequestsElectra<MainnetEthSpec>,
) -> anyhow::Result<ExecutionRequestsElectraFulu> {
    Ok(ExecutionRequestsElectraFulu {
        deposits: deposit_requests(&value.deposits)?,
        withdrawals: withdrawal_requests(&value.withdrawals)?,
        consolidations: consolidation_requests(&value.consolidations)?,
    })
}

/// Builds SSZ `ExecutionRequestsGloas` from a lighthouse Gloas execution requests container, which
/// EIP-8282 extends with the builder deposit and builder exit lists.
fn execution_requests_gloas(
    value: &LighthouseExecutionRequestsGloas<MainnetEthSpec>,
) -> anyhow::Result<ExecutionRequestsGloas> {
    Ok(ExecutionRequestsGloas {
        deposits: deposit_requests(&value.deposits)?,
        withdrawals: withdrawal_requests(&value.withdrawals)?,
        consolidations: consolidation_requests(&value.consolidations)?,
        builder_deposits: builder_deposit_requests(&value.builder_deposits)?,
        builder_exits: builder_exit_requests(&value.builder_exits)?,
    })
}
