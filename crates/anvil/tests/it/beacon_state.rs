//! Beacon-backed state persistence: state dump identity and live state loads.

use crate::{
    beacon_api::{
        BEACON_GENESIS_FORK_VERSION, BEACON_GENESIS_TIME_SECS, BEACON_GENESIS_VALIDATORS_ROOT,
        BEACON_LOCAL_BLOB_DATA, BEACON_ORIGIN_BLOCK, BEACON_ORIGIN_TIMESTAMP_SECS,
        BEACON_SECONDS_PER_SLOT, BeaconTargetFixture, beacon_sidecar, beacon_slot_timestamp,
        beacon_target_config, send_blob_txs,
    },
    utils::http_provider,
};
use alloy_consensus::BlobTransactionSidecar;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256, keccak256};
use alloy_provider::Provider;
use alloy_rpc_types::{TransactionRequest, anvil::MineOptions};
use alloy_rpc_types_beacon::genesis::GenesisData;
use anvil::{NodeConfig, eth::EthApi, spawn};
use flate2::read::GzDecoder;
use foundry_evm::hardfork::EthereumHardfork;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Read,
    time::{Duration, Instant},
};

/// Payload of the single local blob mined at slot 23, after skipped slot 22.
const SLOT_23_BLOB_DATA: &[u8] = b"local slot 23 blob";
/// Payload of a blob mined only by a node whose history must be discarded or kept intact.
const OTHER_BLOB_DATA: &[u8] = b"other node local blob";

async fn rpc_ok(endpoint: &str, method: &str, params: Value) -> Value {
    http_provider(endpoint)
        .raw_request(method.to_owned().into(), params)
        .await
        .unwrap_or_else(|error| panic!("{method} failed: {error}"))
}

/// Reads a JSON number, decimal string, or hex quantity.
fn json_u64(value: &Value) -> u64 {
    match value {
        Value::Number(number) => number.as_u64().unwrap(),
        Value::String(s) => match s.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
            None => s.parse().unwrap(),
        },
        _ => panic!("not an integer: {value}"),
    }
}

/// Replaces an integer while keeping its JSON encoding (number, decimal, or hex string).
fn set_u64(value: &mut Value, n: u64) {
    *value = match value {
        Value::Number(_) => json!(n),
        Value::String(s) if s.starts_with("0x") => json!(format!("{n:#x}")),
        Value::String(_) => json!(n.to_string()),
        _ => panic!("not an integer: {value}"),
    };
}

/// Changes an integer or hex value to a different value of the same shape.
fn perturb(value: &mut Value) {
    match value {
        Value::String(s) if s.starts_with("0x") => {
            let last = if s.ends_with('0') { '1' } else { '0' };
            s.pop();
            s.push(last);
        }
        _ => {
            let n = json_u64(value);
            set_u64(value, n + 1);
        }
    }
}

fn quantity(n: u64) -> Value {
    json!(format!("{n:#x}"))
}

fn ts(slot: u64) -> u64 {
    beacon_slot_timestamp(slot)
}

/// A slow runner may advance several slots while checking the restored state.
fn assert_resumed_timestamp(header: &Value, parent_slot: u64, started: Instant) {
    let timestamp = json_u64(&header["timestamp"]);
    let earliest = ts(parent_slot + 1);
    let latest = earliest.max(ts(parent_slot) + (started.elapsed().as_secs() + 1) / 12 * 12);
    assert!(
        (earliest..=latest).contains(&timestamp)
            && (timestamp - BEACON_GENESIS_TIME_SECS).is_multiple_of(BEACON_SECONDS_PER_SLOT),
        "timestamp {timestamp} outside restored slot range {earliest}..={latest}"
    );
}

async fn send_transfer(endpoint: &str, from: Address, to: Address, value: U256) -> B256 {
    let tx = TransactionRequest::default()
        .with_from(from)
        .with_to(to)
        .with_nonce(0)
        .with_value(value)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(10_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000_000);
    serde_json::from_value(rpc_ok(endpoint, "eth_sendTransaction", json!([tx])).await).unwrap()
}

async fn mine_at(endpoint: &str, timestamp: u64) {
    rpc_ok(endpoint, "evm_mine", json!([MineOptions::Timestamp(Some(timestamp))])).await;
}

async fn block_at(endpoint: &str, id: Value) -> Value {
    rpc_ok(endpoint, "eth_getBlockByNumber", json!([id, false])).await
}

async fn block_number(endpoint: &str) -> u64 {
    http_provider(endpoint).get_block_number().await.unwrap()
}

async fn balance(endpoint: &str, address: Address) -> U256 {
    http_provider(endpoint).get_balance(address).await.unwrap()
}

async fn nonce(endpoint: &str, address: Address) -> u64 {
    http_provider(endpoint).get_transaction_count(address).await.unwrap()
}

fn blobs_path(query: &str) -> String {
    format!("/eth/v1/beacon/blobs/{query}")
}

/// Fetches a Beacon path as JSON or SSZ, returning the status and exact body bytes.
async fn beacon_get(endpoint: &str, path: &str, ssz: bool) -> (u16, Vec<u8>) {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let mut request = client.get(format!("{endpoint}{path}"));
    if ssz {
        request = request.header(reqwest::header::ACCEPT, "application/octet-stream");
    }
    let response = request.send().await.unwrap();
    (response.status().as_u16(), response.bytes().await.unwrap().to_vec())
}

