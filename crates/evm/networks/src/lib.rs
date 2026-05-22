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
use alloy_primitives::{Address, address, map::AddressHashMap};
use alloy_evm::precompiles::DynPrecompile;
use base_common_chains::BaseUpgrade;
use base_common_precompiles::{
    ActivationRegistry, ActivationRegistryStorage, B20SecurityPrecompile,
    B20StablecoinPrecompile, B20TokenPrecompile, PolicyRegistryPrecompile,
    PolicyRegistryStorage, TokenFactory, TokenFactoryStorage, TokenVariant,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The Base precompile singleton addresses, sourced from base_common_precompiles
/// re-exports so the trace labels and warming list never drift from the actual
/// install addresses in base/base.
const BASE_TOKEN_FACTORY_ADDRESS: Address = TokenFactoryStorage::ADDRESS;
const BASE_POLICY_REGISTRY_ADDRESS: Address = PolicyRegistryStorage::ADDRESS;
const BASE_ACTIVATION_REGISTRY_ADDRESS: Address = ActivationRegistryStorage::ADDRESS;

/// Singleton Base precompile addresses to pre-warm with sentinel bytecode so that
/// Solidity high-level wrapper calls survive the codegen `extcodesize(target) > 0`
/// check. B-20 token addresses (every `0xb2..` address claimed by the prefix
/// dispatcher) are NOT pre-warmed here — tests that touch concrete B-20 tokens
/// must etch the same sentinel themselves, since the address space is unbounded.
const BASE_PRECOMPILE_SENTINEL_ADDRESSES: &[Address] = &[
    BASE_TOKEN_FACTORY_ADDRESS,
    BASE_POLICY_REGISTRY_ADDRESS,
    BASE_ACTIVATION_REGISTRY_ADDRESS,
];

/// Default activation admin for the local dev chain, mirroring
/// `BasePrecompiles`'s default in `base/base/crates/common/precompiles/src/provider.rs`
/// (committed in base/base PR #2811). Override at the CLI with
/// `--base-activation-admin <address>` for real-chain forks where the
/// activation admin is a deployed account.
const DEFAULT_BASE_ACTIVATION_ADMIN: Address =
    address!("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc");

/// Combined B-20 prefix-dispatch lookup for all token variants.
///
/// Replicates the private `b20_token_lookup` in
/// `base/base/crates/common/precompiles/src/provider.rs`. A single named
/// function is required because `set_precompile_lookup` takes a
/// function pointer (not a closure) AND replaces rather than chains
/// successive lookups, so we must dispatch all B-20 variants in one
/// match.
fn b20_token_lookup(address: &Address) -> Option<DynPrecompile> {
    match TokenVariant::from_address(*address)? {
        TokenVariant::B20 => Some(B20TokenPrecompile::create_precompile(*address)),
        TokenVariant::Stablecoin => Some(B20StablecoinPrecompile::create_precompile(*address)),
        TokenVariant::Security => Some(B20SecurityPrecompile::create_precompile(*address)),
    }
}

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
    /// ActivationRegistry precompile when `--base` is set. Falls back to
    /// [`DEFAULT_BASE_ACTIVATION_ADMIN`] when no override is provided.
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
            // Mirrors `BasePrecompiles::install` for the Beryl upgrade in
            // base/base/crates/common/precompiles/src/provider.rs. Three
            // singleton precompiles plus a single combined B-20 prefix
            // dispatcher.
            // `set_precompile_lookup` replaces rather than chains, so the
            // combined `b20_token_lookup` covers all variants in one shot.
            let admin = Some(self.base_activation_admin());
            TokenFactory::install(precompiles);
            precompiles.set_precompile_lookup(b20_token_lookup);
            PolicyRegistryPrecompile::install(precompiles);
            ActivationRegistry::install(precompiles, admin);
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
            labels.insert(
                ActivationRegistryStorage::ADDRESS,
                "BaseActivationRegistry".to_string(),
            );
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
        }
        precompiles
    }

    /// Suppress the [`BaseUpgrade`] dead-code lint until additional code paths
    /// consume the upgrade discriminator (e.g. to gate older Base hardforks
    /// out of the B-20 precompile install). Kept as a hook because every
    /// `BasePrecompiles::new_with_spec` call site in base/base takes an upgrade.
    #[doc(hidden)]
    pub fn _base_upgrade_hint() -> BaseUpgrade {
        BaseUpgrade::Beryl
    }

    /// Returns the static list of Base singleton precompile addresses that the
    /// executor pre-warms with sentinel bytecode. See the docstring on
    /// [`BASE_PRECOMPILE_SENTINEL_ADDRESSES`] for the scope and why B-20 tokens
    /// are intentionally excluded.
    pub fn base_precompile_sentinel_addresses(&self) -> &'static [Address] {
        if self.base { BASE_PRECOMPILE_SENTINEL_ADDRESSES } else { &[] }
    }
}
