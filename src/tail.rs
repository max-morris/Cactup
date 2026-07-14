//! Shared `log` machinery for `sim log` and `test log`: print the trailing
//! tail of a run's stdout/stderr, and with `--follow` keep streaming
//! newly-appended bytes (`tail -f` style) until Ctrl-C.

use crate::Res;
use anyhow::Context;
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};

fn print_source_header(path: &Path, label: &str) {
    println!("{}", format!("==> {} ({}) <==", path.display(), label).bold());
}

/// Print the trailing tail of each labeled source and, with `follow`, stream
/// appended bytes until Ctrl-C. `subject` is the already-formatted run
/// descriptor used in the "no output yet" / "waiting" messages (e.g. the
/// bolded name plus the restart/results directory).
pub(crate) fn tail_log(sources: &[(&str, PathBuf); 2], follow: bool, subject: &str) -> Res<()> {
    // Print the trailing tail of each existing file, remembering where we
    // stopped so --follow can resume from exactly the newly-appended bytes.
    let mut offsets = [0u64; 2];
    let mut last_src: Option<usize> = None;
    let mut shown = false;
    for (i, (label, path)) in sources.iter().enumerate() {
        let Ok(content) = fs::read_to_string(path) else { continue };
        shown = true;
        print_source_header(path, label);
        last_src = Some(i);
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(100);
        for line in &lines[start..] {
            println!("{line}");
        }
        offsets[i] = content.len() as u64;
    }

    if !follow {
        if !shown {
            println!(
                "No output files yet for {subject} (looked for {} and {})",
                sources[0].1.display(),
                sources[1].1.display()
            );
        }
        return Ok(());
    }

    if !shown {
        eprintln!("Waiting for output from {subject} (Ctrl-C to stop)…");
    }
    follow_sources(sources, offsets, last_src)
}

/// Poll the stdout/stderr files for appended bytes and stream them (`tail -f`
/// style), printing a source header whenever output switches files. Runs until
/// SIGINT (Ctrl-C).
fn follow_sources(
    sources: &[(&str, PathBuf); 2],
    mut offsets: [u64; 2],
    mut last_src: Option<usize>,
) -> Res<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))
        .context("installing Ctrl-C handler for --follow")?;

    let poll = std::time::Duration::from_millis(500);
    while !stop.load(Ordering::Relaxed) {
        for (i, (label, path)) in sources.iter().enumerate() {
            let Ok(mut f) = fs::File::open(path) else { continue };
            let len = match f.metadata() {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            // File was truncated or rotated — restart from the top.
            if len < offsets[i] {
                offsets[i] = 0;
            }
            if len == offsets[i] || f.seek(SeekFrom::Start(offsets[i])).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() || buf.is_empty() {
                continue;
            }
            offsets[i] += buf.len() as u64;
            if last_src != Some(i) {
                print_source_header(path, label);
                last_src = Some(i);
            }
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(&buf);
            let _ = stdout.flush();
        }
        if !stop.load(Ordering::Relaxed) {
            std::thread::sleep(poll);
        }
    }
    println!();
    Ok(())
}
