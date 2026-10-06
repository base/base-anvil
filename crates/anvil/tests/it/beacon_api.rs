use crate::utils::http_provider;
use alloy_consensus::{Blob, BlobTransactionSidecar, SidecarBuilder, SimpleCoder, Transaction};
use alloy_network::{TransactionBuilder, TransactionBuilder4844, TransactionResponse};
use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256, b256};
use alloy_provider::{
    Provider,
    ext::{DebugApi, TraceApi},
};
use alloy_rpc_types::{
    BlockId, BlockNumberOrTag, Filter, Log, TransactionRequest,
    anvil::{Forking, MineOptions},
    trace::geth::{GethDebugTracingOptions, GethTrace},
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
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use foundry_evm::hardfork::EthereumHardfork;
use ssz::Decode;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

    // Ignoring invalid filters would broaden the request
    let partly_invalid = format!("{},0xzz", actual_versioned_hashes[0]);
    for filter in ["0x1234", "not-a-hash", "", partly_invalid.as_str()] {
        let url = format!(
            "{}/eth/v1/beacon/blobs/{block_number}?versioned_hashes={filter}",
            handle.http_endpoint()
        );
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST, "filter {filter:?}");
    }

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

// Mock Beacon chain: genesis 1,000 with 12-second slots; the target forks at block 100, slot 20.
pub(super) const BEACON_ORIGIN_BLOCK: u64 = 100;
pub(super) const BEACON_ORIGIN_TIMESTAMP_SECS: u64 = 1_240;
pub(super) const BEACON_GENESIS_TIME_SECS: u64 = 1_000;
pub(super) const BEACON_SECONDS_PER_SLOT: u64 = 12;
// Nonzero, so a locally synthesized zero identity is detectable.
pub(super) const BEACON_GENESIS_VALIDATORS_ROOT: B256 =
    b256!("0x4b363db94e286120d76eb905340fdd4e54bfe9f06bf33ff6cf5ad27f511bfe95");
pub(super) const BEACON_GENESIS_FORK_VERSION: [u8; 4] = [0x01, 0x02, 0x03, 0x04];
/// Upstream-owned blob at slot 20.
pub(super) const BEACON_HISTORICAL_BLOB_DATA: &[u8] = b"mock beacon historical slot 20 blob";
/// Upstream blob at slot 21, which the target must never serve.
pub(super) const BEACON_CONFLICTING_BLOB_DATA: &[u8] = b"mock beacon conflicting slot 21 blob";
pub(super) const BEACON_LOCAL_BLOB_DATA: [&[u8]; 2] =
    [b"local slot 21 blob zero", b"local slot 21 blob one"];

pub(super) const fn beacon_slot_timestamp(slot: u64) -> u64 {
    BEACON_GENESIS_TIME_SECS + slot * BEACON_SECONDS_PER_SLOT
}

