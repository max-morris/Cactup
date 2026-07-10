mod args;
mod build;
mod commands;
mod database;
mod installation;
mod lock;
mod manifest;
mod mdb;
mod scheduler;
mod shell;
mod sim;
mod template;
mod testsuite;
mod walltime;

use crate::args::{Args, Commands, ConfigCommand};
use crate::commands::Ctx;
use crate::database::Db;
use clap::Parser;
use directories::BaseDirs;
use std::path::PathBuf;
use std::sync::LazyLock;

type Res<T> = anyhow::Result<T>;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub static CACTUP_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let base_dirs = BaseDirs::new().expect("Failed to get base directories");
    let home_dir = base_dirs.home_dir().to_path_buf();
    home_dir.join(".cactup")
});

fn main() -> Res<()> {
    unsafe {
        // SAFETY: This method is unsafe because the signal handler we pass in has a certain contract.
        //         We satisfy the contract by virtue of doing nothing.
        gix::interrupt::init_handler(0, || {})?;
    }

    let args = Args::parse();

    let ctx = Ctx {
        globals: args.globals,
        db: Db::open()?,
    };

    match args.command {
        Commands::List { all } => commands::list::dispatch(&ctx, all),
        Commands::Show => commands::show::dispatch(&ctx),
        Commands::Use { alias } => commands::use_cmd::dispatch(&ctx, alias),
        Commands::Install(install) => commands::install::dispatch(&ctx, install),
        Commands::Uninstall { alias, force } => commands::uninstall::dispatch(&ctx, alias, force),
        Commands::Config(cmd) => commands::config::dispatch(&ctx, cmd),
        // `cactup build …` is an alias for `cactup config build …` (§3).
        Commands::Build(build) => commands::config::dispatch(&ctx, ConfigCommand::Build(build)),
        Commands::Sim(cmd) => commands::sim::dispatch(&ctx, cmd),
        Commands::Test(cmd) => commands::test::dispatch(&ctx, cmd),
        Commands::Knob { name, value } => commands::knob::dispatch(&ctx, name, value),
        Commands::Machine(cmd) => commands::machine::dispatch(&ctx, cmd),
    }
}
