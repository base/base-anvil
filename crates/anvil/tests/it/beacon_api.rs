use crate::utils::http_provider;
use alloy_consensus::{Blob, BlobTransactionSidecar, SidecarBuilder, SimpleCoder, Transaction};
use alloy_network::{TransactionBuilder, TransactionBuilder4844};
use alloy_primitives::{B256, FixedBytes, U256, b256};
use alloy_provider::Provider;
use alloy_rpc_types::{
    BlockId, BlockNumberOrTag, TransactionRequest,
    anvil::{Forking, MineOptions},
};
use alloy_rpc_types_beacon::{
    genesis::{GenesisData, GenesisResponse},
    sidecar::GetBlobsResponse,
};
use alloy_serde::WithOtherFields;
use anvil::{NodeConfig, NodeHandle, eth::EthApi, spawn};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use foundry_evm::hardfork::EthereumHardfork;
use ssz::{Decode, Encode};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread")]
async fn test_beacon_api_get_blob_sidecars() {
    let node_config = NodeConfig::test().with_hardfork(Some(EthereumHardfork::Cancun.into()));
    let (_api, handle) = spawn(node_config).await;

    // Test Beacon API endpoint using HTTP client
    let client = reqwest::Client::new();
    let url = format!("{}/eth/v1/beacon/blob_sidecars/latest", handle.http_endpoint());

    // This endpoint is deprecated, so we expect a 410 Gone response
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"code":410,"message":"This endpoint is deprecated. Use `GET /eth/v1/beacon/blobs/{block_id}` instead."}"#,
        "Expected deprecation message for blob_sidecars endpoint"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_beacon_api_get_blobs() {
    let node_config = NodeConfig::test().with_hardfork(Some(EthereumHardfork::Cancun.into()));
    let (api, handle) = spawn(node_config).await;

    // Disable auto-mining so we can include multiple transactions in the same block
    api.anvil_set_auto_mine(false).await.unwrap();

    let wallets = handle.dev_wallets().collect::<Vec<_>>();
    let from = wallets[0].address();
    let to = wallets[1].address();

    let provider = http_provider(&handle.http_endpoint());

    let eip1559_est = provider.estimate_eip1559_fees().await.unwrap();
    let gas_price = provider.get_gas_price().await.unwrap();

    // Create multiple blob transactions to be included in the same block
    let blob_data =
        [b"Hello Beacon API - Blob 1", b"Hello Beacon API - Blob 2", b"Hello Beacon API - Blob 3"];

    let mut pending_txs = Vec::new();

    // Send all transactions without waiting for receipts
    for (i, data) in blob_data.iter().enumerate() {
        let sidecar: SidecarBuilder<SimpleCoder> = SidecarBuilder::from_slice(data.as_slice());
        let sidecar: BlobTransactionSidecar = sidecar.build().unwrap();

        let tx = TransactionRequest::default()
            .with_from(from)
            .with_to(to)
            .with_nonce(i as u64)
            .with_max_fee_per_blob_gas(gas_price + 1)
            .with_max_fee_per_gas(eip1559_est.max_fee_per_gas)
            .with_max_priority_fee_per_gas(eip1559_est.max_priority_fee_per_gas)
            .with_blob_sidecar_4844(sidecar)
            .value(U256::from(100));

        let mut tx = WithOtherFields::new(tx);
        tx.populate_blob_hashes();

        let pending = provider.send_transaction(tx).await.unwrap();
        pending_txs.push(pending);
    }

    // Mine a block to include all transactions
    api.evm_mine(None).await.unwrap();

    // Get receipts for all transactions
    let mut receipts = Vec::new();
    for pending in pending_txs {
        let receipt = pending.get_receipt().await.unwrap();
        receipts.push(receipt);
    }

    // Verify all transactions were included in the same block
    let block_number = receipts[0].block_number.unwrap();
    for (i, receipt) in receipts.iter().enumerate() {
        assert_eq!(
            receipt.block_number.unwrap(),
            block_number,
            "Transaction {i} was not included in block {block_number}"
        );
    }

    // Extract the actual versioned hashes from the mined transactions
    let mut actual_versioned_hashes = Vec::new();
    for receipt in &receipts {
        let tx = provider.get_transaction_by_hash(receipt.transaction_hash).await.unwrap().unwrap();
        if let Some(blob_versioned_hashes) = tx.blob_versioned_hashes() {
            actual_versioned_hashes.extend(blob_versioned_hashes.iter().copied());
        }
    }

    // Test Beacon API endpoint using HTTP client
    let client = reqwest::Client::new();
    let url = format!("{}/eth/v1/beacon/blobs/{}", handle.http_endpoint(), block_number);

    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").and_then(|h| h.to_str().ok()),
        Some("application/json"),
        "Expected application/json content-type header"
    );

    let blobs_response: GetBlobsResponse = response.json().await.unwrap();
    // Verify response structure
    assert!(!blobs_response.execution_optimistic);
    assert!(!blobs_response.finalized);

    // Verify we have blob data from all transactions
    assert_eq!(blobs_response.data.len(), 3, "Expected 3 blobs from 3 transactions");

    // Test response with SSZ encoding
    let url = format!("{}/eth/v1/beacon/blobs/{}", handle.http_endpoint(), block_number);
    let response = client
        .get(&url)
        .header(axum::http::header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").and_then(|h| h.to_str().ok()),
        Some("application/octet-stream"),
        "Expected application/octet-stream content-type header"
    );

    let body_bytes = response.bytes().await.unwrap();

    // Decode the SSZ-encoded blobs in a spawned thread with larger stack to handle recursion
    let decoded_blobs = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // 8MB stack for SSZ decoding of large blobs
        .spawn(move || Vec::<Blob>::from_ssz_bytes(&body_bytes))
        .expect("Failed to spawn decode thread")
        .join()
        .expect("Decode thread panicked")
        .expect("Failed to decode SSZ-encoded blobs");

    // Verify we got exactly 3 blobs
    assert_eq!(
        decoded_blobs.len(),
        3,
        "Expected 3 blobs from SSZ-encoded response, got {}",
        decoded_blobs.len()
    );

    // Verify the decoded blobs match the JSON response blobs
    for (i, (decoded, json)) in decoded_blobs.iter().zip(blobs_response.data.iter()).enumerate() {
        assert_eq!(decoded, json, "Blob {i} mismatch between SSZ and JSON responses");
    }

    // Test filtering with versioned_hashes query parameter - single hash
    let url = format!(
        "{}/eth/v1/beacon/blobs/{}?versioned_hashes={}",
        handle.http_endpoint(),
        block_number,
        actual_versioned_hashes[1]
    );
    let response = client.get(&url).send().await.unwrap();
    let status = response.status();
    if status != reqwest::StatusCode::OK {
        let error_body = response.text().await.unwrap();
        panic!("Expected OK status, got {status}: {error_body}");
    }
    let blobs_response: GetBlobsResponse = response.json().await.unwrap();
    assert_eq!(
        blobs_response.data.len(),
        1,
        "Expected 1 blob when filtering by single versioned_hash"
    );

    // Test filtering with versioned_hashes query parameter - multiple versioned_hashes
    // (comma-separated)
    let url = format!(
        "{}/eth/v1/beacon/blobs/{}?versioned_hashes={},{}",
        handle.http_endpoint(),
        block_number,
        actual_versioned_hashes[0],
        actual_versioned_hashes[2]
    );
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let blobs_response: GetBlobsResponse = response.json().await.unwrap();
    assert_eq!(
        blobs_response.data.len(),
        2,
        "Expected 2 blobs when filtering by two versioned_hashes"
    );

    // Test filtering with non-existent versioned_hash
    let non_existent_hash =
        b256!("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    let url = format!(
        "{}/eth/v1/beacon/blobs/{}?versioned_hashes={}",
        handle.http_endpoint(),
        block_number,
        non_existent_hash
    );
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let blobs_response: GetBlobsResponse = response.json().await.unwrap();
    assert_eq!(
        blobs_response.data.len(),
        0,
        "Expected 0 blobs when filtering by non-existent versioned_hash"
    );

    // Test with special block identifiers
    let test_ids = vec!["latest", "finalized", "safe", "earliest"];
    for block_id in test_ids {
        let url = format!("{}/eth/v1/beacon/blobs/{}", handle.http_endpoint(), block_id);
        assert_eq!(client.get(&url).send().await.unwrap().status(), reqwest::StatusCode::OK);
    }
    let url = format!("{}/eth/v1/beacon/blobs/pending", handle.http_endpoint());
    assert_eq!(client.get(&url).send().await.unwrap().status(), reqwest::StatusCode::NOT_FOUND);

    // Test with hex block number
    let url = format!("{}/eth/v1/beacon/blobs/0x{block_number:x}", handle.http_endpoint());
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Test with non-existent block
    let url = format!("{}/eth/v1/beacon/blobs/999999", handle.http_endpoint());
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_beacon_api_get_genesis() {
    let node_config = NodeConfig::test().with_hardfork(Some(EthereumHardfork::Cancun.into()));
    let (_api, handle) = spawn(node_config).await;

    // Test Beacon API genesis endpoint using HTTP client
    let client = reqwest::Client::new();
    let url = format!("{}/eth/v1/beacon/genesis", handle.http_endpoint());

    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let genesis_response: GenesisResponse = response.json().await.unwrap();

    assert!(genesis_response.data.genesis_time > 0);
    assert_eq!(genesis_response.data.genesis_validators_root, B256::ZERO);
    assert_eq!(
        genesis_response.data.genesis_fork_version,
        FixedBytes::from([0x00, 0x00, 0x00, 0x00])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_beacon_api_get_spec() {
    let (_api, handle) =
        spawn(NodeConfig::test().with_blocktime(Some(Duration::from_secs(4)))).await;

    let response: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/eth/v1/config/spec", handle.http_endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(response, serde_json::json!({ "data": { "SECONDS_PER_SLOT": "4" } }));
}

/// Execution block the Beacon-backed target pins as its upstream boundary.
pub(super) const BEACON_ORIGIN_BLOCK: u64 = 100;
/// Timestamp of [`BEACON_ORIGIN_BLOCK`], in seconds; slot 20 on the mock Beacon chain.
pub(super) const BEACON_ORIGIN_TIMESTAMP_SECS: u64 = 1_240;
/// Mock Beacon chain genesis time, in seconds.
pub(super) const BEACON_GENESIS_TIME_SECS: u64 = 1_000;
/// Mock Beacon chain slot duration, in seconds.
pub(super) const BEACON_SECONDS_PER_SLOT: u64 = 12;
/// Nonzero mock genesis validators root, so a locally synthesized zero root is detectable.
pub(super) const BEACON_GENESIS_VALIDATORS_ROOT: B256 =
    b256!("0x4b363db94e286120d76eb905340fdd4e54bfe9f06bf33ff6cf5ad27f511bfe95");
/// Nonzero mock genesis fork version.
pub(super) const BEACON_GENESIS_FORK_VERSION: [u8; 4] = [0x01, 0x02, 0x03, 0x04];
/// Payload of the single upstream-owned blob at slot 20.
pub(super) const BEACON_HISTORICAL_BLOB_DATA: &[u8] = b"mock beacon historical slot 20 blob";
/// Payload of the upstream's conflicting blob at slot 21, which the target must never serve.
pub(super) const BEACON_CONFLICTING_BLOB_DATA: &[u8] = b"mock beacon conflicting slot 21 blob";
/// Payloads of the two local blob transactions mined at slot 21.
pub(super) const BEACON_LOCAL_BLOB_DATA: [&[u8]; 2] =
    [b"local slot 21 blob zero", b"local slot 21 blob one"];

/// Returns the timestamp, in seconds, of `slot` on the mock Beacon chain.
pub(super) const fn beacon_slot_timestamp(slot: u64) -> u64 {
    BEACON_GENESIS_TIME_SECS + slot * BEACON_SECONDS_PER_SLOT
}

/// Builds a single-blob sidecar encoding `data`.
pub(super) fn beacon_sidecar(data: &[u8]) -> BlobTransactionSidecar {
    let sidecar: SidecarBuilder<SimpleCoder> = SidecarBuilder::from_slice(data);
    sidecar.build().unwrap()
}

/// Origin execution node: Cancun genesis at block 100, timestamp 1,240, mining paused.
pub(super) fn beacon_origin_config() -> NodeConfig {
    NodeConfig::test()
        .with_genesis_block_number(Some(BEACON_ORIGIN_BLOCK))
        .with_genesis_timestamp(Some(BEACON_ORIGIN_TIMESTAMP_SECS))
        .with_hardfork(Some(EthereumHardfork::Cancun.into()))
        .with_no_mining(true)
}

/// Target node pinned to `origin_url` at block 100 with `beacon_url` as its Beacon upstream.
pub(super) fn beacon_target_config(origin_url: String, beacon_url: String) -> NodeConfig {
    NodeConfig::test()
        .with_eth_rpc_url(Some(origin_url))
        .with_fork_block_number(Some(BEACON_ORIGIN_BLOCK))
        .no_storage_caching()
        .with_fork_beacon_url(Some(beacon_url))
        .with_hardfork(Some(EthereumHardfork::Cancun.into()))
        .with_no_mining(true)
}

/// Shared state of [`MockBeacon`].
struct MockBeaconState {
    historical_blob: Blob,
    conflicting_blob: Blob,
    blob_requests: Mutex<Vec<String>>,
    seconds_per_slot: AtomicU64,
}

/// Mock Beacon node on an ephemeral local port.
///
/// Serves genesis 1,000 with nonzero root/version and 12-second slots. Blob routes: slot 18 fails
/// with 500, slot 20 returns [`BEACON_HISTORICAL_BLOB_DATA`], slot 21 returns the conflicting
/// [`BEACON_CONFLICTING_BLOB_DATA`], every other block ID is 404. Every blob request's block ID is
/// recorded. The server lives until the test runtime shuts down.
pub(super) struct MockBeacon {
    pub(super) url: String,
    state: Arc<MockBeaconState>,
}

impl MockBeacon {
    pub(super) async fn spawn() -> Self {
        let state = Arc::new(MockBeaconState {
            historical_blob: beacon_sidecar(BEACON_HISTORICAL_BLOB_DATA).blobs[0],
            conflicting_blob: beacon_sidecar(BEACON_CONFLICTING_BLOB_DATA).blobs[0],
            blob_requests: Mutex::default(),
            seconds_per_slot: AtomicU64::new(BEACON_SECONDS_PER_SLOT),
        });
        let router = Router::new()
            .route("/eth/v1/beacon/genesis", get(Self::genesis))
            .route("/eth/v1/config/spec", get(Self::spec))
            .route("/eth/v1/beacon/blobs/{block_id}", get(Self::blobs))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, state }
    }

    /// Exact upstream bytes of the slot 20 blob.
    pub(super) fn historical_blob(&self) -> Blob {
        self.state.historical_blob
    }

    /// Block IDs of all blob requests received so far, in arrival order.
    pub(super) fn blob_requests(&self) -> Vec<String> {
        self.state.blob_requests.lock().unwrap().clone()
    }

    async fn genesis() -> Json<GenesisResponse> {
        Json(GenesisResponse {
            data: GenesisData {
                genesis_time: BEACON_GENESIS_TIME_SECS,
                genesis_validators_root: BEACON_GENESIS_VALIDATORS_ROOT,
                genesis_fork_version: FixedBytes::from(BEACON_GENESIS_FORK_VERSION),
            },
        })
    }

    async fn spec(State(state): State<Arc<MockBeaconState>>) -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "data": { "SECONDS_PER_SLOT": state.seconds_per_slot.load(Ordering::Relaxed).to_string() }
        }))
    }

    async fn blobs(
        State(state): State<Arc<MockBeaconState>>,
        Path(block_id): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        state.blob_requests.lock().unwrap().push(block_id.clone());
        let blobs = match block_id.as_str() {
            "16" => return vec![b' '; 33 * 1024 * 1024].into_response(),
            "17" => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                return StatusCode::NOT_FOUND.into_response();
            }
            "18" => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            "20" => vec![state.historical_blob],
            "21" => vec![state.conflicting_blob],
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        let wants_ssz = headers
            .get(axum::http::header::ACCEPT)
            .and_then(|accept| accept.to_str().ok())
            .is_some_and(|accept| accept.contains("application/octet-stream"));
        if wants_ssz {
            blobs.as_ssz_bytes().into_response()
        } else {
            Json(GetBlobsResponse { execution_optimistic: false, finalized: true, data: blobs })
                .into_response()
        }
    }
}

