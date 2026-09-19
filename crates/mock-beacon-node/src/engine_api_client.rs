//! Client of the Engine API of zkboost. It forwards the requests of the CL unchanged.

use axum::{
    body::Bytes,
    http::{
        HeaderValue, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
};
use url::Url;

/// Client of the Engine API of zkboost.
#[derive(Debug)]
pub(crate) struct EngineApiClient {
    endpoint: Url,
    http_client: reqwest::Client,
}

impl EngineApiClient {
    /// Creates the client of the Engine API at the endpoint.
    pub(crate) fn new(endpoint: Url) -> Self {
        Self {
            endpoint,
            http_client: reqwest::Client::new(),
        }
    }

    /// Forwards a JSON-RPC request with the authorization of the CL unchanged and returns the
    /// status and the body of the answer.
    pub(crate) async fn forward(
        &self,
        authorization: Option<&HeaderValue>,
        body: Bytes,
    ) -> anyhow::Result<(StatusCode, Bytes)> {
        let mut request = self
            .http_client
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        let response = request.send().await?;
        Ok((response.status(), response.bytes().await?))
    }
}
