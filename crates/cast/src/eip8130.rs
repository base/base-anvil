//! EIP-8130 (account abstraction, transaction type `0x79`) support for `cast send` / `cast mktx`.
//!
//! EIP-8130 is not understood by alloy's `EthereumWallet`, so like Tempo (`0x76`), these
//! transactions are built, signed, and encoded here directly using the canonical
//! [`base_common_consensus`] types, then broadcast via `eth_sendRawTransaction` (or printed by
//! `mktx`). This keeps all EIP-8130 handling contained to `cast` with no changes to `anvil` or
//! the shared `FoundryTxEnvelope`.
//!
//! ## Value handling
//!
//! Protocol-level EIP-8130 calls carry no value (`msg.value == 0`); native ETH transfers are
//! performed by the account's wallet bytecode. A code-less EOA sender is auto-delegated to
//! `DEFAULT_ACCOUNT`, whose `executeBatch(Call[])` entrypoint forwards value per sub-call. So any
//! phase that contains a value-bearing call is wrapped into a single self-call to
//! `executeBatch`, letting the wallet forward the value. Zero-value phases are emitted as native
//! `{to, data}` calls.

use alloy_eips::Encodable2718;
use alloy_ens::NameOrAddress;
use alloy_network::AnyNetwork;
use alloy_primitives::{Address, Bytes, U64, U256};
use alloy_provider::Provider;
use alloy_signer::Signer;
use alloy_sol_types::{SolCall, sol};
use base_common_consensus::{Call as ProtocolCall, EIP8130_TX_TYPE_ID, Eip8130Signed, TxEip8130};
use eyre::{Result, eyre};
use foundry_cli::{
    opts::TransactionOpts,
    utils::{self, parse_ether_value, parse_function_args},
};
use foundry_config::{Chain, Config};
use foundry_wallets::WalletSigner;
use serde::Deserialize;

sol! {
    /// Mirrors `base/eip-8130`'s `DefaultAccount` call struct and batch entrypoint.
    #[allow(missing_docs)]
    struct DefaultAccountCall {
        address target;
        uint256 value;
        bytes data;
    }
    #[allow(missing_docs)]
    function executeBatch(DefaultAccountCall[] calls);
}

/// A single call in a `--calls` JSON phase.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JsonCall {
    /// Target address of the call.
    to: Address,
    /// ETH value to forward (wei or unit string, e.g. `0.1ether`). Defaults to zero.
    #[serde(default)]
    value: Option<String>,
    /// Function signature to encode with `args` (alternative to `data`).
    #[serde(default)]
    sig: Option<String>,
    /// Arguments for `sig`.
    #[serde(default)]
    args: Vec<String>,
    /// Raw hex calldata (alternative to `sig`/`args`).
    #[serde(default)]
    data: Option<Bytes>,
}

/// A resolved logical call before value-wrapping: `(to, value, data)`.
struct LogicalCall {
    to: Address,
    value: U256,
    data: Bytes,
}

