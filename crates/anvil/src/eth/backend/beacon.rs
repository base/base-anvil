//! The immutable Beacon slot schedule of an execution fork.

use alloy_rpc_types_beacon::genesis::{GenesisData, GenesisResponse};
use eyre::{Result, ensure, eyre};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::de::DeserializeOwned;
use std::time::Duration;

const MAX_METADATA_RESPONSE_BYTES: usize = 1024 * 1024;

/// Beacon metadata belonging to an execution fork.
#[derive(Clone, Debug)]
pub struct ForkBeacon {
    pub genesis: GenesisData,
    pub seconds_per_slot: u64,
}

impl ForkBeacon {
    /// Captures Beacon metadata before the fork starts serving requests.
    pub async fn connect(endpoint: &str, timeout: Duration, fork_timestamp: u64) -> Result<Self> {
        let url = Url::parse(endpoint).map_err(|_| eyre!("invalid fork Beacon URL"))?;
        ensure!(matches!(url.scheme(), "http" | "https"), "fork Beacon URL must use HTTP(S)");
        let client = Client::builder().timeout(timeout).redirect(Policy::none()).build()?;
        let genesis = Self::get::<GenesisResponse>(
            &client,
            &url,
            "eth/v1/beacon/genesis",
            MAX_METADATA_RESPONSE_BYTES,
        )
        .await?
        .ok_or_else(|| eyre!("fork Beacon genesis is unavailable"))?
        .data;
        let spec = Self::get::<serde_json::Value>(
            &client,
            &url,
            "eth/v1/config/spec",
            MAX_METADATA_RESPONSE_BYTES,
        )
        .await?
        .ok_or_else(|| eyre!("fork Beacon spec is unavailable"))?;
        let seconds_per_slot: u64 = spec["data"]["SECONDS_PER_SLOT"]
            .as_str()
            .ok_or_else(|| eyre!("missing Beacon SECONDS_PER_SLOT"))?
            .parse()?;
        ensure!(seconds_per_slot > 0, "Beacon slot duration must be positive");
        let elapsed = fork_timestamp
            .checked_sub(genesis.genesis_time)
            .ok_or_else(|| eyre!("execution fork precedes Beacon genesis"))?;
        ensure!(
            elapsed.is_multiple_of(seconds_per_slot),
            "execution fork timestamp is not on the Beacon slot grid"
        );
        Ok(Self { genesis, seconds_per_slot })
    }

    /// Converts a slot to its exact execution timestamp without wrapping on user input.
    pub fn timestamp(&self, slot: u64) -> Option<u64> {
        slot.checked_mul(self.seconds_per_slot)?.checked_add(self.genesis.genesis_time)
    }

