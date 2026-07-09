use alloy_primitives::{Bytes, U256};
use clap::Parser;

/// CLI options for EIP-8130 (account abstraction, transaction type `0x79`) transactions.
///
/// EIP-8130 replaces the single scalar nonce with a 2D `(nonce_key, nonce_sequence)`
/// nonce, adds an expiry, opaque metadata and native call batching. The protocol-level
/// calls carry no value (`msg.value == 0`); native ETH transfers are performed by the
/// account's wallet bytecode, so a value-bearing call is transparently wrapped as a
/// self-call to the account's `DEFAULT_ACCOUNT` `executeBatch` entrypoint (see
/// `cast`'s `eip8130` module).
#[derive(Clone, Debug, Default, Parser)]
#[command(next_help_heading = "EIP-8130 (account abstraction)")]
pub struct Eip8130Opts {
    /// Send an EIP-8130 (account abstraction, type `0x79`) transaction.
    ///
    /// Selects the `0x79` transaction type. Incompatible with `--legacy`, `--blob`,
    /// `--auth` and `--create`.
    #[arg(
        long = "8130",
        conflicts_with_all = &["legacy", "blob", "auth"],
        help_heading = "EIP-8130 (account abstraction)"
    )]
    pub eip8130: bool,

    /// EIP-8130 nonce key: the channel selector of the 2D nonce.
    ///
    /// `0` (default) is the standard sequential channel; `1..=NONCE_KEY_MAX-1` are
    /// independent parallel channels; `2^256-1` selects the expiring/nonce-free channel
    /// (which requires `--expiry` and forbids `--nonce-seq`).
    #[arg(long = "nonce-key", requires = "eip8130", value_name = "UINT256")]
    pub nonce_key: Option<U256>,

    /// EIP-8130 nonce sequence: the counter within `--nonce-key`.
    ///
    /// If omitted, it is resolved from the node via `eth_getTransactionCount(from, "pending",
    /// nonceKey)`.
    #[arg(long = "nonce-seq", requires = "eip8130", value_name = "U64")]
    pub nonce_seq: Option<u64>,

    /// EIP-8130 expiry: unix-seconds timestamp after which the transaction is invalid.
    ///
    /// `0` (default) means no expiry. Required (non-zero) in the expiring/nonce-free channel.
    #[arg(long = "expiry", requires = "eip8130", value_name = "UNIX_SECS")]
    pub expiry: Option<u64>,

    /// EIP-8130 opaque metadata bytes (hex). Committed to by the signature but otherwise
    /// uninterpreted by the protocol
    #[arg(long = "metadata", requires = "eip8130", value_name = "HEX")]
    pub metadata: Option<Bytes>,

    /// EIP-8130 calls, as JSON, for native multicall / multi-phase transactions.
    ///
    /// The value is an array of phases; each phase is an array of calls; each call is
    /// `{ "to": address, "value"?: string, "sig"?: string, "args"?: [..], "data"?: hex }`.
    /// Phases execute atomically and in order. A call may specify either `sig`(+`args`) or
    /// raw `data`. Any call with a non-zero `value` causes its whole phase to be wrapped in
    /// a `DEFAULT_ACCOUNT.executeBatch` self-call so the value is forwarded by the wallet.
    ///
    /// When omitted, the positional `TO SIG ARGS` (with `--value`) form a single call.
    ///
    /// Example:
    ///   --calls '[[{"to":"0xA","sig":"approve(address,uint256)","args":["0xB","100"]},
    ///             {"to":"0xB","value":"0.1ether","sig":"deposit()"}]]'
    #[arg(long = "calls", requires = "eip8130", value_name = "JSON")]
    pub calls: Option<String>,
}