/// Builds and signs an EIP-8130 transaction, returning the EIP-2718 encoded raw bytes ready to be
/// broadcast via `eth_sendRawTransaction` or printed by `mktx`.
///
/// `to`/`sig`/`args` describe the single-call form (used when `--calls` is not provided); when
/// `--calls` is set they are ignored in favor of the JSON phases.
pub async fn build_raw_transaction<P: Provider<AnyNetwork>>(
    provider: &P,
    signer: &WalletSigner,
    tx_opts: &TransactionOpts,
    config: &Config,
    to: Option<NameOrAddress>,
    sig: Option<String>,
    args: Vec<String>,
) -> Result<Vec<u8>> {
    let opts = &tx_opts.eip8130;
    let from = signer.address();
    let chain = utils::get_chain(config.chain, provider).await?;
    let chain_id = chain.id();
    let etherscan_api_key = config.get_etherscan_api_key(Some(chain));

    // Resolve the logical call phases
    let phases = if let Some(calls_json) = &opts.calls {
        parse_calls_json(provider, chain, etherscan_api_key.as_deref(), calls_json).await?
    } else {
        // Single-call form from the positional args
        let to = match to {
            Some(to) => to.resolve(provider).await?,
            None => eyre::bail!(
                "EIP-8130 transactions require a recipient (positional TO or --calls); \
                 they cannot be CREATE transactions"
            ),
        };
        let (data, _) = parse_function_args(
            &sig.unwrap_or_default(),
            args,
            Some(to),
            chain,
            provider,
            etherscan_api_key.as_deref(),
        )
        .await?;
        let value = tx_opts.value.unwrap_or(U256::ZERO);
        vec![vec![LogicalCall { to, value, data: data.into() }]]
    };

    // Encode logical phases into protocol calls, wrapping value via the account wallet
    let calls = encode_phases(from, phases)?;

    // Resolve nonce (key + sequence)
    let nonce_key = opts.nonce_key.unwrap_or(U256::ZERO);
    let nonce_free = nonce_key == U256::MAX;
    let nonce_sequence = resolve_nonce_sequence(provider, from, nonce_key, opts.nonce_seq).await?;

    // Expiry + nonce-free structural validation
    let expiry = opts.expiry.unwrap_or(0);
    if nonce_free {
        if expiry == 0 {
            eyre::bail!(
                "the expiring nonce channel (--nonce-key = 2^256-1) requires a non-zero --expiry"
            );
        }
        if nonce_sequence != 0 {
            eyre::bail!(
                "the expiring nonce channel (--nonce-key = 2^256-1) requires --nonce-seq to be 0"
            );
        }
    }

    // Fees
    let (max_fee_per_gas, max_priority_fee_per_gas) = resolve_fees(provider, tx_opts).await?;

    let mut tx = TxEip8130 {
        chain_id,
        sender: None,
        nonce_key,
        nonce_sequence,
        expiry,
        max_priority_fee_per_gas,
        max_fee_per_gas,
        gas_limit: 0,
        account_changes: Vec::new(),
        calls,
        metadata: opts.metadata.clone().unwrap_or_default(),
        payer: None,
    };

    // Gas limit
    tx.gas_limit = match tx_opts.gas_limit {
        Some(gas_limit) => gas_limit.to::<u64>(),
        None => estimate_gas(provider, &tx, from).await?,
    };

    // Sign (EOA path: 65-byte r||s||v over the sender signature hash)
    let signature = signer.sign_hash(&tx.sender_signature_hash()).await?;
    let sender_auth = Bytes::copy_from_slice(&signature.as_bytes());
    let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());

    let mut raw = Vec::with_capacity(signed.encode_2718_len());
    signed.encode_2718(&mut raw);
    Ok(raw)
}

/// Encodes logical phases into protocol `calls`, wrapping any value-bearing phase into a single
/// `executeBatch` self-call (`to == sender`) so the account wallet forwards the value.
fn encode_phases(from: Address, phases: Vec<Vec<LogicalCall>>) -> Result<Vec<Vec<ProtocolCall>>> {
    if phases.is_empty() || phases.iter().all(|p| p.is_empty()) {
        eyre::bail!("EIP-8130 transaction has no calls");
    }

    let mut out = Vec::with_capacity(phases.len());
    for phase in phases {
        if phase.iter().any(|c| !c.value.is_zero()) {
            // Wrap the whole phase in a DEFAULT_ACCOUNT.executeBatch self-call
            let batch = executeBatchCall {
                calls: phase
                    .into_iter()
                    .map(|c| DefaultAccountCall { target: c.to, value: c.value, data: c.data })
                    .collect(),
            };
            out.push(vec![ProtocolCall { to: from, data: batch.abi_encode().into() }]);
        } else {
            out.push(phase.into_iter().map(|c| ProtocolCall { to: c.to, data: c.data }).collect());
        }
    }
    Ok(out)
}

