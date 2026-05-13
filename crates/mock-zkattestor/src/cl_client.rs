use anyhow::{anyhow, bail};
use futures::{Stream, StreamExt};
use lighthouse_types::{
    BeaconBlockRef, ForkName, ForkVersionDecode, Hash256, MainnetEthSpec, SignedBeaconBlock,
    VersionedHash,
};
use reqwest_eventsource::{Event as SseEvent, EventSource};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use ssz_types::VariableList;
use url::Url;
use zkboost_types::{
    NewPayloadRequest, NewPayloadRequestBellatrix, NewPayloadRequestCapella,
    NewPayloadRequestDeneb, NewPayloadRequestElectra, NewPayloadRequestFulu,
};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Block {
    #[serde(with = "serde_utils::quoted_u64")]
    pub(crate) slot: u64,
    pub(crate) block: Hash256,
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
}

pub(crate) fn new_payload_request_from_beacon_block(
    block: &SignedBeaconBlock<MainnetEthSpec>,
) -> anyhow::Result<NewPayloadRequest<MainnetEthSpec>> {
    match block.message() {
        BeaconBlockRef::Base(_) | BeaconBlockRef::Altair(_) => unreachable!(),
        BeaconBlockRef::Bellatrix(b) => {
            Ok(NewPayloadRequest::Bellatrix(NewPayloadRequestBellatrix {
                execution_payload: b.body.execution_payload.execution_payload.clone(),
            }))
        }
        BeaconBlockRef::Capella(b) => Ok(NewPayloadRequest::Capella(NewPayloadRequestCapella {
            execution_payload: b.body.execution_payload.execution_payload.clone(),
        })),
        BeaconBlockRef::Deneb(b) => Ok(NewPayloadRequest::Deneb(NewPayloadRequestDeneb {
            execution_payload: b.body.execution_payload.execution_payload.clone(),
            versioned_hashes: VariableList::new(
                b.body
                    .blob_kzg_commitments
                    .iter()
                    .map(kzg_commitment_to_versioned_hash)
                    .collect(),
            )?,
            parent_beacon_block_root: b.parent_root,
        })),
        BeaconBlockRef::Electra(b) => Ok(NewPayloadRequest::Electra(NewPayloadRequestElectra {
            execution_payload: b.body.execution_payload.execution_payload.clone(),
            versioned_hashes: VariableList::new(
                b.body
                    .blob_kzg_commitments
                    .iter()
                    .map(kzg_commitment_to_versioned_hash)
                    .collect(),
            )?,
            parent_beacon_block_root: b.parent_root,
            execution_requests: b.body.execution_requests.clone(),
        })),
        BeaconBlockRef::Fulu(b) => Ok(NewPayloadRequest::Fulu(NewPayloadRequestFulu {
            execution_payload: b.body.execution_payload.execution_payload.clone(),
            versioned_hashes: VariableList::new(
                b.body
                    .blob_kzg_commitments
                    .iter()
                    .map(kzg_commitment_to_versioned_hash)
                    .collect(),
            )?,
            parent_beacon_block_root: b.parent_root,
            execution_requests: b.body.execution_requests.clone(),
        })),
        BeaconBlockRef::Gloas(_) => unimplemented!(),
    }
}

pub(crate) fn kzg_commitment_to_versioned_hash(
    commitment: &lighthouse_types::KzgCommitment,
) -> VersionedHash {
    let mut hash: [u8; 32] = Sha256::digest(commitment.0).into();
    hash[0] = 0x01;
    VersionedHash::from(hash)
}
