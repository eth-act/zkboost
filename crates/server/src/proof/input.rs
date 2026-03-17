//! Converts a `NewPayloadRequest` and its `ExecutionWitness` into zkVM `Input` for Reth or Ethrex
//! guest programs.

use std::sync::Arc;

use alloy_eips::{eip4895::Withdrawal as AlloyWithdrawal, eip7685::RequestsOrHash};
use alloy_genesis::ChainConfig;
use alloy_primitives::{B256, Bloom, Bytes};
use alloy_rpc_types_engine::{
    CancunPayloadFields, ExecutionData, ExecutionPayload as AlloyExecutionPayload,
    ExecutionPayloadSidecar, ExecutionPayloadV1 as AlloyExecutionPayloadV1,
    ExecutionPayloadV2 as AlloyExecutionPayloadV2, ExecutionPayloadV3 as AlloyExecutionPayloadV3,
    PraguePayloadFields,
};
use ere_zkvm_interface::Input;
use reth_stateless::ExecutionWitness;
use stateless_validator_ethrex::guest::{
    StatelessValidatorEthrexInput, StatelessValidatorEthrexIo,
};
use stateless_validator_reth::guest::{Io, StatelessValidatorRethInput, StatelessValidatorRethIo};
use zkboost_types::{ElKind, Hash256, MainnetEthSpec, NewPayloadRequest, TreeHash};

#[rustfmt::skip]
pub use reth_stateless::StatelessInput;

/// Combines a `NewPayloadRequest` with its execution witness and chain config, eagerly computing
/// the `StatelessInput`.
#[derive(Debug)]
pub(crate) struct NewPayloadRequestWithWitness {
    new_payload_request_root: Hash256,
    stateless_input: StatelessInput,
}

impl NewPayloadRequestWithWitness {
    /// Constructs a new instance by eagerly computing the `StatelessInput`.
    pub(crate) fn new(
        new_payload_request: &NewPayloadRequest<MainnetEthSpec>,
        witness: Arc<ExecutionWitness>,
        chain_config: Arc<ChainConfig>,
    ) -> anyhow::Result<Self> {
        let new_payload_request_root = new_payload_request.tree_hash_root();
        let execution_data = new_payload_request_to_execution_data(new_payload_request)?;
        let block = execution_data
            .payload
            .try_into_block_with_sidecar(&execution_data.sidecar)?;
        let stateless_input = StatelessInput {
            block,
            witness: Arc::unwrap_or_clone(witness),
            chain_config: Arc::unwrap_or_clone(chain_config),
        };
        Ok(Self {
            new_payload_request_root,
            stateless_input,
        })
    }

    /// Returns tree hash root of `NewPayloadRequest`.
    pub(crate) fn root(&self) -> Hash256 {
        self.new_payload_request_root
    }

    /// Generates zkVM input for the given EL kind.
    pub(crate) fn to_zkvm_input(&self, el_kind: ElKind) -> anyhow::Result<Input> {
        let stdin = match el_kind {
            ElKind::Ethrex => StatelessValidatorEthrexIo::serialize_input(
                &StatelessValidatorEthrexInput::new(&self.stateless_input, true)?,
            )?,
            ElKind::Reth => StatelessValidatorRethIo::serialize_input(
                &StatelessValidatorRethInput::new(&self.stateless_input, true)?,
            )?,
        };
        Ok(Input::new().with_prefixed_stdin(stdin))
    }
}

macro_rules! convert_payload_to_v1 {
    ($payload:expr) => {{
        let payload = $payload;
        AlloyExecutionPayloadV1 {
            parent_hash: payload.parent_hash.0,
            fee_recipient: payload.fee_recipient,
            state_root: payload.state_root,
            receipts_root: payload.receipts_root,
            logs_bloom: Bloom::from_slice(&payload.logs_bloom),
            prev_randao: payload.prev_randao,
            block_number: payload.block_number,
            gas_limit: payload.gas_limit,
            gas_used: payload.gas_used,
            timestamp: payload.timestamp,
            extra_data: Bytes::copy_from_slice(payload.extra_data.as_ref()),
            base_fee_per_gas: payload.base_fee_per_gas,
            block_hash: payload.block_hash.0,
            transactions: payload
                .transactions
                .iter()
                .map(|tx| Bytes::copy_from_slice(tx.as_ref()))
                .collect(),
        }
    }};
}

macro_rules! convert_payload_to_v1_with_withdrawals {
    ($payload:expr) => {{
        let payload = $payload;
        let v1 = convert_payload_to_v1!(payload);
        let withdrawals: Vec<AlloyWithdrawal> =
            payload.withdrawals.iter().map(convert_withdrawal).collect();
        (v1, withdrawals)
    }};
}