/// Beacon paths covering metadata, every local slot through `last_slot`, and hash filtering.
fn beacon_paths(sent: &[(u64, &(B256, BlobTransactionSidecar))], last_slot: u64) -> Vec<String> {
    let mut paths = vec!["/eth/v1/beacon/genesis".to_string(), "/eth/v1/config/spec".to_string()];
    paths.extend((20..=last_slot).map(|slot| blobs_path(&slot.to_string())));
    let unknown = B256::repeat_byte(0x11);
    for (slot, (_, sidecar)) in sent {
        let hash = sidecar.versioned_hashes().next().unwrap();
        paths.push(blobs_path(&format!("{slot}?versioned_hashes={hash}")));
        paths.push(blobs_path(&format!("{slot}?versioned_hashes={unknown},{hash}")));
        // A hash from another slot must not leak into this slot.
        paths.push(blobs_path(&format!("{}?versioned_hashes={hash}", slot + 1)));
    }
    paths
}

/// Execution and Beacon views that must be identical across equivalent states: blocks by number
/// and hash, block receipts by number and hash, transactions and receipts by hash, and hashes of
/// exact Beacon JSON and SSZ bodies.
async fn snapshot(
    endpoint: &str,
    blocks: &[u64],
    txs: &[B256],
    beacon_paths: &[String],
) -> BTreeMap<String, Value> {
    let mut snapshot = BTreeMap::new();
    for &n in blocks {
        let block = rpc_ok(endpoint, "eth_getBlockByNumber", json!([quantity(n), true])).await;
        assert!(!block.is_null(), "block {n} is missing");
        let hash = block["hash"].clone();
        // RPC history alone is insufficient: execution must see the same BLOCKHASH.
        let contract = Address::repeat_byte(0x73);
        let code = format!("0x7f{n:064x}4060005260206000f3");
        let evm_hash = rpc_ok(
            endpoint,
            "eth_call",
            json!([
                {"to": contract}, "pending", {contract.to_string(): {"code": code}}
            ]),
        )
        .await;
        assert_eq!(evm_hash, hash, "EVM BLOCKHASH({n})");
        snapshot.insert(format!("block {n}"), block);
        let by_hash = rpc_ok(endpoint, "eth_getBlockByHash", json!([hash, true])).await;
        snapshot.insert(format!("block {n} by hash"), by_hash);
        let receipts = rpc_ok(endpoint, "eth_getBlockReceipts", json!([quantity(n)])).await;
        snapshot.insert(format!("receipts {n}"), receipts);
        let receipts = rpc_ok(endpoint, "eth_getBlockReceipts", json!([hash])).await;
        snapshot.insert(format!("receipts {n} by hash"), receipts);
    }
    for tx in txs {
        let receipt = rpc_ok(endpoint, "eth_getTransactionReceipt", json!([tx])).await;
        snapshot.insert(format!("receipt {tx}"), receipt);
        let transaction = rpc_ok(endpoint, "eth_getTransactionByHash", json!([tx])).await;
        snapshot.insert(format!("tx {tx}"), transaction);
    }
    for path in beacon_paths {
        for ssz in [false, true] {
            let (status, body) = beacon_get(endpoint, path, ssz).await;
            let digest = json!({ "status": status, "len": body.len(), "keccak": keccak256(&body) });
            snapshot.insert(format!("{} {path}", if ssz { "ssz" } else { "json" }), digest);
        }
    }
    snapshot
}

fn brief(value: &Value) -> String {
    let s = value.to_string();
    if s.len() <= 240 { s } else { format!("{}... ({} bytes)", &s[..240], s.len()) }
}

/// Compares snapshots entry by entry, printing only changed entries in truncated form.
fn assert_same(
    expected: &BTreeMap<String, Value>,
    actual: &BTreeMap<String, Value>,
    context: &str,
) {
    assert_eq!(
        expected.keys().collect::<Vec<_>>(),
        actual.keys().collect::<Vec<_>>(),
        "{context}: snapshot keys"
    );
    let changed = expected
        .iter()
        .filter(|(key, value)| actual[*key] != **value)
        .map(|(key, value)| format!("{key}: {} -> {}", brief(value), brief(&actual[key])))
        .collect::<Vec<_>>();
    assert!(
        changed.is_empty(),
        "{context}: {} changed entries:\n{}",
        changed.len(),
        changed.join("\n")
    );
}

/// Asserts the serialized state carries the exact Beacon fork identity.
fn assert_identity(state: &Value, chain_id: u64, boundary_hash: &Value, slots_in_an_epoch: u64) {
    let identity = &state["fork_beacon"];
    assert!(identity.is_object(), "Beacon state must carry a fork_beacon object");
    assert_eq!(json_u64(&identity["chain_id"]), chain_id, "identity chain_id");
    assert_eq!(json_u64(&identity["block_number"]), BEACON_ORIGIN_BLOCK, "identity block_number");
    assert_eq!(identity["block_hash"], *boundary_hash, "identity block_hash");
    assert_eq!(
        json_u64(&identity["timestamp"]),
        BEACON_ORIGIN_TIMESTAMP_SECS,
        "identity timestamp"
    );
    let expected_genesis = GenesisData {
        genesis_time: BEACON_GENESIS_TIME_SECS,
        genesis_validators_root: BEACON_GENESIS_VALIDATORS_ROOT,
        genesis_fork_version: FixedBytes::from(BEACON_GENESIS_FORK_VERSION),
    };
    let genesis = serde_json::from_value::<GenesisData>(identity["genesis"].clone()).unwrap();
    assert_eq!(genesis, expected_genesis, "identity genesis");
    assert_eq!(json_u64(&identity["seconds_per_slot"]), BEACON_SECONDS_PER_SLOT, "identity slot");
    assert_eq!(json_u64(&identity["slots_in_an_epoch"]), slots_in_an_epoch, "identity epoch");
}

