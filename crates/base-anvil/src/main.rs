//! base-anvil: Anvil + Base custom precompiles.
//!
//! Wraps Anvil's standard CLI and adds two flags:
//! - `--base-precompiles`: install Base's B-20 precompile suite into the EVM
//! - `--activation-admin <address>`: set the ActivationRegistry admin address
//!
//! All other flags are inherited from upstream Anvil unchanged. Run
//! `base-anvil --help` for the full surface.

use std::{
    sync::{Arc, atomic::AtomicUsize},
    time::Duration,
};

use alloy_primitives::Address;
use anvil::{cmd::NodeArgs, try_spawn};
use base_common_chains::BaseUpgrade;
use clap::Parser;
use eyre::Result;

mod precompiles;
use precompiles::BasePrecompileFactory;

/// base-anvil top-level CLI. Flattens Anvil's stock [`NodeArgs`] and layers
/// two Base-specific flags on top.
#[derive(Parser, Debug)]
#[command(
    name = "base-anvil",
    about = "Anvil with Base custom precompiles (B-20 / TokenFactory / PolicyRegistry / ActivationRegistry).",
    version
)]
struct Cli {
    #[command(flatten)]
    anvil: NodeArgs,

    /// Install Base's custom precompiles into the EVM. Without this flag,
    /// `base-anvil` behaves identically to upstream `anvil`.
    #[arg(long, env = "BASE_PRECOMPILES")]
    base_precompiles: bool,

    /// Address authorized to mutate feature flags on the ActivationRegistry
    /// precompile. Required for any test that activates a gated precompile
    /// feature (e.g. the TokenFactory feature). Has no effect unless
    /// `--base-precompiles` is set.
    #[arg(long, env = "ACTIVATION_ADMIN")]
    activation_admin: Option<Address>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let mut config = cli.anvil.into_node_config()?;

    if cli.base_precompiles {
        // Pin to the Beryl upgrade for now — that's the first upgrade
        // where Base ships the B-20 precompile suite. When the precompile
        // set evolves in a later upgrade, expose this as a CLI flag.
        let factory = BasePrecompileFactory::new(BaseUpgrade::Beryl, cli.activation_admin);
        config = config.with_precompile_factory(factory);

        tracing::info!(
            activation_admin = ?cli.activation_admin,
            "base-anvil: Base precompiles installed (upgrade=Beryl)"
        );
    }

    let (_api, mut handle) = try_spawn(config).await?;

    // Minimal shutdown handling: wait for ctrl-c, then signal anvil to stop.
    // The full state-dump / signal-broadcast lifecycle from anvil's NodeArgs::run
    // is intentionally omitted from this prototype; add it back when the use
    // case requires durable state persistence across runs.
    let _running = Arc::new(AtomicUsize::new(0));
    let signal = handle.shutdown_signal_mut().take();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("base-anvil: ctrl-c received, shutting down");
            if let Some(s) = signal {
                let _ = s.fire();
            }
        }
        result = &mut handle => {
            result??;
        }
    }

    // Give in-flight tasks a moment to drain.
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}
