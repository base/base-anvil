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
// Cobalt-and-later EIP-8130 singletons, installed only when `--base-fork` selects
// Cobalt or later (mirrors the `upgrade >= Cobalt` arm of `BasePrecompiles::install`).
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
/// TxContext + NonceManager precompiles that `--base-fork cobalt` installs.
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

pub mod celo;

/// Parses a `--base-fork` flag value (or `base_fork` config key) into a [`BaseUpgrade`].
///
/// Accepts the canonical contract fork names and their aliases
fn parse_base_fork(value: &str) -> Result<BaseUpgrade, String> {
    BaseUpgrade::from_contract_fork_name(value).ok_or_else(|| {
        format!(
            "unknown Base fork '{value}'; expected a Base upgrade name such as 'beryl' or 'cobalt'"
        )
    })
}

/// Serde adapter for the optional `base_fork` config key
mod base_fork_serde {
    use base_common_chains::BaseUpgrade;
    use serde::{Deserialize, Deserializer, Serializer};

    use super::parse_base_fork;

    pub(super) fn serialize<S: Serializer>(
        value: &Option<BaseUpgrade>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(upgrade) => serializer.serialize_some(upgrade.contract_id()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<BaseUpgrade>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|raw| parse_base_fork(&raw).map_err(serde::de::Error::custom))
            .transpose()
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
    /// Select the Base upgrade (fork) whose precompile set `--base` installs. Has no
    /// effect unless `--base` is set. Defaults to Beryl
    #[arg(
        help_heading = "Networks",
        long,
        value_name = "FORK",
        requires = "base",
        value_parser = parse_base_fork
    )]
    #[serde(default, with = "base_fork_serde")]
    base_fork: Option<BaseUpgrade>,
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

    /// Enables `--base` dispatch pinned to a specific Base fork
    pub fn with_base_fork(fork: BaseUpgrade) -> Self {
        Self { base: true, base_fork: Some(fork), ..Default::default() }
    }

    /// The Base upgrade (fork) whose precompile set `--base` installs, defaulting to
    /// Beryl when `--base-fork` is not set.
    pub fn base_upgrade(&self) -> BaseUpgrade {
        self.base_fork.unwrap_or(BaseUpgrade::Beryl)
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
            // fork selected by `--base-fork` (default Beryl).
            let admin = Some(self.base_activation_admin());
            let upgrade = self.base_upgrade();
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
            if self.base_upgrade() >= BaseUpgrade::Cobalt {
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
            if self.base_upgrade() >= BaseUpgrade::Cobalt {
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
        if self.base_upgrade() >= BaseUpgrade::Cobalt {
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
    fn base_fork_defaults_to_beryl() {
        // Existing consumers that only pass `--base` stay on Beryl.
        assert_eq!(NetworkConfigs::default().base_upgrade(), BaseUpgrade::Beryl);
        assert_eq!(NetworkConfigs::with_base().base_upgrade(), BaseUpgrade::Beryl);
    }

    #[test]
    fn with_base_fork_selects_upgrade() {
        assert_eq!(
            NetworkConfigs::with_base_fork(BaseUpgrade::Cobalt).base_upgrade(),
            BaseUpgrade::Cobalt
        );
    }

    #[test]
    fn parse_base_fork_accepts_names_and_aliases() {
        assert_eq!(parse_base_fork("beryl").unwrap(), BaseUpgrade::Beryl);
        assert_eq!(parse_base_fork("cobalt").unwrap(), BaseUpgrade::Cobalt);
        assert_eq!(parse_base_fork("COBALT").unwrap(), BaseUpgrade::Cobalt);
        assert_eq!(parse_base_fork("v3").unwrap(), BaseUpgrade::Cobalt);
        assert!(parse_base_fork("not-a-fork").is_err());
    }

    #[test]
    fn cli_parses_base_fork_and_requires_base() {
        // `--base-fork` is reachable through the shared CLI flatten and parses
        let cfg = NetworkConfigs::try_parse_from(["x", "--base", "--base-fork", "cobalt"]).unwrap();
        assert_eq!(cfg.base_upgrade(), BaseUpgrade::Cobalt);
        // `--base-fork` has no meaning without `--base`.
        assert!(NetworkConfigs::try_parse_from(["x", "--base-fork", "cobalt"]).is_err());
        // Unknown fork names are rejected at parse time.
        assert!(NetworkConfigs::try_parse_from(["x", "--base", "--base-fork", "granate"]).is_err());
    }

    #[test]
    fn cobalt_selection_adds_eip8130_singletons() {
        let beryl = NetworkConfigs::with_base();
        let cobalt = NetworkConfigs::with_base_fork(BaseUpgrade::Cobalt);

        // Beryl exposes the three Beryl singletons; Cobalt adds TxContext + NonceManager
        assert_eq!(beryl.base_precompile_sentinel_addresses().len(), 3);
        let cobalt_sentinels = cobalt.base_precompile_sentinel_addresses();
        assert_eq!(cobalt_sentinels.len(), 5);
        assert!(cobalt_sentinels.contains(&BASE_TX_CONTEXT_ADDRESS));
        assert!(cobalt_sentinels.contains(&BASE_NONCE_MANAGER_ADDRESS));

        assert!(!beryl.precompiles_label().contains_key(&BASE_TX_CONTEXT_ADDRESS));
        assert!(cobalt.precompiles_label().contains_key(&BASE_TX_CONTEXT_ADDRESS));
        assert!(cobalt.precompiles_label().contains_key(&BASE_NONCE_MANAGER_ADDRESS));

        assert!(!beryl.precompiles().contains_key("BaseTxContext"));
        assert!(cobalt.precompiles().contains_key("BaseTxContext"));
        assert!(cobalt.precompiles().contains_key("BaseNonceManager"));
    }
}
