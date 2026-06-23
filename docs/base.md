# Testing your Base app with base-anvil

`base-anvil` is a build of Foundry (`forge`, `anvil`, `cast`, `chisel`) that
understands Base's native precompiles, such as the B20 token factory, B20 tokens,
the PolicyRegistry, and the ActivationRegistry. It lets you build and test Base
apps against real precompile behavior, locally, without a live network.

Stock Foundry knows nothing about these precompiles. A `forge test` that calls a
B20 precompile address gets empty data back (the address holds no bytecode), and
high-level calls abort with `call to non-contract address`. base-anvil registers
the precompiles into Foundry's in-process EVM, so the same call runs the real
Base behavior, with no node and no RPC round-trips.

> base-anvil does not reimplement the precompiles; it compiles them in from
> [`base/base`](https://github.com/base/base), so each build reproduces a
> specific `base/base` commit and you test exactly what the chain runs. See
> [`RELEASES.md`](../RELEASES.md) for how the pin and release naming work.

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
any time to update.

> `base-foundryup` and `foundryup` are independent: `base-foundryup` manages only
> the Base build and self-updates separately, so installing or updating Base
> never changes which stock Foundry version you have.

## Picking the Base version

base-anvil's versioned releases are named after the **Base chain release** they
reproduce: a base-anvil `v1.1.0` build runs the precompile behavior of Base
**v1.1.0 ("Beryl")**. (The tool's own `--version`, e.g. `1.6.0-nightly`, is just
the underlying Foundry version and is unrelated to the Base version.) To install
a specific Base release by name:

```bash
base-foundryup --install v1.1.0
```

With no version, `base-foundryup` installs the latest build. Each release on the
[releases page](https://github.com/base/base-anvil/releases) is titled with the
exact `base/base` commit it reproduces, and maintainer details live in
[`RELEASES.md`](../RELEASES.md).

## Add the Base interfaces

```bash
base-forge install base/base-std
```

[base-std](https://github.com/base/base-std) provides the Solidity handles for the
precompiles, including `StdPrecompiles.sol`, `IB20`, `IB20Factory`,
`IPolicyRegistry`, and `IActivationRegistry`.

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

## What is Base-specific

base-anvil is otherwise stock Foundry. The full command reference for `forge`,
`cast`, `anvil`, and `chisel` (every subcommand and flag) is the upstream Foundry
documentation at [getfoundry.sh](https://getfoundry.sh), and `base-forge --help`
/ `base-anvil --help` print the same. Only the following are added by this fork:

| Addition | Applies to | What it does |
| --- | --- | --- |
| `base-forge`, `base-cast`, `base-anvil`, `base-chisel` | all | Wrappers that run the Base build with precompiles enabled by default. Installed by `base-foundryup`. |
| `--base` flag | `anvil` | Installs the Base precompiles into the node's EVM. The `base-anvil` wrapper passes this for you. |
| `base = true` (in `foundry.toml`) | `forge`, `cast`, `chisel` | Installs the Base precompiles into the in-process EVM. Read from the active profile. |
| `FOUNDRY_BASE=true` (env) | `forge`, `cast`, `chisel` | Same as `base = true`, set via the environment. |
| `base-foundryup` | installer | Base-only installer; select a version with `--install <ref>` (e.g. `v1.1.0`, `nightly`). Never touches stock `foundryup`. |

Because the precompiles are native, a `base-cast call` against a precompile
address over RPC only returns data if the node itself runs them (a `base-anvil`
node, or a live Base RPC). In a pure local test, the precompiles live inside
`base-forge`'s own in-process EVM.

## The local base-anvil node

A `base-anvil` node starts with Base's activation-gated features (such as B20
asset, B20 stablecoin, and PolicyRegistry) already active, matching a live
Beryl-or-later chain, so you do not need to `activate()` anything by hand. Its
chain id is `31337` and it ships the usual anvil pre-funded dev accounts.

Networks that already have the precompiles active, for testing against a remote
chain:

| Network | RPC URL | Chain ID |
| --- | --- | --- |
| Local (`base-anvil`) | `http://127.0.0.1:8545` | `31337` |
| Base Sepolia | `https://sepolia.base.org` | `84532` |
| Vibenet | `https://rpc.vibes.base.org/` | `84538453` |

## Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| `call to non-contract address 0x...` at a precompile | You are running stock `forge`, or Base is not enabled. Use `base-forge`, or set `base = true` / `FOUNDRY_BASE=true`. |
| `FeatureNotActivated` against a live network | The precompile's feature is not activated on that chain yet. Local `base-anvil` seeds them active; on a network, use one where the feature is live. |
| Behavior differs from the chain you expect | Your installed build may reproduce a different `base/base` commit than the chain you are comparing against. Check the release title / [`RELEASES.md`](../RELEASES.md) and re-install the matching version with `base-foundryup --install <ref>`. |

## Next steps

- **Launch a B20 token end to end:** the Base docs walkthrough at
  [docs.base.org/get-started/launch-b20-token](https://docs.base.org/get-started/launch-b20-token)
  creates a token, mints supply, and verifies the balance using these commands.
- **The B20 token standard:**
  [docs.base.org/base-chain/specs/upgrades/beryl/b20](https://docs.base.org/base-chain/specs/upgrades/beryl/b20).
- **Maintainers, the `base/base` pin and release lifecycle:**
  [`RELEASES.md`](../RELEASES.md).
