use std::time::Duration;

use anyhow::ensure;

/// Delay between two attempts of a request to a remote service.
pub const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Returns a success response of a remote service unchanged, and any other response as an error
/// with its body.
pub async fn error_for_status(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = response.status();
    ensure!(
        status.is_success(),
        "{status}: {}",
        response.text().await.unwrap_or_default()
    );
    Ok(response)
}