/// Parses the `--calls` JSON into logical phases, encoding each call's `sig`/`args` or `data`.
async fn parse_calls_json<P: Provider<AnyNetwork>>(
    provider: &P,
    chain: Chain,
    etherscan_api_key: Option<&str>,
    json: &str,
) -> Result<Vec<Vec<LogicalCall>>> {
    let phases: Vec<Vec<JsonCall>> = serde_json::from_str(json)
        .map_err(|e| eyre!("failed to parse --calls JSON (expected an array of phases): {e}"))?;

    let mut out = Vec::with_capacity(phases.len());
    for phase in phases {
        let mut resolved = Vec::with_capacity(phase.len());
        for call in phase {
            let data = match (call.data, call.sig) {
                (Some(_), Some(_)) => {
                    eyre::bail!("call specifies both `data` and `sig`; provide only one")
                }
                (Some(data), None) => data,
                (None, Some(sig)) => {
                    let (data, _) = parse_function_args(
                        &sig,
                        call.args,
                        Some(call.to),
                        chain,
                        provider,
                        etherscan_api_key,
                    )
                    .await?;
                    data.into()
                }
                (None, None) => Bytes::new(),
            };
            let value = match call.value {
                Some(v) => parse_ether_value(&v)?,
                None => U256::ZERO,
            };
            resolved.push(LogicalCall { to: call.to, value, data });
        }
        out.push(resolved);
    }
    Ok(out)
}

/// Resolves the nonce sequence: explicit `--nonce-seq`, else queried from the node
/// (`eth_getTransactionCount(from, "pending", nonceKey)`; `nonce_key == 0` is the standard count).
async fn resolve_nonce_sequence<P: Provider<AnyNetwork>>(
    provider: &P,
    from: Address,
    nonce_key: U256,
    explicit: Option<u64>,
) -> Result<u64> {
    if let Some(seq) = explicit {
        return Ok(seq);
    }
    if nonce_key == U256::MAX {
        // Expiring/nonce-free channel: no counter exists, sequence must be 0
        return Ok(0);
    }
    if nonce_key.is_zero() {
        return Ok(provider.get_transaction_count(from).pending().await?);
    }
    // Keyed channel: query the extended eth_getTransactionCount with the nonce key
    let count: U64 = provider
        .raw_request("eth_getTransactionCount".into(), (from, "pending", nonce_key))
        .await
        .map_err(|e| {
            eyre!(
                "failed to query keyed nonce for --nonce-key {nonce_key} (node must support \
                 EIP-8130 eth_getTransactionCount; otherwise pass --nonce-seq): {e}"
            )
        })?;
    Ok(count.to::<u64>())
}

/// Resolves the EIP-1559-style fees, honoring `--gas-price`/`--priority-gas-price` and otherwise
/// estimating from the provider
async fn resolve_fees<P: Provider<AnyNetwork>>(
    provider: &P,
    tx_opts: &TransactionOpts,
) -> Result<(u128, u128)> {
    let mut max_fee = tx_opts.gas_price.map(|v| v.to::<u128>());
    let mut max_priority = tx_opts.priority_gas_price.map(|v| v.to::<u128>());

    if max_fee.is_none() || max_priority.is_none() {
        let estimate = provider.estimate_eip1559_fees().await?;
        max_fee.get_or_insert(estimate.max_fee_per_gas);
        max_priority.get_or_insert(estimate.max_priority_fee_per_gas);
    }

    Ok((max_fee.unwrap(), max_priority.unwrap()))
}

