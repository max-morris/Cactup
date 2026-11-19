//! Command dispatch (spec §3). Each submodule exposes
//! `pub fn dispatch(ctx: &Ctx, …) -> Res<()>`; main.rs routes the parsed
//! subcommand to it.

pub mod add;
pub mod build;
pub mod cache;
pub mod config;
pub mod copy;
pub mod delete;
pub mod delta;
pub mod import;
pub mod install;
pub mod installation;
pub mod knob;
pub mod list;
pub mod machine;
pub mod refetch;
pub mod reify;
pub mod releases;
pub mod show;
pub mod sim;
pub mod test;
pub mod update;
pub mod use_cmd;
pub mod wisdom;

use crate::args::GlobalOpts;
use crate::database::Db;
use crate::Res;
use anyhow::{anyhow, bail};
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

    let input = read_line_interruptibly()?.unwrap_or_default();
    let input = input.trim();
    Ok(if input.is_empty() { default.to_owned() } else { input.to_owned() })
}

/// One line from stdin (`None` at end of input), failing with "interrupted"
/// as soon as Ctrl-C is pressed. The interrupt handler restarts the read a
/// signal interrupts, so a plain `read_line` would sit there until Enter
/// while cactup claims to be stopping; the read happens on a thread of its
/// own instead, and this one polls the flag.
pub fn read_line_interruptibly() -> Res<Option<String>> {
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line).map(|n| (n > 0).then_some(line));
        let _ = send.send(read);
    });
    loop {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        match recv.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(read) => return Ok(read?),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

/// Ask a yes/no `question` on stderr; only `y`/`yes` is a yes, end of input
/// is a no, and Ctrl-C fails with "interrupted".
pub fn confirm(question: &str) -> Res<bool> {
    use std::io::Write;
    eprint!("{question}");
    std::io::stderr().flush()?;
    let answer = read_line_interruptibly()?.unwrap_or_default();
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Convert a `PathBuf` to an owned `String`, erroring on non-UTF-8 paths.
pub fn p2s(pb: PathBuf) -> Res<String> {
    pb.to_str()
        .map(|s| s.to_owned())
        .ok_or(anyhow!("Failed to convert path to string"))
}

/// `remove_dir_all`, but checking `stop` (Ctrl-C) before every entry, failing
/// with "interrupted" when it is set, and calling `removed` after each entry
/// it deletes (for a progress count). Symlinks are removed, never followed.
pub(crate) fn remove_tree_until(
    path: &std::path::Path,
    stop: &std::sync::atomic::AtomicBool,
    removed: &dyn Fn(),
) -> Res<()> {
    use anyhow::Context;
    use std::fs;
    if stop.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("Interrupted while deleting '{}'", path.display());
    }
    let meta = fs::symlink_metadata(path).with_context(|| format!("Failed to stat '{}'", path.display()))?;
    if !meta.is_dir() {
        fs::remove_file(path).with_context(|| format!("Failed to remove '{}'", path.display()))?;
        removed();
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("Failed to list '{}'", path.display()))? {
        remove_tree_until(&entry?.path(), stop, removed)?;
    }
    fs::remove_dir(path).with_context(|| format!("Failed to remove '{}'", path.display()))?;
    removed();
    Ok(())
}
