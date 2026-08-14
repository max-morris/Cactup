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
mod scheduler;
mod shell;
mod sim;
mod tail;
mod template;
mod testsuite;
mod thornlist;
mod walltime;

use crate::args::{Args, Commands, ConfigCommand};
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

    let args = Args::parse();
    shell::set_trace(args.globals.trace);

    let ctx = Ctx {
        globals: args.globals,
        db: Db::open()?,
    };

    match args.command {
        Commands::Releases { all } => commands::releases::dispatch(&ctx, all),
        Commands::List => commands::list::dispatch(&ctx),
        Commands::Show => commands::show::dispatch(&ctx),
        Commands::Use { alias } => commands::use_cmd::dispatch(&ctx, alias),
        Commands::Install(install) => commands::install::dispatch(&ctx, install),
        Commands::Uninstall { alias, force } => commands::uninstall::dispatch(&ctx, alias, force),
        Commands::Installation(cmd) | Commands::Inst(cmd) => commands::installation::dispatch(&ctx, cmd),
        Commands::Config(cmd) => commands::config::dispatch(&ctx, cmd),
        // `cactup build …` is an alias for `cactup config build …` (§3).
        Commands::Build(build) => commands::config::dispatch(&ctx, ConfigCommand::Build(build)),
        Commands::Sim(cmd) => commands::sim::dispatch(&ctx, cmd),
        Commands::Test(cmd) => commands::test::dispatch(&ctx, cmd),
        Commands::Knob { name, value } => commands::knob::dispatch(&ctx, name, value),
        Commands::Machine(cmd) => commands::machine::dispatch(&ctx, cmd),
    }
}
