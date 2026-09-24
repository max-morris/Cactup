mod args;
mod build;
mod build_info;
mod commands;
mod database;
mod fetch;
mod freeze;
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
mod update;
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
/// cactup's home: `$CACTUP_HOME` when it is set to an absolute path (the
/// installer honors the same variable), else `~/.cactup`.
pub static CACTUP_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    match std::env::var_os("CACTUP_HOME").filter(|v| !v.is_empty()).map(PathBuf::from) {
        Some(home) if home.is_absolute() => return home,
        Some(home) => {
            use colored::Colorize;
            eprintln!(
                "{}",
                format!("Warning: ignoring CACTUP_HOME={} (not an absolute path)", home.display())
                    .yellow()
            );
        }
        None => {}
    }
    let home_dir = std::env::home_dir().expect("Failed to get home directory");
    home_dir.join(".cactup")
});

/// Is this the compute-node side of a job (D11)? Such a run must stay
/// hermetic: no wisdom, no update check, no machine-database notice — it
/// only reads what was frozen into its own metadata at submit time.
fn compute_node_path(command: &Commands) -> bool {
    match command {
        Commands::Sim(SimCommand::Run(run)) => run.sim_dir.is_some(),
        Commands::Test(TestCommand::Run(run)) => run.test_dir.is_some(),
        // The build compute-node path, reachable either bare (`cactup build
        // --config-dir …`) or through the explicit `build run` subcommand a
        // generated submit script uses.
        Commands::Build { start, command: None } => start.config_dir.is_some(),
        Commands::Build { command: Some(BuildCommand::Run(run)), .. } => run.config_dir.is_some(),
        _ => false,
    }
}

fn main() -> Res<()> {
    // First, while the process is still single-threaded: it removes an
    // environment variable.
    update::take_updated_marker();

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
    let compute_node = compute_node_path(&args.command);
    let updating = matches!(args.command, Commands::Update { .. });

    // §17: before dispatch, never after — an update re-runs this same
    // command in the new build, so checking after it would run it twice.
    // Never on a compute node (D11), and `cactup update` does it itself.
    if !compute_node && !updating {
        update::maybe_auto_update(&ctx);
        if gix::interrupt::is_triggered() {
            anyhow::bail!("interrupted");
        }
    }
    // §17: a binary pinned to an older MDB generation keeps working on the
    // last revision of that generation; say so loudly until it is updated.
    // `cactup update` reports it after its own sync instead.
    if build_info::is_dist() && !compute_node && !updating && ctx.globals.mdb_path.is_none()
        && let Some(notice) = update::mdb_generation_notice()
    {
        use colored::Colorize;
        eprintln!("\n{}\n", notice.yellow().bold());
    }

    // Skip the random post-command wisdom where it would be noise: the
    // compute-node path must stay hermetic (D11), and the log-follow views
    // end via Ctrl-C, where a trailing aphorism reads as clutter.
    let suppress_wisdom = compute_node || match &args.command {
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
        Commands::Update { check, prune } => commands::update::dispatch(&ctx, check, prune),
    };

    // Wisdom after failure would be flippant — and gating on Ok also keeps
    // anyhow's after-main `Error:` print from landing below decoration.
    if result.is_ok() && !suppress_wisdom {
        commands::wisdom::maybe_print(&ctx);
    }
    result
}
