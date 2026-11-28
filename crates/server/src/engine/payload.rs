//! The `engine_newPayloadV5` payload, from the params of the request to the stdin of the zkVM.
//! Execution requests follow EIP-7685 on the wire. Each element is one request type byte and
//! the SSZ request list, ordered by type and without empty lists. The witness is the RLP list
//! `[headers, codes, state]`, with an optional `keys` list that is ignored.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant, SystemTime},
};

use alloy_primitives::{B256, Bytes as HexBytes, U64};
use alloy_rlp::{Header, PayloadView};
use alloy_rpc_types_engine::ExecutionPayloadV4 as EngineExecutionPayloadV4;
use anyhow::{Context, anyhow, bail, ensure};
use bytes::Bytes;
use serde::{Deserialize, de::IgnoredAny};
use serde_json::value::RawValue;
use stateless_validator_common::{
    HashTreeRoot, ProgressiveList, Sha2Hasher, SszDecode, SszEncode, SszList,
    guest::{
        StatelessInput, StatelessValidationResult,
        input::{
            ExecutionWitness, ProtocolFork,
            new_payload_request::{
                ExecutionPayloadV4, ExecutionRequestsGloas, NewPayloadRequest,
                NewPayloadRequestGloas, Withdrawal,
            },
        },
    },
};
use strum::{EnumIter, IntoStaticStr, VariantNames};

/// The status of a payload in the response of the EL, `Error` without a known status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, EnumIter, IntoStaticStr, VariantNames)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum PayloadStatus {
    Valid,
    Invalid,
    Syncing,
    Accepted,
    #[serde(other)]
    #[strum(serialize = "error")]
    Error,
}

impl PayloadStatus {
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The receipt of a payload on both clocks. The wall clock places it in its slot, and the
/// monotonic clock measures every later time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Receipt {
    pub(crate) wall_clock: SystemTime,
    pub(crate) monotonic: Instant,
}

impl Receipt {
    pub(crate) fn now() -> Self {
        Self {
            wall_clock: SystemTime::now(),
            monotonic: Instant::now(),
        }
    }
}

/// What the EL returned for a new payload.
pub(crate) struct NewPayloadResult {
    pub(crate) status: PayloadStatus,
    /// The witness of a `VALID` payload as a JSON string.
    pub(crate) witness: Option<Bytes>,
    /// The time until the EL responded to the witness method, `None` if it was not called.
    pub(crate) witness_fetch: Option<Duration>,
}

impl NewPayloadResult {
    /// Returns the size of the witness, which is a quoted hex string with the 0x prefix.
    pub(crate) fn witness_size(&self) -> usize {
        (self.witness.as_ref()).map_or(0, |witness| witness.len().saturating_sub(4) / 2)
    }
}

/// The fields of a payload that zkboost reads before it proves, decoded from the params
/// without the transactions.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(from = "HeaderParams")]
pub struct PayloadHeader {
    pub block_hash: B256,
    pub parent_beacon_block_root: B256,
    pub block_number: u64,
    pub slot: u64,
    /// The start of the slot.
    pub timestamp: u64,
    pub gas_used: u64,
    pub gas_limit: u64,
}

#[derive(Deserialize)]
struct HeaderParams(PayloadHeaderJson, IgnoredAny, B256, IgnoredAny);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PayloadHeaderJson {
    block_hash: B256,
    block_number: U64,
    slot_number: U64,
    timestamp: U64,
    gas_used: U64,
    gas_limit: U64,
}

impl PayloadHeader {
    /// Decodes the header from the body of an `engine_newPayloadV5` request, and returns it
    /// with the params of the request. This is CPU-bound work for the blocking pool.
    pub(crate) fn decode(body: &Bytes) -> anyhow::Result<(Self, Bytes)> {
        let request: BTreeMap<&str, &RawValue> = serde_json::from_slice(body)?;
        let params = request.get("params").context("request without params")?;
        let header = serde_json::from_str(params.get())?;
        Ok((header, body.slice_ref(params.get().as_bytes())))
    }
}

impl From<HeaderParams> for PayloadHeader {
    fn from(HeaderParams(payload, _, parent_beacon_block_root, _): HeaderParams) -> Self {
        Self {
            block_hash: payload.block_hash,
            parent_beacon_block_root,
            block_number: payload.block_number.to(),
            slot: payload.slot_number.to(),
            timestamp: payload.timestamp.to(),
            gas_used: payload.gas_used.to(),
            gas_limit: payload.gas_limit.to(),
        }
    }
}

/// The input of the zkVMs for one payload, built once for every proof type.
#[derive(Debug)]
pub(crate) struct ProofInput {
    /// The schema-id-prefixed SSZ `StatelessInput`.
    pub(crate) stdin: Vec<u8>,
    /// The public values that the guest commits for a valid payload.
    pub(crate) public_values: Vec<u8>,
}

