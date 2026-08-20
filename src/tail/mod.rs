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

mod clipboard;
mod search;
mod tui;

/// Adaptive poll-interval backoff shared by every follow loop and by the
/// TUI's poll scheduling. We poll on a timer rather than use inotify (or
/// similar) because sim logs frequently live on network filesystems —
/// Lustre, NFS — where inotify events don't reliably fire. But those same
/// filesystems charge a metadata RPC per `stat`, so an idle follow must not
/// hammer them either: we hold the floor for near-live latency while a sim
/// is actively writing, and back off exponentially (capped at the ceiling)
/// once it has really gone quiet, snapping back to the floor the moment new
/// bytes show up.
///
/// Backing off begins only after a *grace period* of accumulated quiet.
/// Without one, a producer whose write cadence is slower than the interval
/// sits in an oscillation — interval grows past the cadence, a clump of
/// output lands, reset to the floor, repeat — which reads as bursty, bouncy
/// output in the follow views, and (worse) means we routinely sleep past
/// bytes that were already readable.
///
/// The grace period is measured in **elapsed quiet time, not rounds**,
/// because rounds are not a fixed unit once the interval starts growing.
/// The previous tuning (10 empty rounds ≈ 500ms) was sized for a producer
/// emitting a couple of lines per second, which is not what real sim output
/// looks like: the writer's stdio is block-buffered and its stdout goes
/// through the batch scheduler to a shared filesystem, so bytes arrive as
/// one ~8192-byte block roughly once every 0.9s — about 1.1 rounds-with-data
/// per second. Measured against a live job, the 500ms grace expired before
/// each block landed, the interval climbed 100 → 200 → … → 1000ms, and gaps
/// between consecutive writes by the follower hit 2.06s and once 3.06s
/// against a producer appending every ~0.9s. (The filesystem was not the
/// constraint: `stat` and `read` on that file both measured well under 5ms.)
///
/// So the grace period is at least [`Self::GRACE_MIN`] — comfortably longer
/// than that ~0.9s block cadence plus jitter — which pins a 1Hz block
/// writer to the floor indefinitely, for a worst-case added latency of one
/// floor interval (50ms).
///
/// A fixed grace only stretches so far, so we also remember the producer's
/// rhythm: [`Self::cadence`] is an envelope follower over the quiet
/// stretches that preceded recent rounds-with-data — it jumps straight to a
/// longer gap and decays only slowly toward shorter ones, so a *bursty*
/// producer (several rounds with data, then a long pause) is still paced by
/// its long pause rather than by its burst. The grace period is twice that
/// cadence, clamped to [`Self::GRACE_MIN`]..=[`Self::GRACE_MAX`], so any
/// roughly-periodic producer up to a ~1.5s cadence stays pinned at the
/// floor, and slower ones back off by at most one doubling step per further
/// quiet interval.
///
/// A truly dead log — a queued job, a finished run left open overnight —
/// still walks to the ceiling within ~3s (typical) to ~4.5s (worst case,
/// after a slow producer) of its last byte, and idles there at one `stat`
/// per second per followed file.
///
/// Quiet time is accumulated by summing the intervals we scheduled rather
/// than by reading a clock: it keeps the pacer pure and unit-testable
/// without sleeping, and it errs in the safe direction — a real round takes
/// the sleep *plus* the poll work, so we slightly under-count elapsed time
/// and are therefore slightly more patient than the constants suggest,
/// never less.
pub(crate) struct PollBackoff {
    current: std::time::Duration,
    /// Quiet time accumulated since the last round that saw bytes, summed
    /// from the intervals this pacer scheduled.
    quiet: std::time::Duration,
    /// Envelope-followed estimate of the producer's inter-arrival gap: the
    /// quiet stretch preceding recent rounds-with-data, attacking fast and
    /// decaying slowly (see the type docs).
    cadence: std::time::Duration,
}

impl PollBackoff {
    const FLOOR: std::time::Duration = std::time::Duration::from_millis(50);
    const CEILING: std::time::Duration = std::time::Duration::from_millis(1000);
    /// Shortest grace period: quiet time tolerated at the floor before
    /// growth starts, even for a producer we know nothing about yet. Sized
    /// to clear the measured ~0.9s block-write cadence with margin.
    const GRACE_MIN: std::time::Duration = std::time::Duration::from_millis(1500);
    /// Longest grace period, however slow the observed cadence: bounds how
    /// long a dead log keeps polling at the floor before it starts backing
    /// off.
    const GRACE_MAX: std::time::Duration = std::time::Duration::from_millis(3000);

    pub fn new() -> Self {
        PollBackoff {
            current: Self::FLOOR,
            quiet: std::time::Duration::ZERO,
            cadence: std::time::Duration::ZERO,
        }
    }

    /// The interval to wait before the next poll.
    pub fn interval(&self) -> std::time::Duration {
        self.current
    }

