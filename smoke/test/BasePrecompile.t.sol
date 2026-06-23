// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IActivationRegistry {
    function isActivated(bytes32 feature) external view returns (bool);
}

/// @notice Proof that a base-anvil binary actually installed and seeded the Base
/// precompiles. Run via `base-forge test` (Base enabled by default) or
/// `forge test --base`.
///
/// The address and feature ids are the real Base values:
///   - ActivationRegistry address: base-std `StdPrecompiles.sol`.
///   - feature ids: base-std `ActivationRegistryFeatureList.sol`, which match
///     the three features base-anvil seeds active in
///     `crates/evm/networks/src/lib.rs` (`base_activation_seeds()`).
///
/// WITHOUT --base the address is an empty account, `isActivated()` returns
/// false, and this test FAILS: the false-pass becomes a visible failure.
/// WITH --base the precompile is registered and seeded, so it returns true.
contract BasePrecompileSmokeTest {
    IActivationRegistry constant ACTIVATION_REGISTRY =
        IActivationRegistry(0x8453000000000000000000000000000000000001);

    // keccak256("base.b20_asset")
    bytes32 constant B20_ASSET = 0xcdcc772fe4cbdb1029f822861176d09e646db96723d4c1e82ddfdeb8163ef54c;
    // keccak256("base.b20_stablecoin")
    bytes32 constant B20_STABLECOIN = 0xecfa0def2c10020caaf65e6155aa69c84b24892aaef76eeac52e0e2b3a0b8601;
    // keccak256("base.policy_registry")
    bytes32 constant POLICY_REGISTRY = 0xb582ebae03f16fee49a6763f78df482fb11ae73f103ed0d330bbe556aa90a43f;

    function test_basePrecompileSeededActive() external view {
        require(
            ACTIVATION_REGISTRY.isActivated(B20_ASSET),
            "B20_ASSET inactive: --base not applied or precompile missing"
        );
        require(
            ACTIVATION_REGISTRY.isActivated(B20_STABLECOIN),
            "B20_STABLECOIN inactive: --base not applied or precompile missing"
        );
        require(
            ACTIVATION_REGISTRY.isActivated(POLICY_REGISTRY),
            "POLICY_REGISTRY inactive: --base not applied or precompile missing"
        );
    }
}