    async fn get<T: DeserializeOwned>(
        client: &Client,
        endpoint: &Url,
        path: &str,
        max_bytes: usize,
    ) -> Result<Option<T>> {
        // Append to the endpoint's path and keep its query; either may carry credentials.
        let mut url = endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| eyre!("invalid fork Beacon URL"))?
            .pop_if_empty()
            .extend(path.split('/'));
        let request = client.get(url).header("Accept", "application/json");
        let response = request.send().await.map_err(reqwest::Error::without_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let mut response = response.error_for_status().map_err(reqwest::Error::without_url)?;
        ensure!(response.status().is_success(), "unexpected Beacon response status");
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(reqwest::Error::without_url)? {
            ensure!(chunk.len() <= max_bytes - bytes.len(), "Beacon response exceeds size limit");
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, FixedBytes};
    use axum::{
        Json, Router,
        extract::RawQuery,
        response::{IntoResponse, Redirect},
        routing::get,
    };
    use tokio::net::TcpListener;

    const TIMEOUT: Duration = Duration::from_secs(1);
    const GENESIS: u64 = 1000;
    const FORK: u64 = 1240;

    fn genesis() -> GenesisData {
        GenesisData {
            genesis_time: GENESIS,
            genesis_validators_root: B256::repeat_byte(0x4b),
            genesis_fork_version: FixedBytes::from([1, 2, 3, 4]),
        }
    }

    /// Serves genesis and a spec with `seconds_per_slot` under `prefix`.
    fn metadata(prefix: &str, seconds_per_slot: &'static str) -> Router {
        Router::new()
            .route(
                &format!("{prefix}/eth/v1/beacon/genesis"),
                get(|| async { Json(GenesisResponse { data: genesis() }) }),
            )
            .route(
                &format!("{prefix}/eth/v1/config/spec"),
                get(move || async move {
                    Json(serde_json::json!({ "data": { "SECONDS_PER_SLOT": seconds_per_slot } }))
                }),
            )
    }

    async fn serve(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        url
    }

    async fn connect_err(endpoint: &str, fork_timestamp: u64) -> String {
        let err = ForkBeacon::connect(endpoint, TIMEOUT, fork_timestamp).await.unwrap_err();
        format!("{err:?}")
    }

    #[tokio::test]
    async fn connect_captures_metadata_and_converts_slots() {
        let url = serve(metadata("", "12")).await;
        let beacon = ForkBeacon::connect(&format!("{url}/"), TIMEOUT, FORK).await.unwrap();
        assert_eq!(beacon.genesis, genesis());
        assert_eq!(beacon.seconds_per_slot, 12);
        assert_eq!(beacon.timestamp(0), Some(GENESIS));
        assert_eq!(beacon.timestamp(20), Some(FORK));
        assert_eq!(beacon.timestamp(u64::MAX), None);
        assert_eq!(beacon.timestamp(u64::MAX / 12), None);
    }

    #[tokio::test]
    async fn connect_rejects_invalid_schedules() {
        let url = serve(metadata("", "12")).await;
        assert!(connect_err(&url, GENESIS - 12).await.contains("precedes Beacon genesis"));
        assert!(connect_err(&url, FORK + 1).await.contains("not on the Beacon slot grid"));
        for (spec, error) in [("0", "must be positive"), ("12s", "invalid digit")] {
            let url = serve(metadata("", spec)).await;
            assert!(connect_err(&url, FORK).await.contains(error), "spec {spec}");
        }
        let url = serve(Router::new()).await;
        assert!(connect_err(&url, FORK).await.contains("genesis is unavailable"));
        assert!(connect_err("ftp://127.0.0.1", FORK).await.contains("must use HTTP(S)"));
    }

    #[tokio::test]
    async fn connect_keeps_credentials_out_of_errors() {
        let router = metadata("/secret-key", "12").layer(axum::middleware::from_fn(
            |RawQuery(query): RawQuery,
             request: axum::extract::Request,
             next: axum::middleware::Next| async move {
                if query.as_deref() == Some("token=secret-token") {
                    next.run(request).await
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            },
        ));
        let url = serve(router).await;
        ForkBeacon::connect(&format!("{url}/secret-key?token=secret-token"), TIMEOUT, FORK)
            .await
            .unwrap();

        let failing = [
            format!("{url}/secret-key?token=secret-wrong"),
            format!("{url}/secret-missing?token=secret-token"),
            "http://secret:secret@127.0.0.1:1/secret?token=secret".to_string(),
            "http://[secret?token=secret".to_string(),
        ];
        for endpoint in failing {
            let err = connect_err(&endpoint, FORK).await;
            assert!(!err.contains("secret"), "{err}");
        }
    }

    #[tokio::test]
    async fn connect_bounds_upstream_responses() {
        let redirect = serve(
            Router::new()
                .route("/eth/v1/beacon/genesis", get(|| async { Redirect::temporary("/genesis") })),
        )
        .await;
        assert!(connect_err(&redirect, FORK).await.contains("unexpected Beacon response status"));

        let failing =
            serve(Router::new().route(
                "/eth/v1/beacon/genesis",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            ))
            .await;
        assert!(connect_err(&failing, FORK).await.contains("500"));

        let oversized = serve(Router::new().route(
            "/eth/v1/beacon/genesis",
            get(|| async { vec![b' '; MAX_METADATA_RESPONSE_BYTES + 1] }),
        ))
        .await;
        assert!(connect_err(&oversized, FORK).await.contains("exceeds size limit"));

        let slow = serve(Router::new().route(
            "/eth/v1/beacon/genesis",
            get(|| async {
                tokio::time::sleep(TIMEOUT * 3).await;
                Json(GenesisResponse { data: genesis() })
            }),
        ))
        .await;
        assert!(connect_err(&slow, FORK).await.contains("timed out"));
    }
}
