//! Command dispatch (spec §3). Each submodule exposes
//! `pub fn dispatch(ctx: &Ctx, …) -> Res<()>`; main.rs routes the parsed
//! subcommand to it.

pub mod config;
pub mod delta;
pub mod install;
pub mod installation;
pub mod knob;
pub mod list;
pub mod machine;
pub mod refetch;
pub mod releases;
pub mod show;
pub mod sim;
pub mod test;
pub mod uninstall;
pub mod use_cmd;

use crate::args::GlobalOpts;
use crate::database::Db;
use crate::Res;
use anyhow::anyhow;
use colored::Colorize;
use std::path::PathBuf;

/// Shared command context: the parsed global flags plus the global-DB handle.
///
/// The handle follows the §2.3 model: `ctx.db.read()` for a snapshot,
/// `ctx.db.update(|db| …)` for a self-contained locked read-modify-write.
/// Never hold either across long-running work.
pub struct Ctx {
    pub globals: GlobalOpts,
    pub db: Db,
}

/// Ask a question on the terminal, offering `default` (accepted by an empty
/// line or EOF).
pub fn prompt_with_default(question: &str, default: &str) -> Res<String> {
    use std::io::{self, Write};

    print!("{question} (default: {}): ", default.bold());
    io::stdout().flush()?;

    let mut input = String::new();
    let n = io::stdin().read_line(&mut input)?;

    let input = input.trim();
    Ok(if n == 0 || input.is_empty() {
        default.to_owned()
    } else {
        input.to_owned()
    })
}

/// Convert a `PathBuf` to an owned `String`, erroring on non-UTF-8 paths.
pub fn p2s(pb: PathBuf) -> Res<String> {
    pb.to_str()
        .map(|s| s.to_owned())
        .ok_or(anyhow!("Failed to convert path to string"))
}
