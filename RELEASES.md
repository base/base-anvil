# base-anvil releases & the base/base pin

This repo is a fork of Foundry (`forge`, `cast`, `anvil`, `chisel`) that adds a
`--base` mode dispatching Base's native precompiles. Those precompiles
are not reimplemented here — they are compiled in from
[`base/base`](https://github.com/base/base) via a pinned git dependency. Every
binary we ship therefore *reproduces a specific `base/base` commit*.

This document explains how that pin works, how release builds are triggered and
named, where the artifacts land, and how to download the build you want.

> Consumer docs (how to install and test your Base app) live in
> [`docs/base.md`](./docs/base.md). This file is the maintainer-facing reference
> for the build/release lifecycle.

## The pin is the source of truth

The `base/base` commit this fork targets is declared in one place:

**`crates/evm/networks/Cargo.toml`**

```toml
base-common-precompiles = { git = "https://github.com/base/base.git", rev = "<base/base commit sha>" }
base-common-chains      = { git = "https://github.com/base/base.git", rev = "<base/base commit sha>" }
```

To see exactly where this fork currently points, read that `rev`:

```bash
grep 'rev =' crates/evm/networks/Cargo.toml
```

Both lines always carry the same commit. Nothing else chooses a `base/base`
version — release names, tags, and labels are all *derived* from this `rev` at
build time.

## Re-pinning to a different base/base commit

Use the helper, which resolves a branch / tag / sha against `base/base` and
rewrites both `rev` lines (and refreshes `Cargo.lock`):

```bash
./script/bump-base.sh <branch | tag | sha>     # rebuild anvil + forge after
./script/bump-base.sh --no-build <ref>         # update the manifest only
```

Changing the pin is a normal code change: open a PR, land it on
`base-anvil-fork`, and the *next* build will reproduce the new commit. There is
no way to pick a `base/base` commit at build time — the build always reproduces
whatever is pinned at the ref it builds.

## What triggers a build

All builds run through [`.github/workflows/release.yml`](./.github/workflows/release.yml),
which has exactly three triggers:

| Trigger | How | Produces |
| --- | --- | --- |
| **Scheduled** | cron `0 6 * * *` (06:00 UTC, daily) | a nightly |
| **Manual** | `workflow_dispatch` (Actions tab, or `gh workflow run release.yml --ref base-anvil-fork`) | a nightly |
| **Tag push** | push a tag matching `v*.*.*`, `stable`, `rc`, or `rc-*` | a versioned release |

A nightly and a tagged release run the same build matrix; they differ only in
how the resulting release is named and whether it is marked pre-release.

## Channels & naming

Each build produces (or moves) several refs. Two of them are **downloadable
GitHub Releases**; one is a **label tag** for resolution only.

| Ref | Kind | Meaning |
| --- | --- | --- |
| `nightly` | release (rolling) | Always the newest nightly. Its git tag is moved to the built commit each run. Install it explicitly with `base-foundryup --install nightly`. |
| `nightly-<base-anvil-sha>` | release (immutable) | A permanent snapshot of one build, e.g. `nightly-df3a4abdc…`. Titled `Nightly (YYYY-MM-DD, base <base/base-short-sha>)` so the targeted `base/base` commit is visible in the name. Use these to pin or roll back. |
| `nightly-base-<base/base-short-sha>` | tag only (no assets) | A moving label pointing at the newest base-anvil commit built against that `base/base` commit. Lets `base/base`'s own tooling resolve "the latest base-anvil for this chain commit" without hardcoding a base-anvil sha. **Not** a download target. |
| `v*.*.*` / `stable` / `rc` | release (versioned) | Created by pushing the matching tag; marked as a normal (non-pre) release. |

## Versioning: release names track base/base

The binary's own version (e.g. `1.6.0-nightly`, shown by `anvil --version`) is
inherited from upstream Foundry and says **nothing** about which Base chain
version it reproduces. The Base version is conveyed by the **release name**:

- **Versioned releases are named to match the `base/base` release they
  reproduce.** A base-anvil `v1.1.0` release reproduces Base **v1.1.0
  ("Beryl")** — the same precompile behavior that Base release ships. This is
  the human-friendly handle to reach for instead of a 40-char commit sha.
- **Nightlies** are named `nightly-<base-anvil-sha>` and titled
  `Nightly (date, base <base/base-short-sha>)`, so the chain commit is still
  visible without a stable version number.
- **`nightly-base-<base/base-short-sha>`** is the label tag that resolves "the
  newest base-anvil for this exact `base/base` commit."

So once a stable Beryl release is cut, `base-foundryup --install v1.1.0` gets
you the Beryl build by name. Until then, use the dated `nightly-<sha>` whose
title shows the `base/base` commit it was built against. Once stable releases
exist, `base-foundryup` will default to the latest stable release rather than
the rolling nightly.

## Where artifacts land

GitHub Releases on this repo: <https://github.com/base/base-anvil/releases>.

Each platform leg uploads a tarball (zip on Windows), an attestation, and a
shared man-page tarball:

```
foundry_<version>_<platform>_<arch>.tar.gz      (forge, cast, anvil, chisel)
foundry_<version>_<platform>_<arch>.attestation.txt
foundry_man_<version>.tar.gz
```

The build matrix covers six targets: `linux_amd64`, `linux_arm64`,
`alpine_amd64`, `darwin_amd64`, `darwin_arm64`, and `win32_amd64`.

## Downloading & using a specific build

The easy path (latest stable, once stable releases exist):

```bash
base-foundryup
```

A specific dated build (the tag is the channel; the asset name stays
`foundry_nightly_...`):

```bash
base-foundryup --install nightly-<base-anvil-sha>
```

Raw download for CI or ad-hoc testing — grab a tarball and point the patched
binaries at your harness:

```bash
gh release download nightly-<base-anvil-sha> --repo base/base-anvil \
  --pattern 'foundry_nightly_darwin_arm64.tar.gz'
tar -xzf foundry_nightly_darwin_arm64.tar.gz
export ANVIL_BIN="$PWD/anvil" FORGE_BIN="$PWD/forge"
```

To confirm which `base/base` commit a given build reproduces, read the release
title/body (`Built against base/base <sha>`) or the `nightly-base-<sha>` label
tag. (`anvil --version` reports base-anvil's own commit, not the `base/base`
pin.)

## Gotchas

- **A release run can show `cancelled` while still publishing every binary.**
  The platform build legs are independent of the Docker-image leg; if the late
  Docker leg is cancelled the run is marked cancelled, but all six tarballs are
  already attached. Verify the release *assets*, not the run status.
- **The rolling `nightly` release does not prune assets dropped from the build
  matrix.** If a target is removed, its old asset lingers on `nightly` until
  deleted by hand. (This is why a stale `alpine_arm64` asset had to be cleaned
  up manually.)
