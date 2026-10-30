//! The proven `engine_newPayload` versions and the decoding of their parameters into the stateless
//! validator `NewPayloadRequest`. Execution requests follow EIP-7685 on the wire. Each element is
//! one request type byte and the SSZ request list. The elements are ordered by type and empty lists
//! are omitted.

use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_engine::ExecutionPayloadV4 as EngineExecutionPayloadV4;
use anyhow::{anyhow, bail, ensure};
use serde::Deserialize;
use stateless_validator_common::{
    ProgressiveList, SszDecode, SszList,
    guest::input::new_payload_request::{
        ExecutionPayloadV4, ExecutionRequestsGloas, NewPayloadRequest, NewPayloadRequestGloas,
        Withdrawal,
    },
};

const DEPOSIT_REQUEST_TYPE: u8 = 0;
const WITHDRAWAL_REQUEST_TYPE: u8 = 1;
const CONSOLIDATION_REQUEST_TYPE: u8 = 2;
const BUILDER_DEPOSIT_REQUEST_TYPE: u8 = 3;
const BUILDER_EXIT_REQUEST_TYPE: u8 = 4;

/// An `engine_newPayload` version that zkboost proves. A new version is one more variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewPayloadVersion {
    /// `engine_newPayloadV5`, a Gloas payload.
    V5,
}

impl NewPayloadVersion {
    /// Returns the version of a proven `engine_newPayload` method.
    pub fn from_method(method: &str) -> Option<Self> {
        match method {
            "engine_newPayloadV5" => Some(Self::V5),
            _ => None,
        }
    }

    /// Returns the `engine_newPayload` method.
    pub fn method(self) -> &'static str {
        match self {
            Self::V5 => "engine_newPayloadV5",
        }
    }

    /// Returns the `engine_newPayloadWithWitness` method.
    pub fn with_witness_method(self) -> &'static str {
        match self {
            Self::V5 => "engine_newPayloadWithWitnessV5",
        }
    }

    /// Decodes the JSON params of the method.
    pub fn decode_params(self, params: &str) -> anyhow::Result<NewPayloadRequest> {
        match self {
            Self::V5 => serde_json::from_str::<NewPayloadRequestV5>(params)?.try_into(),
        }
    }
}

/// The `engine_newPayloadV5` params as `NewPayloadRequestGloas` with the Engine API types.
#[derive(Debug, Deserialize)]
struct NewPayloadRequestV5 {
    execution_payload: EngineExecutionPayloadV4,
    versioned_hashes: Vec<B256>,
    parent_beacon_block_root: B256,
    execution_requests: Vec<Bytes>,
}

impl TryFrom<NewPayloadRequestV5> for NewPayloadRequest {
    type Error = anyhow::Error;

    fn try_from(request: NewPayloadRequestV5) -> anyhow::Result<Self> {
        let versioned_hashes: Vec<[u8; 32]> = request
            .versioned_hashes
            .into_iter()
            .map(|hash| hash.0)
            .collect();
        Ok(Self::Gloas(NewPayloadRequestGloas {
            execution_payload: execution_payload_v4(request.execution_payload)?,
            versioned_hashes: versioned_hashes.into(),
            parent_beacon_block_root: request.parent_beacon_block_root.0,
            execution_requests: execution_requests_gloas(&request.execution_requests)?,
        }))
    }
}

fn execution_payload_v4(payload: EngineExecutionPayloadV4) -> anyhow::Result<ExecutionPayloadV4> {
    let v3 = payload.payload_inner;
    let v2 = v3.payload_inner;
    let v1 = v2.payload_inner;
    let transactions: Vec<ProgressiveList<u8>> = v1
        .transactions
        .iter()
        .map(|transaction| transaction.to_vec().into())
        .collect();
    let withdrawals: Vec<Withdrawal> = v2
        .withdrawals
        .iter()
        .map(|withdrawal| Withdrawal {
            index: withdrawal.index,
            validator_index: withdrawal.validator_index,
            address: withdrawal.address.into_array(),
            amount: withdrawal.amount,
        })
        .collect();
    Ok(ExecutionPayloadV4 {
        parent_hash: v1.parent_hash.0,
        fee_recipient: v1.fee_recipient.into_array(),
        state_root: v1.state_root.0,
        receipts_root: v1.receipts_root.0,
        logs_bloom: v1.logs_bloom.0.0,
        prev_randao: v1.prev_randao.0,
        block_number: v1.block_number,
        gas_limit: v1.gas_limit,
        gas_used: v1.gas_used,
        timestamp: v1.timestamp,
        extra_data: SszList::try_from(v1.extra_data.to_vec())
            .map_err(|err| anyhow!("extra data length should be within bounds: {err:?}"))?,
        base_fee_per_gas: v1.base_fee_per_gas.to_le_bytes(),
        block_hash: v1.block_hash.0,
        transactions: transactions.into(),
        withdrawals: withdrawals.into(),
        blob_gas_used: v3.blob_gas_used,
        excess_blob_gas: v3.excess_blob_gas,
        block_access_list: payload.block_access_list.to_vec().into(),
        slot_number: payload.slot_number,
    })
}