/// Best-effort gas estimation via the node's EIP-8130-aware `eth_estimateGas`. Falls back to a
/// clear error asking for `--gas-limit` if the node does not accept the request shape
async fn estimate_gas<P: Provider<AnyNetwork>>(
    provider: &P,
    tx: &TxEip8130,
    from: Address,
) -> Result<u64> {
    let mut request = serde_json::to_value(tx)?;
    if let Some(obj) = request.as_object_mut() {
        obj.insert("from".into(), serde_json::json!(from));
        obj.insert("type".into(), serde_json::json!(format!("0x{EIP8130_TX_TYPE_ID:x}")));
        obj.remove("gasLimit");
    }

    let estimated: U64 =
        provider.raw_request("eth_estimateGas".into(), (request,)).await.map_err(|e| {
            eyre!(
                "failed to estimate gas for the EIP-8130 transaction (pass --gas-limit to set it \
                 explicitly): {e}"
            )
        })?;
    Ok(estimated.to::<u64>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, bytes};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;

    // Well-known anvil test key (account 0).
    const TEST_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    fn logical(to: Address, value: u64, data: &[u8]) -> LogicalCall {
        LogicalCall { to, value: U256::from(value), data: Bytes::copy_from_slice(data) }
    }

    #[test]
    fn zero_value_phase_emits_native_calls() {
        let from = address!("0x1111111111111111111111111111111111111111");
        let a = address!("0x00000000000000000000000000000000000000aa");
        let b = address!("0x00000000000000000000000000000000000000bb");
        let calls = encode_phases(
            from,
            vec![vec![logical(a, 0, &hex_lit("dead")), logical(b, 0, &hex_lit("beef"))]],
        )
        .unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 2, "zero-value calls stay native, one protocol call each");
        assert_eq!(calls[0][0].to, a);
        assert_eq!(calls[0][0].data, bytes!("dead"));
        assert_eq!(calls[0][1].to, b);
    }

    #[test]
    fn value_phase_wraps_in_execute_batch_self_call() {
        let from = address!("0x1111111111111111111111111111111111111111");
        let target = address!("0x00000000000000000000000000000000000000aa");
        let calls =
            encode_phases(from, vec![vec![logical(target, 100, &hex_lit("dead"))]]).unwrap();

        // Value-bearing phase collapses to a single self-call (to == sender).
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
        assert_eq!(calls[0][0].to, from, "value phase is a self-call to the account wallet");

        // The calldata must decode as DEFAULT_ACCOUNT.executeBatch([{target,value,data}]).
        let decoded = executeBatchCall::abi_decode(&calls[0][0].data).unwrap();
        assert_eq!(decoded.calls.len(), 1);
        assert_eq!(decoded.calls[0].target, target);
        assert_eq!(decoded.calls[0].value, U256::from(100));
        assert_eq!(decoded.calls[0].data, bytes!("dead"));
    }

    #[test]
    fn empty_calls_is_rejected() {
        let from = address!("0x1111111111111111111111111111111111111111");
        assert!(encode_phases(from, vec![]).is_err());
        assert!(encode_phases(from, vec![vec![]]).is_err());
    }

    #[test]
    fn sign_encode_decode_roundtrip_recovers_sender() {
        let signer: PrivateKeySigner = TEST_KEY.parse().unwrap();
        let from = signer.address();

        let tx = TxEip8130 {
            chain_id: 8453,
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 1,
            expiry: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 2_000_000_000,
            gas_limit: 21_000,
            account_changes: Vec::new(),
            calls: vec![vec![ProtocolCall {
                to: address!("0x00000000000000000000000000000000000000aa"),
                data: bytes!("deadbeef"),
            }]],
            metadata: Bytes::new(),
            payer: None,
        };

        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let sender_auth = Bytes::copy_from_slice(&signature.as_bytes());
        let signed = Eip8130Signed::new(tx.clone(), sender_auth, Bytes::new());

        let mut raw = Vec::with_capacity(signed.encode_2718_len());
        signed.encode_2718(&mut raw);

        // EIP-2718 typed envelope prefixed with the pinned AA_TX_TYPE.
        assert_eq!(raw[0], EIP8130_TX_TYPE_ID, "EIP-8130 tx uses the pinned AA tx type byte");

        // Round-trips through base-common-consensus's own decoder (proves wire compatibility)
        let decoded = Eip8130Signed::rlp_decode_signed(&mut &raw[1..]).unwrap();
        assert_eq!(decoded.tx(), &tx);

        // The EOA signature recovers to the signer (proves the sender_auth format is accepted)
        assert_eq!(decoded.recover_sender().unwrap(), from);
    }

    // Small hex-literal helper for building call data in tests
    fn hex_lit(s: &str) -> Vec<u8> {
        alloy_primitives::hex::decode(s).unwrap()
    }
}
