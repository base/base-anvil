//! # foundry-evm-networks
//!
//! Foundry EVM network configuration.

use crate::celo::transfer::{
    CELO_TRANSFER_ADDRESS, CELO_TRANSFER_LABEL, PRECOMPILE_ID_CELO_TRANSFER,
};
use alloy_chains::{
    NamedChain,
    NamedChain::{Chiado, Gnosis, Moonbase, Moonbeam, MoonbeamDev, Moonriver, Rsk, RskTestnet},
};
use alloy_eips::eip1559::BaseFeeParams;
use alloy_evm::precompiles::PrecompilesMap;
use alloy_op_hardforks::{OpChainHardforks, OpHardforks};
use alloy_primitives::{Address, U256, address, keccak256, map::AddressHashMap};
use base_common_chains::BaseUpgrade;
use base_common_precompiles::{
    ActivationAdminConfig, ActivationFeature, ActivationRegistry, ActivationRegistryStorage,
    B20Factory, B20FactoryStorage, BerylLookup, NonceManager, NonceManagerStorage,
    NoopPrecompileCallObserver, PolicyRegistryPrecompile, PolicyRegistryStorage, TxContext,
    TxContextStorage,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The Base precompile singleton addresses, sourced from base_common_precompiles
/// re-exports so the trace labels and warming list never drift from the actual
/// install addresses in base/base.
const BASE_TOKEN_FACTORY_ADDRESS: Address = B20FactoryStorage::ADDRESS;
const BASE_POLICY_REGISTRY_ADDRESS: Address = PolicyRegistryStorage::ADDRESS;
const BASE_ACTIVATION_REGISTRY_ADDRESS: Address = ActivationRegistryStorage::ADDRESS;
// Cobalt-and-later EIP-8130 singletons, installed only when the source-pinned
// `BASE_PRECOMPILE_UPGRADE` is Cobalt or later (mirrors the `upgrade >= Cobalt`
// arm of `BasePrecompiles::install`).
const BASE_TX_CONTEXT_ADDRESS: Address = TxContextStorage::ADDRESS;
const BASE_NONCE_MANAGER_ADDRESS: Address = NonceManagerStorage::ADDRESS;

/// Singleton Base precompile addresses to pre-warm with sentinel bytecode so that
/// Solidity high-level wrapper calls survive the codegen `extcodesize(target) > 0`
/// check. B-20 token addresses (every `0xb2..` address claimed by the prefix
/// dispatcher) are NOT pre-warmed here — tests that touch concrete B-20 tokens
/// must etch the same sentinel themselves, since the address space is unbounded.
const BASE_PRECOMPILE_SENTINEL_ADDRESSES: &[Address] =
    &[BASE_TOKEN_FACTORY_ADDRESS, BASE_POLICY_REGISTRY_ADDRESS, BASE_ACTIVATION_REGISTRY_ADDRESS];

/// Cobalt-and-later singleton sentinel set: the Beryl singletons plus the EIP-8130
/// TxContext + NonceManager precompiles installed when `BASE_PRECOMPILE_UPGRADE`
/// pins Cobalt or later.
const BASE_COBALT_PRECOMPILE_SENTINEL_ADDRESSES: &[Address] = &[
    BASE_TOKEN_FACTORY_ADDRESS,
    BASE_POLICY_REGISTRY_ADDRESS,
    BASE_ACTIVATION_REGISTRY_ADDRESS,
    BASE_TX_CONTEXT_ADDRESS,
    BASE_NONCE_MANAGER_ADDRESS,
];

/// Default activation admin for the local dev chain, mirroring
/// `BasePrecompiles`'s default in `base/base/crates/common/precompiles/src/provider.rs`
/// (committed in base/base PR #2811). Override at the CLI with
/// `--base-activation-admin <address>` for real-chain forks where the
/// activation admin is a deployed account.
const DEFAULT_BASE_ACTIVATION_ADMIN: Address =
    address!("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc");

/// The Base upgrade (fork) whose precompile set `--base` installs, pinned at
/// compile time. Per the fork-test snapshot strategy (BOP-427 / BOP-453) each
/// snapshot is an immutable base-anvil commit that selects one fork in source —
/// there is deliberately no public runtime `--base-fork` / `--hardfork` selector.
///
/// This snapshot pins Cobalt, so `--base` installs the complete Cobalt precompile
/// set: the Beryl singletons plus the EIP-8130 TxContext + NonceManager, with the
/// ActivationRegistry handed a state-backed admin config. Everything downstream
/// (installer, trace labels, precompile map, sentinel warming) keys off this one
/// constant, so re-pinning a snapshot to another fork (e.g. the Beryl pair) is a
/// single-line change.
const BASE_PRECOMPILE_UPGRADE: BaseUpgrade = BaseUpgrade::Cobalt;

pub mod celo;

#[derive(Clone, Debug, Default, Parser, Copy, Serialize, Deserialize, PartialEq)]
pub struct NetworkConfigs {
    /// Enable Optimism network features.
    #[arg(help_heading = "Networks", long, conflicts_with = "celo")]
    // Skipped from configs (forge) as there is no feature to be added yet.
    #[serde(skip)]
    optimism: bool,
    /// Enable Celo network features.
    #[arg(help_heading = "Networks", long, conflicts_with = "optimism")]
    #[serde(default)]
    celo: bool,
    /// Enable Base custom precompile dispatch (TokenFactory, B-20 tokens,
    /// PolicyRegistry, ActivationRegistry). Required to fork-test against
    /// chains that host Base's Rust precompiles (vibenet, Beryl-or-later
    /// mainnets/testnets).
    #[arg(help_heading = "Networks", long, conflicts_with = "celo")]
    #[serde(default)]
    base: bool,
    /// Override the activation registry admin address. Has no effect unless
    /// `--base` is set. Defaults to the canonical local-dev admin
    /// (`0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc`); override for real-chain
    /// forks where the activation admin is a deployed account.
    #[arg(help_heading = "Networks", long, requires = "base")]
    #[serde(default)]
    base_activation_admin: Option<Address>,
    /// Whether to bypass prevrandao.
    #[arg(skip)]
    #[serde(default)]
    bypass_prevrandao: bool,
}

impl NetworkConfigs {
    pub fn with_optimism() -> Self {
        Self { optimism: true, ..Default::default() }
    }

    pub fn with_celo() -> Self {
        Self { celo: true, ..Default::default() }
    }

    pub fn is_optimism(&self) -> bool {
        self.optimism
    }

    /// Returns the base fee parameters for the configured network.
    ///
    /// For Optimism networks, returns Canyon parameters if the Canyon hardfork is active
    /// at the given timestamp, otherwise returns pre-Canyon parameters.
    pub fn base_fee_params(&self, timestamp: u64) -> BaseFeeParams {
        if self.is_optimism() {
            let op_hardforks = OpChainHardforks::op_mainnet();
            if op_hardforks.is_canyon_active_at_timestamp(timestamp) {
                BaseFeeParams::optimism_canyon()
            } else {
                BaseFeeParams::optimism()
            }
        } else {
            BaseFeeParams::ethereum()
        }
    }

    pub fn bypass_prevrandao(&self, chain_id: u64) -> bool {
        if let Ok(
            Moonbeam | Moonbase | Moonriver | MoonbeamDev | Rsk | RskTestnet | Gnosis | Chiado,
        ) = NamedChain::try_from(chain_id)
        {
            return true;
        }
        self.bypass_prevrandao
    }

    pub fn is_celo(&self) -> bool {
        self.celo
    }

    pub fn is_base(&self) -> bool {
        self.base
    }

    pub fn with_base() -> Self {
        Self { base: true, ..Default::default() }
    }

    /// Returns the activation admin address that will be configured on the
    /// ActivationRegistry precompile when `--base` is set. Falls back to the
    /// default Base activation admin when no override is provided.
    pub fn base_activation_admin(&self) -> Address {
        self.base_activation_admin.unwrap_or(DEFAULT_BASE_ACTIVATION_ADMIN)
    }

    pub fn with_chain_id(mut self, chain_id: u64) -> Self {
        if let Ok(NamedChain::Celo | NamedChain::CeloSepolia) = NamedChain::try_from(chain_id) {
            self.celo = true;
        }
        // Auto-enable Base for Base mainnet (8453), Base Sepolia (84532), and
        // vibenet (84538453). Users targeting other Base-derived chains can
        // pass `--base` explicitly.
        if matches!(chain_id, 8453 | 84532 | 84538453) {
            self.base = true;
        }
        self
    }

    /// Inject precompiles for configured networks.
    pub fn inject_precompiles(self, precompiles: &mut PrecompilesMap) {
        if self.celo {
            precompiles.apply_precompile(&CELO_TRANSFER_ADDRESS, move |_| {
                Some(celo::transfer::precompile())
            });
        }
        if self.base {
            // Mirrors `BasePrecompiles::install_with_observer` in
            // base/base/crates/common/precompiles/src/provider.rs for the
            // source-pinned `BASE_PRECOMPILE_UPGRADE` (this snapshot: Cobalt).
            let admin = Some(self.base_activation_admin());
            let upgrade = BASE_PRECOMPILE_UPGRADE;
            B20Factory::install_with_observer(precompiles, upgrade, NoopPrecompileCallObserver);
            BerylLookup::install(precompiles, upgrade);
            PolicyRegistryPrecompile::install(precompiles, upgrade);
            ActivationRegistry::install_with_config(
                precompiles,
                ActivationAdminConfig::new(admin, upgrade >= BaseUpgrade::Cobalt),
            );
            if upgrade >= BaseUpgrade::Cobalt {
                TxContext::install(precompiles);
                NonceManager::install(precompiles);
            }
        }
    }

    /// Returns precompiles label for configured networks, to be used in traces.
    pub fn precompiles_label(self) -> AddressHashMap<String> {
        let mut labels = AddressHashMap::default();
        if self.celo {
            labels.insert(CELO_TRANSFER_ADDRESS, CELO_TRANSFER_LABEL.to_string());
        }
        if self.base {
            labels.insert(BASE_TOKEN_FACTORY_ADDRESS, "BaseTokenFactory".to_string());
            labels.insert(BASE_POLICY_REGISTRY_ADDRESS, "BasePolicyRegistry".to_string());
            labels.insert(ActivationRegistryStorage::ADDRESS, "BaseActivationRegistry".to_string());
            if BASE_PRECOMPILE_UPGRADE >= BaseUpgrade::Cobalt {
                labels.insert(BASE_TX_CONTEXT_ADDRESS, "BaseTxContext".to_string());
                labels.insert(BASE_NONCE_MANAGER_ADDRESS, "BaseNonceManager".to_string());
            }
        }
        labels
    }

    /// Returns precompiles for configured networks.
    pub fn precompiles(self) -> BTreeMap<String, Address> {
        let mut precompiles = BTreeMap::new();
        if self.celo {
            precompiles
                .insert(PRECOMPILE_ID_CELO_TRANSFER.name().to_string(), CELO_TRANSFER_ADDRESS);
        }
        if self.base {
            precompiles.insert("BaseTokenFactory".to_string(), BASE_TOKEN_FACTORY_ADDRESS);
            precompiles.insert("BasePolicyRegistry".to_string(), BASE_POLICY_REGISTRY_ADDRESS);
            precompiles
                .insert("BaseActivationRegistry".to_string(), ActivationRegistryStorage::ADDRESS);
            if BASE_PRECOMPILE_UPGRADE >= BaseUpgrade::Cobalt {
                precompiles.insert("BaseTxContext".to_string(), BASE_TX_CONTEXT_ADDRESS);
                precompiles.insert("BaseNonceManager".to_string(), BASE_NONCE_MANAGER_ADDRESS);
            }
        }
        precompiles
    }

    /// Returns the static list of Base singleton precompile addresses that the
    /// executor pre-warms with sentinel bytecode. B-20 token addresses are
    /// intentionally excluded because they are handled by the shared prefix
    /// dispatcher instead of individual singleton sentinels.
    pub fn base_precompile_sentinel_addresses(&self) -> &'static [Address] {
        if !self.base {
            return &[];
        }
        if BASE_PRECOMPILE_UPGRADE >= BaseUpgrade::Cobalt {
            BASE_COBALT_PRECOMPILE_SENTINEL_ADDRESSES
        } else {
            BASE_PRECOMPILE_SENTINEL_ADDRESSES
        }
    }

    /// `(address, slot, value)` writes that mark Base's activation-gated
    /// features active, so local `forge test --base` (no fork) matches a live
    /// Beryl+ chain instead of reverting `FeatureNotActivated`. Empty unless
    /// `--base` is set.
    ///
    /// Slot derivation MUST stay in lockstep with the `features` mapping in
    /// base/base `precompiles/src/activation/storage.rs`. That mapping lives at
    /// the ERC-7201 root of namespace `"base.activation_registry"` (base/base's
    /// `activation_registry_namespace_matches_base_std_root` test pins this root
    /// to `0x43ee..cce00`), and each feature flag sits at the Solidity mapping
    /// slot `keccak256(feature_id ‖ root)`.
    pub fn base_activation_seeds(&self) -> Vec<(Address, U256, U256)> {
        if !self.base {
            return Vec::new();
        }
        // ERC-7201 root: keccak256(abi.encode(uint256(keccak256(id)) - 1)) & ~0xff.
        let id_hash = U256::from_be_bytes(keccak256("base.activation_registry").0);
        let root = (U256::from_be_bytes(keccak256((id_hash - U256::ONE).to_be_bytes::<32>()).0)
            & !U256::from(0xffu64))
        .to_be_bytes::<32>();
        [
            ActivationFeature::B20Asset,
            ActivationFeature::B20Stablecoin,
            ActivationFeature::PolicyRegistry,
        ]
        .into_iter()
        .map(|feature| {
            // Solidity mapping slot: keccak256(lpad32(key) ‖ base_slot); key and root are 32-byte
            // words.
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(feature.id().as_slice());
            buf[32..].copy_from_slice(&root);
            let slot = U256::from_be_bytes(keccak256(buf).0);
            (ActivationRegistryStorage::ADDRESS, slot, U256::ONE)
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_pins_cobalt() {
        // This base-anvil snapshot is the Cobalt fork-test pair (BOP-453). If the
        // pin is changed, the install set below and the paired base-std suite must
        // be re-reviewed together, so fail loudly rather than silently dispatching
        // a different fork.
        assert_eq!(BASE_PRECOMPILE_UPGRADE, BaseUpgrade::Cobalt);
    }

    #[test]
    fn base_installs_complete_cobalt_precompile_set() {
        let cfg = NetworkConfigs::with_base();

        // The Cobalt sentinel set is the three Beryl singletons plus the EIP-8130
        // TxContext + NonceManager, matching the `upgrade >= Cobalt` arm of
        // `BasePrecompiles::install`.
        let sentinels = cfg.base_precompile_sentinel_addresses();
        assert_eq!(sentinels.len(), 5);
        assert!(sentinels.contains(&BASE_TX_CONTEXT_ADDRESS));
        assert!(sentinels.contains(&BASE_NONCE_MANAGER_ADDRESS));

        let labels = cfg.precompiles_label();
        assert_eq!(labels.get(&BASE_TX_CONTEXT_ADDRESS).map(String::as_str), Some("BaseTxContext"));
        assert_eq!(
            labels.get(&BASE_NONCE_MANAGER_ADDRESS).map(String::as_str),
            Some("BaseNonceManager")
        );

        let precompiles = cfg.precompiles();
        assert_eq!(precompiles.get("BaseTxContext"), Some(&BASE_TX_CONTEXT_ADDRESS));
        assert_eq!(precompiles.get("BaseNonceManager"), Some(&BASE_NONCE_MANAGER_ADDRESS));

        // The activation seeds still cover exactly the three gated feature flags.
        assert_eq!(cfg.base_activation_seeds().len(), 3);
    }

    #[test]
    fn without_base_installs_nothing() {
        let cfg = NetworkConfigs::default();
        assert!(cfg.base_precompile_sentinel_addresses().is_empty());
        assert!(cfg.base_activation_seeds().is_empty());
        assert!(!cfg.precompiles_label().contains_key(&BASE_TX_CONTEXT_ADDRESS));
        assert!(!cfg.precompiles().contains_key("BaseTxContext"));
    }
}
