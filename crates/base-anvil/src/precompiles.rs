//! [`PrecompileFactory`] implementation that installs Base's custom precompiles
//! (TokenFactory, B-20 tokens, PolicyRegistry, ActivationRegistry) into Anvil's EVM.
//!
//! The four Base precompiles are exposed by `base-common-precompiles` as four
//! independent installers that each call into a [`PrecompilesMap`]. Three of
//! them (TokenFactory, PolicyRegistry, ActivationRegistry) are singletons at
//! fixed addresses; one of them (B-20 token) is a prefix-based dispatcher that
//! claims every address with the `0xb2` prefix via
//! [`PrecompilesMap::set_precompile_lookup`].
//!
//! Anvil's stock [`PrecompileFactory::precompiles`] surface only handles static
//! `(Address, DynPrecompile)` pairs, which is insufficient for prefix dispatch.
//! base-anvil's fork extends the trait with an [`PrecompileFactory::install`]
//! method whose default implementation preserves the old behavior; we override
//! it to call each Base installer directly on the precompiles map.

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use alloy_primitives::Address;
use anvil::PrecompileFactory;
use base_common_chains::BaseUpgrade;
use base_common_precompiles::{
    ActivationRegistry, B20TokenPrecompile, PolicyRegistryPrecompile, TokenFactory,
};

/// Installs Base's custom precompiles (B-20, factory, policy, activation)
/// into Anvil's EVM at the canonical addresses.
#[derive(Debug, Clone)]
pub struct BasePrecompileFactory {
    /// The Base upgrade whose precompile set to install. Use
    /// [`BaseUpgrade::Beryl`] (or later) to include the B-20 precompile suite;
    /// earlier upgrades only install the standard EVM precompile evolutions
    /// (BLS12-381, BN254 pairing, P256, etc.) and are effectively no-ops here
    /// because Anvil already configures those via its standard hardfork wiring.
    pub upgrade: BaseUpgrade,
    /// The address authorized to mutate feature flags on the ActivationRegistry
    /// precompile. Pass `None` to leave the activation admin unset (every
    /// feature stays inactive and the registry rejects all `activate` /
    /// `deactivate` calls).
    pub activation_admin: Option<Address>,
}

impl BasePrecompileFactory {
    /// Constructs a factory pinned to a specific Base upgrade and activation admin.
    pub fn new(upgrade: BaseUpgrade, activation_admin: Option<Address>) -> Self {
        Self { upgrade, activation_admin }
    }
}

impl PrecompileFactory for BasePrecompileFactory {
    /// Static precompile list is empty — all Base precompiles install via
    /// [`Self::install`] below so the prefix-based B-20 dispatcher can use
    /// [`PrecompilesMap::set_precompile_lookup`].
    fn precompiles(&self) -> Vec<(Address, DynPrecompile)> {
        Vec::new()
    }

    /// Installs each Base precompile into the precompiles map.
    ///
    /// Only registers the B-20 precompile suite (TokenFactory, B-20 tokens,
    /// PolicyRegistry, ActivationRegistry) when `upgrade >= Beryl`, which
    /// mirrors the gating in `BasePrecompiles::install` from base/base.
    fn install(&self, precompiles: &mut PrecompilesMap) {
        if self.upgrade >= BaseUpgrade::Beryl {
            TokenFactory::install(precompiles);
            B20TokenPrecompile::install(precompiles);
            PolicyRegistryPrecompile::install(precompiles);
            ActivationRegistry::install(precompiles, self.activation_admin);
        }
    }
}
