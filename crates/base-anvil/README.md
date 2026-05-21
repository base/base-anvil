# `base-anvil`

Anvil with Base's custom precompiles (TokenFactory, B-20 tokens, PolicyRegistry,
ActivationRegistry) registered into the EVM. Drop-in replacement for `anvil` for
Foundry tests that need to fork-test against Base chains where those precompiles
live natively (vibenet, Beryl-or-later mainnets/testnets).

## Why this exists

Stock Foundry's REVM only knows the standard EVM precompiles (`0x01..0x09` + KZG
+ Prague BLS12-381). It has no dispatch entry for Base's custom precompile
addresses (`0xb20F...000f`, `0xb000...0001`, `0x8453...00ff`), nor for the
prefix-based B-20 token dispatcher that claims every address with the `0xb2`
prefix. So `forge test --fork-url <a-base-chain>` silently routes every
precompile call into the EVM's empty-address behavior (success, zero return
data), giving false-pass results.

`base-anvil` is a tiny fork of foundry-rs/foundry that pulls in
`base-common-precompiles` from `base/base` and registers it into Anvil's EVM
behind a single CLI flag. Local node, real precompile dispatch, unmodified
Foundry test suite.

## What's different from upstream foundry

Two files in the foundry tree are patched (all changes backwards-compatible —
running this fork with no extra flags behaves identically to stock anvil):

- `crates/anvil/src/evm.rs`: `PrecompileFactory` trait gains an `install(&mut
  PrecompilesMap)` method with a default implementation that preserves the
  existing `precompiles() -> Vec<(Address, DynPrecompile)>` behavior.
  Implementors can override it to use `PrecompilesMap::set_precompile_lookup`
  for prefix-based dispatch (the B-20 pattern).
- `crates/anvil/src/eth/backend/{executor.rs, mem/mod.rs}`: the two real EVM
  injection sites call `factory.install(precompiles)` instead of
  `precompiles.extend_precompiles(factory.precompiles())`. The third call site
  in `mem/mod.rs` (a name-to-address registry used for diagnostics) is left
  unchanged — it doesn't apply to dynamic dispatch.

Plus the new `crates/base-anvil/` binary crate, this README, and Cargo.toml
version bumps to match base/base's dependency versions (alloy-primitives 1.5.6,
alloy-evm 0.27.2; revm stays at 34.0.0 which already matches base/base).

## Building

This fork uses path-dep against your local `base/base` clone. Default layout
assumes both repos are siblings:

```
~/code/
├── base/         ← github.com/base/base, on a B-20-containing branch (e.g. activation-address-devnet)
└── base-anvil/   ← this repo
```

If your `base/` clone lives elsewhere, edit the `path` lines in
`crates/base-anvil/Cargo.toml` accordingly.

Prerequisites (one-time):

```bash
# Rust toolchain (rustup auto-installs the toolchain pinned by foundry's MSRV)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal

# Fast linker (macOS — Linux uses mold per .cargo/config.toml)
brew install lld
```

Then:

```bash
# Make sure your base/ clone is on a branch with the B-20 precompile code:
cd ../base && git checkout activation-address-devnet && cd -

# Build (first build takes ~15-30 min; subsequent are incremental)
cargo build -p base-anvil --release

# Binary lands at target/release/base-anvil
./target/release/base-anvil --help
```

## Usage

To run Anvil exactly like upstream:

```bash
base-anvil
base-anvil --fork-url https://eth.merkle.io
# ...any other anvil flag
```

To enable Base's custom precompiles:

```bash
base-anvil --base-precompiles --activation-admin 0xYourAdminAddress
```

Or via env vars:

```bash
BASE_PRECOMPILES=true ACTIVATION_ADMIN=0xYourAdminAddress base-anvil
```

When the flag is set, calls to `0xb20F...000f` (TokenFactory), `0xb000...0001`
(PolicyRegistry), `0x8453...00ff` (ActivationRegistry), and any address with
the `0xb2` prefix (B-20 tokens) dispatch through `base-common-precompiles`.
Standard EVM precompiles still work unchanged.

## Using with base-std fork tests

After `cargo build -p base-anvil --release`, run base-anvil locally:

```bash
./target/release/base-anvil \
  --base-precompiles \
  --activation-admin 0xYourAdminAddress \
  --port 8545
```

In a second terminal, point base-std's fork test invocation at it:

```bash
cd /path/to/base-std
LIVE_PRECOMPILES=true FOUNDRY_PROFILE=fork forge test --fork-url http://localhost:8545
```

The `LIVE_PRECOMPILES=true` env var tells base-std's `BaseTest.setUp` to skip
its `vm.etch` of the mock precompiles, so calls dispatch to the precompiles
your local base-anvil is now serving.

To activate gated features (e.g. the TokenFactory feature, which reverts
`FeatureNotActivated(bytes32)` until activated), send an `activate(bytes32)`
transaction to the ActivationRegistry from the admin address. See
`base/devnet/src/b20.rs` for a Rust client that automates the activation
sequence.

## Maintenance

- This fork lives on the `base-anvil-fork` branch off foundry-rs's `v1.6.0-rc1`
  tag. To rebase onto a newer foundry tag, `git rebase` onto the tag and
  re-apply any patches that conflict (the changes are small and localized).
- When `base/base` bumps its revm/alloy-evm versions, bump
  `Cargo.toml` here to match. Comments at each version line document the
  upstream that was rebased onto.
- If `PrecompileFactory::install` lands upstream in foundry-rs, drop the local
  trait patch and use the upstream version.

## Known limitations

- No CI for this fork yet. Builds are local-only.
- The shutdown handler is minimal (ctrl-c only); upstream anvil's
  `NodeArgs::run` also handles SIGTERM, periodic state dumps, and other
  niceties. Add back as needed.
- Path dependency on `base/base` means CI would need to also clone `base/base`
  at a compatible commit, or this Cargo.toml needs to switch to a `git = "..."`
  dep with a pinned commit hash.
