# Testing your Base app with base-anvil

`base-anvil` is a build of Foundry (`forge`, `anvil`, `cast`, `chisel`) that
understands Base's native precompiles: the B-20 token factory, B-20 tokens, the
PolicyRegistry, and the ActivationRegistry. It lets you build and test Base apps
against real precompile behavior, locally, without a live network.

## Install

```bash
curl -L https://raw.githubusercontent.com/base/base-anvil/HEAD/foundryup/install | bash
base-foundryup
```

The installer adds **`base-foundryup`**, a Base-specific installer that lives
alongside (and never overwrites) your stock `foundryup`. Running `base-foundryup`
installs the Base build under **namespaced commands**: `base-forge`, `base-cast`,
`base-anvil`, `base-chisel`. Your stock Foundry, `foundryup` plus
`forge`/`cast`/`anvil`/`chisel`, is left completely untouched, so the two
toolchains coexist: keep using `forge` for everyday work, and reach for
`base-forge` only when you want Base precompile behavior. Re-run `base-foundryup`
any time to update. Each build is reproduced from a specific `base/base` commit;
`base-anvil --version` prints that commit.

> `base-foundryup` and `foundryup` are independent: `base-foundryup` manages only
> the Base build and self-updates separately, so installing or updating Base
> never changes which stock Foundry version you have.

## Add the Base interfaces

```bash
base-forge install base/base-std
```

[base-std](https://github.com/base/base-std) provides the Solidity handles for the
precompiles: `StdPrecompiles.sol` plus `IB20`, `IB20Factory`, `IPolicyRegistry`,
and `IActivationRegistry`.

## Run your tests

The `base-*` commands enable Base precompiles **by default**, so there is nothing
to add to your `foundry.toml`:

```bash
base-forge test
```

No node and no fork required: the precompiles run in-process. For an interactive
local node with the precompiles live, run:

```bash
base-anvil
```

Your tests now exercise the same precompile behavior they would hit on Base,
while your stock `forge`/`anvil` continue to behave exactly as before.

> Prefer to keep a single `forge` and toggle Base per-profile instead of using
> the `base-*` commands? Stock `forge` also honors `base = true` under a
> `foundry.toml` profile (run with `FOUNDRY_PROFILE=base forge test`), or the
> `FOUNDRY_BASE=true` environment variable. The `base-*` commands are simply a
> preconfigured, non-clobbering convenience.