impl ProofInput {
    /// Decodes the params and the JSON string of the witness, and encodes the stdin. This is
    /// CPU-bound work for the blocking pool.
    pub(crate) fn build(
        params: &[u8],
        witness: &[u8],
        fork: ProtocolFork,
        chain_id: u64,
    ) -> anyhow::Result<Self> {
        let new_payload_request = decode_new_payload_request(params)?;
        let witness = serde_json::from_slice::<HexBytes>(witness).context("witness is not hex")?;
        let public_values = public_values(&new_payload_request, fork, chain_id);
        let stdin = StatelessInput {
            new_payload_request,
            witness: decode_witness(&witness)?,
            chain_id,
        }
        .to_schema_prefixed_ssz(fork);
        Ok(Self {
            stdin,
            public_values,
        })
    }
}

/// Returns the SSZ `StatelessValidationResult` that a guest commits for a valid payload.
pub fn public_values(request: &NewPayloadRequest, fork: ProtocolFork, chain_id: u64) -> Vec<u8> {
    StatelessValidationResult {
        new_payload_request_root: request.hash_tree_root(&Sha2Hasher),
        successful_validation: true,
        chain_id,
        schema_id: fork.schema_id(),
    }
    .to_ssz()
}

pub fn decode_new_payload_request(params: &[u8]) -> anyhow::Result<NewPayloadRequest> {
    let (payload, versioned_hashes, parent_beacon_block_root, execution_requests): (
        EngineExecutionPayloadV4,
        Vec<B256>,
        B256,
        Vec<HexBytes>,
    ) = serde_json::from_slice(params)?;
    let versioned_hashes = Vec::from_iter(versioned_hashes.into_iter().map(|hash| hash.0));
    Ok(NewPayloadRequest::Gloas(NewPayloadRequestGloas {
        execution_payload: execution_payload_v4(payload)?,
        versioned_hashes: SszList::try_from(versioned_hashes)
            .map_err(|error| anyhow!("too many versioned hashes: {error:?}"))?,
        parent_beacon_block_root: parent_beacon_block_root.0,
        execution_requests: execution_requests_gloas(&execution_requests)?,
    }))
}

fn execution_payload_v4(payload: EngineExecutionPayloadV4) -> anyhow::Result<ExecutionPayloadV4> {
    let v3 = payload.payload_inner;
    let v2 = v3.payload_inner;
    let v1 = v2.payload_inner;
    let transactions = v1
        .transactions
        .iter()
        .map(|transaction| transaction.to_vec().into());
    let withdrawals = v2.withdrawals.iter().map(|withdrawal| Withdrawal {
        index: withdrawal.index,
        validator_index: withdrawal.validator_index,
        address: withdrawal.address.into_array(),
        amount: withdrawal.amount,
    });
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
            .map_err(|error| anyhow!("extra data too long: {error:?}"))?,
        base_fee_per_gas: v1.base_fee_per_gas.to_le_bytes(),
        block_hash: v1.block_hash.0,
        transactions: Vec::from_iter(transactions).into(),
        withdrawals: Vec::from_iter(withdrawals).into(),
        blob_gas_used: v3.blob_gas_used,
        excess_blob_gas: v3.excess_blob_gas,
        block_access_list: payload.block_access_list.to_vec().into(),
        slot_number: payload.slot_number,
    })
}

fn execution_requests_gloas(requests: &[HexBytes]) -> anyhow::Result<ExecutionRequestsGloas> {
    let mut decoded = ExecutionRequestsGloas::default();
    let mut previous_type = None;
    for request in requests {
        let (&request_type, data) = request.split_first().context("empty execution request")?;
        ensure!(
            !data.is_empty(),
            "execution request {request_type} has no data"
        );
        ensure!(
            previous_type < Some(request_type),
            "execution request {request_type} is out of order"
        );
        previous_type = Some(request_type);
        fn decode<T: SszDecode>(
            request_type: u8,
            data: &[u8],
        ) -> anyhow::Result<ProgressiveList<T>> {
            ProgressiveList::from_ssz_bytes(data)
                .map_err(|error| anyhow!("execution request {request_type}: {error:?}"))
        }
        match request_type {
            0 => decoded.deposits = decode(0, data)?,
            1 => decoded.withdrawals = decode(1, data)?,
            2 => decoded.consolidations = decode(2, data)?,
            3 => decoded.builder_deposits = decode(3, data)?,
            4 => decoded.builder_exits = decode(4, data)?,
            _ => bail!("unknown execution request type {request_type}"),
        }
    }
    Ok(decoded)
}

