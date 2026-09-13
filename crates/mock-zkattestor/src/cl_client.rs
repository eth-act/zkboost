//! CL beacon API client, request forwarding to the CL, and Gloas beacon block decoding.

use std::time::Duration;

use anyhow::{anyhow, bail};
use axum::{
    body::Bytes,
    http::{HeaderValue, Method, StatusCode, header::CONTENT_TYPE},
};
use futures::{Stream, StreamExt};
use lighthouse_bls::PublicKey;
use lighthouse_types::{
    BeaconBlockRef, BuilderDepositRequests as LighthouseBuilderDepositRequests,
    BuilderExitRequests as LighthouseBuilderExitRequests,
    ConsolidationRequests as LighthouseConsolidationRequests,
    DepositRequests as LighthouseDepositRequests,
    ExecutionRequestsGloas as LighthouseExecutionRequestsGloas, ForkName, ForkVersionDecode,
    Hash256, KzgCommitment, MainnetEthSpec, SignedBeaconBlock, SignedExecutionPayloadEnvelope,
    WithdrawalRequests as LighthouseWithdrawalRequests,
};
use reqwest_eventsource::{Event as SseEvent, EventSource};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;
use zkboost_types::{
    BuilderDepositRequest, BuilderDepositRequests, BuilderExitRequest, BuilderExitRequests,
    ConsolidationRequest, ConsolidationRequests, DepositRequest, DepositRequests,
    ExecutionPayloadV4, ExecutionRequestsGloas, NewPayloadRequestGloas, ProgressiveList, SszList,
    VersionedHashes, Withdrawal, WithdrawalRequest, WithdrawalRequests,
};

/// Interval between execution payload envelope fetches.
const ENVELOPE_RETRY_INTERVAL: Duration = Duration::from_millis(200);

/// Budget for fetching an execution payload envelope. Under ePBS the builder reveals the payload
/// after the block itself and may do so as late as the payload deadline three quarters into the
/// slot, so a whole mainnet slot bounds the wait.
const ENVELOPE_TIMEOUT: Duration = Duration::from_secs(12);

/// A `block` event of the beacon API event stream.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Block {
    /// Slot of the block.
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) slot: u64,
    /// Root of the block.
    pub(crate) block: Hash256,
}

/// CL config.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) struct Spec {
    /// Chain id of the deposit contract, which is the chain id of the EL.
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) deposit_chain_id: u64,
}

/// Client of the beacon API of the CL.
#[derive(Clone)]
pub(crate) struct ClClient {
    base_url: Url,
    http: reqwest::Client,
}

impl ClClient {
    /// Creates a client of the beacon API at `base_url`.
    pub(crate) fn new(base_url: Url) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
        }
    }

    /// Streams every `block` event of the beacon API event stream.
    pub(crate) fn subscribe_block_events(
        &self,
    ) -> impl Stream<Item = Result<Block, anyhow::Error>> + Send + '_ {
        async_stream::try_stream! {
            let mut url = self.base_url.join("eth/v1/events")?;
            url.query_pairs_mut().append_pair("topics", "block");
            let mut event_source = EventSource::new(self.http.get(url))?;
            while let Some(event) = event_source.next().await {
                match event {
                    Ok(SseEvent::Open) => {}
                    Ok(SseEvent::Message(message)) if message.event == "block" => {
                        let block_event: Block = serde_json::from_str(&message.data)?;
                        yield block_event
                    }
                    Ok(SseEvent::Message(_)) => {}
                    Err(error) => {
                        event_source.close();
                        Err(anyhow!("{error}"))?;
                    }
                }
            }
        }
    }

    /// Fetches a signed beacon block by root, decoded from SSZ by its fork.
    pub(crate) async fn get_beacon_block(
        &self,
        block_root: Hash256,
    ) -> anyhow::Result<SignedBeaconBlock<MainnetEthSpec>> {
        let url = self
            .base_url
            .join(&format!("eth/v2/beacon/blocks/{block_root}"))?;
        let response = self
            .http
            .get(url)
            .header("Accept", "application/octet-stream")
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("{status}: {body}");
        }
        let fork_name: ForkName = response
            .headers()
            .get("Eth-Consensus-Version")
            .ok_or_else(|| anyhow!("missing Eth-Consensus-Version"))?
            .to_str()?
            .parse()
            .map_err(|error: String| anyhow!("{error}"))?;
        let bytes = response.bytes().await?;
        SignedBeaconBlock::from_ssz_bytes_by_fork(&bytes, fork_name)
            .map_err(|error| anyhow!("{error:?}"))
    }

    /// Fetches the execution payload envelope carrying a Gloas block's execution payload and
    /// execution requests, retrying until the builder publishes it or the budget expires.
    pub(crate) async fn get_execution_payload_envelope(
        &self,
        block_root: Hash256,
    ) -> anyhow::Result<SignedExecutionPayloadEnvelope<MainnetEthSpec>> {
        let path = format!("eth/v1/beacon/execution_payload_envelopes/{block_root}");
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
        self.get_json("eth/v1/config/spec").await
    }

    /// Fetches the genesis validators root.
    pub(crate) async fn get_genesis_validators_root(&self) -> anyhow::Result<Hash256> {
        #[derive(Deserialize)]
        struct Genesis {
            genesis_validators_root: Hash256,
        }

        Ok(self
            .get_json::<Genesis>("eth/v1/beacon/genesis")
            .await?
            .genesis_validators_root)
    }

    /// Fetches the current fork version of a state.
    pub(crate) async fn get_fork_version(&self, state_root: Hash256) -> anyhow::Result<[u8; 4]> {
        #[derive(Deserialize)]
        struct Fork {
            #[serde(with = "serde_utils::bytes_4_hex")]
            current_version: [u8; 4],
        }

        Ok(self
            .get_json::<Fork>(&format!("eth/v1/beacon/states/{state_root}/fork"))
            .await?
            .current_version)
    }

    /// Fetches the public key of a validator.
    pub(crate) async fn get_validator_pubkey(
        &self,
        validator_index: u64,
    ) -> anyhow::Result<PublicKey> {
        #[derive(Deserialize)]
        struct ValidatorData {
            validator: Validator,
        }
        #[derive(Deserialize)]
        struct Validator {
            #[serde(with = "serde_utils::hex_vec")]
            pubkey: Vec<u8>,
        }

        let data: ValidatorData = self
            .get_json(&format!(
                "eth/v1/beacon/states/head/validators/{validator_index}"
            ))
            .await?;
        PublicKey::deserialize(&data.validator.pubkey)
            .map_err(|error| anyhow!("validator {validator_index} pubkey: {error:?}"))
    }

    /// Forwards a request to the CL and returns the status, content type, and body of its
    /// response.
    pub(crate) async fn forward(
        &self,
        method: Method,
        path_and_query: &str,
        body: Bytes,
    ) -> anyhow::Result<(StatusCode, Option<HeaderValue>, Bytes)> {
        let url = self.base_url.join(path_and_query)?;
        let response = self.http.request(method, url).body(body).send().await?;
        let status = response.status();
        let content_type = response.headers().get(CONTENT_TYPE).cloned();
        Ok((status, content_type, response.bytes().await?))
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

/// Converts a lighthouse Gloas beacon block and its execution payload envelope into the
/// `NewPayloadRequestGloas`.
///
/// The block carries only a payload bid, so `envelope` supplies the execution payload and this
/// block's execution requests.
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

/// Builds SSZ `ExecutionRequestsGloas` from a lighthouse Gloas execution requests container, which
/// EIP-8282 extends with the builder deposit and builder exit lists.
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
