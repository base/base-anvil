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
pub use base_common_chains::BaseUpgrade;
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
const BASE_TX_CONTEXT_ADDRESS: Address = TxContextStorage::ADDRESS;
const BASE_NONCE_MANAGER_ADDRESS: Address = NonceManagerStorage::ADDRESS;

/// Singleton Base precompile addresses to pre-warm with sentinel bytecode so that
/// Solidity high-level wrapper calls survive the codegen `extcodesize(target) > 0`
/// check. B-20 token addresses (every `0xb2..` address claimed by the prefix
/// dispatcher) are NOT pre-warmed here — tests that touch concrete B-20 tokens
/// must etch the same sentinel themselves, since the address space is unbounded.
const BASE_PRECOMPILE_SENTINEL_ADDRESSES: &[Address] = &[
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

/// base-anvil's own current default Base upgrade — the upgrade registered when
/// `--base` is passed with no value. Deliberately not tied to upstream
/// `BaseUpgrade::LATEST` (`Beryl`), which lags base-anvil's own release
/// cadence; bump this alongside `./script/bump-base.sh` as new upgrades land.
const DEFAULT_BASE_UPGRADE: BaseUpgrade = BaseUpgrade::Cobalt;

pub mod celo;

/// Serde support for the `base` field that keeps accepting the historical
/// `base = true`/`base = false` shape in `foundry.toml`, while also accepting
/// `base = "beryl"` to pin a specific historical upgrade. Mirrors the CLI's
/// `--base`/`--base <upgrade>` duality (see the `base` field below).
mod base_flag {
    use super::{BaseUpgrade, DEFAULT_BASE_UPGRADE};
    use serde::{Deserialize, Deserializer, Serializer};
    use std::str::FromStr;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Bool(bool),
        Name(String),
    }

    pub fn serialize<S: Serializer>(
        value: &Option<BaseUpgrade>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            None => s.serialize_bool(false),
            Some(upgrade) => s.serialize_str(&upgrade.to_string()),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<BaseUpgrade>, D::Error> {
        match Repr::deserialize(d)? {
            Repr::Bool(false) => Ok(None),
            Repr::Bool(true) => Ok(Some(DEFAULT_BASE_UPGRADE)),
            Repr::Name(s) => BaseUpgrade::from_str(&s).map(Some).map_err(serde::de::Error::custom),
        }
    }
}

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
    /// PolicyRegistry, ActivationRegistry, TxContext, NonceManager), optionally
    /// pinned to a specific historical Base upgrade. Bare `--base` uses
    /// base-anvil's current default upgrade; `--base <upgrade>` (e.g. `--base
    /// beryl`) selects a specific past upgrade, so one base-anvil revision can
    /// test the latest `base` precompile implementation against every frozen
    /// base-std hardfork suite. Required to fork-test against chains that host
    /// Base's Rust precompiles (vibenet, Cobalt-or-later mainnets/testnets).
    #[arg(
        help_heading = "Networks",
        long,
        conflicts_with = "celo",
        num_args(0..=1),
        default_missing_value = "cobalt", // keep in sync with DEFAULT_BASE_UPGRADE
    )]
    #[serde(default, with = "base_flag")]
    base: Option<BaseUpgrade>,
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
        self.base.is_some()
    }

    /// Returns the specific Base upgrade selected via `--base <upgrade>`, or
    /// `None` if `--base` was not passed at all.
    pub fn base_upgrade(&self) -> Option<BaseUpgrade> {
        self.base
    }

    pub fn with_base() -> Self {
        Self { base: Some(DEFAULT_BASE_UPGRADE), ..Default::default() }
    }

    /// Like [`Self::with_base`], but pins a specific historical Base upgrade
    /// instead of `DEFAULT_BASE_UPGRADE`.
    pub fn with_base_upgrade(upgrade: BaseUpgrade) -> Self {
        Self { base: Some(upgrade), ..Default::default() }
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
        // pass `--base` explicitly. `.or(..)` preserves an explicit `--base
        // <upgrade>` selection instead of clobbering it with the default.
        if matches!(chain_id, 8453 | 84532 | 84538453) {
            self.base = self.base.or(Some(DEFAULT_BASE_UPGRADE));
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
        if let Some(upgrade) = self.base {
            // Mirrors `BasePrecompiles::install_with_observer` for the Cobalt
            // upgrade in base/base/crates/common/precompiles/src/provider.rs.
            // The Beryl-and-later singletons plus the versioned B-20 prefix
            // dispatcher, and the Cobalt-and-later precompiles below.
            // `BerylLookup::install` owns the dispatcher and threads the Base
            // upgrade so each B-20 token resolves its logic version per-call.
            // The upgrade is selected via `--base <upgrade>`, defaulting to
            // `DEFAULT_BASE_UPGRADE` when `--base` is passed bare.
            //
            // Factory/policy take a no-op observer: metrics observation is
            // scoped to the B-20 token call path, which anvil does not wire up.
            let admin = Some(self.base_activation_admin());
            B20Factory::install_with_observer(precompiles, upgrade, NoopPrecompileCallObserver);
            BerylLookup::install(precompiles, upgrade);
            PolicyRegistryPrecompile::install(precompiles, upgrade);
            // Cobalt moves the activation registry onto its state-backed admin
            // path (the admin storage slot, falling back to the static config
            // admin while unset), matching `ActivationAdminConfig::new(admin,
            // upgrade >= Cobalt)` in base/base.
            ActivationRegistry::install_with_config(
                precompiles,
                ActivationAdminConfig::state_backed(admin),
                upgrade,
            );
            // Cobalt adds the transaction-context and nonce-manager singleton
            // precompiles. Neither is observed, by design (metrics are scoped to
            // the B-20 token call path).
            TxContext::install(precompiles);
            NonceManager::install(precompiles);
        }
    }

    /// Returns precompiles label for configured networks, to be used in traces.
    pub fn precompiles_label(self) -> AddressHashMap<String> {
        let mut labels = AddressHashMap::default();
        if self.celo {
            labels.insert(CELO_TRANSFER_ADDRESS, CELO_TRANSFER_LABEL.to_string());
        }
        if self.is_base() {
            labels.insert(BASE_TOKEN_FACTORY_ADDRESS, "BaseTokenFactory".to_string());
            labels.insert(BASE_POLICY_REGISTRY_ADDRESS, "BasePolicyRegistry".to_string());
            labels.insert(ActivationRegistryStorage::ADDRESS, "BaseActivationRegistry".to_string());
            labels.insert(BASE_TX_CONTEXT_ADDRESS, "BaseTxContext".to_string());
            labels.insert(BASE_NONCE_MANAGER_ADDRESS, "BaseNonceManager".to_string());
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
        if self.is_base() {
            precompiles.insert("BaseTokenFactory".to_string(), BASE_TOKEN_FACTORY_ADDRESS);
            precompiles.insert("BasePolicyRegistry".to_string(), BASE_POLICY_REGISTRY_ADDRESS);
            precompiles
                .insert("BaseActivationRegistry".to_string(), ActivationRegistryStorage::ADDRESS);
            precompiles.insert("BaseTxContext".to_string(), BASE_TX_CONTEXT_ADDRESS);
            precompiles.insert("BaseNonceManager".to_string(), BASE_NONCE_MANAGER_ADDRESS);
        }
        precompiles
    }

    /// Returns the static list of Base singleton precompile addresses that the
    /// executor pre-warms with sentinel bytecode. B-20 token addresses are
    /// intentionally excluded because they are handled by the shared prefix
    /// dispatcher instead of individual singleton sentinels.
    pub fn base_precompile_sentinel_addresses(&self) -> &'static [Address] {
        if self.is_base() { BASE_PRECOMPILE_SENTINEL_ADDRESSES } else { &[] }
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
        if !self.is_base() {
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
    fn default_missing_value_matches_default_base_upgrade() {
        // Guards against the CLI's `default_missing_value = "cobalt"` string
        // literal drifting from `DEFAULT_BASE_UPGRADE`, since clap can't
        // reference the constant directly.
        assert_eq!("cobalt".parse::<BaseUpgrade>().unwrap(), DEFAULT_BASE_UPGRADE);
    }

    #[test]
    fn base_flag_serde_round_trip() {
        assert_eq!(NetworkConfigs::default().base_upgrade(), None);
        assert_eq!(NetworkConfigs::with_base().base_upgrade(), Some(DEFAULT_BASE_UPGRADE));

        let disabled: NetworkConfigs = serde_json::from_str(r#"{"base": false}"#).unwrap();
        assert_eq!(disabled.base_upgrade(), None);
        assert_eq!(serde_json::to_value(disabled).unwrap()["base"], serde_json::json!(false));

        let default_on: NetworkConfigs = serde_json::from_str(r#"{"base": true}"#).unwrap();
        assert_eq!(default_on.base_upgrade(), Some(DEFAULT_BASE_UPGRADE));

        let pinned: NetworkConfigs = serde_json::from_str(r#"{"base": "beryl"}"#).unwrap();
        assert_eq!(pinned.base_upgrade(), Some(BaseUpgrade::Beryl));
        assert_eq!(serde_json::to_value(pinned).unwrap()["base"], serde_json::json!("Beryl"));
    }

    #[test]
    fn with_chain_id_preserves_explicit_base_upgrade() {
        let networks = NetworkConfigs::with_base_upgrade(BaseUpgrade::Beryl).with_chain_id(8453);
        assert_eq!(networks.base_upgrade(), Some(BaseUpgrade::Beryl));

        let networks = NetworkConfigs::default().with_chain_id(8453);
        assert_eq!(networks.base_upgrade(), Some(DEFAULT_BASE_UPGRADE));
    }
}