/// Decodes the EIP-7685 request list into the Gloas execution requests container.
fn execution_requests_gloas(requests: &[Bytes]) -> anyhow::Result<ExecutionRequestsGloas> {
    let mut decoded = ExecutionRequestsGloas::default();
    let mut previous_type = None;
    for (index, request) in requests.iter().enumerate() {
        let Some((&request_type, request_data)) = request.split_first() else {
            bail!("execution request #{index} is empty");
        };
        ensure!(
            !request_data.is_empty(),
            "execution request #{index} has empty request data"
        );
        ensure!(
            previous_type.is_none_or(|previous| previous < request_type),
            "execution request #{index} is out of order"
        );
        previous_type = Some(request_type);
        match request_type {
            DEPOSIT_REQUEST_TYPE => {
                decoded.deposits = decode_requests(request_data, "deposit requests")?
            }
            WITHDRAWAL_REQUEST_TYPE => {
                decoded.withdrawals = decode_requests(request_data, "withdrawal requests")?
            }
            CONSOLIDATION_REQUEST_TYPE => {
                decoded.consolidations = decode_requests(request_data, "consolidation requests")?
            }
            BUILDER_DEPOSIT_REQUEST_TYPE => {
                decoded.builder_deposits =
                    decode_requests(request_data, "builder deposit requests")?
            }
            BUILDER_EXIT_REQUEST_TYPE => {
                decoded.builder_exits = decode_requests(request_data, "builder exit requests")?
            }
            other => bail!("execution request #{index} has unknown type {other}"),
        }
    }
    Ok(decoded)
}

fn decode_requests<T: SszDecode>(bytes: &[u8], label: &str) -> anyhow::Result<ProgressiveList<T>> {
    ProgressiveList::from_ssz_bytes(bytes)
        .map_err(|err| anyhow!("{label} are not decodable: {err:?}"))
}

/// Reads of a `NewPayloadRequest` for every fork. A field that only later forks have is `None`
/// for an earlier fork.
pub trait NewPayloadRequestExt {
    /// Returns the block hash of the execution payload.
    fn block_hash(&self) -> B256;
    /// Returns the gas used by the execution payload.
    fn gas_used(&self) -> u64;
    /// Returns the gas limit of the execution payload.
    fn gas_limit(&self) -> u64;
    /// Returns the timestamp of the execution payload, the start of its slot.
    fn timestamp(&self) -> u64;
    /// Returns the parent beacon block root, part of the request since Deneb.
    fn parent_beacon_block_root(&self) -> Option<B256>;
    /// Returns the slot of the execution payload, part of the payload since Gloas.
    fn slot(&self) -> Option<u64>;
}

/// Reads a field that the execution payload of every fork has.
macro_rules! execution_payload_field {
    ($request:expr, $field:ident) => {
        match $request {
            NewPayloadRequest::Bellatrix(request) => &request.execution_payload.$field,
            NewPayloadRequest::Capella(request) => &request.execution_payload.$field,
            NewPayloadRequest::Deneb(request) => &request.execution_payload.$field,
            NewPayloadRequest::ElectraFulu(request) => &request.execution_payload.$field,
            NewPayloadRequest::Gloas(request) => &request.execution_payload.$field,
        }
    };
}

impl NewPayloadRequestExt for NewPayloadRequest {
    fn block_hash(&self) -> B256 {
        B256::from(*execution_payload_field!(self, block_hash))
    }

    fn gas_used(&self) -> u64 {
        *execution_payload_field!(self, gas_used)
    }

    fn gas_limit(&self) -> u64 {
        *execution_payload_field!(self, gas_limit)
    }

    fn timestamp(&self) -> u64 {
        *execution_payload_field!(self, timestamp)
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        match self {
            Self::Bellatrix(_) | Self::Capella(_) => None,
            Self::Deneb(request) => Some(B256::from(request.parent_beacon_block_root)),
            Self::ElectraFulu(request) => Some(B256::from(request.parent_beacon_block_root)),
            Self::Gloas(request) => Some(B256::from(request.parent_beacon_block_root)),
        }
    }

    fn slot(&self) -> Option<u64> {
        match self {
            Self::Bellatrix(_) | Self::Capella(_) | Self::Deneb(_) | Self::ElectraFulu(_) => None,
            Self::Gloas(request) => Some(request.execution_payload.slot_number),
        }
    }
}
