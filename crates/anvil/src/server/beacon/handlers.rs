use super::{
    error::{BeaconError, BeaconErrorCode},
    utils::must_be_ssz,
};
use crate::eth::EthApi;
use alloy_eips::BlockId;
use alloy_primitives::{B256, aliases::B32};
use alloy_rpc_types_beacon::{
    genesis::{GenesisData, GenesisResponse},
    sidecar::GetBlobsResponse,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use ssz::Encode;
use std::{collections::HashMap, str::FromStr as _};

/// Handles incoming Beacon API requests for blob sidecars
///
/// This endpoint is deprecated. Use `GET /eth/v1/beacon/blobs/{block_id}` instead.
///
/// GET /eth/v1/beacon/blob_sidecars/{block_id}
pub async fn handle_get_blob_sidecars(
    State(_api): State<EthApi>,
    Path(_block_id): Path<String>,
    Query(_params): Query<HashMap<String, String>>,
) -> Response {
    BeaconError::deprecated_endpoint_with_hint("Use `GET /eth/v1/beacon/blobs/{block_id}` instead.")
        .into_response()
}

/// Handles incoming Beacon API requests for blobs
///
/// GET /eth/v1/beacon/blobs/{block_id}
pub async fn handle_get_blobs(
    headers: HeaderMap,
    State(api): State<EthApi>,
    Path(block_id): Path<String>,
    Query(versioned_hashes): Query<HashMap<String, String>>,
) -> Response {
    let versioned_hashes = versioned_hashes
        .get("versioned_hashes")
        .map(|s| {
            s.split(',').map(|hash| B256::from_str(hash.trim())).collect::<Result<Vec<_>, _>>()
        })
        .transpose();
    let versioned_hashes = match versioned_hashes {
        Ok(hashes) => hashes.unwrap_or_default(),
        Err(_) => {
            return BeaconError::new(BeaconErrorCode::BadRequest, "Invalid versioned_hashes")
                .into_response();
        }
    };

    let fork_beacon = api.backend.get_fork().and_then(|fork| {
        let config = fork.config.read();
        config.beacon.clone().map(|beacon| (beacon, config.timestamp, config.block_number))
    });
    let result = if let Some((beacon, fork_timestamp, fork_number)) = fork_beacon {
        let Ok(slot) = block_id.parse::<u64>() else {
            return BeaconError::invalid_block_id(block_id).into_response();
        };
        let Some(timestamp) = beacon.timestamp(slot) else {
            return BeaconError::invalid_block_id(block_id).into_response();
        };
        if timestamp <= fork_timestamp {
            beacon.blobs(slot, &versioned_hashes).await
        } else {
            api.backend.get_blobs_by_timestamp(timestamp, fork_number + 1, &versioned_hashes)
        }
    } else {
        let Ok(block_id) = BlockId::from_str(&block_id) else {
            return BeaconError::invalid_block_id(block_id).into_response();
        };
        api.anvil_get_blobs_by_block_id(block_id, versioned_hashes).map_err(Into::into)
    };

    match result {
        Ok(Some(blobs)) => {
            if must_be_ssz(&headers) {
                blobs.as_ssz_bytes().into_response()
            } else {
                Json(GetBlobsResponse {
                    execution_optimistic: false,
                    finalized: false,
                    data: blobs,
                })
                .into_response()
            }
        }
        Ok(None) => BeaconError::block_not_found().into_response(),
        Err(error) => {
            warn!(target: "beacon", %error, %block_id, "Beacon blob lookup failed");
            BeaconError::internal_error().into_response()
        }
    }
}

/// Handles incoming Beacon API requests for genesis details
///
/// Fork-Beacon mode returns the captured upstream identity; otherwise only genesis time is set.
///
/// GET /eth/v1/beacon/genesis
pub async fn handle_get_genesis(State(api): State<EthApi>) -> Response {
    if let Some(fork) = api.backend.get_fork()
        && let Some(beacon) = &fork.config.read().beacon
    {
        return Json(GenesisResponse { data: beacon.genesis.clone() }).into_response();
    }
    match api.anvil_get_genesis_time() {
        Ok(genesis_time) => Json(GenesisResponse {
            data: GenesisData {
                genesis_time,
                genesis_validators_root: B256::ZERO,
                genesis_fork_version: B32::ZERO,
            },
        })
        .into_response(),
        Err(_) => BeaconError::internal_error().into_response(),
    }
}

/// Handles requests for the Beacon chain configuration used by Base clients.
///
/// GET /eth/v1/config/spec
pub async fn handle_get_spec(State(api): State<EthApi>) -> Response {
    let captured_duration = api
        .backend
        .get_fork()
        .and_then(|fork| fork.config.read().beacon.as_ref().map(|beacon| beacon.seconds_per_slot));
    match captured_duration
        .or_else(|| api.anvil_get_interval_mining().ok().flatten().filter(|interval| *interval > 0))
    {
        Some(interval) => Json(serde_json::json!({
            "data": {
                "SECONDS_PER_SLOT": interval.to_string()
            }
        }))
        .into_response(),
        None => BeaconError::internal_error().into_response(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn header_map_with_accept(accept: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::ACCEPT, HeaderValue::from_str(accept).unwrap());
        headers
    }

    #[test]
    fn test_must_be_ssz() {
        let test_cases = vec![
            (None, false, "no Accept header"),
            (Some("application/json"), false, "JSON only"),
            (Some("application/octet-stream"), true, "octet-stream only"),
            (Some("application/octet-stream;q=1.0,application/json;q=0.9"), true, "SSZ preferred"),
            (
                Some("application/json;q=1.0,application/octet-stream;q=0.9"),
                false,
                "JSON preferred",
            ),
            (Some("application/octet-stream;q=0.5,application/json;q=0.5"), false, "equal quality"),
            (
                Some("text/html;q=0.9, application/octet-stream;q=1.0, application/json;q=0.8"),
                true,
                "multiple types",
            ),
            (
                Some("application/octet-stream ; q=1.0 , application/json ; q=0.9"),
                true,
                "whitespace handling",
            ),
            (Some("application/octet-stream, application/json;q=0.9"), true, "default quality"),
        ];

        for (accept_header, expected, description) in test_cases {
            let headers = match accept_header {
                None => HeaderMap::new(),
                Some(header) => header_map_with_accept(header),
            };
            assert_eq!(
                must_be_ssz(&headers),
                expected,
                "Test case '{}' failed: expected {}, got {}",
                description,
                expected,
                !expected
            );
        }
    }
}
