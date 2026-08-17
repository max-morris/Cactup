//! Shared `log` machinery for `sim log` and `test log`: print the trailing
//! tail of a run's stdout/stderr, and — depending on [`FollowMode`] — keep
//! streaming newly-appended bytes (`tail -f` style) until Ctrl-C.
//!
//! [`LogTail`] is the reusable incremental-read poller behind every follow
//! mode: [`follow_combined`] (both sources, headers on switch — also Part
//! B's non-TTY fallback for the side-by-side TUI) and the single-source
//! `--follow-out`/`--follow-err` loop in [`tail_log`] itself. [`PollBackoff`]
//! paces how often those loops stat their files.

use crate::Res;
use anyhow::Context;
use colored::Colorize;
use std::fs;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod tui;

/// Adaptive poll-interval backoff shared by every follow loop (and, later,
/// Part B's TUI event loop). We poll on a timer rather than use inotify (or
/// similar) because sim logs frequently live on network filesystems —
/// Lustre, NFS — where inotify events don't reliably fire. But those same
/// filesystems charge a metadata RPC per `stat`, so an idle follow must not
/// hammer them either: we start at the floor for near-live latency while a
/// sim is actively writing, and back off exponentially (capped at the
/// ceiling) while it's quiet, snapping back to the floor the moment new
/// bytes show up.
///
/// Backing off only begins once [`Self::GRACE_ROUNDS`] consecutive polls
/// have come up empty (~500ms at the floor). Without that grace period a
/// producer writing at a steady few lines per second sits in an oscillation
/// — interval grows past the write cadence, a clump of lines lands, reset,
/// repeat — which reads as bursty, bouncy output in the follow views. With
/// it, anything writing at least ~2 lines/sec stays pinned to the floor
/// (smooth, near-live delivery), while a truly quiet log still reaches the
/// ceiling within ~2.5s of its last byte.
pub(crate) struct PollBackoff {
    current: std::time::Duration,
    /// Consecutive empty polling rounds seen since the last one with data.
    empty_rounds: u32,
}

impl PollBackoff {
    const FLOOR: std::time::Duration = std::time::Duration::from_millis(50);
    const CEILING: std::time::Duration = std::time::Duration::from_millis(1000);
    /// Empty rounds tolerated at the current interval before growth starts.
    const GRACE_ROUNDS: u32 = 10;

    pub fn new() -> Self {
        PollBackoff { current: Self::FLOOR, empty_rounds: 0 }
    }

    /// The interval to wait before the next poll.
    pub fn interval(&self) -> std::time::Duration {
        self.current
    }

    /// Record the outcome of a polling round: any round that saw new bytes
    /// (from any source) resets to the floor; an empty round past the grace
    /// period doubles the interval, clamped to the ceiling.
    pub fn note(&mut self, had_data: bool) {
        if had_data {
            self.current = Self::FLOOR;
            self.empty_rounds = 0;
        } else {
            self.empty_rounds += 1;
            if self.empty_rounds > Self::GRACE_ROUNDS {
                self.current = (self.current * 2).min(Self::CEILING);
            }
        }
    }
}

/// How `sim log` / `test log` should follow the run's output. The three
/// follow flags (`-f`/`-o`/`-e`) are mutually exclusive at the clap level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FollowMode {
    /// Static tail only — the original, unchanged behavior.
    None,
    /// Follow both stdout and stderr. On a TTY, Part B fronts this with a
    /// full-screen side-by-side TUI; [`follow_combined`] is the combined
    /// `tail -f`-with-headers stream used as its non-TTY fallback (and, for
    /// now, Part A's whole implementation of this mode).
    Both,
    /// Follow stdout only, raw, no headers on stdout.
    Out,
    /// Follow stderr only, raw, no headers on stdout.
    Err,
}

impl FollowMode {
    /// Build a mode from the three mutually-exclusive CLI flags. clap
    /// enforces exclusivity via `conflicts_with`, so at most one is ever set.
    pub(crate) fn from_flags(follow: bool, follow_out: bool, follow_err: bool) -> Self {
        if follow {
            FollowMode::Both
        } else if follow_out {
            FollowMode::Out
        } else if follow_err {
            FollowMode::Err
        } else {
            FollowMode::None
        }
    }
}

fn print_source_header(path: &Path, label: &str) {
    println!("{}", format!("==> {} ({}) <==", path.display(), label).bold());
}

/// Incremental reader over a single log file: remembers how far it has read
/// and hands back only newly-appended bytes on each [`LogTail::poll`]. Used
/// by every follow mode, and by Part B's TUI panes.
pub(crate) struct LogTail {
    path: PathBuf,
    offset: u64,
}