async fn dump_json(api: &EthApi) -> Value {
    let dump = api.anvil_dump_state(None).await.unwrap();
    let mut json = Vec::new();
    GzDecoder::new(dump.as_ref()).read_to_end(&mut json).unwrap();
    serde_json::from_slice(&json).unwrap()
}

async fn load_json(api: &EthApi, state: &Value) -> Result<bool, String> {
    let state = Bytes::from(serde_json::to_vec(state).unwrap());
    api.anvil_load_state(state).await.map_err(|error| error.to_string())
}

/// Returns the serialized header of local block `number`.
fn header_mut(state: &mut Value, number: u64) -> &mut Value {
    let blocks = state["blocks"].as_array_mut().unwrap();
    let block = blocks.iter_mut().find(|block| json_u64(&block["header"]["number"]) == number);
    &mut block.unwrap_or_else(|| panic!("block {number} not in dump"))["header"]
}

fn without_identity(state: &Value) -> Value {
    let mut state = state.clone();
    state.as_object_mut().unwrap().remove("fork_beacon");
    state
}

const IDENTITY_MISMATCH: &str = "State dump Beacon identity does not match";
const INVALID_CHAIN: &str = "Invalid local Beacon chain in state dump";
const INVALID_TIP: &str = "State dump does not match its canonical Beacon tip";

/// Invalid dump: case name, expected error, and mutation of the dump.
type InvalidCase = (&'static str, &'static str, fn(&mut Value));

