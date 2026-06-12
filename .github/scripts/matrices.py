#!/usr/bin/env python3

import json
import os


# A runner target
class Target:
    # GHA runner
    runner_label: str
    # Rust target triple
    target: str
    # SVM Solc target
    svm_target_platform: str

    def __init__(self, runner_label: str, target: str, svm_target_platform: str):
        self.runner_label = runner_label
        self.target = target
        self.svm_target_platform = svm_target_platform


# A single test suite to run.
class Case:
    # Name of the test suite.
    name: str
    # Nextest filter expression.
    filter: str
    # Number of partitions to split the test suite into.
    n_partitions: int
    # Whether to run on non-Linux platforms for PRs. All platforms and tests are run on pushes.
    pr_cross_platform: bool

    def __init__(
        self, name: str, filter: str, n_partitions: int, pr_cross_platform: bool
    ):
        self.name = name
        self.filter = filter
        self.n_partitions = n_partitions
        self.pr_cross_platform = pr_cross_platform


# GHA matrix entry
class Expanded:
    name: str
    runner_label: str
    target: str
    svm_target_platform: str
    flags: str
    partition: int

    def __init__(
        self,
        name: str,
        runner_label: str,
        target: str,
        svm_target_platform: str,
        flags: str,
        partition: int,
    ):
        self.name = name
        self.runner_label = runner_label
        self.target = target
        self.svm_target_platform = svm_target_platform
        self.flags = flags
        self.partition = partition


profile = os.environ.get("PROFILE")
is_pr = os.environ.get("EVENT_NAME") == "pull_request"
t_linux_x86 = Target(
    "ubuntu-latest", "x86_64-unknown-linux-gnu", "linux-amd64"
)
t_linux_arm = Target(
    "ubuntu-24.04-arm", "aarch64-unknown-linux-gnu", "linux-aarch64"
)
t_macos = Target("macos-latest", "aarch64-apple-darwin", "macosx-aarch64")
t_windows = Target("windows-latest", "x86_64-pc-windows-msvc", "windows-amd64")
targets = [t_linux_x86] if is_pr else [t_linux_x86, t_linux_arm, t_macos, t_windows]

# base-anvil's only behavioral delta vs upstream Foundry is the `--base` precompile
# support (crates/evm/networks). It does NOT modify forking, live-RPC, Etherscan/ENS,
# git dependency installation, or docs fetching. Tests exercising those upstream features
# depend on external endpoints (archive RPC nodes, Etherscan, GitHub, book.getfoundry.sh)
# that are rate-limited or unavailable in CI, making the suite nondeterministically red.
# Upstream foundry-rs/foundry already covers them, so we scope them out of base-anvil's
# `all` suite to keep CI deterministic. Keep this list curated: only add tests that exercise
# upstream behavior base-anvil never changes (never base-specific or anvil-core regressions).
NETWORK_TEST_EXCLUDES = [
    "test(/fork/)",  # all forking tests (anvil, forge test_cmd, evm-core, traces, eip4844, simulate, state)
    "package(cast)",  # cast CLI suite is overwhelmingly live-RPC / Etherscan / ENS integration
    "test(/\\binstall::/)",  # git-network dependency installation
    "test(/ensure_lint_rule_docs/)",  # fetches https://book.getfoundry.sh
    # Remaining non-"fork"-named RPC / broadcast tests:
    "test(/script::can_broadcast/)",
    "test(/script::can_execute_script_command_with_manual_gas_limit/)",
    "test(/script::should_set_correct_sender_nonce_via_cli/)",
    "test(/script::call_to_non_contract_address_does_not_panic/)",
    "test(/anvil_api::can_impersonate_gnosis_safe/)",
    "test(/eip4844::can_send_eip4844_transaction_eth_send_transaction/)",
    "test(/genesis::chain_id_precedence/)",
    "test(/test_cmd::spec::test_set_evm_version/)",
    "test(/test_cmd::testdata/)",
    "test(/backend::tests::can_read_write_cache/)",
    # Toolchain-nondeterministic (not network): CI's Linux solc emits a "Warning (6335):
    # error will be promoted to keyword" block for the test fixture that macOS solc does not,
    # so this upstream forge-backtrace snapshot cannot be regenerated deterministically across
    # platforms. base-anvil never changes forge's backtrace formatting.
    "test(/backtrace::test_library_backtrace/)",
]

# The `all` case runs the full foundry+anvil suite EXCEPT ext_integration (covered by the
# `external` case) and EXCEPT the network-dependent upstream tests listed above.
_all_filter = " & ".join(
    ["!test(/\\bext_integration/)"] + [f"!{e}" for e in NETWORK_TEST_EXCLUDES]
)

config = [
    Case(
        name="all",
        filter=_all_filter,
        n_partitions=1,
        pr_cross_platform=True,
    ),
    Case(
        name="external",
        filter="package(=forge) & test(/\\bext_integration/)",
        n_partitions=1,
        pr_cross_platform=False,
    ),
]


def main():
    expanded = []
    for target in targets:
        for case in config:
            if is_pr and (not case.pr_cross_platform and target != t_linux_x86):
                continue

            for partition in range(1, case.n_partitions + 1):
                os_str = ""
                if len(targets) > 1:
                    os_str = f" ({target.target})"

                name = case.name
                flags = f"-E '{case.filter}'"
                if case.n_partitions > 1:
                    s = f"{partition}/{case.n_partitions}"
                    name += f" ({s})"
                    flags += f" --partition count:{s}"

                if profile == "isolate":
                    flags += " --features=isolate-by-default"
                name += os_str

                flags += " --no-fail-fast"

                obj = Expanded(
                    name=name,
                    runner_label=target.runner_label,
                    target=target.target,
                    svm_target_platform=target.svm_target_platform,
                    flags=flags,
                    partition=partition,
                )
                expanded.append(vars(obj))

    print_json({"include": expanded})


def print_json(obj):
    print(json.dumps(obj), end="", flush=True)


if __name__ == "__main__":
    main()