impl LogTail {
    pub fn new(path: PathBuf, offset: u64) -> Self {
        LogTail { path, offset }
    }

    /// Read any bytes appended since the last poll. Returns `None` if the
    /// file doesn't exist yet, or exists but has nothing new. Handles
    /// truncation/rotation: if the file has shrunk below the last-seen
    /// offset, restarts from the top and returns whatever is there now.
    pub fn poll(&mut self) -> Option<Vec<u8>> {
        let mut f = fs::File::open(&self.path).ok()?;
        let len = f.metadata().ok()?.len();
        if len < self.offset {
            self.offset = 0;
        }
        if len == self.offset {
            return None;
        }
        f.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        if buf.is_empty() {
            return None;
        }
        self.offset += buf.len() as u64;
        Some(buf)
    }
}

/// Trailing bytes of an existing file read to seed a static tail or a TUI
/// pane; bounds startup memory/IO against multi-GB sim logs.
pub(crate) const SEED_BYTES: u64 = 4 * 1024 * 1024;

/// Lines of backlog the static tail prints before a follow takes over.
const TAIL_LINES: usize = 100;

/// The seed of a static tail: the backlog to print, plus where a follow
/// should pick up. Produced by [`read_static_tail`].
pub(crate) struct StaticTail {
    /// Up to `max_lines` trailing lines, ready to print (no line endings).
    pub lines: Vec<String>,
    /// Byte offset the file had been read to — hand this straight to
    /// [`LogTail::new`] so the follow resumes on the first *unprinted* byte.
    pub offset: u64,
}

/// Read the trailing backlog of `path`: at most `max_lines` lines taken from
/// the last [`SEED_BYTES`] of the file, decoded lossily, plus the byte offset
/// a follow should resume from. Returns `None` — and *only* — when the file
/// can't be opened or read at all, which is the genuine "no output yet" case
/// the callers report to the user.
///
/// Three things this deliberately does not do, each of which the old
/// `fs::read_to_string` did:
///
/// - It never reads the whole file. Sim logs run to tens of GB on Lustre;
///   slurping one to show its last 100 lines cost seconds of stall before a
///   single line appeared. Only a bounded window is touched, so a very long
///   line can push the yield below `max_lines` — an acceptable trade, which
///   is why the window is megabytes rather than kilobytes.
/// - It never fails on invalid UTF-8. Cactus logs carry the occasional stray
///   byte, and `read_to_string`'s `Err` made those files look *absent*: the
///   user was told there was no output about a file full of it. Decoding is
///   lossy (`U+FFFD` for the bad bytes) so the file displays like any other.
/// - It never measures the offset in decoded chars. `U+FFFD` is three bytes
///   standing in for as little as one, so a decoded string's length is not
///   the file's; the offset here comes from the raw read accounting, keeping
///   [`LogTail`] byte-exact — no re-printed and no skipped bytes.
fn read_static_tail(path: &Path, max_lines: usize) -> Option<StaticTail> {
    let mut f = fs::File::open(path).ok()?;
    // A file that grows between the `stat` and the read is fine: the window
    // start is only a lower bound, and the offset below is derived from what
    // was actually read, not from this length.
    let start = f.metadata().ok()?.len().saturating_sub(SEED_BYTES);
    let mut window = Vec::new();
    f.seek(SeekFrom::Start(start)).ok()?;
    f.read_to_end(&mut window).ok()?;
    // Everything up to here was consumed, partial leading line included, so
    // this is where the follow resumes regardless of what we end up printing.
    let offset = start + window.len() as u64;

    let mut seed = &window[..];
    if start > 0 {
        // Started mid-file: the first line is a fragment, so drop it.
        seed = match seed.iter().position(|&b| b == b'\n') {
            Some(i) => &seed[i + 1..],
            None => &[],
        };
    }
    let text = String::from_utf8_lossy(seed);
    let all: Vec<&str> = text.lines().collect();
    let first = all.len().saturating_sub(max_lines);
    let lines = all[first..].iter().map(|l| (*l).to_string()).collect();
    Some(StaticTail { lines, offset })
}

/// Register the shared Ctrl-C flag used by every follow loop below.
fn install_sigint_flag() -> Res<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))
        .context("installing Ctrl-C handler for --follow")?;
    Ok(stop)
}

/// Longest single sleep chunk used by [`sleep_interruptibly`] — bounds how
/// long SIGINT can take to be noticed even when `PollBackoff` has backed all
/// the way off to its 1s ceiling.
const SLEEP_CHUNK: std::time::Duration = std::time::Duration::from_millis(100);

