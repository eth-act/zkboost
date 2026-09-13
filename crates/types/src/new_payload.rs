//! Conversion between the `engine_newPayloadV5` parameters and the `NewPayloadRequest` of the
//! stateless validator.
//!
//! Execution requests follow EIP-7685 on the wire. Each element is one byte of request type
//! followed by the SSZ encoding of the request list, elements are ordered by request type, and
//! empty lists are omitted.

use alloy_eips::eip4895::Withdrawal as EngineWithdrawal;
use alloy_primitives::{B256, Bytes, U256};
use alloy_rpc_types_engine::{
    ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3 as EngineExecutionPayloadV3,
    ExecutionPayloadV4 as EngineExecutionPayloadV4,
};
use anyhow::{anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stateless_validator_common::{
    ProgressiveList, SszDecode, SszEncode, SszList,
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

/// Positional parameters of `engine_newPayloadV5`, the only proven `engine_newPayload` version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPayloadParams(
    /// `executionPayload`
    pub EngineExecutionPayloadV4,
    /// `expectedBlobVersionedHashes`
    pub Vec<B256>,
    /// `parentBeaconBlockRoot`
    pub B256,
    /// `executionRequests`
    pub Vec<Bytes>,
);

impl NewPayloadParams {
    /// The proven `engine_newPayload` method.
    pub const METHOD: &str = "engine_newPayloadV5";
    /// The `engine_newPayloadWithWitness` variant of the proven method.
    pub const WITH_WITNESS_METHOD: &str = "engine_newPayloadWithWitnessV5";

    /// Decodes the parameters of `method`, or returns `None` for any other method.
    pub fn decode(method: &str, params: Value) -> Option<serde_json::Result<Self>> {
        (method == Self::METHOD).then(|| serde_json::from_value(params))
    }

    /// Returns the fork-independent fields of the execution payload.
    pub fn execution_payload_v1(&self) -> &ExecutionPayloadV1 {
        &self.0.payload_inner.payload_inner.payload_inner
    }
}

impl TryFrom<NewPayloadParams> for NewPayloadRequest {
    type Error = anyhow::Error;

    fn try_from(params: NewPayloadParams) -> anyhow::Result<Self> {
        let NewPayloadParams(payload, versioned_hashes, parent_beacon_block_root, requests) =
            params;
        let versioned_hashes: Vec<[u8; 32]> =
            versioned_hashes.into_iter().map(|hash| hash.0).collect();
        Ok(Self::Gloas(NewPayloadRequestGloas {
            execution_payload: execution_payload_v4(payload)?,
            versioned_hashes: versioned_hashes.into(),
            parent_beacon_block_root: parent_beacon_block_root.0,
            execution_requests: execution_requests_gloas(&requests)?,
        }))
    }
}

impl TryFrom<&NewPayloadRequest> for NewPayloadParams {
    type Error = anyhow::Error;

    fn try_from(request: &NewPayloadRequest) -> anyhow::Result<Self> {
        let NewPayloadRequest::Gloas(request) = request else {
            bail!("payloads before gloas have no proven engine_newPayload version");
        };
        let payload = &request.execution_payload;
        let execution_payload = EngineExecutionPayloadV4 {
            payload_inner: EngineExecutionPayloadV3 {
                payload_inner: ExecutionPayloadV2 {
                    payload_inner: ExecutionPayloadV1 {
                        parent_hash: payload.parent_hash.into(),
                        fee_recipient: payload.fee_recipient.into(),
                        state_root: payload.state_root.into(),
                        receipts_root: payload.receipts_root.into(),
                        logs_bloom: payload.logs_bloom.into(),
                        prev_randao: payload.prev_randao.into(),
                        block_number: payload.block_number,
                        gas_limit: payload.gas_limit,
                        gas_used: payload.gas_used,
                        timestamp: payload.timestamp,
                        extra_data: Bytes::copy_from_slice(&payload.extra_data),
                        base_fee_per_gas: U256::from_le_bytes(payload.base_fee_per_gas),
                        block_hash: payload.block_hash.into(),
                        transactions: payload
                            .transactions
                            .iter()
                            .map(|transaction| Bytes::copy_from_slice(transaction))
                            .collect(),
                    },
                    withdrawals: payload
                        .withdrawals
                        .iter()
                        .map(|withdrawal| EngineWithdrawal {
                            index: withdrawal.index,
                            validator_index: withdrawal.validator_index,
                            address: withdrawal.address.into(),
                            amount: withdrawal.amount,
                        })
                        .collect(),
                },
                blob_gas_used: payload.blob_gas_used,
                excess_blob_gas: payload.excess_blob_gas,
            },
            block_access_list: Bytes::copy_from_slice(&payload.block_access_list),
            slot_number: payload.slot_number,
        };
        let requests = &request.execution_requests;
        let execution_requests = [
            encode_requests(DEPOSIT_REQUEST_TYPE, &requests.deposits),
            encode_requests(WITHDRAWAL_REQUEST_TYPE, &requests.withdrawals),
            encode_requests(CONSOLIDATION_REQUEST_TYPE, &requests.consolidations),
            encode_requests(BUILDER_DEPOSIT_REQUEST_TYPE, &requests.builder_deposits),
            encode_requests(BUILDER_EXIT_REQUEST_TYPE, &requests.builder_exits),
        ]
        .into_iter()
        .flatten()
        .collect();
        Ok(Self(
            execution_payload,
            request.versioned_hashes.iter().map(B256::from).collect(),
            request.parent_beacon_block_root.into(),
            execution_requests,
        ))
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

fn encode_requests<T: SszEncode>(request_type: u8, requests: &ProgressiveList<T>) -> Option<Bytes> {
    if requests.is_empty() {
        return None;
    }
    let mut bytes = vec![request_type];
    requests.ssz_append(&mut bytes);
    Some(bytes.into())
}

fn decode_requests<T: SszDecode>(bytes: &[u8], label: &str) -> anyhow::Result<ProgressiveList<T>> {
    ProgressiveList::from_ssz_bytes(bytes)
        .map_err(|err| anyhow!("{label} are not decodable: {err:?}"))
}

#[cfg(test)]
mod tests {
    use stateless_validator_common::guest::{
        StatelessInput, input::new_payload_request::NewPayloadRequest,
    };

    use crate::NewPayloadParams;

    /// The stateless input of block 93354 of glamsterdam-devnet-8.
    const AMSTERDAM_STATELESS_INPUT: &[u8] =
        include_bytes!("../../server/tests/fixture/stateless_input_amsterdam.ssz");

    /// A payload survives the trip through the Engine API parameters, including their JSON form,
    /// unchanged.
    #[test]
    fn test_new_payload_params_round_trip() {
        let (_, input) =
            StatelessInput::from_schema_prefixed_ssz(AMSTERDAM_STATELESS_INPUT).unwrap();
        let request = input.new_payload_request;

        let params = NewPayloadParams::try_from(&request).unwrap();
        let json = serde_json::to_value(&params).unwrap();
        assert!(json.as_array().is_some_and(|params| params.len() == 4));
        let params = NewPayloadParams::decode(NewPayloadParams::METHOD, json)
            .unwrap()
            .unwrap();

        assert_eq!(NewPayloadRequest::try_from(params).unwrap(), request);
    }
}
