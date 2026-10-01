//! Historical Beacon reads and the immutable slot schedule of an execution fork.

use alloy_consensus::{Blob, EnvKzgSettings};
use alloy_eips::eip4844::kzg_to_versioned_hash;
use alloy_primitives::B256;
use alloy_rpc_types_beacon::{
    genesis::{GenesisData, GenesisResponse},
    sidecar::GetBlobsResponse,
};
use eyre::{Result, ensure, eyre};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::de::DeserializeOwned;
use std::{fmt, time::Duration};

// JSON encodes each 128 KiB blob as hex. This permits over 100 blobs without accepting
// unbounded responses from an upstream provider.
const MAX_BLOB_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_METADATA_RESPONSE_BYTES: usize = 1024 * 1024;

/// Beacon connectivity and metadata belonging to an execution fork.
#[derive(Clone)]
pub struct ForkBeacon {
    url: Url,
    client: Client,
    pub genesis: GenesisData,
    pub seconds_per_slot: u64,
}

impl fmt::Debug for ForkBeacon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Endpoint paths and query parameters may contain credentials.
        f.debug_struct("ForkBeacon")
            .field("genesis", &self.genesis)
            .field("seconds_per_slot", &self.seconds_per_slot)
            .finish_non_exhaustive()
    }
}

impl ForkBeacon {
    /// Captures Beacon metadata before the fork starts serving requests.
    pub async fn connect(endpoint: &str, timeout: Duration, fork_timestamp: u64) -> Result<Self> {
        let url = Url::parse(&format!("{}/", endpoint.trim_end_matches('/')))
            .map_err(|_| eyre!("invalid fork Beacon URL"))?;
        ensure!(matches!(url.scheme(), "http" | "https"), "fork Beacon URL must use HTTP(S)");
        let client = Client::builder().timeout(timeout).redirect(Policy::none()).build()?;
        let genesis = Self::get::<GenesisResponse>(
            &client,
            url.join("eth/v1/beacon/genesis")?,
            &[],
            MAX_METADATA_RESPONSE_BYTES,
        )
        .await?
        .ok_or_else(|| eyre!("fork Beacon genesis is unavailable"))?
        .data;
        let spec = Self::get::<serde_json::Value>(
            &client,
            url.join("eth/v1/config/spec")?,
            &[],
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
        Ok(Self { url, client, genesis, seconds_per_slot })
    }

    /// Converts a slot to its exact execution timestamp without wrapping on user input.
    pub fn timestamp(&self, slot: u64) -> Option<u64> {
        slot.checked_mul(self.seconds_per_slot)?.checked_add(self.genesis.genesis_time)
    }

    /// Reads historical blobs. The caller must enforce the inclusive execution fork boundary.
    pub async fn blobs(&self, slot: u64, hashes: &[B256]) -> Result<Option<Vec<Blob>>> {
        let Some(response) = Self::get::<GetBlobsResponse>(
            &self.client,
            self.url.join(&format!("eth/v1/beacon/blobs/{slot}"))?,
            hashes,
            MAX_BLOB_RESPONSE_BYTES,
        )
        .await?
        else {
            return Ok(None);
        };
        let mut blobs = response.data;
        if !hashes.is_empty() {
            // Providers may ignore the filter. Apply it to the actual blob content, not
            // to untrusted upstream labels; Base also validates the requested hashes.
            let hashes = hashes.to_vec();
            blobs = tokio::task::spawn_blocking(move || -> Result<Vec<Blob>> {
                let settings = EnvKzgSettings::Default;
                let mut filtered = Vec::new();
                for blob in blobs {
                    let commitment = settings.get().blob_to_kzg_commitment(&blob.0.into())?;
                    if hashes.contains(&kzg_to_versioned_hash(commitment.as_slice())) {
                        filtered.push(blob);
                    }
                }
                Ok(filtered)
            })
            .await??;
        }
        Ok(Some(blobs))
    }

    async fn get<T: DeserializeOwned>(
        client: &Client,
        url: Url,
        hashes: &[B256],
        max_bytes: usize,
    ) -> Result<Option<T>> {
        let mut request = client.get(url).header("Accept", "application/json");
        if !hashes.is_empty() {
            let hashes = hashes.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
            request = request.query(&[("versioned_hashes", hashes)]);
        }
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