pub(super) fn beacon_sidecar(data: &[u8]) -> BlobTransactionSidecar {
    SidecarBuilder::<SimpleCoder>::from_slice(data).build().unwrap()
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

struct MockBeaconState {
    historical_blob: Blob,
    conflicting_blob: Blob,
    blob_requests: Mutex<Vec<String>>,
    seconds_per_slot: AtomicU64,
}

/// Mock Beacon node. Blob routes: slot 18 fails with 500, slots 20 and 21 return
/// [`BEACON_HISTORICAL_BLOB_DATA`] and [`BEACON_CONFLICTING_BLOB_DATA`] ignoring filters, every
/// other block ID is 404. Records every blob request's block ID.
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
            .route("/eth/v1/beacon/genesis", get(|| async { Json(Self::genesis()) }))
            .route(
                "/eth/v1/config/spec",
                get(|State(state): State<Arc<MockBeaconState>>| async move {
                    let slot = state.seconds_per_slot.load(Ordering::Relaxed).to_string();
                    Json(serde_json::json!({ "data": { "SECONDS_PER_SLOT": slot } }))
                }),
            )
            .route("/eth/v1/beacon/blobs/{block_id}", get(Self::blobs))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, state }
    }

    pub(super) fn historical_blob(&self) -> Blob {
        self.state.historical_blob
    }

    /// Block IDs of all blob requests received so far, in arrival order.
    pub(super) fn blob_requests(&self) -> Vec<String> {
        self.state.blob_requests.lock().unwrap().clone()
    }

    fn genesis() -> GenesisResponse {
        GenesisResponse {
            data: GenesisData {
                genesis_time: BEACON_GENESIS_TIME_SECS,
                genesis_validators_root: BEACON_GENESIS_VALIDATORS_ROOT,
                genesis_fork_version: FixedBytes::from(BEACON_GENESIS_FORK_VERSION),
            },
        }
    }

    async fn blobs(
        State(state): State<Arc<MockBeaconState>>,
        Path(block_id): Path<String>,
    ) -> Response {
        state.blob_requests.lock().unwrap().push(block_id.clone());
        let blobs = match block_id.as_str() {
            "18" => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            "20" => vec![state.historical_blob],
            "21" => vec![state.conflicting_blob],
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        Json(GetBlobsResponse { execution_optimistic: false, finalized: true, data: blobs })
            .into_response()
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

    /// Mines one single-blob transaction per payload into one block at `timestamp`. Returns
    /// hashes and sidecars in nonce order.
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

/// Requests `/eth/v1/beacon/blobs/{block_id}`, which may include a query.
async fn beacon_blobs(endpoint: &str, block_id: &str, ssz: bool) -> reqwest::Response {
    let accept = if ssz { "application/octet-stream" } else { "application/json" };
    let url = format!("{endpoint}/eth/v1/beacon/blobs/{block_id}");
    reqwest::Client::new().get(url).header("accept", accept).send().await.unwrap()
}

async fn beacon_blobs_status(endpoint: &str, block_id: &str) -> StatusCode {
    beacon_blobs(endpoint, block_id, false).await.status()
}

async fn beacon_blobs_json(endpoint: &str, block_id: &str) -> Vec<Blob> {
    let response = beacon_blobs(endpoint, block_id, false).await;
    assert_eq!(response.status(), StatusCode::OK, "JSON blobs for {block_id}");
    response.json::<GetBlobsResponse>().await.unwrap().data
}

async fn beacon_blobs_ssz(endpoint: &str, block_id: &str) -> Vec<Blob> {
    let response = beacon_blobs(endpoint, block_id, true).await;
    assert_eq!(response.status(), StatusCode::OK, "SSZ blobs for {block_id}");
    assert_eq!(response.headers()["content-type"], "application/octet-stream");
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
    let get = |path: &str| {
        let url = format!("{endpoint}/eth/v1/{path}");
        async move { reqwest::get(url).await.unwrap().json::<serde_json::Value>().await.unwrap() }
    };

    // Mining starts paused; then switch through interval modes too long to fire, then pause.
    for interval_secs in [None, Some(3_600), Some(7_200), Some(0)] {
        if let Some(interval_secs) = interval_secs {
            fixture.api.anvil_set_interval_mining(interval_secs).unwrap();
        }
        let genesis = serde_json::from_value::<GenesisResponse>(get("beacon/genesis").await);
        assert_eq!(genesis.unwrap(), MockBeacon::genesis(), "genesis with {interval_secs:?}");
        assert_eq!(
            get("config/spec").await["data"]["SECONDS_PER_SLOT"],
            BEACON_SECONDS_PER_SLOT.to_string(),
            "spec with {interval_secs:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_numeric_ids_resolve_local_slots() {
    let fixture = BeaconTargetFixture::spawn().await;
    let endpoint = fixture.handle.http_endpoint();

    // Slot 21 has no local block yet; the upstream's conflicting slot 21 must not be consulted.
    assert_eq!(beacon_blobs_status(&endpoint, "21").await, StatusCode::NOT_FOUND);

    let sent = fixture.mine_blob_block(beacon_slot_timestamp(21), &BEACON_LOCAL_BLOB_DATA).await;
    let expected = [sent[0].1.blobs[0], sent[1].1.blobs[0]];
    assert!(expected[0] != expected[1], "fixture blobs must be distinct");
    assert_blobs_eq(&beacon_blobs_json(&endpoint, "21").await, &expected, "slot 21 JSON");
    assert_blobs_eq(&beacon_blobs_ssz(&endpoint, "21").await, &expected, "slot 21 SSZ");

    let second_hash = sent[1].1.versioned_hashes().next().unwrap();
    let filtered =
        beacon_blobs_json(&endpoint, &format!("21?versioned_hashes={second_hash}")).await;
    assert_blobs_eq(&filtered, &expected[1..], "slot 21 filtered to second hash");
    let partly_malformed = format!("21?versioned_hashes={second_hash},0xzz");
    assert_eq!(beacon_blobs_status(&endpoint, &partly_malformed).await, StatusCode::BAD_REQUEST);

    // Skip slot 22 and mine slot 23: the skipped slot is missing, not slot 21's blobs.
    let at_23 = MineOptions::Timestamp(Some(beacon_slot_timestamp(23)));
    fixture.api.evm_mine(Some(at_23)).await.unwrap();
    assert_eq!(beacon_blobs_status(&endpoint, "22").await, StatusCode::NOT_FOUND);
    assert!(beacon_blobs_json(&endpoint, "23").await.is_empty());
    assert_eq!(fixture.beacon.blob_requests(), Vec::<String>::new(), "post-boundary upstream");
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_historical_slots_use_upstream() {
    let fixture = BeaconTargetFixture::spawn().await;
    let endpoint = fixture.handle.http_endpoint();
    let expected = [fixture.beacon.historical_blob()];

    // Slot 20 holds the pinned execution block itself, so it is upstream-owned.
    assert_blobs_eq(&beacon_blobs_json(&endpoint, "20").await, &expected, "slot 20 JSON");
    assert_blobs_eq(&beacon_blobs_ssz(&endpoint, "20").await, &expected, "slot 20 SSZ");
    assert_eq!(fixture.beacon.blob_requests(), ["20", "20"]);

    let status = beacon_blobs_status(&endpoint, "18").await;
    assert!(status.is_server_error(), "upstream failure must not look missing, got {status}");
    assert_eq!(beacon_blobs_status(&endpoint, "19").await, StatusCode::NOT_FOUND);
    let malformed = "20?versioned_hashes=0x1234";
    assert_eq!(beacon_blobs_status(&endpoint, malformed).await, StatusCode::BAD_REQUEST);
    for invalid_slot in ["18446744073709551615", "latest", "0x15"] {
        assert_eq!(beacon_blobs_status(&endpoint, invalid_slot).await, StatusCode::BAD_REQUEST);
    }
    assert_eq!(fixture.beacon.blob_requests(), ["20", "20", "18", "19"]);

    // The mock ignores filters; the target must filter actual content itself.
    let [historical, conflicting] = [BEACON_HISTORICAL_BLOB_DATA, BEACON_CONFLICTING_BLOB_DATA]
        .map(|data| beacon_sidecar(data).versioned_hashes().next().unwrap());
    let filtered = beacon_blobs_json(&endpoint, &format!("20?versioned_hashes={historical}")).await;
    assert_blobs_eq(&filtered, &expected, "historical filter");
    let filtered =
        beacon_blobs_json(&endpoint, &format!("20?versioned_hashes={conflicting}")).await;
    assert!(filtered.is_empty(), "conflicting filter");
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
    let block = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    assert_eq!(block.header.number, BEACON_ORIGIN_BLOCK + 1);
    assert_eq!(block.header.timestamp, beacon_slot_timestamp(21));
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_execution_reads_around_boundary() {
    // Upstream history: block 101 emits a log; the target pins empty block 102.
    const PINNED: u64 = BEACON_ORIGIN_BLOCK + 2;
    let mine_at = async |api: &EthApi, slot| {
        api.evm_mine(Some(MineOptions::Timestamp(Some(beacon_slot_timestamp(slot))))).await.unwrap()
    };
    let (origin_api, origin) = spawn(beacon_origin_config()).await;
    let origin_provider = http_provider(&origin.http_endpoint());
    let origin_sender = origin.dev_accounts().nth(1).unwrap();
    let historical = send_log_tx(&origin.http_endpoint(), origin_sender).await;
    mine_at(&origin_api, 21).await;
    mine_at(&origin_api, 22).await;
    let origin_block = async |number: u64| {
        origin_provider.get_block(BlockId::number(number)).await.unwrap().unwrap()
    };
    let origin_boundary = origin_block(PINNED).await;

    let beacon = MockBeacon::spawn().await;
    let config = beacon_target_config(origin.http_endpoint(), beacon.url.clone())
        .with_fork_block_number(Some(PINNED));
    let (api, handle) = spawn(config).await;
    let fixture = BeaconTargetFixture { origin_api, origin, beacon, api, handle };
    let provider = http_provider(&fixture.handle.http_endpoint());

    // Boundary block F is the upstream block, by number and by hash.
    let boundary = provider.get_block(BlockId::number(PINNED)).await.unwrap().unwrap();
    let boundary_hash = boundary.header.hash;
    assert_eq!(boundary_hash, origin_boundary.header.hash);
    let by_hash = provider.get_block(BlockId::hash(boundary_hash)).await.unwrap().unwrap();
    assert_eq!(by_hash.header.number, PINNED);
    let by_hash = provider.get_block(BlockId::hash(boundary_hash)).full().await.unwrap().unwrap();
    assert_eq!(by_hash.header.number, PINNED);

    // Upstream replaces F, continues past it, and holds a pending transaction.
    let origin_endpoint = fixture.origin.http_endpoint();
    fixture.origin_api.anvil_rollback(Some(1)).await.unwrap();
    let competing = send_log_tx(&origin_endpoint, origin_sender).await;
    mine_at(&fixture.origin_api, 22).await;
    let remote = send_log_tx(&origin_endpoint, origin_sender).await;
    mine_at(&fixture.origin_api, 25).await;
    let pending = send_log_tx(&origin_endpoint, origin_sender).await;
    let competing_boundary = origin_block(PINNED).await;
    assert_ne!(competing_boundary.header.hash, boundary_hash);
    let remote_successor = origin_block(PINNED + 1).await;

    let sent = fixture.mine_blob_block(beacon_slot_timestamp(23), &BEACON_LOCAL_BLOB_DATA).await;
    let sent_hashes = sent.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
    let local_log =
        send_log_tx(&fixture.handle.http_endpoint(), fixture.handle.dev_accounts().next().unwrap())
            .await;
    mine_at(&fixture.api, 24).await;

    // F+1 is the local blob block, by number and by hash.
    let local = provider.get_block(BlockId::number(PINNED + 1)).await.unwrap().unwrap();
    let local_hash = local.header.hash;
    assert_ne!(local_hash, remote_successor.header.hash);
    assert_eq!(local.header.parent_hash, boundary_hash);
    let by_hash = provider.get_block(BlockId::hash(local_hash)).full().await.unwrap().unwrap();
    assert_eq!(by_hash.header.number, PINNED + 1);
    assert_eq!(by_hash.transactions.hashes().collect::<Vec<_>>(), sent_hashes);
    for hash in &sent_hashes {
        let receipt = provider.get_transaction_receipt(*hash).await.unwrap().unwrap();
        assert_eq!(receipt.block_hash, Some(local_hash));
    }

    // Upstream blocks off the pinned chain are not part of the local chain.
    for (block, context) in [(&competing_boundary, "competing F"), (&remote_successor, "F+1")] {
        let hash = BlockId::hash(block.header.hash);
        assert!(provider.get_block(hash).await.unwrap().is_none(), "{context} hashes lookup");
        assert!(provider.get_block(hash).full().await.unwrap().is_none(), "{context} full lookup");
    }

    // Nor are their transactions and logs, unlike historical and local ones.
    let tx_by_hash = async |hash| provider.get_transaction_by_hash(hash).await.unwrap();
    let historical_tx = tx_by_hash(historical).await.unwrap();
    assert_eq!(historical_tx.block_number(), Some(BEACON_ORIGIN_BLOCK + 1));
    let local_tx = tx_by_hash(local_log).await.unwrap();
    assert_eq!(local_tx.block_number(), Some(PINNED + 2));
    let receipt = async |hash| provider.get_transaction_receipt(hash).await.unwrap();
    let traces = async |hash| provider.trace_transaction(hash).await.unwrap();
    let debug_trace = async |hash| {
        provider.debug_trace_transaction(hash, GethDebugTracingOptions::default()).await.unwrap()
    };
    let no_debug_trace = GethTrace::Default(Default::default());
    for (hash, context) in [(historical, "historical"), (local_log, "local")] {
        assert!(receipt(hash).await.is_some(), "{context} receipt");
        assert!(!traces(hash).await.is_empty(), "{context} traces");
        assert_ne!(debug_trace(hash).await, no_debug_trace, "{context} debug trace");
    }
    for (hash, context) in [(competing, "competing F"), (remote, "F+1"), (pending, "pending")] {
        assert!(tx_by_hash(hash).await.is_none(), "{context} transaction");
        assert!(receipt(hash).await.is_none(), "{context} receipt");
        assert!(traces(hash).await.is_empty(), "{context} traces");
        assert_eq!(debug_trace(hash).await, no_debug_trace, "{context} debug trace");
    }
    let receipts = provider.get_block_receipts(BlockId::number(PINNED)).await.unwrap();
    assert_eq!(receipts.map(|receipts| receipts.len()), Some(0), "F block receipts");
    let log_txs = |logs: Vec<Log>| logs.into_iter().map(|log| log.transaction_hash.unwrap());
    let logs_at = async |hash| {
        let logs = provider.get_logs(&Filter::new().at_block_hash(hash)).await.unwrap();
        log_txs(logs).collect::<Vec<_>>()
    };
    assert_eq!(logs_at(historical_tx.block_hash().unwrap()).await, [historical]);
    assert_eq!(logs_at(local_tx.block_hash().unwrap()).await, [local_log]);
    assert!(logs_at(competing_boundary.header.hash).await.is_empty(), "competing F logs");
    assert!(logs_at(remote_successor.header.hash).await.is_empty(), "F+1 logs");
    let range = provider.get_logs(&Filter::new().from_block(BEACON_ORIGIN_BLOCK)).await.unwrap();
    assert_eq!(log_txs(range).collect::<Vec<_>>(), [historical, local_log], "range logs");
}

/// Sends a contract creation whose init code emits one empty `LOG0`.
async fn send_log_tx(endpoint: &str, from: Address) -> B256 {
    // PUSH1 0, PUSH1 0, LOG0
    let init_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xa0]);
    let tx = TransactionRequest::default().with_from(from).with_deploy_code(init_code);
    let pending = http_provider(endpoint).send_transaction(WithOtherFields::new(tx)).await;
    *pending.unwrap().tx_hash()
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_revert_and_reset_follow_canonical_history() {
    let fixture = BeaconTargetFixture::spawn().await;
    let endpoint = fixture.handle.http_endpoint();
    let provider = http_provider(&endpoint);
    let snapshot = fixture.api.evm_snapshot().await.unwrap();
    fixture.mine_blob_block(1252, &BEACON_LOCAL_BLOB_DATA).await;
    assert_eq!(beacon_blobs_json(&endpoint, "21").await.len(), 2);
    assert!(fixture.api.evm_revert(snapshot).await.unwrap());
    assert_eq!(beacon_blobs_status(&endpoint, "21").await, StatusCode::NOT_FOUND);
    fixture.mine_blob_block(1276, &BEACON_LOCAL_BLOB_DATA).await;

    // A failed Beacon refresh must preserve the execution fork, local blobs, metadata and clock.
    let head = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    fixture.api.evm_set_next_block_timestamp(beacon_slot_timestamp(30)).unwrap();
    fixture.beacon.state.seconds_per_slot.store(0, Ordering::Relaxed);
    let reset = Forking { json_rpc_url: None, block_number: Some(100) };
    assert!(fixture.api.anvil_reset(Some(reset.clone())).await.is_err());
    let kept = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    assert_eq!(kept.header.hash, head.header.hash);
    assert_eq!(beacon_blobs_json(&endpoint, "23").await.len(), 2);
    let spec = reqwest::get(format!("{endpoint}/eth/v1/config/spec")).await.unwrap();
    let spec = spec.json::<serde_json::Value>().await.unwrap();
    assert_eq!(spec["data"]["SECONDS_PER_SLOT"], BEACON_SECONDS_PER_SLOT.to_string());
    fixture.api.evm_mine(None).await.unwrap();
    let next = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    assert_eq!((next.header.number, next.header.timestamp), (102, beacon_slot_timestamp(30)));

    fixture.beacon.state.seconds_per_slot.store(12, Ordering::Relaxed);
    fixture.api.anvil_reset(Some(reset)).await.unwrap();
    assert_eq!(fixture.api.block_number().unwrap(), U256::from(100));
    assert_eq!(beacon_blobs_status(&endpoint, "23").await, StatusCode::NOT_FOUND);

    // Advance the origin and reset to that new boundary; the next local slot follows it.
    fixture.origin_api.evm_mine(Some(MineOptions::Timestamp(Some(1276)))).await.unwrap();
    let reset = Forking { json_rpc_url: None, block_number: Some(101) };
    fixture.api.anvil_reset(Some(reset)).await.unwrap();
    fixture.api.evm_mine(None).await.unwrap();
    let head = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    assert_eq!((head.header.number, head.header.timestamp), (102, 1288));

    // In-memory mode drops the Beacon slot restrictions.
    fixture.api.anvil_reset(None).await.unwrap();
    fixture.api.evm_set_block_timestamp_interval(5).unwrap();
    fixture.api.evm_mine(None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_api_failed_reset_preserves_live_chain() {
    let (origin_api, origin) = spawn(beacon_origin_config()).await;
    let beacon = MockBeacon::spawn().await;
    // Execution upstream that serves blocks but can reject account reads, like a pruned node.
    let fail_accounts = Arc::new(AtomicBool::new(false));
    let (fail, origin_url) = (Arc::clone(&fail_accounts), origin.http_endpoint());
    let proxy = Router::new().route(
        "/",
        post(move |Json(request): Json<serde_json::Value>| {
            let (fail, origin_url) = (Arc::clone(&fail), origin_url.clone());
            async move {
                let method = request["method"].as_str().unwrap_or_default();
                let account_read =
                    matches!(method, "eth_getBalance" | "eth_getTransactionCount" | "eth_getCode");
                if account_read && fail.load(Ordering::Relaxed) {
                    let error = serde_json::json!({ "code": -32000, "message": "missing trie node" });
                    return Json(serde_json::json!({ "jsonrpc": "2.0", "id": request["id"], "error": error }));
                }
                let response = reqwest::Client::new().post(origin_url).json(&request).send().await;
                Json(response.unwrap().json::<serde_json::Value>().await.unwrap())
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
    let (api, handle) = spawn(beacon_target_config(proxy_url, beacon.url.clone())).await;
    let fixture = BeaconTargetFixture { origin_api, origin, beacon, api, handle };
    let endpoint = fixture.handle.http_endpoint();
    let provider = http_provider(&endpoint);
    let dev = fixture.handle.dev_accounts().next().unwrap();

    let snapshot = fixture.api.evm_snapshot().await.unwrap();
    fixture.mine_blob_block(1252, &BEACON_LOCAL_BLOB_DATA).await;
    fixture.api.evm_set_next_block_timestamp(beacon_slot_timestamp(30)).unwrap();
    let head = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    let blobs = beacon_blobs_json(&endpoint, "21").await;
    let balance = provider.get_balance(dev).await.unwrap();
    assert_eq!(provider.get_transaction_count(dev).await.unwrap(), 2);

    fail_accounts.store(true, Ordering::Relaxed);
    fixture.beacon.state.seconds_per_slot.store(6, Ordering::Relaxed);
    let reset = Forking { json_rpc_url: None, block_number: Some(BEACON_ORIGIN_BLOCK) };
    assert!(fixture.api.anvil_reset(Some(reset)).await.is_err());
    fail_accounts.store(false, Ordering::Relaxed);

    let kept = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    assert_eq!(kept.header.hash, head.header.hash);
    assert_blobs_eq(&beacon_blobs_json(&endpoint, "21").await, &blobs, "slot 21 after reset");
    assert_eq!(provider.get_balance(dev).await.unwrap(), balance);
    assert_eq!(provider.get_transaction_count(dev).await.unwrap(), 2);
    let spec = reqwest::get(format!("{endpoint}/eth/v1/config/spec")).await.unwrap();
    let spec = spec.json::<serde_json::Value>().await.unwrap();
    assert_eq!(spec["data"]["SECONDS_PER_SLOT"], BEACON_SECONDS_PER_SLOT.to_string());
    fixture.api.evm_mine(None).await.unwrap();
    let next = provider.get_block_by_number(BlockNumberOrTag::Latest).await.unwrap().unwrap();
    assert_eq!(
        (next.header.number, next.header.timestamp),
        (head.header.number + 1, beacon_slot_timestamp(30))
    );
    assert!(fixture.api.evm_revert(snapshot).await.unwrap());
    assert_eq!(fixture.api.block_number().unwrap(), U256::from(BEACON_ORIGIN_BLOCK));
}
