//! CL beacon API client and beacon block decoding.

use anyhow::{anyhow, bail};
use futures::{Stream, StreamExt};
use lighthouse_types::{
    BeaconBlockRef, ExecutionRequestsElectra as LighthouseExecutionRequests, ForkName,
    ForkVersionDecode, Hash256, KzgCommitment, MainnetEthSpec, SignedBeaconBlock,
    Transactions as LighthouseTransactions, Withdrawals as LighthouseWithdrawals,
};
use reqwest_eventsource::{Event as SseEvent, EventSource};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use url::Url;
use zkboost_types::{
    ConsolidationRequest, DepositRequest, ExecutionPayloadV1, ExecutionPayloadV2,
    ExecutionPayloadV3, ExecutionRequests, NewPayloadRequest, NewPayloadRequestBellatrix,
    NewPayloadRequestCapella, NewPayloadRequestDeneb, NewPayloadRequestElectraFulu, SszList,
    Transaction, Transactions, VersionedHashes, Withdrawal, WithdrawalRequest, Withdrawals,
};

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
    #[serde(default, with = "serde_utils::quoted_u64")]
    pub(crate) max_blobs_per_block: u64,
    #[serde(default, with = "serde_utils::quoted_u64")]
    pub(crate) max_blobs_per_block_electra: u64,
    #[serde(default)]
    pub(crate) blob_schedule: Vec<BlobParameters>,
}

/// A single EIP-7892 blob-parameter-only fork entry from the consensus spec.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) struct BlobParameters {
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) epoch: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) max_blobs_per_block: u64,
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

/// Converts a lighthouse beacon block into the `NewPayloadRequest`.
///
/// The execution payload and execution requests are converted field by field into the SSZ types,
/// so a lighthouse layout change surfaces as a compile error rather than a silent mismatch. The
/// lighthouse Electra and Fulu forks both map to the `ElectraFulu` variant.
pub(crate) fn new_payload_request_from_beacon_block(
    block: &SignedBeaconBlock<MainnetEthSpec>,
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
                execution_requests: execution_requests(&b.body.execution_requests)?,
            },
        )),
        BeaconBlockRef::Fulu(b) => Ok(NewPayloadRequest::ElectraFulu(
            NewPayloadRequestElectraFulu {
                execution_payload: execution_payload_v3!(
                    &b.body.execution_payload.execution_payload
                ),
                versioned_hashes: versioned_hashes(&b.body.blob_kzg_commitments)?,
                parent_beacon_block_root: b.parent_root.0,
                execution_requests: execution_requests(&b.body.execution_requests)?,
            },
        )),
        BeaconBlockRef::Gloas(_) => unimplemented!(),
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

/// Builds SSZ `ExecutionRequests` from a lighthouse Electra or Fulu execution requests container,
/// which share the identical `ExecutionRequestsElectra` type.
fn execution_requests(
    value: &LighthouseExecutionRequests<MainnetEthSpec>,
) -> anyhow::Result<ExecutionRequests> {
    let deposits: Vec<DepositRequest> = value
        .deposits
        .iter()
        .map(|deposit| DepositRequest {
            pubkey: deposit.pubkey.serialize(),
            withdrawal_credentials: deposit.withdrawal_credentials.into(),
            amount: deposit.amount,
            signature: deposit.signature.serialize(),
            index: deposit.index,
        })
        .collect();
    let withdrawals: Vec<WithdrawalRequest> = value
        .withdrawals
        .iter()
        .map(|withdrawal| WithdrawalRequest {
            source_address: withdrawal.source_address.into(),
            validator_pubkey: withdrawal.validator_pubkey.serialize(),
            amount: withdrawal.amount,
        })
        .collect();
    let consolidations: Vec<ConsolidationRequest> = value
        .consolidations
        .iter()
        .map(|consolidation| ConsolidationRequest {
            source_address: consolidation.source_address.into(),
            source_pubkey: consolidation.source_pubkey.serialize(),
            target_pubkey: consolidation.target_pubkey.serialize(),
        })
        .collect();
    Ok(ExecutionRequests {
        deposits: SszList::try_from(deposits)
            .map_err(|error| anyhow!("deposit requests exceed bound: {error:?}"))?,
        withdrawals: SszList::try_from(withdrawals)
            .map_err(|error| anyhow!("withdrawal requests exceed bound: {error:?}"))?,
        consolidations: SszList::try_from(consolidations)
            .map_err(|error| anyhow!("consolidation requests exceed bound: {error:?}"))?,
    })
}