/// Origin execution node, mock Beacon upstream, and a target pinned to both at block 100.
pub(super) struct BeaconTargetFixture {
    pub(super) origin_api: EthApi,
    pub(super) origin: NodeHandle,
    pub(super) beacon: MockBeacon,
    pub(super) api: EthApi,
    pub(super) handle: NodeHandle,
}

impl BeaconTargetFixture {
    pub(super) async fn spawn() -> Self {
        let (origin_api, origin) = spawn(beacon_origin_config()).await;
        let beacon = MockBeacon::spawn().await;
        let (api, handle) =
            spawn(beacon_target_config(origin.http_endpoint(), beacon.url.clone())).await;
        Self { origin_api, origin, beacon, api, handle }
    }

    /// Submits one single-blob transaction per payload from the first dev account, then mines
    /// them into one target block at the explicit `timestamp`. Returns hashes and sidecars in
    /// submission (nonce) order.
    pub(super) async fn mine_blob_block(
        &self,
        timestamp: u64,
        payloads: &[&[u8]],
    ) -> Vec<(B256, BlobTransactionSidecar)> {
        let provider = http_provider(&self.handle.http_endpoint());
        let from = self.handle.dev_accounts().next().unwrap();
        let mut sent = Vec::with_capacity(payloads.len());
        for (nonce, payload) in payloads.iter().enumerate() {
            let sidecar = beacon_sidecar(payload);
            let tx = TransactionRequest::default()
                .with_from(from)
                .with_to(from)
                .with_nonce(nonce as u64)
                .with_gas_limit(21_000)
                .with_max_fee_per_gas(10_000_000_000)
                .with_max_priority_fee_per_gas(1_000_000_000)
                .with_max_fee_per_blob_gas(10_000_000_000)
                .with_blob_sidecar_4844(sidecar.clone());
            let mut tx = WithOtherFields::new(tx);
            tx.populate_blob_hashes();
            let pending = provider.send_transaction(tx).await.unwrap();
            sent.push((*pending.tx_hash(), sidecar));
        }
        self.api.evm_mine(Some(MineOptions::Timestamp(Some(timestamp)))).await.unwrap();
        sent
    }
}