    /// How much accumulated quiet is tolerated at the current interval
    /// before growth starts, given the producer's observed cadence.
    fn grace(&self) -> std::time::Duration {
        self.cadence.saturating_mul(2).clamp(Self::GRACE_MIN, Self::GRACE_MAX)
    }

    /// Record the outcome of a polling round: any round that saw new bytes
    /// (from any source) resets to the floor and re-arms the grace period;
    /// an empty round adds the interval we just waited out to the quiet
    /// total, and once that total passes the grace period each further
    /// empty round doubles the interval, clamped to the ceiling.
    pub fn note(&mut self, had_data: bool) {
        if had_data {
            // Fast attack, slow decay: a single burst of back-to-back data
            // rounds (gap ≈ 0) must not erase what we learned about this
            // producer's long pauses.
            let gap = self.quiet;
            self.cadence = if gap > self.cadence {
                gap
            } else {
                (self.cadence.saturating_mul(3) + gap) / 4
            };
            self.current = Self::FLOOR;
            self.quiet = std::time::Duration::ZERO;
        } else {
            self.quiet = self.quiet.saturating_add(self.current);
            if self.quiet >= self.grace() {
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

    /// Drive a backoff against a producer that appends strictly every
    /// `period`, simulating time by summing the intervals the pacer asks
    /// for (exactly how the pacer accounts for quiet time, and how the real
    /// follow loops spend it). Returns the widest interval the pacer ever
    /// asked for and the worst latency between a byte becoming readable and
    /// the round that picked it up, ignoring the first `warmup` of
    /// simulated time (during which the pacer has yet to observe the
    /// producer's cadence at all).
    fn drive_periodic(
        period: std::time::Duration,
        rounds: u32,
        warmup: std::time::Duration,
    ) -> (std::time::Duration, std::time::Duration) {
        let mut backoff = PollBackoff::new();
        let mut now = std::time::Duration::ZERO;
        let mut next_write = period;
        let mut worst_interval = std::time::Duration::ZERO;
        let mut worst_latency = std::time::Duration::ZERO;
        for _ in 0..rounds {
            // Sleep the interval the pacer asked for, then poll.
            now += backoff.interval();
            let had_data = now >= next_write;
            if had_data {
                if now >= warmup {
                    worst_latency = worst_latency.max(now - next_write);
                }
                // The producer keeps its own rhythm regardless of when we
                // got around to noticing.
                while next_write <= now {
                    next_write += period;
                }
            }
            backoff.note(had_data);
            if now >= warmup {
                worst_interval = worst_interval.max(backoff.interval());
            }
        }
        assert!(now > warmup, "not enough rounds to get past the warmup");
        (worst_interval, worst_latency)
    }

    #[test]
    fn backoff_holds_the_floor_through_the_grace_period() {
        let mut backoff = PollBackoff::new();
        let mut quiet = std::time::Duration::ZERO;
        // With no cadence learned yet the grace period is GRACE_MIN.
        while quiet + PollBackoff::FLOOR < PollBackoff::GRACE_MIN {
            backoff.note(false);
            quiet += PollBackoff::FLOOR;
            assert_eq!(backoff.interval(), PollBackoff::FLOOR);
        }
        // The round that takes accumulated quiet past the grace period is
        // the first to grow.
        backoff.note(false);
        assert_eq!(backoff.interval(), PollBackoff::FLOOR * 2);
        // A grace period measured in time, not rounds: GRACE_MIN of it.
        assert_eq!(quiet + PollBackoff::FLOOR, PollBackoff::GRACE_MIN);
    }

    #[test]
    fn backoff_doubles_past_grace_and_clamps_to_ceiling() {
        let mut backoff = PollBackoff::new();
        let mut seen = Vec::new();
        let mut quiet = std::time::Duration::ZERO;
        for _ in 0..80 {
            quiet += backoff.interval();
            backoff.note(false);
            seen.push((quiet, backoff.interval()));
        }
        // Each further empty round doubles, once the grace period is spent.
        let grown: Vec<_> =
            seen.iter().skip_while(|(_, d)| *d == PollBackoff::FLOOR).map(|(_, d)| *d).collect();
        assert_eq!(grown[0], PollBackoff::FLOOR * 2);
        assert_eq!(grown[1], PollBackoff::FLOOR * 4);
        // ...until it clamps at the ceiling and stays there.
        assert!(seen.iter().all(|(_, d)| *d <= PollBackoff::CEILING));
        assert_eq!(seen.last().unwrap().1, PollBackoff::CEILING);
    }

    #[test]
    fn backoff_reaches_a_filesystem_friendly_interval_when_idle() {
        // A genuinely dead log (queued job, finished run left open) must
        // stop hammering the fileserver within a few seconds of the last
        // byte, and idle at one stat/sec.
        let mut backoff = PollBackoff::new();
        // Worst case: the producer's last observed cadence was slow, so the
        // grace period is at its maximum.
        for _ in 0..(PollBackoff::GRACE_MAX.as_millis() as u32
            / PollBackoff::FLOOR.as_millis() as u32)
        {
            backoff.note(false);
        }
        backoff.note(true);
        assert_eq!(backoff.grace(), PollBackoff::GRACE_MAX);

        let mut quiet = std::time::Duration::ZERO;
        while backoff.interval() < PollBackoff::CEILING {
            quiet += backoff.interval();
            backoff.note(false);
        }
        assert_eq!(backoff.interval(), PollBackoff::CEILING);
        assert!(
            quiet <= std::time::Duration::from_millis(4600),
            "idle log took {quiet:?} to reach the ceiling"
        );
        // And it stays there — one stat per second per followed file.
        for _ in 0..100 {
            backoff.note(false);
            assert_eq!(backoff.interval(), PollBackoff::CEILING);
        }
    }

    #[test]
    fn backoff_resets_interval_and_grace_on_data() {
        let mut backoff = PollBackoff::new();
        for _ in 0..200 {
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

    #[test]
    fn backoff_stays_at_floor_for_a_1hz_block_buffered_producer() {
        // The measured real-world shape this tuning exists for: a
        // block-buffered writer whose stdout lands as one ~8KB block about
        // once every 0.9s — roughly 1.1 rounds-with-data per second. Under
        // the old 10-empty-rounds (500ms) grace this producer climbed to
        // the ceiling and the follower slept past readable bytes for 2s+ at
        // a time. It must now sit at the floor for as long as the job runs,
        // not just for the first few rounds.
        // 4000 rounds at the floor is over three minutes of simulated time
        // — "sustained indefinitely", not "for the first few rounds".
        let (worst_interval, worst_latency) =
            drive_periodic(std::time::Duration::from_millis(900), 4_000, std::time::Duration::ZERO);
        assert_eq!(
            worst_interval,
            PollBackoff::FLOOR,
            "a ~1Hz block writer must never leave the floor"
        );
        assert!(
            worst_latency <= PollBackoff::FLOOR,
            "added latency {worst_latency:?} exceeds one floor interval"
        );

        // Same story with jitter either side of 1s.
        for period_ms in [773, 935, 1_075, 1_222, 1_460] {
            let (worst_interval, worst_latency) = drive_periodic(
                std::time::Duration::from_millis(period_ms),
                4_000,
                std::time::Duration::ZERO,
            );
            assert_eq!(
                worst_interval,
                PollBackoff::FLOOR,
                "a producer writing every {period_ms}ms left the floor"
            );
            assert!(worst_latency <= PollBackoff::FLOOR);
        }
    }

    #[test]
    fn backoff_learns_a_slow_periodic_cadence_instead_of_oscillating() {
        // Past GRACE_MIN the remembered cadence takes over: a producer with
        // a steady multi-second rhythm is paced by that rhythm rather than
        // bouncing between the floor and the ceiling on every block.
        // (The very first gap is longer than GRACE_MIN and there is nothing
        // learned yet, so one warmup cycle is allowed to back off.)
        let (worst_interval, worst_latency) = drive_periodic(
            std::time::Duration::from_millis(2_500),
            2_000,
            std::time::Duration::from_millis(6_000),
        );
        assert_eq!(worst_interval, PollBackoff::FLOOR);
        assert!(worst_latency <= PollBackoff::FLOOR);

        // And beyond even GRACE_MAX, growth is bounded by one doubling per
        // quiet interval, so delivery stays inside the ceiling.
        let (worst_interval, worst_latency) = drive_periodic(
            std::time::Duration::from_millis(6_000),
            2_000,
            std::time::Duration::from_millis(12_000),
        );
        assert!(worst_interval <= PollBackoff::CEILING);
        assert!(
            worst_latency <= PollBackoff::CEILING,
            "latency {worst_latency:?} for a 6s producer"
        );
    }

    #[test]
    fn backoff_cadence_memory_survives_a_burst() {
        // Bursty producer: a run of back-to-back data rounds every ~1.2s.
        // The burst's zero-length gaps must not wipe out what we learned
        // from the pause, or the next pause re-enters the oscillation.
        let mut backoff = PollBackoff::new();
        let mut now = std::time::Duration::ZERO;
        let mut next_burst = std::time::Duration::from_millis(1_200);
        for _ in 0..2_000 {
            now += backoff.interval();
            let mut had_data = false;
            if now >= next_burst {
                // Five rounds of data in a row, then quiet until the next
                // burst.
                for _ in 0..5 {
                    backoff.note(true);
                    now += backoff.interval();
                }
                next_burst += std::time::Duration::from_millis(1_200);
                while next_burst <= now {
                    next_burst += std::time::Duration::from_millis(1_200);
                }
                had_data = true;
            }
            if !had_data {
                backoff.note(false);
            }
            assert_eq!(backoff.interval(), PollBackoff::FLOOR);
        }
    }
}
