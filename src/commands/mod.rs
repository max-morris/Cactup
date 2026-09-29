//! Command dispatch (spec §3). Each submodule exposes
//! `pub fn dispatch(ctx: &Ctx, …) -> Res<()>`; main.rs routes the parsed
//! subcommand to it.

pub mod build;
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
pub mod update;
pub mod use_cmd;
pub mod wisdom;

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

    let (n, input) = read_line_interruptibly(
        || {
            let mut input = String::new();
            io::stdin().read_line(&mut input).map(|n| (n, input))
        },
        gix::interrupt::is_triggered,
    )?;

    let input = input.trim();
    Ok(if n == 0 || input.is_empty() {
        default.to_owned()
    } else {
        input.to_owned()
    })
}

/// Run a blocking line read, but give up as soon as `interrupted()` says so.
///
/// A read from stdin blocks through Ctrl-C (std retries it after the signal),
/// so the first Ctrl-C — which only asks cactup to stop — would otherwise do
/// nothing at a prompt. The read runs on a thread of its own and is polled
/// here; an abandoned read is left blocked, and the process exits soon after.
fn read_line_interruptibly(
    read: impl FnOnce() -> std::io::Result<(usize, String)> + Send + 'static,
    interrupted: impl Fn() -> bool,
) -> Res<(usize, String)> {
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::time::Duration;

    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(read());
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => return Ok(result?),
            Err(RecvTimeoutError::Timeout) if interrupted() => {
                println!();
                anyhow::bail!("interrupted at a prompt");
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => anyhow::bail!("the prompt's input went away"),
        }
    }
}

/// Convert a `PathBuf` to an owned `String`, erroring on non-UTF-8 paths.
pub fn p2s(pb: PathBuf) -> Res<String> {
    pb.to_str()
        .map(|s| s.to_owned())
        .ok_or(anyhow!("Failed to convert path to string"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_prompt_gives_up_when_interrupted() {
        let started = Instant::now();
        let err = read_line_interruptibly(
            || {
                std::thread::sleep(Duration::from_secs(30));
                Ok((0, String::new()))
            },
            || true,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("interrupted"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_prompt_returns_what_was_typed() {
        let (n, line) = read_line_interruptibly(|| Ok((4, "yes\n".to_owned())), || false).unwrap();
        assert_eq!((n, line.as_str()), (4, "yes\n"));
    }
}