fn new_payload_request_to_execution_data(
    request: &NewPayloadRequest<MainnetEthSpec>,
) -> anyhow::Result<ExecutionData> {
    match request {
        NewPayloadRequest::Bellatrix(inner) => {
            let v1 = convert_payload_to_v1!(&inner.execution_payload);
            Ok(ExecutionData::new(
                AlloyExecutionPayload::V1(v1),
                ExecutionPayloadSidecar::none(),
            ))
        }
        NewPayloadRequest::Capella(inner) => {
            let (v1, withdrawals) =
                convert_payload_to_v1_with_withdrawals!(&inner.execution_payload);
            let v2 = AlloyExecutionPayloadV2 {
                payload_inner: v1,
                withdrawals,
            };
            Ok(ExecutionData::new(
                AlloyExecutionPayload::V2(v2),
                ExecutionPayloadSidecar::none(),
            ))
        }
        NewPayloadRequest::Deneb(inner) => {
            let (v1, withdrawals) =
                convert_payload_to_v1_with_withdrawals!(&inner.execution_payload);
            let v3 = AlloyExecutionPayloadV3 {
                payload_inner: AlloyExecutionPayloadV2 {
                    payload_inner: v1,
                    withdrawals,
                },
                blob_gas_used: inner.execution_payload.blob_gas_used,
                excess_blob_gas: inner.execution_payload.excess_blob_gas,
            };
            let versioned_hashes = inner
                .versioned_hashes
                .iter()
                .map(|versioned_hash| B256::from(versioned_hash.0))
                .collect();
            let cancun_fields =
                CancunPayloadFields::new(inner.parent_beacon_block_root, versioned_hashes);
            Ok(ExecutionData::new(
                AlloyExecutionPayload::V3(v3),
                ExecutionPayloadSidecar::v3(cancun_fields),
            ))
        }
        NewPayloadRequest::Electra(inner) => {
            let (v1, withdrawals) =
                convert_payload_to_v1_with_withdrawals!(&inner.execution_payload);
            let v3 = AlloyExecutionPayloadV3 {
                payload_inner: AlloyExecutionPayloadV2 {
                    payload_inner: v1,
                    withdrawals,
                },
                blob_gas_used: inner.execution_payload.blob_gas_used,
                excess_blob_gas: inner.execution_payload.excess_blob_gas,
            };
            let versioned_hashes = inner
                .versioned_hashes
                .iter()
                .map(|versioned_hash| B256::from(versioned_hash.0))
                .collect();
            let cancun_fields =
                CancunPayloadFields::new(inner.parent_beacon_block_root, versioned_hashes);
            let requests_hash = inner.execution_requests.requests_hash();
            let prague_fields = PraguePayloadFields::new(RequestsOrHash::Hash(requests_hash));
            Ok(ExecutionData::new(
                AlloyExecutionPayload::V3(v3),
                ExecutionPayloadSidecar::v4(cancun_fields, prague_fields),
            ))
        }
        NewPayloadRequest::Fulu(inner) => {
            let (v1, withdrawals) =
                convert_payload_to_v1_with_withdrawals!(&inner.execution_payload);
            let v3 = AlloyExecutionPayloadV3 {
                payload_inner: AlloyExecutionPayloadV2 {
                    payload_inner: v1,
                    withdrawals,
                },
                blob_gas_used: inner.execution_payload.blob_gas_used,
                excess_blob_gas: inner.execution_payload.excess_blob_gas,
            };
            let versioned_hashes = inner
                .versioned_hashes
                .iter()
                .map(|versioned_hash| B256::from(versioned_hash.0))
                .collect();
            let cancun_fields =
                CancunPayloadFields::new(inner.parent_beacon_block_root, versioned_hashes);
            let requests_hash = inner.execution_requests.requests_hash();
            let prague_fields = PraguePayloadFields::new(RequestsOrHash::Hash(requests_hash));
            Ok(ExecutionData::new(
                AlloyExecutionPayload::V3(v3),
                ExecutionPayloadSidecar::v4(cancun_fields, prague_fields),
            ))
        }
        NewPayloadRequest::Gloas(inner) => {
            let (v1, withdrawals) =
                convert_payload_to_v1_with_withdrawals!(&inner.execution_payload);
            let v3 = AlloyExecutionPayloadV3 {
                payload_inner: AlloyExecutionPayloadV2 {
                    payload_inner: v1,
                    withdrawals,
                },
                blob_gas_used: inner.execution_payload.blob_gas_used,
                excess_blob_gas: inner.execution_payload.excess_blob_gas,
            };
            let versioned_hashes = inner
                .versioned_hashes
                .iter()
                .map(|versioned_hash| B256::from(versioned_hash.0))
                .collect();
            let cancun_fields =
                CancunPayloadFields::new(inner.parent_beacon_block_root, versioned_hashes);
            let requests_hash = inner.execution_requests.requests_hash();
            let prague_fields = PraguePayloadFields::new(RequestsOrHash::Hash(requests_hash));
            Ok(ExecutionData::new(
                AlloyExecutionPayload::V3(v3),
                ExecutionPayloadSidecar::v4(cancun_fields, prague_fields),
            ))
        }
    }
}

fn convert_withdrawal(withdrawal: &zkboost_types::Withdrawal) -> AlloyWithdrawal {
    AlloyWithdrawal {
        index: withdrawal.index,
        validator_index: withdrawal.validator_index,
        address: withdrawal.address,
        amount: withdrawal.amount,
    }
}