#[tokio::test(flavor = "multi_thread")]
async fn beacon_state_rejected_loads_preserve_target() {
    let fixture = BeaconTargetFixture::spawn().await;
    let source = fixture.handle.http_endpoint();
    let from = fixture.handle.dev_accounts().next().unwrap();
    fixture.mine_blob_block(ts(21), &BEACON_LOCAL_BLOB_DATA).await;
    send_blob_txs(&source, from, 2, &[SLOT_23_BLOB_DATA]).await;
    mine_at(&source, ts(23)).await;
    let dump = dump_json(&fixture.api).await;
    let source_txs = dump["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tx| serde_json::from_value::<B256>(tx["info"]["transaction_hash"].clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(source_txs.len(), 3, "source transactions in dump");

    // The target has its own chain, account changes and blobs at different slots.
    let config = beacon_target_config(fixture.origin.http_endpoint(), fixture.beacon.url.clone());
    let (api, target) = spawn(config).await;
    let endpoint = target.http_endpoint();
    let sent = send_blob_txs(&endpoint, from, 0, &[OTHER_BLOB_DATA]).await;
    let started = Instant::now();
    mine_at(&endpoint, ts(22)).await;
    let paths = ["21", "22", "23"].map(blobs_path);
    let blocks = [BEACON_ORIGIN_BLOCK, BEACON_ORIGIN_BLOCK + 1];
    let target_txs = [sent[0].0];
    let expected = snapshot(&endpoint, &blocks, &target_txs, &paths).await;
    let expected_nonce = nonce(&endpoint, from).await;
    let expected_balance = balance(&endpoint, from).await;

    let assert_rejected = async |state: &Value, case: &str, error: &str| {
        let result = load_json(&api, state).await;
        assert!(
            result.as_ref().is_err_and(|message| message.contains(error)),
            "load with {case} must fail with {error:?}, got {result:?}"
        );
        assert_eq!(block_number(&endpoint).await, blocks[1], "{case} moved the tip");
        let actual = snapshot(&endpoint, &blocks, &target_txs, &paths).await;
        assert_same(&expected, &actual, &format!("after rejected load with {case}"));
        assert_eq!(nonce(&endpoint, from).await, expected_nonce, "{case} nonce");
        assert_eq!(balance(&endpoint, from).await, expected_balance, "{case} balance");
        for tx in &source_txs {
            let receipt = rpc_ok(&endpoint, "eth_getTransactionReceipt", json!([tx])).await;
            assert!(receipt.is_null(), "{case} imported source transaction {tx}");
        }
    };

    assert_rejected(&without_identity(&dump), "missing identity", IDENTITY_MISMATCH).await;

    let chain_id = json_u64(&rpc_ok(&source, "eth_chainId", json!([])).await);
    let boundary = block_at(&source, quantity(BEACON_ORIGIN_BLOCK)).await;
    assert_identity(&dump, chain_id, &boundary["hash"], 32);
    for field in [
        "chain_id",
        "block_number",
        "block_hash",
        "timestamp",
        "genesis",
        "seconds_per_slot",
        "slots_in_an_epoch",
    ] {
        let mut state = dump.clone();
        state["fork_beacon"].as_object_mut().unwrap().remove(field);
        assert_rejected(&state, &format!("missing {field}"), "decode state").await;
    }
    for pointer in [
        "/chain_id",
        "/block_number",
        "/block_hash",
        "/timestamp",
        "/genesis/genesis_time",
        "/genesis/genesis_validators_root",
        "/genesis/genesis_fork_version",
        "/seconds_per_slot",
        "/slots_in_an_epoch",
    ] {
        let mut state = dump.clone();
        perturb(state["fork_beacon"].pointer_mut(pointer).unwrap());
        assert_rejected(&state, &format!("mismatched {pointer}"), IDENTITY_MISMATCH).await;
    }

    // Local history must extend the boundary contiguously on strictly later slots, ending at the
    // selected head and block environment.
    let invalid_cases: [InvalidCase; 9] = [
        ("off-grid timestamp", INVALID_CHAIN, |s| {
            set_u64(&mut header_mut(s, 101)["timestamp"], ts(21) + 1)
        }),
        ("reused boundary slot", INVALID_CHAIN, |s| {
            set_u64(&mut header_mut(s, 101)["timestamp"], BEACON_ORIGIN_TIMESTAMP_SECS)
        }),
        ("non-increasing timestamp", INVALID_CHAIN, |s| {
            set_u64(&mut header_mut(s, 102)["timestamp"], ts(21))
        }),
        ("timestamp after year 9999", INVALID_CHAIN, |s| {
            set_u64(&mut header_mut(s, 102)["timestamp"], ts(21_116_858_317))
        }),
        ("detached parent", INVALID_CHAIN, |s| perturb(&mut header_mut(s, 101)["parentHash"])),
        ("missing block 101", INVALID_CHAIN, |s| {
            let blocks = s["blocks"].as_array_mut().unwrap();
            blocks.retain(|block| json_u64(&block["header"]["number"]) != 101);
        }),
        ("stale best number", INVALID_TIP, |s| perturb(&mut s["best_block_number"])),
        ("stale environment number", INVALID_TIP, |s| perturb(&mut s["block"]["number"])),
        ("stale environment timestamp", INVALID_TIP, |s| perturb(&mut s["block"]["timestamp"])),
    ];
    for (case, error, mutate) in invalid_cases {
        let mut state = dump.clone();
        mutate(&mut state);
        assert_rejected(&state, case, error).await;
    }

    // Rejections leave the clock on the target's own tip.
    rpc_ok(&endpoint, "evm_mine", json!([])).await;
    let head = block_at(&endpoint, json!("latest")).await;
    assert_eq!(json_u64(&head["number"]), blocks[1] + 1);
    assert_resumed_timestamp(&head, 22, started);
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_state_legacy_load_works_without_beacon() {
    let fixture = BeaconTargetFixture::spawn().await;
    let beacon_dump = dump_json(&fixture.api).await;
    let execution_only = || {
        NodeConfig::test()
            .with_eth_rpc_url(Some(fixture.origin.http_endpoint()))
            .with_fork_block_number(Some(BEACON_ORIGIN_BLOCK))
            .no_storage_caching()
            .with_hardfork(Some(EthereumHardfork::Cancun.into()))
    };
    let recipient = Address::repeat_byte(0x24);
    let value = U256::from(1_000_000_000u64);
    for mode in ["plain", "execution"] {
        let config = || match mode {
            "plain" => NodeConfig::test().with_hardfork(Some(EthereumHardfork::Cancun.into())),
            _ => execution_only(),
        };
        let (source_api, source) = spawn(config()).await;
        let from = source.dev_accounts().next().unwrap();
        send_transfer(&source.http_endpoint(), from, recipient, value).await;
        let tip = block_number(&source.http_endpoint()).await;
        let state = dump_json(&source_api).await;
        assert!(state.get("fork_beacon").is_none(), "{mode} dump has a Beacon identity");

        let (api, target) = spawn(config()).await;
        load_json(&api, &state).await.unwrap_or_else(|error| panic!("{mode} legacy load: {error}"));
        let endpoint = target.http_endpoint();
        assert_eq!(block_number(&endpoint).await, tip, "{mode}");
        assert_eq!(balance(&endpoint, recipient).await, value, "{mode} balance");

        // Beacon dumps need the Beacon upstream that defines their slots.
        let result = load_json(&api, &beacon_dump).await;
        assert!(
            result.as_ref().is_err_and(|message| message.contains(IDENTITY_MISMATCH)),
            "{mode} loaded a Beacon dump: {result:?}"
        );
        assert_eq!(block_number(&endpoint).await, tip, "{mode} tip after Beacon dump");
        assert_eq!(balance(&endpoint, recipient).await, value, "{mode} balance after Beacon dump");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_state_load_replaces_local_history_and_resets_clock() {
    let fixture = BeaconTargetFixture::spawn().await;
    let source = fixture.handle.http_endpoint();
    let mut accounts = fixture.handle.dev_accounts();
    let (from, other) = (accounts.next().unwrap(), accounts.next().unwrap());
    let slot21 = fixture.mine_blob_block(ts(21), &BEACON_LOCAL_BLOB_DATA).await;
    let slot23 = send_blob_txs(&source, from, 2, &[SLOT_23_BLOB_DATA]).await;
    mine_at(&source, ts(23)).await;
    let source_tip = BEACON_ORIGIN_BLOCK + 2;
    let sent = [(21, &slot21[0]), (21, &slot21[1]), (23, &slot23[0])];
    let source_txs = sent.iter().map(|(_, (hash, _))| *hash).collect::<Vec<_>>();
    let paths = beacon_paths(&sent, 26);
    let blocks = (BEACON_ORIGIN_BLOCK..=source_tip).collect::<Vec<_>>();
    let expected = snapshot(&source, &blocks, &source_txs, &paths).await;
    let dump = dump_json(&fixture.api).await;

    // A longer, later local history with an account change the source never made.
    let config = beacon_target_config(fixture.origin.http_endpoint(), fixture.beacon.url.clone());
    let (api, target) = spawn(config).await;
    let endpoint = target.http_endpoint();
    let recipient = Address::repeat_byte(0x42);
    let transfer = send_transfer(&endpoint, other, recipient, U256::from(1_000_000_000u64)).await;
    mine_at(&endpoint, ts(22)).await;
    let stale_blob = send_blob_txs(&endpoint, from, 0, &[OTHER_BLOB_DATA]).await[0].0;
    mine_at(&endpoint, ts(24)).await;
    mine_at(&endpoint, ts(26)).await;
    let mut stale_hashes = Vec::new();
    for n in BEACON_ORIGIN_BLOCK + 1..=BEACON_ORIGIN_BLOCK + 3 {
        stale_hashes.push(block_at(&endpoint, quantity(n)).await["hash"].clone());
    }
    assert_eq!(balance(&endpoint, recipient).await, U256::from(1_000_000_000u64));
    let stale_snapshot = api.evm_snapshot().await.unwrap();

    let started = Instant::now();
    load_json(&api, &dump).await.expect("matching Beacon state must load");
    assert!(!api.evm_revert(stale_snapshot).await.unwrap(), "stale snapshot survived load");

    assert_eq!(block_number(&endpoint).await, source_tip);
    let actual = snapshot(&endpoint, &blocks, &source_txs, &paths).await;
    assert_same(&expected, &actual, "loaded history");
    assert!(block_at(&endpoint, quantity(source_tip + 1)).await.is_null(), "stale future block");
    for hash in &stale_hashes {
        let block = rpc_ok(&endpoint, "eth_getBlockByHash", json!([hash, false])).await;
        assert!(block.is_null(), "stale block {hash} still served");
    }
    for tx in [transfer, stale_blob] {
        let receipt = rpc_ok(&endpoint, "eth_getTransactionReceipt", json!([tx])).await;
        assert!(receipt.is_null(), "stale transaction {tx} still served");
    }
    assert_eq!(balance(&endpoint, recipient).await, U256::ZERO, "stale account change");
    assert_eq!(nonce(&endpoint, other).await, 0, "stale sender nonce");
    assert_eq!(nonce(&endpoint, from).await, 3, "loaded sender nonce");

    rpc_ok(&endpoint, "evm_mine", json!([])).await;
    let head = block_at(&endpoint, json!("latest")).await;
    assert_eq!(json_u64(&head["number"]), source_tip + 1);
    assert_resumed_timestamp(&head, 23, started);
    assert_eq!(head["parentHash"], expected[&format!("block {source_tip}")]["hash"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_state_boundary_load_restores_fees() {
    let fixture = BeaconTargetFixture::spawn().await;
    // An explicit base fee replaces the one derived from the fork block.
    for base_fee in [None, Some(7)] {
        let config = || {
            beacon_target_config(fixture.origin.http_endpoint(), fixture.beacon.url.clone())
                .with_base_fee(base_fee)
        };
        let (api, handle) = spawn(config()).await;
        let endpoint = handle.http_endpoint();
        let from = handle.dev_accounts().next().unwrap();
        let boundary_dump = api.anvil_dump_state(None).await.unwrap();
        mine_at(&endpoint, ts(21)).await;
        let first = block_at(&endpoint, json!("latest")).await;
        if let Some(base_fee) = base_fee {
            assert_eq!(json_u64(&first["baseFeePerGas"]), base_fee);
        }

        // Full blob blocks and a new base fee move both fee inputs away from the successor's.
        api.anvil_set_next_block_base_fee_per_gas(U256::from(1_000)).await.unwrap();
        let full = vec![1u8; 5 * 126_976 + 1];
        assert_eq!(beacon_sidecar(&full).blobs.len(), 6, "full blob payload");
        for (nonce, slot) in [(0, 22), (1, 23)] {
            send_blob_txs(&endpoint, from, nonce, &[&full]).await;
            mine_at(&endpoint, ts(slot)).await;
        }
        let moved = block_at(&endpoint, json!("latest")).await;
        assert_ne!(moved["baseFeePerGas"], first["baseFeePerGas"]);
        // Explicit fees skip the fork's Cancun blob params, so six blobs don't exceed the target.
        if base_fee.is_none() {
            assert_ne!(moved["excessBlobGas"], first["excessBlobGas"]);
        }

        let (restarted_api, restarted) = spawn(config()).await;
        for (api, endpoint, case) in
            [(&api, endpoint, "live"), (&restarted_api, restarted.http_endpoint(), "restarted")]
        {
            api.anvil_load_state(boundary_dump.clone()).await.unwrap();
            mine_at(&endpoint, ts(21)).await;
            let restored = block_at(&endpoint, json!("latest")).await;
            let context = format!("{case} load with base fee {base_fee:?}");
            assert_eq!(restored["baseFeePerGas"], first["baseFeePerGas"], "{context}");
            assert_eq!(restored["excessBlobGas"], first["excessBlobGas"], "{context}");
            assert_eq!(restored["hash"], first["hash"], "{context}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_state_load_restores_nonce_below_upstream() {
    let fixture = BeaconTargetFixture::spawn().await;
    let account = Address::repeat_byte(0x55);
    fixture.origin_api.anvil_set_nonce(account, U256::from(5)).await.unwrap();
    let endpoint = fixture.handle.http_endpoint();
    assert_eq!(nonce(&endpoint, account).await, 5, "upstream nonce");
    fixture.api.anvil_set_nonce(account, U256::ZERO).await.unwrap();
    let dump = fixture.api.anvil_dump_state(None).await.unwrap();

    let config = beacon_target_config(fixture.origin.http_endpoint(), fixture.beacon.url.clone());
    let (restarted_api, restarted) = spawn(config).await;
    for (api, endpoint, case) in
        [(&fixture.api, endpoint, "live"), (&restarted_api, restarted.http_endpoint(), "restarted")]
    {
        api.anvil_load_state(dump.clone()).await.unwrap();
        assert_eq!(nonce(&endpoint, account).await, 0, "{case} load nonce");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn beacon_state_load_keeps_boundary_hash_without_upstream() {
    let fixture = BeaconTargetFixture::spawn().await;
    let endpoint = fixture.handle.http_endpoint();
    let boundary = block_at(&endpoint, quantity(BEACON_ORIGIN_BLOCK)).await["hash"].clone();
    mine_at(&endpoint, ts(21)).await;
    let dump = fixture.api.anvil_dump_state(None).await.unwrap();

    // Regenerating the origin's genesis with another base fee gives the fork block a new hash.
    fixture.origin_api.anvil_set_next_block_base_fee_per_gas(U256::from(1)).await.unwrap();
    fixture.origin_api.anvil_reset(None).await.unwrap();
    let origin = fixture.origin.http_endpoint();
    assert_ne!(block_at(&origin, quantity(BEACON_ORIGIN_BLOCK)).await["hash"], boundary);

    fixture.api.anvil_load_state(dump).await.unwrap();
    let contract = Address::repeat_byte(0x73);
    let code = format!("0x7f{BEACON_ORIGIN_BLOCK:064x}4060005260206000f3");
    let evm_hash = rpc_ok(
        &endpoint,
        "eth_call",
        json!([{"to": contract}, "pending", {contract.to_string(): {"code": code}}]),
    )
    .await;
    assert_eq!(evm_hash, boundary, "EVM BLOCKHASH(fork block)");
}

/// Graceful `--state` restarts of the CLI.
#[cfg(unix)]
mod process {
    use super::*;
    use crate::beacon_api::{
        MockBeacon, assert_blobs_eq, beacon_blobs_json, beacon_blobs_ssz, beacon_origin_config,
    };
    use alloy_consensus::Blob;
    use std::{
        fs::File,
        path::{Path, PathBuf},
        process::{Child, Command, ExitStatus, Stdio},
    };

    /// Bound on a CLI child reaching `Listening on` or exiting.
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
    /// Bound on a CLI child exiting after SIGTERM, including its state dump.
    const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
    /// Bound on a whole restart scenario; dropping it kills any live child.
    const SCENARIO_TIMEOUT: Duration = Duration::from_secs(300);
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    /// Non-default epoch length, so safe and finalized sit one and two blocks below the tip.
    const SLOTS_IN_AN_EPOCH: u64 = 1;
    /// Payload of the single local blob mined at slot 24.
    const SLOT_24_BLOB_DATA: &[u8] = b"local slot 24 blob";

    /// Anvil CLI child process. It is killed on drop, so a panicking test leaves no process
    /// behind.
    struct AnvilChild {
        child: Child,
        log: PathBuf,
    }

    /// How a CLI child finished starting.
    enum Startup {
        Ready(String),
        Exited(ExitStatus),
    }

    impl AnvilChild {
        fn spawn(args: &[String], dir: &Path, log_name: &str) -> Self {
            let log = dir.join(log_name);
            let file = File::create(&log).unwrap();
            let child = Command::new(env!("CARGO_BIN_EXE_anvil"))
                .args(args)
                .current_dir(dir)
                .stdin(Stdio::null())
                .stdout(file.try_clone().unwrap())
                .stderr(file)
                .spawn()
                .unwrap();
            Self { child, log }
        }

        /// Waits until the child prints its bound address or exits.
        async fn startup(&mut self) -> Startup {
            let deadline = Instant::now() + STARTUP_TIMEOUT;
            loop {
                let log = std::fs::read_to_string(&self.log).unwrap_or_default();
                let address = log
                    .split_inclusive('\n')
                    .filter(|line| line.ends_with('\n'))
                    .find_map(|line| line.trim().strip_prefix("Listening on "))
                    .map(|addresses| addresses.split(", ").next().unwrap().to_string());
                if let Some(address) = address {
                    return Startup::Ready(format!("http://{address}"));
                }
                if let Some(status) = self.child.try_wait().unwrap() {
                    return Startup::Exited(status);
                }
                assert!(Instant::now() < deadline, "anvil did not start:\n{}", self.log_tail());
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }

        async fn ready(&mut self) -> String {
            match self.startup().await {
                Startup::Ready(endpoint) => endpoint,
                Startup::Exited(status) => {
                    panic!("anvil exited during startup with {status}:\n{}", self.log_tail())
                }
            }
        }

        /// Sends SIGTERM and waits for a successful exit, which includes the `--state` dump.
        async fn terminate(&mut self) {
            let pid = self.child.id().to_string();
            assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
            let deadline = Instant::now() + EXIT_TIMEOUT;
            let status = loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                assert!(Instant::now() < deadline, "anvil ignored SIGTERM:\n{}", self.log_tail());
                tokio::time::sleep(POLL_INTERVAL).await;
            };
            assert!(status.success(), "anvil exited with {status}:\n{}", self.log_tail());
        }

        fn log_tail(&self) -> String {
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            let lines = log.lines().collect::<Vec<_>>();
            lines[lines.len().saturating_sub(20)..]
                .iter()
                .map(|line| line.chars().take(300).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    impl Drop for AnvilChild {
        fn drop(&mut self) {
            if matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// CLI arguments for a Beacon-backed target pinned to `fork_block` that persists to `state`.
    fn cli_args(
        origin: &str,
        beacon: &str,
        state: &Path,
        fork_block: u64,
        slots_in_an_epoch: u64,
    ) -> Vec<String> {
        [
            "--port",
            "0",
            "--fork-url",
            origin,
            "--fork-block-number",
            &fork_block.to_string(),
            "--fork-beacon-url",
            beacon,
            "--no-storage-caching",
            "--hardfork",
            "cancun",
            "--no-mining",
            "--slots-in-an-epoch",
            &slots_in_an_epoch.to_string(),
            "--state-interval",
            "3600",
            "--state",
            state.to_str().unwrap(),
        ]
        .map(String::from)
        .to_vec()
    }

    async fn first_account(endpoint: &str) -> Address {
        let accounts = rpc_ok(endpoint, "eth_accounts", json!([])).await;
        serde_json::from_value(accounts[0].clone()).unwrap()
    }

    /// Asserts a blob query returns exactly `expected` as JSON and as SSZ.
    async fn assert_blobs(endpoint: &str, query: &str, expected: &[Blob]) {
        assert_blobs_eq(&beacon_blobs_json(endpoint, query).await, expected, query);
        assert_blobs_eq(&beacon_blobs_ssz(endpoint, query).await, expected, query);
    }

    fn blobs(sent: &[(B256, BlobTransactionSidecar)]) -> Vec<Blob> {
        sent.iter().map(|(_, sidecar)| sidecar.blobs[0]).collect()
    }

    /// Asserts `latest`, `safe`, and `finalized` resolve 0, 1, and 2 epochs below `tip` for
    /// blocks, block receipts and fee history.
    async fn assert_tags(endpoint: &str, tip: u64) {
        for (tag, depth) in
            [("latest", 0), ("safe", SLOTS_IN_AN_EPOCH), ("finalized", 2 * SLOTS_IN_AN_EPOCH)]
        {
            let number = quantity(tip - depth);
            let tagged = block_at(endpoint, json!(tag)).await;
            assert_eq!(tagged, block_at(endpoint, number.clone()).await, "{tag} block");
            let receipts = |id| rpc_ok(endpoint, "eth_getBlockReceipts", json!([id]));
            assert_eq!(receipts(json!(tag)).await, receipts(number).await, "{tag} receipts");
            let history = rpc_ok(endpoint, "eth_feeHistory", json!(["0x1", tag, []])).await;
            assert_eq!(json_u64(&history["oldestBlock"]), tip - depth, "{tag} fee history");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn beacon_state_graceful_restart_preserves_history_and_resets_clock() {
        tokio::time::timeout(SCENARIO_TIMEOUT, restart_preserves_history_and_resets_clock())
            .await
            .expect("restart scenario timed out");
    }

    async fn restart_preserves_history_and_resets_clock() {
        let (origin_api, origin) = spawn(beacon_origin_config()).await;
        let beacon = MockBeacon::spawn().await;
        let origin_url = origin.http_endpoint();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");
        let args =
            cli_args(&origin_url, &beacon.url, &state, BEACON_ORIGIN_BLOCK, SLOTS_IN_AN_EPOCH);

        let mut first = AnvilChild::spawn(&args, dir.path(), "first.log");
        let endpoint = first.ready().await;
        let from = first_account(&endpoint).await;

        // Competing upstream continuation at the slot the local chain skips.
        origin_api.evm_mine(Some(MineOptions::Timestamp(Some(ts(22))))).await.unwrap();
        let competing = block_at(&origin_url, quantity(BEACON_ORIGIN_BLOCK + 1)).await;

        let slot21 = send_blob_txs(&endpoint, from, 0, &BEACON_LOCAL_BLOB_DATA).await;
        mine_at(&endpoint, ts(21)).await;
        let slot23 = send_blob_txs(&endpoint, from, 2, &[SLOT_23_BLOB_DATA]).await;
        mine_at(&endpoint, ts(23)).await;
        let slot24 = send_blob_txs(&endpoint, from, 3, &[SLOT_24_BLOB_DATA]).await;
        mine_at(&endpoint, ts(24)).await;
        let tip = BEACON_ORIGIN_BLOCK + 3;

        let head = block_at(&endpoint, json!("latest")).await;
        assert_eq!((json_u64(&head["number"]), json_u64(&head["timestamp"])), (tip, ts(24)));
        assert_ne!(block_at(&endpoint, quantity(tip - 2)).await["hash"], competing["hash"]);
        assert_blobs(&endpoint, "20", &[beacon.historical_blob()]).await;
        assert_blobs(&endpoint, "21", &blobs(&slot21)).await;
        assert_blobs(&endpoint, "23", &blobs(&slot23)).await;
        assert_blobs(&endpoint, "24", &blobs(&slot24)).await;
        let second_hash = slot21[1].1.versioned_hashes().next().unwrap();
        let filtered = format!("21?versioned_hashes={second_hash}");
        assert_blobs(&endpoint, &filtered, &blobs(&slot21)[1..]).await;
        for missing in ["22", "25"] {
            let status = beacon_get(&endpoint, &blobs_path(missing), false).await.0;
            assert_eq!(status, 404, "slot {missing}");
        }
        assert_tags(&endpoint, tip).await;

        let sent = [(21, &slot21[0]), (21, &slot21[1]), (23, &slot23[0]), (24, &slot24[0])];
        let tx_hashes = sent.iter().map(|(_, (hash, _))| *hash).collect::<Vec<_>>();
        let paths = beacon_paths(&sent, 25);
        let blocks = (BEACON_ORIGIN_BLOCK..=tip).collect::<Vec<_>>();
        let before = snapshot(&endpoint, &blocks, &tx_hashes, &paths).await;

        // Pending clock overrides are process state and must not survive the restart.
        rpc_ok(&endpoint, "evm_setTime", json!([ts(10_000)])).await;
        rpc_ok(&endpoint, "evm_setNextBlockTimestamp", json!([ts(20_000)])).await;
        first.terminate().await;

        let started = Instant::now();
        let mut second = AnvilChild::spawn(&args, dir.path(), "second.log");
        let endpoint = second.ready().await;

        let restarted = block_at(&endpoint, json!("latest")).await;
        assert_eq!(restarted, head, "restarted tip");
        assert_tags(&endpoint, tip).await;
        assert_same(&before, &snapshot(&endpoint, &blocks, &tx_hashes, &paths).await, "restart");
        let competing_lookup =
            rpc_ok(&endpoint, "eth_getBlockByHash", json!([competing["hash"], false]));
        assert!(competing_lookup.await.is_null(), "served the upstream post-boundary block");

        rpc_ok(&endpoint, "evm_mine", json!([])).await;
        let next = block_at(&endpoint, json!("latest")).await;
        assert_eq!(json_u64(&next["number"]), tip + 1);
        assert_resumed_timestamp(&next, 24, started);
        assert_eq!(next["parentHash"], head["hash"]);
        assert_tags(&endpoint, tip + 1).await;

        let requests = beacon.blob_requests();
        assert!(requests.iter().all(|id| id == "20"), "post-boundary upstream reads {requests:?}");
        second.terminate().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn beacon_state_restart_rejects_changed_identity() {
        tokio::time::timeout(SCENARIO_TIMEOUT, restart_rejects_changed_identity())
            .await
            .expect("restart scenario timed out");
    }

    async fn restart_rejects_changed_identity() {
        let (origin_api, origin) = spawn(beacon_origin_config()).await;
        let beacon = MockBeacon::spawn().await;
        let origin_url = origin.http_endpoint();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");
        let args =
            cli_args(&origin_url, &beacon.url, &state, BEACON_ORIGIN_BLOCK, SLOTS_IN_AN_EPOCH);

        let mut first = AnvilChild::spawn(&args, dir.path(), "first.log");
        let endpoint = first.ready().await;
        let from = first_account(&endpoint).await;
        send_blob_txs(&endpoint, from, 0, &BEACON_LOCAL_BLOB_DATA).await;
        mine_at(&endpoint, ts(21)).await;
        let local = block_at(&endpoint, json!("latest")).await;
        first.terminate().await;
        // A competing on-grid upstream block that a restart could wrongly pin instead.
        origin_api.evm_mine(Some(MineOptions::Timestamp(Some(ts(22))))).await.unwrap();

        let dumped_bytes = std::fs::read(&state).unwrap();
        let dumped: Value = serde_json::from_slice(&dumped_bytes).unwrap();
        let chain_id = json_u64(&rpc_ok(&origin_url, "eth_chainId", json!([])).await);
        let boundary = block_at(&origin_url, quantity(BEACON_ORIGIN_BLOCK)).await;
        assert_identity(&dumped, chain_id, &boundary["hash"], SLOTS_IN_AN_EPOCH);
        let legacy = dir.path().join("legacy.json");
        let legacy_bytes = serde_json::to_vec(&without_identity(&dumped)).unwrap();
        std::fs::write(&legacy, &legacy_bytes).unwrap();

        let variants = [
            (
                "competing boundary",
                cli_args(&origin_url, &beacon.url, &state, 101, SLOTS_IN_AN_EPOCH),
            ),
            ("epoch length", cli_args(&origin_url, &beacon.url, &state, BEACON_ORIGIN_BLOCK, 2)),
            (
                "missing identity",
                cli_args(&origin_url, &beacon.url, &legacy, BEACON_ORIGIN_BLOCK, SLOTS_IN_AN_EPOCH),
            ),
        ];
        for (index, (variant, args)) in variants.iter().enumerate() {
            let mut child = AnvilChild::spawn(args, dir.path(), &format!("variant{index}.log"));
            match child.startup().await {
                Startup::Ready(_) => panic!("restart with {variant} must be rejected"),
                Startup::Exited(status) => {
                    assert!(!status.success(), "{variant} exited with {status}");
                    let log = std::fs::read_to_string(&child.log).unwrap();
                    assert!(log.contains(IDENTITY_MISMATCH), "{}", child.log_tail());
                }
            }
            assert!(std::fs::read(&state).unwrap() == dumped_bytes, "{variant} rewrote the state");
            let legacy_now = std::fs::read(&legacy).unwrap();
            assert!(legacy_now == legacy_bytes, "{variant} rewrote legacy state");
        }

        // The unchanged identity still restarts onto the local history.
        let mut restarted = AnvilChild::spawn(&args, dir.path(), "restarted.log");
        let endpoint = restarted.ready().await;
        assert_eq!(block_at(&endpoint, json!("latest")).await, local);
        restarted.terminate().await;
    }
}
