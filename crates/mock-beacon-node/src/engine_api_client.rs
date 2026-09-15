//! Authenticated Engine API calls to zkboost.

use std::time::Duration;

use alloy_rpc_types_engine::{
    Claims, ForkchoiceState, ForkchoiceUpdated, JwtSecret, PayloadStatus, PayloadStatusEnum,
};
use anyhow::{Context, bail, ensure};
use lighthouse_types::Hash256;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;
use zkboost_types::NewPayloadParams;

/// The shared JWT secret accepted by execution nodes in a Kurtosis testnet.
const JWT_SECRET: &str = "0xdc49981516e8e72b401a63e6405495a32dafc3939b5d6d83cc319ac0388bca1b";

pub(crate) struct EngineApiClient {
    endpoint: Url,
    jwt_secret: JwtSecret,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct RpcRequest<'a, P> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: P,
}

#[derive(Deserialize)]
#[serde(untagged, bound(deserialize = "T: Deserialize<'de>"))]
enum RpcResponse<T> {
    Error {
        error: RpcError,
    },
    Success {
        #[serde(deserialize_with = "T::deserialize")]
        result: T,
    },
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
    data: Option<serde_json::Value>,
}

/// Only the block hash is needed to check whether the execution node knows a block.
#[derive(Deserialize)]
struct Block {
    #[serde(rename = "hash")]
    _hash: Hash256,
}

impl EngineApiClient {
    pub(crate) fn new(endpoint: Url) -> anyhow::Result<Self> {
        Ok(Self {
            endpoint,
            jwt_secret: JwtSecret::from_hex(JWT_SECRET)?,
            http: reqwest::Client::new(),
        })
    }

    /// Reuses execution state after a restart or reorg, without requesting another witness.
    pub(crate) async fn update_known_head(
        &self,
        head: Hash256,
        safe: Hash256,
        finalized: Hash256,
    ) -> anyhow::Result<bool> {
        let block: Option<Block> = self.call("eth_getBlockByHash", (head, false)).await?;
        if block.is_none() {
            return Ok(false);
        }
        // A downloaded body alone is insufficient: forkchoice must confirm the state is valid.
        Ok(self.forkchoice_updated(head, safe, finalized).await? == PayloadStatusEnum::Valid)
    }

    /// Updates forkchoice without requesting payload construction or custody services.
    pub(crate) async fn forkchoice_updated(
        &self,
        head: Hash256,
        safe: Hash256,
        finalized: Hash256,
    ) -> anyhow::Result<PayloadStatusEnum> {
        let state = ForkchoiceState {
            head_block_hash: head.0.into(),
            safe_block_hash: safe.0.into(),
            finalized_block_hash: finalized.0.into(),
        };
        let response: ForkchoiceUpdated = self
            .call("engine_forkchoiceUpdatedV4", (state, (), ()))
            .await?;
        let status = response.payload_status.status;
        ensure!(
            matches!(
                status,
                PayloadStatusEnum::Valid | PayloadStatusEnum::Syncing
            ),
            "forkchoice rejected: {status:?}"
        );
        Ok(status)
    }

    /// Sends a new payload and returns its execution status.
    pub(crate) async fn new_payload(
        &self,
        params: &NewPayloadParams,
    ) -> anyhow::Result<PayloadStatusEnum> {
        let response: PayloadStatus = self.call(NewPayloadParams::METHOD, params).await?;
        Ok(response.status)
    }

    async fn call<P: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> anyhow::Result<T> {
        let token = self.jwt_secret.encode(&Claims::with_current_timestamp())?;
        let request = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method,
            params,
        };
        let response: RpcResponse<T> = self
            .http
            .post(self.endpoint.clone())
            .timeout(Duration::from_secs(10))
            .bearer_auth(token)
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .with_context(|| format!("invalid {method} response"))?;
        match response {
            RpcResponse::Success { result } => Ok(result),
            RpcResponse::Error { error } => bail!(
                "{method} failed ({}): {}; data: {:?}",
                error.code,
                error.message,
                error.data
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Block, RpcResponse};

    #[test]
    fn block_lookup_requires_a_result_but_accepts_null() {
        assert!(serde_json::from_str::<RpcResponse<Option<Block>>>(r#"{"result":null}"#).is_ok());
        assert!(serde_json::from_str::<RpcResponse<Option<Block>>>("{}").is_err());
    }
}
