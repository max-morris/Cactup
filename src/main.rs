mod args;
mod build;
mod commands;
mod database;
mod fetch;
mod installation;
mod lock;
mod manifest;
mod mdb;
mod par;
mod progress;
mod scheduler;
mod shell;
mod sim;
mod tail;
mod template;
mod testsuite;
mod thornlist;
mod walltime;
// The corpus parser: used by build.rs (via include!) at build time, and by
// the crate only in tests — hence cfg(test).
#[cfg(test)]
mod wisdom_parse;

use crate::args::{Args, BuildCommand, Commands, SimCommand, TestCommand};
use crate::commands::Ctx;
use crate::database::Db;
use clap::Parser;
use std::path::PathBuf;
use std::sync::LazyLock;

type Res<T> = anyhow::Result<T>;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub static CACTUP_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let home_dir = std::env::home_dir().expect("Failed to get home directory");
    home_dir.join(".cactup")
});

fn main() -> Res<()> {
    // Grace count 1: the FIRST Ctrl-C sets the interrupt flag — which every
    // long-running path polls, so cactup winds down within moments (locks
    // released, tempfiles cleaned) — and the SECOND aborts on the spot. Both
    // facts are announced immediately, because a grace period the user
    // cannot see just reads as a hung process.
    //
    // SAFETY: the handler runs in signal context, so it may not lock,
    // allocate, or block. A raw write(2) to fd 2 through a File that never
    // owns the fd (mem::forget skips the close) is async-signal-safe; std's
    // Stderr handle is not (it locks and may allocate).
    let announce = || {
        use std::io::Write;
        use std::os::fd::FromRawFd;
        let mut stderr = unsafe { std::fs::File::from_raw_fd(2) };
        let _ =
            stderr.write_all(b"\ncactup: interrupted, stopping (Ctrl-C again aborts instantly)\n");
        std::mem::forget(stderr);
    };
    unsafe {
        gix::interrupt::init_handler(1, announce)?;
    }

    // `-K NAME VALUE` is folded into `-K NAME=VALUE` before clap parses.
    let args = Args::parse_from(args::normalize_knob_args(std::env::args_os()));
    shell::set_trace(args.globals.trace);
    // The `-K` overlay (§5.1): every DB snapshot this command reads carries
    // these values, nothing persists them.
    database::set_knob_overrides(
        args.globals.knob.iter().map(|k| (k.name.clone(), k.value.clone())).collect(),
    );

    let ctx = Ctx {
        globals: args.globals,
        db: Db::open()?,
    };

    // Skip the random post-command wisdom where it would be noise: the
    // compute-node path must stay hermetic (D11), and the log-follow views
    // end via Ctrl-C, where a trailing aphorism reads as clutter.
    let suppress_wisdom = match &args.command {
        Commands::Sim(SimCommand::Run(run)) if run.sim_dir.is_some() => true,
        Commands::Test(TestCommand::Run(run)) if run.test_dir.is_some() => true,
        // The build compute-node path (D11), same as the two arms above —
        // reachable either bare (`cactup build --config-dir …`) or through
        // the explicit `build run` subcommand a generated submit script uses.
        Commands::Build { start, command: None } if start.config_dir.is_some() => true,
        Commands::Build { command: Some(BuildCommand::Run(run)), .. } if run.config_dir.is_some() => true,
        Commands::Sim(SimCommand::Log { follow, follow_out, follow_err, .. })
        | Commands::Test(TestCommand::Log { follow, follow_out, follow_err, .. })
            if *follow || *follow_out || *follow_err =>
        {
            true
        }
        Commands::Build { command: Some(BuildCommand::Log { follow, follow_out, follow_err, .. }), .. }
            if *follow || *follow_out || *follow_err =>
        {
            true
        }
        Commands::Wisdom => true, // no double dose
        _ => false,
    };

    let result = match args.command {
        Commands::Releases { all } => commands::releases::dispatch(&ctx, all),
        Commands::List => commands::list::dispatch(&ctx),
        Commands::Show => commands::show::dispatch(&ctx),
        Commands::Use { alias } => commands::use_cmd::dispatch(&ctx, alias),
        Commands::Install(install) => commands::install::dispatch(&ctx, install),
        Commands::Uninstall { alias, force } => commands::uninstall::dispatch(&ctx, alias, force),
        Commands::Installation(cmd) | Commands::Inst(cmd) => commands::installation::dispatch(&ctx, cmd),
        Commands::Config(cmd) => commands::config::dispatch(&ctx, cmd),
        Commands::Build { start, command } => commands::build::dispatch(&ctx, *start, command),
        Commands::Sim(cmd) => commands::sim::dispatch(&ctx, cmd),
        Commands::Test(cmd) => commands::test::dispatch(&ctx, cmd),
        Commands::Knob { command: Some(cmd), .. } => commands::knob::dispatch_sub(&ctx, cmd),
        Commands::Knob { name, value, custom, command: None } => {
            commands::knob::dispatch(&ctx, name, value, custom)
        }
        Commands::Machine(cmd) => commands::machine::dispatch(&ctx, cmd),
        Commands::Wisdom => commands::wisdom::dispatch(&ctx),
    };

    // Wisdom after failure would be flippant — and gating on Ok also keeps
    // anyhow's after-main `Error:` print from landing below decoration.
    if result.is_ok() && !suppress_wisdom {
        commands::wisdom::maybe_print(&ctx);
    }
    result
}
