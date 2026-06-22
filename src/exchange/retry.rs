use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use reqwest::Response;
use tracing::warn;

const MAX_ATTEMPTS: u32 = 5;

/// GET with exponential backoff on 429/5xx.
pub async fn retry_get(client: &Client, url: &str) -> Result<Response> {
    retry_request(client, |c| c.get(url)).await
}

/// Authenticated GET with exponential backoff on 429/5xx.
pub async fn retry_get_with(
    client: &Client,
    url: &str,
    build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> Result<Response> {
    retry_request(client, |c| build(c.get(url))).await
}

/// Authenticated DELETE with exponential backoff on 429/5xx.
pub async fn retry_delete_with(
    client: &Client,
    url: &str,
    build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> Result<Response> {
    retry_request(client, |c| build(c.delete(url))).await
}

/// Authenticated POST with exponential backoff on 429/5xx.
pub async fn retry_post_with(
    client: &Client,
    url: &str,
    build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> Result<Response> {
    retry_request(client, |c| build(c.post(url))).await
}

async fn retry_request(
    client: &Client,
    build: impl Fn(&Client) -> reqwest::RequestBuilder,
) -> Result<Response> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let resp = build(client)
            .send()
            .await
            .with_context(|| format!("HTTP request failed (attempt {attempt})"))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }

        if status.as_u16() == 429 || status.is_server_error() {
            if attempt >= MAX_ATTEMPTS {
                bail!("HTTP {status} after {MAX_ATTEMPTS} attempts");
            }

            let wait_secs = if status.as_u16() == 429 {
                resp.headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or_else(|| 2u64.pow(attempt).min(32))
            } else {
                2u64.pow(attempt).min(32)
            };

            warn!(
                attempt,
                url = %resp.url(),
                status = %status,
                wait_secs,
                "retrying HTTP request after backoff"
            );

            tokio::time::sleep(Duration::from_secs(wait_secs)).await;
            continue;
        }

        bail!("HTTP {status} (no retry for client error)");
    }
}
