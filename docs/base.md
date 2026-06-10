# Testing your Base app with base-anvil

`base-anvil` is a build of Foundry (`forge`, `anvil`, `cast`, `chisel`) that
understands Base's native precompiles: the B-20 token factory, B-20 tokens, the
PolicyRegistry, and the ActivationRegistry. It lets you build and test Base apps
against real precompile behavior, locally, without a live network.

## Install

```bash
curl -L https://raw.githubusercontent.com/base/base-anvil/HEAD/foundryup/install | bash
foundryup --network base
```

`foundryup --network base` installs the Base build of `forge`/`cast`/`anvil`/`chisel`
into a dedicated slot (`~/.foundry/versions/base-nightly`) that does not collide
with a stock Foundry install, so you can keep both. Re-run it any time to update.
Each build is reproduced from a specific `base/base` commit; `anvil --version`
prints that commit.

## Add the Base interfaces

```bash
forge install base/base-std
```

[base-std](https://github.com/base/base-std) provides the Solidity handles for the
precompiles: `StdPrecompiles.sol` plus `IB20`, `IB20Factory`, `IPolicyRegistry`,
and `IActivationRegistry`.

## Enable Base precompiles

Add a profile to your `foundry.toml`:

```toml
[profile.base]
base = true
```

`base = true` switches forge's EVM to dispatch Base precompile calls to the native
implementations. Run your tests against it:

```bash
FOUNDRY_PROFILE=base forge test
```

No node and no fork required: the precompiles run in-process. For an interactive
local node with the precompiles live, run:

```bash
anvil --base
```

Your tests now exercise the same precompile behavior they would hit on Base.