fn decode_witness(witness: &[u8]) -> anyhow::Result<ExecutionWitness> {
    /// Splits an RLP list into the raw encoding of its items.
    fn list(bytes: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
        match Header::decode_raw(&mut &*bytes) {
            Ok(PayloadView::List(items)) => Ok(items),
            Ok(PayloadView::String(_)) => bail!("not an RLP list"),
            Err(error) => bail!("{error}"),
        }
    }
    /// Decodes an RLP list of byte strings.
    fn strings(bytes: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
        fn string(item: &[u8]) -> anyhow::Result<&[u8]> {
            match Header::decode_raw(&mut &*item) {
                Ok(PayloadView::String(string)) => Ok(string),
                Ok(PayloadView::List(_)) => bail!("not an RLP string"),
                Err(error) => bail!("{error}"),
            }
        }
        list(bytes)?.into_iter().map(string).collect()
    }
    fn bounded<const N: usize>(items: Vec<&[u8]>) -> anyhow::Result<Vec<SszList<u8, N>>> {
        let bounded = |item: &[u8]| SszList::try_from(item.to_vec());
        Result::from_iter(items.into_iter().map(bounded))
            .map_err(|error| anyhow!("item too long: {error:?}"))
    }

    let fields = list(witness).context("witness")?;
    let [headers, codes, state, ..] = fields.as_slice() else {
        bail!(
            "witness has {} of the fields headers, codes, state",
            fields.len()
        );
    };
    let headers = (list(headers).and_then(bounded)).context("witness headers")?;
    Ok(ExecutionWitness {
        state: (strings(state).and_then(bounded))
            .context("witness state")?
            .into(),
        codes: (strings(codes).and_then(bounded))
            .context("witness codes")?
            .into(),
        headers: SszList::try_from(headers)
            .map_err(|error| anyhow!("too many witness headers: {error:?}"))?,
    })
}

#[cfg(test)]
mod tests {
    use std::slice;

    use alloy_primitives::{b256, hex};
    use alloy_rlp::Header;
    use stateless_validator_common::{
        SszList,
        guest::input::{ExecutionWitness, ProtocolFork},
    };

    use crate::engine::payload::{PayloadHeader, ProofInput, decode_witness};

    /// The stdin of block 93354 of glamsterdam-devnet-8, as the guests read it.
    const STDIN: &[u8] = include_bytes!("../../tests/fixture/stateless_input_amsterdam.ssz");

    /// The request and the witness of that block as the CL and the EL send them.
    const PARAMS: &[u8] = include_bytes!("../../tests/fixture/new_payload_params_amsterdam.json");

    const WITNESS: &[u8] = include_bytes!("../../tests/fixture/witness_amsterdam.rlp");

    #[test]
    fn test_proof_input_matches_fixture() {
        let witness = serde_json::to_vec(&hex::encode_prefixed(WITNESS)).unwrap();
        let input =
            ProofInput::build(PARAMS, &witness, ProtocolFork::Amsterdam, 7091047534).unwrap();
        assert!(input.stdin == STDIN);
        let public_values = hex!(
            "ae5c1a4a1808f07a248132e44180babaa796b5499b78509b96392cd7709f814e016ecca8a6010000000115"
        );
        assert_eq!(input.public_values, public_values);

        let header: PayloadHeader = serde_json::from_slice(PARAMS).unwrap();
        assert_eq!(
            header.block_hash,
            b256!("0x85548de50b6513bd3bfff100f9f2ad5316087c8ab73de9424d31b9b703b6a424")
        );
        assert_eq!(
            header.parent_beacon_block_root,
            b256!("0x1996b0d7d012895c2e44c72ea4171fe133ab9b175a67afddb11b2d46a6c62ab4")
        );
        assert_eq!(
            (header.block_number, header.slot, header.timestamp),
            (93354, 95877, 1787772924)
        );
        assert_eq!((header.gas_used, header.gas_limit), (102412080, 200000000));
    }

    /// A witness keeps the raw RLP of every header and ignores a trailing `keys` list, and a
    /// witness without the state does not decode.
    #[test]
    fn test_decode_witness() {
        fn list(items: &[Vec<u8>]) -> Vec<u8> {
            let mut out = Vec::new();
            let payload_length = items.iter().map(Vec::len).sum();
            Header {
                list: true,
                payload_length,
            }
            .encode(&mut out);
            out.extend(items.concat());
            out
        }
        let strings = |items: &[&[u8]]| list(&Vec::from_iter(items.iter().map(alloy_rlp::encode)));
        fn ssz<const N: usize>(items: &[&[u8]]) -> Vec<SszList<u8, N>> {
            Vec::from_iter(items.iter().map(|item| item.to_vec().try_into().unwrap()))
        }

        let header = alloy_rlp::encode(vec![vec![0xaau8; 32], vec![0x01u8]]);
        let headers = list(slice::from_ref(&header));
        let codes: &[&[u8]] = &[&[0x60, 0x00]];
        let state: &[&[u8]] = &[&[0xf8; 3], &[]];
        let witness = ExecutionWitness {
            headers: ssz(&[&header]).try_into().unwrap(),
            codes: ssz(codes).into(),
            state: ssz(state).into(),
        };
        let fields = [headers, strings(codes), strings(state), strings(&[&[0x02]])];
        assert_eq!(decode_witness(&list(&fields)).unwrap(), witness);
        assert_eq!(decode_witness(&list(&fields[..3])).unwrap(), witness);
        assert!(decode_witness(&list(&fields[..2])).is_err());
        assert!(decode_witness(&strings(&[&[1], &[2], &[3]])).is_err());
    }
}