/// Sleep `interval`, but in chunks of at most [`SLEEP_CHUNK`], checking
/// `stop` between each one and returning early the moment it's set. A plain
/// `thread::sleep(interval)` could delay a Ctrl-C exit by the full interval
/// — up to `PollBackoff`'s 1s ceiling — which reads as a hang.
fn sleep_interruptibly(interval: std::time::Duration, stop: &AtomicBool) {
    let mut remaining = interval;
    while !stop.load(Ordering::Relaxed) && !remaining.is_zero() {
        let chunk = remaining.min(SLEEP_CHUNK);
        std::thread::sleep(chunk);
        remaining -= chunk;
    }
}

/// Print the trailing tail of each labeled source and, per `mode`, keep
/// streaming appended bytes until Ctrl-C. `subject` is the already-formatted
/// run descriptor used in the "no output yet" / "waiting" messages (e.g. the
/// bolded name plus the restart/results directory). `sources[0]` is always
/// `("stdout", ...)`, `sources[1]` `("stderr", ...)`.
pub(crate) fn tail_log(sources: &[(&str, PathBuf); 2], mode: FollowMode, subject: &str) -> Res<()> {
    // Out/Err modes stream a single source raw to stdout; headers and
    // notices go to stderr instead, so stdout stays pipeable (e.g. into
    // grep). Handled entirely separately from the combined tail below.
    let single = match mode {
        FollowMode::Out => Some(0),
        FollowMode::Err => Some(1),
        FollowMode::None | FollowMode::Both => None,
    };
    if let Some(i) = single {
        return follow_single(&sources[i], subject);
    }

    // `Both` on a TTY gets the full-screen side-by-side TUI, which seeds
    // its own panes from each file's contents — it does *not* want the
    // static tail printed to the normal screen first (that would just sit
    // behind the alternate screen and reappear, stale, on exit).
    if mode == FollowMode::Both && std::io::stdout().is_terminal() {
        return tui::follow_tui(sources, subject);
    }
    if mode == FollowMode::Both {
        eprintln!("stdout is not a terminal; falling back to combined streaming");
    }

    // Print the trailing tail of each existing file, remembering where we
    // stopped so --follow can resume from exactly the newly-appended bytes.
    let mut offsets = [0u64; 2];
    let mut last_src: Option<usize> = None;
    let mut shown = false;
    for (i, (label, path)) in sources.iter().enumerate() {
        // Only a file we can't open at all counts as "not there yet"; one
        // that merely holds non-UTF-8 bytes still gets shown.
        let Some(seed) = read_static_tail(path, TAIL_LINES) else { continue };
        shown = true;
        print_source_header(path, label);
        last_src = Some(i);
        for line in &seed.lines {
            println!("{line}");
        }
        offsets[i] = seed.offset;
    }

    if mode == FollowMode::None {
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
    follow_combined(sources, offsets, last_src)
}

/// Poll both sources for appended bytes and stream them (`tail -f` style),
/// printing a source header whenever output switches files. Runs until
/// SIGINT (Ctrl-C). This is `FollowMode::Both`'s stream: Part A's whole
/// implementation of that mode, and Part B's non-TTY fallback for the
/// side-by-side TUI.
pub(crate) fn follow_combined(
    sources: &[(&str, PathBuf); 2],
    offsets: [u64; 2],
    mut last_src: Option<usize>,
) -> Res<()> {
    let stop = install_sigint_flag()?;
    let mut tails = [
        LogTail::new(sources[0].1.clone(), offsets[0]),
        LogTail::new(sources[1].1.clone(), offsets[1]),
    ];
    let mut backoff = PollBackoff::new();

    while !stop.load(Ordering::Relaxed) {
        let mut had_data = false;
        for (i, (label, path)) in sources.iter().enumerate() {
            let Some(buf) = tails[i].poll() else { continue };
            had_data = true;
            if last_src != Some(i) {
                print_source_header(path, label);
                last_src = Some(i);
            }
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(&buf);
            let _ = stdout.flush();
        }
        backoff.note(had_data);
        sleep_interruptibly(backoff.interval(), &stop);
    }
    println!();
    Ok(())
}

/// `FollowMode::Out` / `FollowMode::Err`: print the static tail and then
/// stream appended bytes of a single source raw to stdout (flushing after
/// each write), with no headers ever on stdout. Headers and the "waiting"
/// notice go to stderr. Runs until SIGINT (Ctrl-C).
fn follow_single(source: &(&str, PathBuf), subject: &str) -> Res<()> {
    let (label, path) = source;
    eprintln!("{}", format!("==> {} ({}) <==", path.display(), label).bold());

    let offset = match read_static_tail(path, TAIL_LINES) {
        // The file is there — print its backlog and follow from the end of
        // what we printed, even if some of its bytes decoded lossily.
        Some(seed) => {
            for line in &seed.lines {
                println!("{line}");
            }
            seed.offset
        }
        // Genuinely absent: nothing to print, and the follow starts at the
        // top of whatever eventually shows up.
        None => {
            eprintln!("Waiting for output from {subject} (Ctrl-C to stop)…");
            0
        }
    };

    let stop = install_sigint_flag()?;
    let mut tail = LogTail::new(path.clone(), offset);
    let mut backoff = PollBackoff::new();
    while !stop.load(Ordering::Relaxed) {
        let had_data = if let Some(buf) = tail.poll() {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(&buf);
            let _ = stdout.flush();
            true
        } else {
            false
        };
        backoff.note(had_data);
        sleep_interruptibly(backoff.interval(), &stop);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_returns_none_for_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = LogTail::new(tmp.path().join("missing.log"), 0);
        assert!(tail.poll().is_none());
    }

    #[test]
    fn poll_detects_appended_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        fs::write(&path, b"first\n").unwrap();

        let mut tail = LogTail::new(path.clone(), 0);
        assert_eq!(tail.poll().unwrap(), b"first\n");
        // Nothing new yet.
        assert!(tail.poll().is_none());

        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"second\n").unwrap();
        drop(f);

        assert_eq!(tail.poll().unwrap(), b"second\n");
    }

    #[test]
    fn poll_restarts_from_zero_on_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        fs::write(&path, b"0123456789").unwrap();

        let mut tail = LogTail::new(path.clone(), 0);
        assert_eq!(tail.poll().unwrap(), b"0123456789");

        // Simulate rotation: file replaced with something shorter than the
        // last-seen offset.
        fs::write(&path, b"new").unwrap();
        assert_eq!(tail.poll().unwrap(), b"new");
    }

    #[test]
    fn poll_starts_mid_file_from_given_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        fs::write(&path, b"0123456789").unwrap();

        let mut tail = LogTail::new(path, 5);
        assert_eq!(tail.poll().unwrap(), b"56789");
    }

    #[test]
    fn static_tail_reports_a_missing_file_as_absent() {
        // The one case that legitimately means "no output yet" — callers key
        // their "No output files yet" / "Waiting for output" notices off it.
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_static_tail(&tmp.path().join("missing.log"), 100).is_none());
    }

    #[test]
    fn static_tail_keeps_only_the_last_n_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        let body: String = (0..250).map(|i| format!("line {i}\n")).collect();
        fs::write(&path, &body).unwrap();

        let seed = read_static_tail(&path, 100).unwrap();
        assert_eq!(seed.lines.len(), 100);
        assert_eq!(seed.lines[0], "line 150");
        assert_eq!(seed.lines[99], "line 249");
        assert_eq!(seed.offset, body.len() as u64);
    }

    #[test]
    fn static_tail_displays_a_file_with_invalid_utf8() {
        // The old `read_to_string` returned Err here, so the file was
        // reported as *missing* even though it was sitting there full of
        // output. It must display instead, with the bad bytes replaced.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        fs::write(&path, b"good\n\xff\xfe bad bytes\ntrailing\n").unwrap();

        let seed = read_static_tail(&path, 100).unwrap();
        assert_eq!(seed.lines.len(), 3);
        assert_eq!(seed.lines[0], "good");
        assert!(seed.lines[1].contains("bad bytes"));
        assert!(seed.lines[1].contains('\u{fffd}'));
        assert_eq!(seed.lines[2], "trailing");
    }

    #[test]
    fn static_tail_offset_is_raw_bytes_not_decoded_chars() {
        // `U+FFFD` is three bytes replacing one, so the decoded string is
        // longer than the file: an offset taken from it would make the first
        // poll skip real bytes (or, on a shrinking file, re-print them).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        let raw = b"line one\n\xff\xff\xff\nline three\n";
        fs::write(&path, raw).unwrap();

        let seed = read_static_tail(&path, 100).unwrap();
        assert_eq!(seed.offset, raw.len() as u64);
        // The decoded backlog really is longer than the file it came from.
        let decoded: usize = seed.lines.iter().map(|l| l.len() + 1).sum();
        assert!(decoded > raw.len());

        // And resuming there sees nothing new — no double-printed tail.
        let mut tail = LogTail::new(path.clone(), seed.offset);
        assert!(tail.poll().is_none());
        // Only genuinely new bytes come back.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"line four\n").unwrap();
        drop(f);
        assert_eq!(tail.poll().unwrap(), b"line four\n");
    }

    #[test]
    fn static_tail_reads_only_a_bounded_window_of_a_huge_file() {
        // A file bigger than SEED_BYTES must not be slurped whole, but must
        // still leave the follow positioned at its true end.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        let filler = "x".repeat(4095);
        let mut body = String::new();
        while body.len() < (SEED_BYTES as usize) + 1024 * 1024 {
            body.push_str(&filler);
            body.push('\n');
        }
        body.push_str("the very last line\n");
        fs::write(&path, &body).unwrap();

        let seed = read_static_tail(&path, 100).unwrap();
        assert_eq!(seed.offset, body.len() as u64);
        assert_eq!(seed.lines.last().unwrap(), "the very last line");
        // Only the window was read: the backlog can't exceed it, and is far
        // short of the whole file.
        let read: usize = seed.lines.iter().map(|l| l.len() + 1).sum();
        assert!(read <= SEED_BYTES as usize);
        assert!((read as u64) < body.len() as u64);
        // ...and the follow starts clean at the end.
        let mut tail = LogTail::new(path, seed.offset);
        assert!(tail.poll().is_none());
    }

    #[test]
    fn static_tail_drops_the_partial_line_when_the_window_starts_mid_file() {
        // The window lands mid-line by construction; that fragment must not
        // be printed as though it were a line of its own.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        let mut body = "A".repeat(SEED_BYTES as usize);
        body.push_str("cut-here\nwhole line\n");
        fs::write(&path, &body).unwrap();

        let seed = read_static_tail(&path, 100).unwrap();
        assert_eq!(seed.lines, vec!["whole line".to_string()]);
        assert!(!seed.lines.iter().any(|l| l.contains("cut-here")));
        assert_eq!(seed.offset, body.len() as u64);
    }

    #[test]
    fn backoff_starts_at_floor() {
        let backoff = PollBackoff::new();
        assert_eq!(backoff.interval(), PollBackoff::FLOOR);
    }

    #[test]
    fn backoff_holds_the_floor_through_the_grace_period() {
        let mut backoff = PollBackoff::new();
        for _ in 0..PollBackoff::GRACE_ROUNDS {
            backoff.note(false);
            assert_eq!(backoff.interval(), PollBackoff::FLOOR);
        }
        // The round after the grace period is the first to grow.
        backoff.note(false);
        assert_eq!(backoff.interval(), PollBackoff::FLOOR * 2);
    }

    #[test]
    fn backoff_doubles_past_grace_and_clamps_to_ceiling() {
        let mut backoff = PollBackoff::new();
        let mut seen = Vec::new();
        for _ in 0..(PollBackoff::GRACE_ROUNDS + 10) {
            backoff.note(false);
            seen.push(backoff.interval());
        }
        // Doubles each empty round once the grace period is spent...
        assert_eq!(seen[PollBackoff::GRACE_ROUNDS as usize], PollBackoff::FLOOR * 2);
        assert_eq!(seen[PollBackoff::GRACE_ROUNDS as usize + 1], PollBackoff::FLOOR * 4);
        // ...until it clamps at the ceiling and stays there.
        assert!(seen.iter().all(|d| *d <= PollBackoff::CEILING));
        assert_eq!(*seen.last().unwrap(), PollBackoff::CEILING);
    }

    #[test]
    fn backoff_resets_interval_and_grace_on_data() {
        let mut backoff = PollBackoff::new();
        for _ in 0..(PollBackoff::GRACE_ROUNDS + 10) {
            backoff.note(false);
        }
        assert_eq!(backoff.interval(), PollBackoff::CEILING);
        backoff.note(true);
        assert_eq!(backoff.interval(), PollBackoff::FLOOR);
        // Data also re-arms the grace period, not just the interval.
        backoff.note(false);
        assert_eq!(backoff.interval(), PollBackoff::FLOOR);
    }

    #[test]
    fn backoff_stays_at_floor_for_steady_slow_producers() {
        // A writer slower than the floor but faster than the grace period —
        // e.g. one line every 4 rounds (~200ms) — must never leave the
        // floor, or delivery turns bursty (the "bouncy cadence" bug).
        let mut backoff = PollBackoff::new();
        for round in 0..100 {
            backoff.note(round % 4 == 0);
            assert_eq!(backoff.interval(), PollBackoff::FLOOR);
        }
    }
}
