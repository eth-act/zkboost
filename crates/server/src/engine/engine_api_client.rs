//! Client of the EL engine endpoint. Every JSON-RPC request body is forwarded as is with the
//! caller's `Authorization` header, and the raw response is returned.

use axum::http::{HeaderValue, StatusCode};
use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use url::Url;

/// Raw HTTP response of the EL engine endpoint.
#[derive(Debug)]
pub(crate) struct EngineResponse {
    pub(crate) status: StatusCode,
    pub(crate) body: Bytes,
}

/// Client of the EL engine endpoint.
#[derive(Debug)]
pub(crate) struct EngineApiClient {
    url: Url,
    http_client: reqwest::Client,
}

impl EngineApiClient {
    /// Creates the client of the EL engine endpoint at the url.
    pub(crate) fn new(url: Url) -> Self {
        Self {
            url,
            http_client: reqwest::Client::new(),
        }
    }

    /// Forwards a JSON-RPC request body with the caller's `Authorization` header.
    pub(crate) async fn forward(
        &self,
        authorization: Option<&HeaderValue>,
        body: Bytes,
    ) -> reqwest::Result<EngineResponse> {
        let mut request = self
            .http_client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization.as_bytes());
        }
        let response = request.send().await?;
        Ok(EngineResponse {
            status: response.status(),
            body: response.bytes().await?,
        })
    }
}