/// Fetches `/eth/v1/beacon/blobs/{block_id}` (which may include a query) and returns its status.
async fn beacon_blobs_status(endpoint: &str, block_id: &str) -> reqwest::StatusCode {
    reqwest::Client::new()
        .get(format!("{endpoint}/eth/v1/beacon/blobs/{block_id}"))
        .send()
        .await
        .unwrap()
        .status()
}

/// Fetches `/eth/v1/beacon/blobs/{block_id}` as JSON, asserting success.
async fn beacon_blobs_json(endpoint: &str, block_id: &str) -> Vec<Blob> {
    let response = reqwest::Client::new()
        .get(format!("{endpoint}/eth/v1/beacon/blobs/{block_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK, "JSON blobs for {block_id}");
    response.json::<GetBlobsResponse>().await.unwrap().data
}

/// Fetches `/eth/v1/beacon/blobs/{block_id}` as SSZ, asserting success and content type.
async fn beacon_blobs_ssz(endpoint: &str, block_id: &str) -> Vec<Blob> {
    let response = reqwest::Client::new()
        .get(format!("{endpoint}/eth/v1/beacon/blobs/{block_id}"))
        .header(axum::http::header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK, "SSZ blobs for {block_id}");
    assert_eq!(
        response.headers().get("content-type").and_then(|h| h.to_str().ok()),
        Some("application/octet-stream")
    );
    let body = response.bytes().await.unwrap();
    // SSZ decoding of full blobs recurses deeply; use a larger stack like the test above.
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || Vec::<Blob>::from_ssz_bytes(&body))
        .unwrap()
        .join()
        .unwrap()
        .unwrap()
}

/// Asserts blob equality without dumping 128 KiB blobs on failure.
fn assert_blobs_eq(actual: &[Blob], expected: &[Blob], context: &str) {
    assert_eq!(actual.len(), expected.len(), "{context}: blob count");
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert!(actual == expected, "{context}: blob {index} bytes differ");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_upstream_metadata_is_stable_across_mining_modes() {
    let fixture = BeaconTargetFixture::spawn().await;
    let endpoint = fixture.handle.http_endpoint();
    let client = reqwest::Client::new();
    let expected_genesis = MockBeacon::genesis().await.0;

    // Mining starts paused; then switch through interval modes too long to fire, then pause.
    for interval_secs in [None, Some(3_600), Some(7_200), Some(0)] {
        if let Some(interval_secs) = interval_secs {
            fixture.api.anvil_set_interval_mining(interval_secs).unwrap();
        }

        let genesis: GenesisResponse = client
            .get(format!("{endpoint}/eth/v1/beacon/genesis"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(genesis, expected_genesis, "genesis with interval {interval_secs:?}");

        let spec = client.get(format!("{endpoint}/eth/v1/config/spec")).send().await.unwrap();
        assert_eq!(spec.status(), reqwest::StatusCode::OK, "spec with interval {interval_secs:?}");
        let spec: serde_json::Value = spec.json().await.unwrap();
        assert_eq!(
            spec["data"]["SECONDS_PER_SLOT"],
            serde_json::json!(BEACON_SECONDS_PER_SLOT.to_string()),
            "spec with interval {interval_secs:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_numeric_ids_resolve_local_slots() {
    let fixture = BeaconTargetFixture::spawn().await;
    let endpoint = fixture.handle.http_endpoint();

    // Slot 21 has no local block yet; the upstream's conflicting slot 21 must not be consulted.
    assert_eq!(beacon_blobs_status(&endpoint, "21").await, reqwest::StatusCode::NOT_FOUND);

    let sent = fixture.mine_blob_block(beacon_slot_timestamp(21), &BEACON_LOCAL_BLOB_DATA).await;
    let block = http_provider(&endpoint)
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(block.header.number, BEACON_ORIGIN_BLOCK + 1);
    assert_eq!(block.header.timestamp, beacon_slot_timestamp(21));

    let expected = [sent[0].1.blobs[0], sent[1].1.blobs[0]];
    assert!(expected[0] != expected[1], "fixture blobs must be distinct");
    let json = beacon_blobs_json(&endpoint, "21").await;
    assert_blobs_eq(&json, &expected, "slot 21 JSON");
    let ssz = beacon_blobs_ssz(&endpoint, "21").await;
    assert_blobs_eq(&ssz, &expected, "slot 21 SSZ");

    let second_hash = sent[1].1.versioned_hashes().next().unwrap();
    let filtered =
        beacon_blobs_json(&endpoint, &format!("21?versioned_hashes={second_hash}")).await;
    assert_blobs_eq(&filtered, &expected[1..], "slot 21 filtered to second hash");

    let partly_malformed = format!("{second_hash},0xzz");
    for malformed in ["0x1234", "not-a-hash", partly_malformed.as_str()] {
        assert_eq!(
            beacon_blobs_status(&endpoint, &format!("21?versioned_hashes={malformed}")).await,
            reqwest::StatusCode::BAD_REQUEST,
            "malformed versioned_hashes {malformed}"
        );
    }

    // Skip slot 22 and mine slot 23: the skipped slot is missing, not slot 21's blobs.
    fixture
        .api
        .evm_mine(Some(MineOptions::Timestamp(Some(beacon_slot_timestamp(23)))))
        .await
        .unwrap();
    assert_eq!(beacon_blobs_status(&endpoint, "22").await, reqwest::StatusCode::NOT_FOUND);
    assert!(beacon_blobs_json(&endpoint, "23").await.is_empty());

    assert_eq!(
        fixture.beacon.blob_requests(),
        Vec::<String>::new(),
        "post-boundary upstream reads"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_historical_slots_use_upstream() {
    let fixture = BeaconTargetFixture::spawn().await;
    let endpoint = fixture.handle.http_endpoint();
    let expected = [fixture.beacon.historical_blob()];

    // Slot 20 holds the pinned execution block itself, so it is upstream-owned.
    let json = beacon_blobs_json(&endpoint, "20").await;
    assert_blobs_eq(&json, &expected, "slot 20 JSON");
    let ssz = beacon_blobs_ssz(&endpoint, "20").await;
    assert_blobs_eq(&ssz, &expected, "slot 20 SSZ");
    let requests = fixture.beacon.blob_requests();
    assert!(!requests.is_empty(), "slot 20 must be read upstream");
    assert!(requests.iter().all(|id| id == "20"), "unexpected upstream reads {requests:?}");

    let status = beacon_blobs_status(&endpoint, "18").await;
    assert!(status.is_server_error(), "upstream failure must not look missing, got {status}");
    assert_eq!(beacon_blobs_status(&endpoint, "19").await, reqwest::StatusCode::NOT_FOUND);
    let requests = fixture.beacon.blob_requests();
    assert!(requests.iter().any(|id| id == "18"), "slot 18 not read upstream: {requests:?}");
    assert!(requests.iter().any(|id| id == "19"), "slot 19 not read upstream: {requests:?}");

    assert_eq!(
        beacon_blobs_status(&endpoint, "20?versioned_hashes=0x1234").await,
        reqwest::StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.beacon.blob_requests(), requests, "malformed query reached upstream");
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_rejects_offgrid_and_reused_slot_timestamps() {
    let fixture = BeaconTargetFixture::spawn().await;
    let provider = http_provider(&fixture.handle.http_endpoint());
    let mine_at = |timestamp| fixture.api.evm_mine(Some(MineOptions::Timestamp(Some(timestamp))));

    assert!(mine_at(BEACON_ORIGIN_TIMESTAMP_SECS).await.is_err(), "reused boundary slot");
    assert!(mine_at(beacon_slot_timestamp(21) + 1).await.is_err(), "off-grid timestamp");
    assert_eq!(provider.get_block_number().await.unwrap(), BEACON_ORIGIN_BLOCK);

    mine_at(beacon_slot_timestamp(21)).await.unwrap();
    assert!(mine_at(beacon_slot_timestamp(21)).await.is_err(), "reused local slot");
    assert_eq!(provider.get_block_number().await.unwrap(), BEACON_ORIGIN_BLOCK + 1);
    let block = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    assert_eq!(block.header.timestamp, beacon_slot_timestamp(21));
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_execution_reads_around_boundary() {
    let fixture = BeaconTargetFixture::spawn().await;
    let provider = http_provider(&fixture.handle.http_endpoint());
    let origin_provider = http_provider(&fixture.origin.http_endpoint());

    // Competing upstream continuation after the pinned block.
    fixture
        .origin_api
        .evm_mine(Some(MineOptions::Timestamp(Some(beacon_slot_timestamp(25)))))
        .await
        .unwrap();
    let origin_block = |number: u64| origin_provider.get_block(BlockId::number(number));
    let origin_boundary = origin_block(BEACON_ORIGIN_BLOCK).await.unwrap().unwrap();
    let remote_successor = origin_block(BEACON_ORIGIN_BLOCK + 1).await.unwrap().unwrap();

    let sent = fixture.mine_blob_block(beacon_slot_timestamp(21), &BEACON_LOCAL_BLOB_DATA).await;
    let sent_hashes = sent.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();

    // Boundary block F is the upstream block, by number and by hash.
    let boundary = provider.get_block(BlockId::number(BEACON_ORIGIN_BLOCK)).await.unwrap().unwrap();
    assert_eq!(boundary.header.hash, origin_boundary.header.hash);
    assert_eq!(boundary.header.timestamp, BEACON_ORIGIN_TIMESTAMP_SECS);
    let boundary_hash = boundary.header.hash;
    let by_hash = provider.get_block(BlockId::hash(boundary_hash)).await.unwrap().unwrap();
    assert_eq!(by_hash.header.number, BEACON_ORIGIN_BLOCK);
    let by_hash = provider.get_block(BlockId::hash(boundary_hash)).full().await.unwrap().unwrap();
    assert_eq!(by_hash.header.number, BEACON_ORIGIN_BLOCK);
    let receipts = provider.get_block_receipts(BlockId::number(BEACON_ORIGIN_BLOCK)).await.unwrap();
    assert_eq!(receipts.map(|receipts| receipts.len()), Some(0), "boundary receipts");

    // F+1 is the local blob block, by number and by hash, with local receipts.
    let local =
        provider.get_block(BlockId::number(BEACON_ORIGIN_BLOCK + 1)).await.unwrap().unwrap();
    let local_hash = local.header.hash;
    assert_ne!(local_hash, remote_successor.header.hash);
    assert_eq!(local.header.parent_hash, boundary_hash);
    assert_eq!(local.header.timestamp, beacon_slot_timestamp(21));
    assert_eq!(local.transactions.hashes().collect::<Vec<_>>(), sent_hashes);
    let by_hash = provider.get_block(BlockId::hash(local_hash)).full().await.unwrap().unwrap();
    assert_eq!(by_hash.header.number, BEACON_ORIGIN_BLOCK + 1);
    assert_eq!(by_hash.transactions.hashes().collect::<Vec<_>>(), sent_hashes);
    for hash in &sent_hashes {
        let receipt = provider.get_transaction_receipt(*hash).await.unwrap().unwrap();
        assert_eq!(receipt.block_number, Some(BEACON_ORIGIN_BLOCK + 1));
        assert_eq!(receipt.block_hash, Some(local_hash));
    }
    for id in [BlockId::number(BEACON_ORIGIN_BLOCK + 1), BlockId::hash(local_hash)] {
        let receipts = provider.get_block_receipts(id).await.unwrap().unwrap();
        let receipt_hashes = receipts.iter().map(|r| r.transaction_hash).collect::<Vec<_>>();
        assert_eq!(receipt_hashes, sent_hashes, "block receipts by {id:?}");
    }

    // The upstream's post-boundary block is not part of the local chain.
    let remote_hash = BlockId::hash(remote_successor.header.hash);
    assert!(provider.get_block(remote_hash).await.unwrap().is_none(), "hashes lookup");
    assert!(provider.get_block(remote_hash).full().await.unwrap().is_none(), "full lookup");
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_revert_and_reset_follow_canonical_history() {
    let fixture = BeaconTargetFixture::spawn().await;
    let endpoint = fixture.handle.http_endpoint();
    let snapshot = fixture.api.evm_snapshot().await.unwrap();
    fixture.mine_blob_block(1252, &BEACON_LOCAL_BLOB_DATA).await;
    assert_eq!(beacon_blobs_json(&endpoint, "21").await.len(), 2);
    assert!(fixture.api.evm_revert(snapshot).await.unwrap());
    assert_eq!(beacon_blobs_status(&endpoint, "21").await, StatusCode::NOT_FOUND);
    fixture.mine_blob_block(1276, &BEACON_LOCAL_BLOB_DATA).await;

    // A failed Beacon refresh must preserve the execution fork and local blobs.
    fixture.beacon.state.seconds_per_slot.store(0, Ordering::Relaxed);
    let reset = Forking { json_rpc_url: None, block_number: Some(100) };
    assert!(fixture.api.anvil_reset(Some(reset.clone())).await.is_err());
    assert_eq!(fixture.api.block_number().unwrap(), U256::from(101));
    assert_eq!(beacon_blobs_json(&endpoint, "23").await.len(), 2);
    fixture.beacon.state.seconds_per_slot.store(12, Ordering::Relaxed);
    fixture.api.anvil_reset(Some(reset)).await.unwrap();
    assert_eq!(fixture.api.block_number().unwrap(), U256::from(100));
    assert_eq!(beacon_blobs_status(&endpoint, "23").await, StatusCode::NOT_FOUND);

    // Advance the origin and reset to that new boundary; the next local slot follows it.
    fixture.origin_api.evm_mine(Some(MineOptions::Timestamp(Some(1276)))).await.unwrap();
    fixture
        .api
        .anvil_reset(Some(Forking { json_rpc_url: None, block_number: Some(101) }))
        .await
        .unwrap();
    fixture.api.evm_mine(None).await.unwrap();
    let head = http_provider(&endpoint)
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((head.header.number, head.header.timestamp), (102, 1288));

    fixture.api.anvil_reset(None).await.unwrap();
    fixture.api.evm_set_block_timestamp_interval(5).unwrap();
    fixture.api.evm_mine(None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_bounds_upstream_reads_and_filters_content() {
    let (_, origin) = spawn(beacon_origin_config()).await;
    let beacon = MockBeacon::spawn().await;
    let config = beacon_target_config(origin.http_endpoint(), beacon.url.clone())
        .fork_request_timeout(Some(Duration::from_secs(1)));
    let (_, target) = spawn(config).await;
    let endpoint = target.http_endpoint();
    for slot in ["16", "17"] {
        let status = beacon_blobs_status(&endpoint, slot).await;
        assert!(status.is_server_error(), "oversize/timeout must not become absence: {status}");
    }
    let historical_hash =
        beacon_sidecar(BEACON_HISTORICAL_BLOB_DATA).versioned_hashes().next().unwrap();
    let conflicting_hash =
        beacon_sidecar(BEACON_CONFLICTING_BLOB_DATA).versioned_hashes().next().unwrap();
    let filtered =
        beacon_blobs_json(&endpoint, &format!("20?versioned_hashes={historical_hash}")).await;
    assert_blobs_eq(&filtered, &[beacon.historical_blob()], "historical filter");
    // Mock ignores query filters; the target must filter actual content itself.
    assert!(
        beacon_blobs_json(&endpoint, &format!("20?versioned_hashes={conflicting_hash}"))
            .await
            .is_empty()
    );
    for invalid_slot in ["18446744073709551615", "latest", "0x15"] {
        assert_eq!(beacon_blobs_status(&endpoint, invalid_slot).await, StatusCode::BAD_REQUEST);
    }
}
