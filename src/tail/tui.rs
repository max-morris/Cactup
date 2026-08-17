//! Full-screen side-by-side TUI for `FollowMode::Both` when stdout is a
//! terminal: stdout live-tails in the left pane, stderr in the right, each
//! independently scrollable. [`crate::tail::follow_combined`] is the
//! non-TTY fallback ([`follow_tui`] is only ever entered after that check).
//!
//! This module is split into two halves on purpose: plain data/state
//! (`ScrollState`, `PaneContent`, `FramePacer`, `sanitize`) that's exercised
//! directly by the unit tests at the bottom, and the terminal/event-loop
//! plumbing around it that isn't practical to unit-test and is kept as thin
//! as possible instead.
//!
//! # Reading and rendering are decoupled, on purpose
//!
//! They used to be one loop on one thread, which made the *terminal* the
//! pacer for *file polling* — exactly backwards on the links this tool is
//! used over. ~8KB/s of Cactus output is ~100 lines per second, so by the
//! time a frame is drawn every cell of a pane has changed: ratatui's damage
//! diff saves nothing and writes the whole grid, tens of KB of escape
//! sequences, into an SSH pty whose buffer eventually fills and blocks. With
//! one loop, a `draw()` blocking for two seconds delayed the next
//! [`LogTail::poll`] by two seconds, so the backlog — and the visible lag —
//! grew without bound instead of settling at some fixed cost.
//!
//! So the two are split: a [reader thread](reader_loop) owns the
//! [`LogTail`]s and the [`PollBackoff`] and appends into `Mutex`-guarded
//! [`PaneContent`]s, while the main thread owns the terminal, the input and
//! the rendering. The reader never touches the terminal and the renderer
//! never touches a file, so a slow write cannot delay a poll.
//!
//! Coordination between them is a single dirty *flag*, never a queue, so any
//! number of data arrivals during one slow repaint collapse into one
//! subsequent frame showing the final state. Dropping frames is not merely
//! tolerable here, it is correct: a pane retains [`MAX_LINES`] and displays
//! about forty of them, so the overwhelming majority of what is read is
//! discarded anyway — the user needs the *freshest* tail, not every
//! intermediate one. [`FramePacer`] then paces each frame off the
//! *completion* of the previous one, so how fast the terminal really is sets
//! the frame rate.

use super::{LogTail, PollBackoff, SEED_BYTES};
use crate::Res;
use anyhow::Context;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Lines retained per pane; oldest are dropped once this is exceeded.
const MAX_LINES: usize = 10_000;
/// Columns panned per Left/Right keypress or horizontal wheel notch.
const PAN_STEP: u16 = 8;
/// Lines scrolled per mouse-wheel notch.
const WHEEL_STEP: u16 = 3;
/// Longest the main loop ever blocks in one `event::poll`. File polling
/// happens on the reader thread, so this only bounds two things: how long a
/// keypress can sit unnoticed, and how long a Ctrl-C takes to wind the TUI
/// down (the interrupt contract in `CLAUDE.md`).
const TICK: Duration = Duration::from_millis(50);
/// Longest the *reader* thread sleeps in one go between checks of its stop
/// flags. `PollBackoff` can ask for up to a second; chunking the wait keeps
/// the thread's join latency well inside the interrupt contract's budget.
const READER_NAP: Duration = Duration::from_millis(25);

// ---------------------------------------------------------------------
// Scroll / follow state — pure, unit-tested.
// ---------------------------------------------------------------------

/// Per-pane vertical-scroll and auto-follow state. This is the core UX rule
/// from the spec, factored out so it can be tested without a terminal:
/// while `follow` is set the view is pinned to the bottom and new lines
/// auto-scroll; any upward move releases it; reaching the bottom again (or
/// `End`/`G`) re-engages it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollState {
    offset: u16,
    follow: bool,
}

impl ScrollState {
    fn new() -> Self {
        ScrollState { offset: 0, follow: true }
    }

    /// Scroll up (toward older lines) by `n`. Always releases follow, even
    /// if already at the top — any upward gesture pauses it per spec.
    fn up(&mut self, n: u16) {
        self.offset = self.offset.saturating_sub(n);
        self.follow = false;
    }

    /// Scroll down (toward newer lines) by `n`, clamped to `max`.
    /// Re-engages follow once the bottom is reached.
    fn down(&mut self, n: u16, max: u16) {
        self.offset = self.offset.saturating_add(n).min(max);
        if self.offset >= max {
            self.follow = true;
        }
    }

    fn top(&mut self) {
        self.offset = 0;
        self.follow = false;
    }

    fn bottom(&mut self, max: u16) {
        self.offset = max;
        self.follow = true;
    }

    /// Pin to `max` when following (content may have grown or the viewport
    /// resized since the last call), then clamp regardless. Returns the
    /// resolved offset to render with. Call this once per pane per draw,
    /// right before rendering.
    fn resolve(&mut self, max: u16) -> u16 {
        if self.follow {
            self.offset = max;
        } else {
            self.offset = self.offset.min(max);
        }
        self.offset
    }
}

// ---------------------------------------------------------------------
// Frame pacing — pure, unit-tested.
// ---------------------------------------------------------------------

/// Shortest gap between one frame *finishing* and the next being allowed to
/// start: the cap on the top frame rate (~30/s), so a fast local terminal
/// plus a chatty producer can't burn a core repainting hundreds of times a
/// second for frames nobody can read apart.
const MIN_FRAME_GAP: Duration = Duration::from_millis(33);
/// Longest such gap, however slow the terminal turns out to be. Together
/// with the draw itself this bounds the worst-case period at
/// `cost + MAX_FRAME_GAP`, i.e. it keeps the *minimum* frame rate from
/// collapsing further than the terminal itself forces.
const MAX_FRAME_GAP: Duration = Duration::from_millis(500);

/// Decides *when* the next repaint may start. Rendering is paced off the
/// moment the previous frame **finished**, never off when data arrived:
///
/// - A fixed frame rate would be wrong in both directions. On a slow link a
///   fixed 10Hz is strictly worse than the one-frame-per-data-round loop
///   this replaces, because it queues up more work than the terminal can
///   drain; on a fast local terminal it needlessly throttles.
/// - Pacing off completion makes the terminal's own speed the controller.
///   The effective period is `draw cost + gap`, so a terminal that takes 2s
///   to paint settles near 0.4 frames/s all by itself, while one that takes
///   2ms runs at the [`MIN_FRAME_GAP`] cap. Nothing has to *measure* the
///   link: a blocked write is simply a frame that hasn't finished yet, and
///   an unfinished frame can't schedule its successor.
///
/// The gap itself also grows with the smoothed cost of recent frames
/// (clamped to [`MIN_FRAME_GAP`]..=[`MAX_FRAME_GAP`]), so a struggling
/// terminal gets idle time between repaints rather than having the next one
/// queued the instant it draws breath. Cost is envelope-followed with a
/// fast attack and slow decay — the same shape `PollBackoff` uses on the
/// reading side — so one slow frame backs the rate off immediately while
/// recovery is gradual, instead of oscillating between the two rates.
///
/// Nothing here can starve rendering: the schedule depends only on the last
/// frame's finish time, never on data arrivals, so once the gap has elapsed
/// the next dirty round draws. Freshly-arrived data can only ever make a
/// frame happen, never postpone one.
#[derive(Debug, Clone, Copy)]
struct FramePacer {
    /// When the last frame finished, or `None` before the first one (which
    /// is due immediately).
    last_finish: Option<Instant>,
    /// Envelope-followed cost of a repaint: fast attack, slow decay.
    cost: Duration,
}

impl FramePacer {
    fn new() -> Self {
        FramePacer { last_finish: None, cost: Duration::ZERO }
    }

    /// Required idle gap between the last frame finishing and the next one
    /// starting, given how expensive recent frames have been.
    fn gap(&self) -> Duration {
        self.cost.clamp(MIN_FRAME_GAP, MAX_FRAME_GAP)
    }

    /// How long until a repaint may start. `ZERO` means "now"; the caller
    /// uses this as its input-poll timeout so it wakes up exactly on time.
    fn due_in(&self, now: Instant) -> Duration {
        match self.last_finish {
            None => Duration::ZERO,
            Some(t) => (t + self.gap()).saturating_duration_since(now),
        }
    }

    fn due(&self, now: Instant) -> bool {
        self.due_in(now).is_zero()
    }

    /// Record a completed frame: `started`/`finished` bracket the whole
    /// repaint, including the blocking write to the terminal, which is the
    /// part we're actually pacing against.
    fn note_frame(&mut self, started: Instant, finished: Instant) {
        let sample = finished.saturating_duration_since(started);
        self.cost = if sample > self.cost {
            sample
        } else {
            (self.cost * 3 + sample) / 4
        };
        self.last_finish = Some(finished);
    }
}

// ---------------------------------------------------------------------
// Sanitization — pure, unit-tested.
// ---------------------------------------------------------------------

/// Sanitize a chunk of freshly-read log bytes for display: lossily decode
/// UTF-8, strip ANSI CSI/OSC escape sequences and other control bytes
/// (newlines aside), and expand tabs to the next multiple of 8 columns.
///
/// `col` tracks the visual column across calls (reset to 0 on every
/// newline) so a tab landing right after a chunk boundary — mid-line,
/// before the poll that completes it — still lands on the column-correct
/// stop. An escape sequence split across a chunk boundary is the one case
/// this can't handle perfectly: the trailing fragment of an unterminated
/// sequence is passed through literally on the next call rather than
/// re-joined, which is a rare enough log-writing pattern not to be worth
/// the extra state.
pub(crate) fn sanitize(raw: &[u8], col: &mut usize) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next(); // consume '['
                    // CSI: parameter/intermediate bytes are 0x20-0x3F, the
                    // sequence ends at the first final byte, 0x40-0x7E.
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next(); // consume ']'
                    // OSC: terminated by BEL or ESC \ (ST).
                    while let Some(c2) = chars.next() {
                        if c2 == '\x07' {
                            break;
                        }
                        if c2 == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next(); // two-char escape, e.g. ESC ( B
                }
                None => {}
            },
            '\n' => {
                out.push('\n');
                *col = 0;
            }
            '\t' => {
                let spaces = 8 - (*col % 8);
                for _ in 0..spaces {
                    out.push(' ');
                }
                *col += spaces;
            }
            c if c.is_control() => { /* drop other control bytes (CR, BEL, ...) */ }
            c => {
                out.push(c);
                *col += 1;
            }
        }
    }
    out
}

/// The half of a pane the reader thread writes and the main thread reads,
/// behind a `Mutex`. It deliberately holds no view state: the scroll
/// position and horizontal pan belong to the main thread alone, so the
/// reader can never move the user's viewport out from under them — and
/// never has to know a viewport exists.
struct PaneContent {
    /// Whether the underlying file has been seen to exist yet — before that
    /// the pane shows a "waiting for ..." placeholder.
    exists: bool,
    lines: VecDeque<String>,
    /// True if the last entry in `lines` has no trailing newline yet, i.e.
    /// the next ingest should extend it rather than start a new line.
    open: bool,
    tab_col: usize,
    /// Lines dropped off the front of `lines` since the main thread last
    /// looked. Eviction used to shift the scroll offset in place; now that
    /// it happens on the reader thread the count is left here instead and
    /// [`Pane::apply_evictions`] applies it, which preserves exactly the
    /// same "a paused view stays on the same text" behavior without the
    /// reader reaching into view state.
    evicted: usize,
}

impl PaneContent {
    fn new() -> Self {
        PaneContent {
            exists: false,
            lines: VecDeque::new(),
            open: false,
            tab_col: 0,
            evicted: 0,
        }
    }

    /// Sanitize and append newly-read bytes, splitting into lines and
    /// carrying over any trailing partial line to the next call.
    fn ingest(&mut self, raw: &[u8]) {
        if raw.is_empty() {
            return;
        }
        let text = sanitize(raw, &mut self.tab_col);
        if text.is_empty() {
            return;
        }
        // A trailing newline just closes out the current line — it doesn't
        // introduce a new (empty) one — so strip it before splitting rather
        // than dealing with the bogus empty trailing segment `split` would
        // otherwise hand back.
        let ends_with_newline = text.ends_with('\n');
        let body = if ends_with_newline { &text[..text.len() - 1] } else { &text[..] };
        let mut parts = body.split('\n');
        // The first segment always continues whatever's already open (the
        // partial-line carry-over), or starts a fresh line otherwise.
        let first = parts.next().expect("split always yields at least one item");
        if self.open {
            if let Some(last) = self.lines.back_mut() {
                last.push_str(first);
            } else {
                self.lines.push_back(first.to_string());
            }
        } else {
            self.lines.push_back(first.to_string());
        }
        for seg in parts {
            self.lines.push_back(seg.to_string());
        }
        self.open = !ends_with_newline;
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            // Record it so the view can be kept stable as the oldest line
            // drops off; the shift itself is the main thread's business.
            self.evicted += 1;
        }
    }
}

/// One pane: the shared content above, plus the view state that only the
/// main thread ever touches.
struct Pane {
    label: String,
    path: PathBuf,
    content: Arc<Mutex<PaneContent>>,
    scroll: ScrollState,
    hscroll: u16,
}

impl Pane {
    /// Seed a pane from the trailing [`SEED_BYTES`] of `path` (if it exists
    /// yet), capped at [`MAX_LINES`], and hand back a `LogTail` positioned
    /// to pick up exactly where the seed read left off (the reader thread
    /// takes ownership of it). Only the tail is read — sim logs can run to
    /// tens of GB, far past what the pane can retain anyway.
    fn seeded(label: &str, path: PathBuf) -> (Self, LogTail) {
        use std::io::{Read, Seek, SeekFrom};
        let mut content = PaneContent::new();
        let mut tail = LogTail::new(path.clone(), 0);
        if let Ok(mut f) = std::fs::File::open(&path) {
            content.exists = true;
            let start = f.metadata().map_or(0, |m| m.len().saturating_sub(SEED_BYTES));
            let mut seeded = Vec::new();
            if f.seek(SeekFrom::Start(start)).is_ok() && f.read_to_end(&mut seeded).is_ok() {
                let mut seed = &seeded[..];
                if start > 0 {
                    // Started mid-file: drop the leading partial line.
                    seed = match seed.iter().position(|&b| b == b'\n') {
                        Some(i) => &seed[i + 1..],
                        None => &[],
                    };
                }
                content.ingest(seed);
                tail = LogTail::new(path.clone(), start + seeded.len() as u64);
            }
        }
        let pane = Pane {
            label: label.to_string(),
            path,
            content: Arc::new(Mutex::new(content)),
            scroll: ScrollState::new(),
            hscroll: 0,
        };
        (pane, tail)
    }

    fn lock(&self) -> MutexGuard<'_, PaneContent> {
        lock(&self.content)
    }

    /// Fold any lines the reader has evicted since the last call into the
    /// scroll offset, so a paused view keeps showing the same text as the
    /// buffer slides out from under it. Draining the counter makes this
    /// idempotent, so it's safe to call as often as the loop likes.
    fn apply_evictions(&mut self) {
        let evicted = std::mem::take(&mut self.lock().evicted);
        if evicted > 0 {
            let by = u16::try_from(evicted).unwrap_or(u16::MAX);
            self.scroll.offset = self.scroll.offset.saturating_sub(by);
        }
    }

    fn max_scroll(&self, inner_height: u16) -> u16 {
        (self.lock().lines.len() as u16).saturating_sub(inner_height)
    }

    fn max_line_width(&self) -> usize {
        max_line_width(&self.lock().lines)
    }
}

/// Take a lock, tolerating poisoning. A panic in the reader thread must not
/// take the renderer's terminal restoration down with it: the buffer is
/// plain data with no invariant a half-finished `ingest` could break, so
/// carrying on with whatever it holds is strictly better than a second
/// panic (see `install_panic_hook`).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------
// Terminal plumbing / event loop.
// ---------------------------------------------------------------------

type Backend = CrosstermBackend<std::io::Stdout>;

/// Entry point for `FollowMode::Both` on a TTY: a full-screen side-by-side
/// TUI of stdout (left) and stderr (right). Callers must have already
/// checked `stdout().is_terminal()`; the non-TTY fallback is
/// [`crate::tail::follow_combined`].
pub(crate) fn follow_tui(sources: &[(&str, PathBuf); 2], subject: &str) -> Res<()> {
    debug_assert!(std::io::stdout().is_terminal());
    let stop = super::install_sigint_flag()?;
    install_panic_hook();

    let mut terminal = setup_terminal().context("setting up the log-follow TUI")?;
    let (out_pane, out_tail) = Pane::seeded(sources[0].0, sources[0].1.clone());
    let (err_pane, err_tail) = Pane::seeded(sources[1].0, sources[1].1.clone());
    let result = run_app(&mut terminal, [out_pane, err_pane], [out_tail, err_tail], subject, &stop);
    restore_terminal();
    result
}

fn setup_terminal() -> Res<Terminal<Backend>> {
    enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("entering the alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("creating the ratatui terminal")
}

/// Best-effort: leave raw mode / the alternate screen / mouse capture,
/// ignoring errors since this runs on every exit path, including after a
/// panic or mid-error, when the terminal may already be in a mixed state.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

/// Chain onto the existing panic hook so a panic inside the TUI still
/// restores the terminal before the default (or any other) hook prints.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

/// `(FollowMode::Out's label, stderr's label)` indexed the same way as
/// `sources`/`panes` throughout this module: `0` is stdout, `1` is stderr.
const STDOUT: usize = 0;
const STDERR: usize = 1;

/// The file-reading half of the TUI, running on its own thread: poll both
/// `LogTail`s on `PollBackoff`'s schedule, append into the shared
/// [`PaneContent`]s, and raise `dirty` when anything landed.
///
/// This is the whole point of the split. Nothing in here can block on the
/// terminal — the thread never writes to it — so however long a repaint
/// takes, polling keeps its own cadence and the buffers stay current. The
/// lock is held only for the `ingest` of a chunk already in memory
/// (microseconds), never across I/O in either direction.
///
/// Per the interrupt contract it polls two flags at fine granularity:
/// `stop` (the shared SIGINT flag, so Ctrl-C winds the reader down with
/// everything else) and `quit` (set by [`ReaderThread`]'s `Drop` when the
/// TUI exits by any other route, including `?` on a draw error).
fn reader_loop(
    mut tails: [LogTail; 2],
    contents: [Arc<Mutex<PaneContent>>; 2],
    dirty: &AtomicBool,
    quit: &AtomicBool,
    stop: &AtomicBool,
) {
    let mut backoff = PollBackoff::new();
    let running = || !quit.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed);
    while running() {
        let mut had_data = false;
        for (tail, content) in tails.iter_mut().zip(contents.iter()) {
            let Some(buf) = tail.poll() else { continue };
            let mut content = lock(content);
            content.exists = true;
            content.ingest(&buf);
            had_data = true;
        }
        if had_data {
            // Set *after* the data is visible under the lock, so a renderer
            // that sees the flag is guaranteed to see the lines behind it.
            dirty.store(true, Ordering::Release);
        }
        backoff.note(had_data);
        // Chunked so both flags are seen promptly even at the 1s ceiling.
        let mut remaining = backoff.interval();
        while !remaining.is_zero() && running() {
            let chunk = remaining.min(READER_NAP);
            std::thread::sleep(chunk);
            remaining -= chunk;
        }
    }
}

/// Owns the reader thread and guarantees it never outlives the TUI: `Drop`
/// asks it to quit and joins it, on every exit path — normal quit, Ctrl-C,
/// or an error propagated out of [`run_app`] with `?`. Joining before
/// [`restore_terminal`] also means no stray thread is still running while
/// the terminal is being put back.
struct ReaderThread {
    quit: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ReaderThread {
    fn spawn(
        tails: [LogTail; 2],
        contents: [Arc<Mutex<PaneContent>>; 2],
        dirty: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let quit = Arc::new(AtomicBool::new(false));
        let thread_quit = Arc::clone(&quit);
        let handle = std::thread::spawn(move || {
            reader_loop(tails, contents, &dirty, &thread_quit, &stop);
        });
        ReaderThread { quit, handle: Some(handle) }
    }
}

impl Drop for ReaderThread {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // Bounded by one READER_NAP plus a poll — far inside the
            // "wind-down feels instant" budget.
            let _ = handle.join();
        }
    }
}

fn run_app(
    terminal: &mut Terminal<Backend>,
    mut panes: [Pane; 2],
    tails: [LogTail; 2],
    subject: &str,
    stop: &Arc<AtomicBool>,
) -> Res<()> {
    // The subject arrives styled for the plain-text notices (colored's
    // `.bold()` embeds SGR sequences in the string); an ESC inside the
    // title's OSC sequence aborts it and dumps the rest as literal bold
    // text into the grid — where ratatui's diff can't see it to repaint.
    // Strip every escape before it goes anywhere near the title.
    let _ = queue!(
        std::io::stdout(),
        crossterm::terminal::SetTitle(format!("cactup log — {}", sanitize(subject.as_bytes(), &mut 0)))
    );

    let mut focused = STDOUT;
    // Cached pane content-rects from the last draw, used to size PgUp/PgDn
    // and to hit-test mouse events between redraws.
    let mut layout = (Rect::default(), Rect::default());

    // Raised by the reader thread whenever bytes land. A *flag*, not a
    // queue: three arrivals during one slow repaint leave it set once, and
    // the single frame that follows shows the state after all three. That is
    // what stops a backlog from forming — there is nothing to back up.
    let content_dirty = Arc::new(AtomicBool::new(false));
    // Dropped (and so joined) before we return, on every path.
    let _reader = ReaderThread::spawn(
        tails,
        [Arc::clone(&panes[STDOUT].content), Arc::clone(&panes[STDERR].content)],
        Arc::clone(&content_dirty),
        Arc::clone(stop),
    );

    // View-side dirtiness (keys, mouse, resize), which the reader knows
    // nothing about; ORed with `content_dirty` to decide whether to draw.
    let mut view_dirty = false;
    let mut pacer = FramePacer::new();
    let started = Instant::now();
    draw(terminal, &mut panes, focused, &mut layout)?;
    pacer.note_frame(started, Instant::now());

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        for pane in &mut panes {
            pane.apply_evictions();
        }

        // Block for input until the next frame is due (or TICK, whichever
        // comes first, so Ctrl-C is still noticed promptly). When nothing is
        // dirty there's no frame pending, so wait out the whole tick.
        let dirty = view_dirty || content_dirty.load(Ordering::Acquire);
        let timeout = if dirty { pacer.due_in(Instant::now()).min(TICK) } else { TICK };
        if event::poll(timeout).context("polling terminal events")? {
            match event::read().context("reading a terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(key, &mut panes, &mut focused, layout) {
                        return Ok(());
                    }
                    view_dirty = true;
                }
                Event::Mouse(mouse) => {
                    handle_mouse(mouse, &mut panes, &mut focused, layout);
                    view_dirty = true;
                }
                Event::Resize(..) => view_dirty = true,
                _ => {}
            }
        }

        let dirty = view_dirty || content_dirty.load(Ordering::Acquire);
        let now = Instant::now();
        if dirty && pacer.due(now) {
            // Clear *before* drawing: anything the reader appends while the
            // frame is in flight re-raises the flag and is picked up by the
            // next one, rather than being swallowed by a clear afterwards.
            content_dirty.store(false, Ordering::Relaxed);
            view_dirty = false;
            for pane in &mut panes {
                pane.apply_evictions();
            }
            draw(terminal, &mut panes, focused, &mut layout)?;
            // Timed from `now` (before the draw) to the moment it lands, so
            // the pacer sees the true cost of a frame, blocking write and
            // all, and paces the next one off its completion.
            pacer.note_frame(now, Instant::now());
        }
    }
}

/// Handle one key press. Returns `true` if the app should quit.
fn handle_key(key: KeyEvent, panes: &mut [Pane; 2], focused: &mut usize, layout: (Rect, Rect)) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    let pane_rect = if *focused == STDOUT { layout.0 } else { layout.1 };
    let inner_height = pane_rect.height.saturating_sub(2);
    let page = inner_height.saturating_sub(1).max(1);
    let pane = &mut panes[*focused];
    let max = pane.max_scroll(inner_height);
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Tab | KeyCode::BackTab => *focused = 1 - *focused,
        KeyCode::Up | KeyCode::Char('k') => pane.scroll.up(1),
        KeyCode::Down | KeyCode::Char('j') => pane.scroll.down(1, max),
        KeyCode::PageUp => pane.scroll.up(page),
        KeyCode::PageDown => pane.scroll.down(page, max),
        KeyCode::Home => pane.scroll.top(),
        KeyCode::Char('g') => pane.scroll.top(),
        KeyCode::End | KeyCode::Char('G') => pane.scroll.bottom(max),
        KeyCode::Left | KeyCode::Char('h') => pane.hscroll = pane.hscroll.saturating_sub(PAN_STEP),
        KeyCode::Right | KeyCode::Char('l') => {
            let max_h = pane.max_line_width();
            let inner_width = pane_rect.width.saturating_sub(2) as usize;
            let max_hscroll = max_h.saturating_sub(inner_width).min(u16::MAX as usize) as u16;
            pane.hscroll = (pane.hscroll + PAN_STEP).min(max_hscroll);
        }
        _ => {}
    }
    false
}

fn handle_mouse(mouse: MouseEvent, panes: &mut [Pane; 2], focused: &mut usize, layout: (Rect, Rect)) {
    let pos = (mouse.column, mouse.row);
    let hit = if in_rect(pos, layout.0) {
        Some(STDOUT)
    } else if in_rect(pos, layout.1) {
        Some(STDERR)
    } else {
        None
    };
    let Some(i) = hit else { return };
    let rect = if i == STDOUT { layout.0 } else { layout.1 };
    let inner_height = rect.height.saturating_sub(2);
    match mouse.kind {
        MouseEventKind::Down(_) => *focused = i,
        MouseEventKind::ScrollUp => {
            *focused = i;
            panes[i].scroll.up(WHEEL_STEP);
        }
        MouseEventKind::ScrollDown => {
            *focused = i;
            let max = panes[i].max_scroll(inner_height);
            panes[i].scroll.down(WHEEL_STEP, max);
        }
        _ => {}
    }
}

fn in_rect(pos: (u16, u16), rect: Rect) -> bool {
    let (x, y) = pos;
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn max_line_width(lines: &VecDeque<String>) -> usize {
    lines.iter().map(|l| l.chars().count()).max().unwrap_or(0)
}

/// Split the full terminal area into (stdout pane, stderr pane, footer).
fn compute_layout(area: Rect) -> (Rect, Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    (cols[0], cols[1], rows[1])
}

fn draw(
    terminal: &mut Terminal<Backend>,
    panes: &mut [Pane; 2],
    focused: usize,
    layout: &mut (Rect, Rect),
) -> Res<()> {
    // ratatui-crossterm doesn't wrap draws in a synchronized update itself
    // (it just diffs and writes cells), so we do it here to avoid a
    // half-painted frame flashing on slow links.
    let _ = execute!(std::io::stdout(), BeginSynchronizedUpdate);
    terminal
        .draw(|frame| {
            let (left, right, footer) = compute_layout(frame.area());
            *layout = (left, right);
            render_pane(frame, left, &mut panes[STDOUT], focused == STDOUT);
            render_pane(frame, right, &mut panes[STDERR], focused == STDERR);
            render_footer(frame, footer);
        })
        .context("drawing the log-follow TUI")?;
    let _ = execute!(std::io::stdout(), EndSynchronizedUpdate);
    Ok(())
}

fn render_pane(frame: &mut ratatui::Frame, area: Rect, pane: &mut Pane, focused: bool) {
    let inner_height = area.height.saturating_sub(2);
    // Snapshot everything this frame needs from the shared buffer in one
    // short critical section — the visible slice is only ~40 lines, so the
    // clones are trivial and the reader thread gets the lock straight back.
    // Nothing below touches it again, which is what keeps the lock off the
    // path of the (potentially blocking) terminal write in `draw`.
    let (exists, line_count, visible) = {
        // Locked through the field rather than `pane.lock()` so the guard
        // borrows only `pane.content`, leaving `pane.scroll` free to move.
        let mut content = lock(&pane.content);
        // Fold in anything evicted since the loop last looked, so `offset`
        // is resolved against the buffer as it is right now.
        let evicted = std::mem::take(&mut content.evicted);
        if evicted > 0 {
            let by = u16::try_from(evicted).unwrap_or(u16::MAX);
            pane.scroll.offset = pane.scroll.offset.saturating_sub(by);
        }
        let count = content.lines.len();
        let max = (count as u16).saturating_sub(inner_height);
        let offset = pane.scroll.resolve(max);
        // Hand the Paragraph only the visible slice — vertical scrolling is
        // done here by slicing at `offset` (building all MAX_LINES Lines per
        // frame just for Paragraph to skip them is wasted work), horizontal
        // panning is left to `scroll`.
        let visible: Vec<Line> = content
            .lines
            .iter()
            .skip(offset as usize)
            .take(inner_height as usize)
            .map(|l| Line::from(l.clone()))
            .collect();
        (content.exists, count, visible)
    };
    let max = (line_count as u16).saturating_sub(inner_height);
    let offset = pane.scroll.offset;

    let indicator = if pane.scroll.follow {
        "● live".to_string()
    } else {
        format!("⏸ +{}", max - offset)
    };
    let file_name = pane
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| pane.path.display().to_string());

    let border_style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(format!("{} — {file_name}", pane.label)).left_aligned())
        .title(Line::from(indicator).right_aligned());

    if !exists && line_count == 0 {
        let waiting = Line::from(Span::styled(
            format!("waiting for {}…", pane.path.display()),
            Style::new().add_modifier(Modifier::DIM),
        ));
        frame.render_widget(Paragraph::new(waiting).block(block), area);
        return;
    }

    let paragraph = Paragraph::new(visible).block(block).scroll((0, pane.hscroll));
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect) {
    let footer = Paragraph::new("Tab focus · ↑/↓ PgUp/PgDn scroll · ←/→ pan · End follow · q quit")
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(footer, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ScrollState ----------------------------------------------------

    #[test]
    fn scroll_starts_following_at_top() {
        let s = ScrollState::new();
        assert!(s.follow);
        assert_eq!(s.offset, 0);
    }

    #[test]
    fn scroll_up_releases_follow_even_at_top() {
        let mut s = ScrollState::new();
        s.up(1);
        assert!(!s.follow);
        assert_eq!(s.offset, 0); // saturating: can't go negative
    }

    #[test]
    fn scroll_down_to_bottom_reengages_follow() {
        let mut s = ScrollState::new();
        s.up(5); // pause, say offset went to 0 with max elsewhere > 0
        s.offset = 3;
        s.follow = false;
        s.down(10, 10); // overshoot clamps to max and re-engages
        assert_eq!(s.offset, 10);
        assert!(s.follow);
    }

    #[test]
    fn scroll_down_short_of_bottom_stays_paused() {
        let mut s = ScrollState { offset: 2, follow: false };
        s.down(3, 10);
        assert_eq!(s.offset, 5);
        assert!(!s.follow);
    }

    #[test]
    fn top_and_bottom_helpers() {
        let mut s = ScrollState::new();
        s.top();
        assert_eq!((s.offset, s.follow), (0, false));
        s.bottom(42);
        assert_eq!((s.offset, s.follow), (42, true));
    }

    #[test]
    fn resolve_pins_to_max_while_following() {
        let mut s = ScrollState::new();
        assert_eq!(s.resolve(7), 7);
        assert_eq!(s.offset, 7);
    }

    #[test]
    fn resolve_clamps_when_not_following_and_content_shrinks() {
        let mut s = ScrollState { offset: 20, follow: false };
        assert_eq!(s.resolve(5), 5);
        assert_eq!(s.offset, 5);
    }

    // -- sanitize ---------------------------------------------------------

    #[test]
    fn sanitize_strips_csi_sequences() {
        let mut col = 0;
        let out = sanitize(b"\x1b[31mred\x1b[0m plain", &mut col);
        assert_eq!(out, "red plain");
    }

    #[test]
    fn sanitize_strips_osc_terminated_by_bel() {
        let mut col = 0;
        let out = sanitize(b"\x1b]0;window title\x07visible", &mut col);
        assert_eq!(out, "visible");
    }

    #[test]
    fn sanitize_strips_osc_terminated_by_st() {
        let mut col = 0;
        let out = sanitize(b"\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\", &mut col);
        assert_eq!(out, "link");
    }

    #[test]
    fn sanitize_drops_other_control_bytes_but_keeps_newline() {
        let mut col = 0;
        let out = sanitize(b"a\rb\x07c\nd", &mut col);
        assert_eq!(out, "abc\nd");
    }

    #[test]
    fn sanitize_expands_tabs_to_next_multiple_of_8() {
        let mut col = 0;
        let out = sanitize(b"a\tb", &mut col);
        // 'a' -> col 1, tab fills to col 8 (7 spaces), then 'b'.
        assert_eq!(out, "a       b");
        assert_eq!(out.len(), 9);
    }

    #[test]
    fn sanitize_tab_column_state_carries_across_calls() {
        let mut col = 0;
        let first = sanitize(b"12345", &mut col); // col now 5
        assert_eq!(first, "12345");
        let second = sanitize(b"\tX", &mut col); // tab from col 5 -> 8: 3 spaces
        assert_eq!(second, "   X");
    }

    #[test]
    fn sanitize_resets_tab_column_on_newline() {
        let mut col = 0;
        let out = sanitize(b"1234\n\tX", &mut col);
        // newline resets column to 0, so the tab fills a full 8 spaces.
        assert_eq!(out, "1234\n        X");
    }

    #[test]
    fn sanitize_lossy_decodes_invalid_utf8() {
        let mut col = 0;
        let raw = [b'a', 0xFF, b'b'];
        let out = sanitize(&raw, &mut col);
        assert!(out.starts_with('a') && out.ends_with('b'));
        assert!(out.contains('\u{FFFD}'));
    }

    // -- PaneContent::ingest partial-line carry-over ----------------------

    fn test_content() -> PaneContent {
        let mut content = PaneContent::new();
        content.exists = true;
        content
    }

    /// A pane with no file behind it, wrapping `content` — for the view-side
    /// state the reader thread never touches.
    fn test_pane(content: PaneContent) -> Pane {
        Pane {
            label: "stdout".to_string(),
            path: PathBuf::from("/nonexistent"),
            content: Arc::new(Mutex::new(content)),
            scroll: ScrollState::new(),
            hscroll: 0,
        }
    }

    #[test]
    fn ingest_splits_complete_lines() {
        let mut content = test_content();
        content.ingest(b"one\ntwo\nthree\n");
        assert_eq!(
            content.lines,
            VecDeque::from(["one".to_string(), "two".to_string(), "three".to_string()])
        );
        assert!(!content.open);
    }

    #[test]
    fn ingest_carries_partial_line_to_next_poll() {
        let mut content = test_content();
        content.ingest(b"partial line, no newline yet");
        assert_eq!(content.lines.len(), 1);
        assert!(content.open);
        content.ingest(b" continues here\n");
        assert_eq!(content.lines.len(), 1);
        assert_eq!(content.lines[0], "partial line, no newline yet continues here");
        assert!(!content.open);
    }

    #[test]
    fn ingest_partial_then_more_partial_then_newline() {
        let mut content = test_content();
        content.ingest(b"a");
        content.ingest(b"b");
        content.ingest(b"c\nd");
        assert_eq!(content.lines, VecDeque::from(["abc".to_string(), "d".to_string()]));
        assert!(content.open);
    }

    #[test]
    fn ingest_enforces_line_cap_and_records_evictions() {
        let mut content = test_content();
        for i in 0..(MAX_LINES + 10) {
            content.ingest(format!("line{i}\n").as_bytes());
        }
        assert_eq!(content.lines.len(), MAX_LINES);
        assert_eq!(content.lines.front().unwrap(), "line10");
        // The reader can't touch view state, so it leaves the count for the
        // main thread to fold in.
        assert_eq!(content.evicted, 10);
    }

    #[test]
    fn apply_evictions_shifts_the_view_and_drains_the_count() {
        let mut content = test_content();
        for i in 0..(MAX_LINES + 10) {
            content.ingest(format!("line{i}\n").as_bytes());
        }
        let mut pane = test_pane(content);
        pane.scroll.follow = false;
        pane.scroll.offset = 25;
        pane.apply_evictions();
        // 10 lines were evicted, so the same text stays under the cursor.
        assert_eq!(pane.scroll.offset, 15);
        // Idempotent: the count is drained, so a second call is a no-op.
        pane.apply_evictions();
        assert_eq!(pane.scroll.offset, 15);
        assert_eq!(pane.lock().evicted, 0);
    }

    #[test]
    fn apply_evictions_never_scrolls_past_the_top() {
        let mut content = test_content();
        for i in 0..(MAX_LINES + 10) {
            content.ingest(format!("line{i}\n").as_bytes());
        }
        let mut pane = test_pane(content);
        pane.scroll.follow = false;
        pane.scroll.offset = 5;
        pane.apply_evictions();
        assert_eq!(pane.scroll.offset, 0);
    }

    // -- in_rect / layout hit-testing -------------------------------------

    #[test]
    fn in_rect_hit_test() {
        let rect = Rect { x: 5, y: 2, width: 10, height: 4 };
        assert!(in_rect((5, 2), rect));
        assert!(in_rect((14, 5), rect));
        assert!(!in_rect((15, 2), rect)); // one past the right edge
        assert!(!in_rect((5, 6), rect)); // one past the bottom edge
        assert!(!in_rect((4, 2), rect));
    }

    // -- FramePacer -------------------------------------------------------

    #[test]
    fn pacer_draws_the_first_frame_immediately() {
        let pacer = FramePacer::new();
        let now = Instant::now();
        assert_eq!(pacer.due_in(now), Duration::ZERO);
        assert!(pacer.due(now));
    }

    #[test]
    fn pacer_caps_the_top_frame_rate() {
        // A local terminal that repaints instantly must not be asked to do
        // it hundreds of times a second just because the producer is chatty.
        let mut pacer = FramePacer::new();
        let t = Instant::now();
        pacer.note_frame(t, t);
        assert_eq!(pacer.due_in(t), MIN_FRAME_GAP);
        assert!(!pacer.due(t + MIN_FRAME_GAP - Duration::from_millis(1)));
        assert!(pacer.due(t + MIN_FRAME_GAP));
    }

    #[test]
    fn pacer_paces_off_frame_completion_not_frame_start() {
        // The whole point: the clock for the next frame starts when the
        // previous one *finished* writing to the terminal.
        let mut pacer = FramePacer::new();
        let start = Instant::now();
        let finish = start + Duration::from_millis(200);
        pacer.note_frame(start, finish);
        assert!(!pacer.due(finish));
        assert_eq!(pacer.due_in(finish), pacer.gap());
        assert!(pacer.due(finish + pacer.gap()));
    }

    #[test]
    fn pacer_backs_off_when_frames_are_slow() {
        let mut pacer = FramePacer::new();
        let t = Instant::now();
        // A fast frame sits at the top-rate cap...
        pacer.note_frame(t, t + Duration::from_millis(2));
        assert_eq!(pacer.gap(), MIN_FRAME_GAP);
        // ...and one slow frame backs the gap off immediately (fast attack),
        // rather than needing several to notice.
        let t = t + Duration::from_secs(1);
        pacer.note_frame(t, t + Duration::from_millis(120));
        assert_eq!(pacer.gap(), Duration::from_millis(120));
        // However bad it gets, the gap is bounded — the terminal's own cost
        // is what dominates the period beyond that point.
        let t = t + Duration::from_secs(1);
        pacer.note_frame(t, t + Duration::from_secs(2));
        assert_eq!(pacer.gap(), MAX_FRAME_GAP);
    }

    #[test]
    fn pacer_recovers_slowly_from_a_slow_frame() {
        // Slow decay: a link that stalls once shouldn't snap straight back
        // to full rate on the next frame and start the cycle over.
        let mut pacer = FramePacer::new();
        let mut t = Instant::now();
        pacer.note_frame(t, t + Duration::from_secs(2));
        assert_eq!(pacer.gap(), MAX_FRAME_GAP);
        let mut frames = 0;
        while pacer.gap() > MIN_FRAME_GAP {
            t += Duration::from_secs(1);
            pacer.note_frame(t, t); // instant frames from here on
            frames += 1;
            assert!(frames < 100, "cost never decayed back to the floor");
        }
        assert!(frames > 3, "recovered after only {frames} frames — too abrupt");
    }

    // -- frame coalescing / backlog ---------------------------------------

    #[derive(Debug)]
    struct FrameSim {
        arrivals: u32,
        frames: u32,
        /// Worst delay between bytes landing in a pane and the frame that
        /// first put them on screen.
        worst_staleness: Duration,
        /// Most arrivals ever waiting to be shown. Bounded == no backlog.
        max_pending: u32,
    }

    /// Replay the main loop's draw scheduling over a virtual timeline
    /// without a terminal: data arrives every `arrival_every`, every repaint
    /// costs `draw_cost` (which is what a blocked pty write looks like from
    /// here), and the clock jumps between events. Arrivals only ever set a
    /// flag — exactly as the reader thread does — so this measures whether
    /// coalescing really collapses them.
    fn simulate_frames(arrival_every: Duration, draw_cost: Duration, span: Duration) -> FrameSim {
        // `Instant` can't be constructed from thin air, so offsets from one
        // real instant stand in for the virtual clock.
        let origin = Instant::now();
        let mut pacer = FramePacer::new();
        let mut now = Duration::ZERO;
        let mut next_arrival = arrival_every;
        let mut dirty = false;
        let mut oldest_unshown: Option<Duration> = None;
        let mut pending = 0u32;
        let mut sim =
            FrameSim { arrivals: 0, frames: 0, worst_staleness: Duration::ZERO, max_pending: 0 };

        while now < span {
            let due_at = now + pacer.due_in(origin + now);
            // The loop wakes on whichever comes first; a tie is resolved in
            // favour of the arrival, which is the pessimistic order (the
            // frame then renders it one iteration later).
            let next = if dirty { next_arrival.min(due_at) } else { next_arrival };
            now = next;
            if now >= next_arrival {
                sim.arrivals += 1;
                pending += 1;
                dirty = true;
                oldest_unshown.get_or_insert(now);
                next_arrival += arrival_every;
                continue;
            }
            // Draw: the flag is cleared before the frame starts, so anything
            // arriving mid-frame sets it again for the next one.
            let (start, finish) = (now, now + draw_cost);
            let shown = pending;
            let oldest = oldest_unshown.take();
            dirty = false;
            while next_arrival <= finish {
                sim.arrivals += 1;
                pending += 1;
                dirty = true;
                oldest_unshown.get_or_insert(next_arrival);
                next_arrival += arrival_every;
            }
            pending -= shown;
            sim.max_pending = sim.max_pending.max(pending);
            if let Some(t) = oldest {
                sim.worst_staleness = sim.worst_staleness.max(finish - t);
            }
            sim.frames += 1;
            now = finish;
            pacer.note_frame(origin + start, origin + finish);
        }
        sim
    }

    #[test]
    fn many_arrivals_between_frames_collapse_into_one_frame() {
        // The measured field case: ~100 lines/s of output into a terminal
        // that needs a second to repaint. One frame per data round (the old
        // loop) would mean ten frames queued per second of drawing; here
        // every arrival during a frame collapses into the single frame that
        // follows it.
        let sim = simulate_frames(
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_secs(60),
        );
        assert!(sim.arrivals > 500, "{sim:?}");
        assert!(
            sim.frames * 5 < sim.arrivals,
            "{} frames for {} arrivals — barely coalescing",
            sim.frames,
            sim.arrivals
        );
        // Nothing queues: the number of arrivals still waiting to be shown
        // is whatever landed during one frame, not a growing backlog.
        let per_frame = (Duration::from_secs(1) + MAX_FRAME_GAP).as_millis() / 100 + 1;
        assert!(sim.max_pending as u128 <= per_frame, "{sim:?}");
    }

    #[test]
    fn staleness_settles_instead_of_growing_without_bound() {
        // The actual bug: with reading and drawing in one loop the lag grew
        // for as long as you watched. Pacing off frame completion bounds it,
        // no matter how long the run is. The bound is two frames plus a gap:
        // bytes landing just after a frame starts wait out the rest of that
        // frame, then the gap, then the whole frame that shows them.
        let bound = 2 * Duration::from_secs(1) + MAX_FRAME_GAP + Duration::from_millis(100);
        let short = simulate_frames(
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_secs(30),
        );
        let long = simulate_frames(
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_secs(600),
        );
        assert!(short.worst_staleness <= bound, "{short:?}");
        assert!(long.worst_staleness <= bound, "{long:?}");
        // Twenty times the runtime, the same lag.
        assert_eq!(short.worst_staleness, long.worst_staleness);
    }

    #[test]
    fn a_fast_terminal_stays_responsive_but_bounded() {
        // The other end of the range: an instant local terminal runs at the
        // top-rate cap, not at the producer's rate and not unbounded.
        let span = Duration::from_secs(10);
        let sim = simulate_frames(Duration::from_millis(10), Duration::from_millis(1), span);
        assert!(sim.frames as u128 <= span.as_millis() / MIN_FRAME_GAP.as_millis() + 1, "{sim:?}");
        assert!(sim.frames < sim.arrivals, "{sim:?}");
        // Still visibly live: at most one frame gap of lag.
        assert!(sim.worst_staleness <= MIN_FRAME_GAP + Duration::from_millis(20), "{sim:?}");
    }

    #[test]
    fn rendering_is_never_starved_by_a_relentless_producer() {
        // Data arriving faster than frames can be drawn must not be able to
        // hold a frame off: the schedule depends only on the last frame's
        // finish time, never on arrivals.
        let sim = simulate_frames(
            Duration::from_millis(1),
            Duration::from_millis(800),
            Duration::from_secs(30),
        );
        assert!(sim.frames >= 20, "{sim:?}");
        assert!(sim.worst_staleness <= 2 * Duration::from_millis(800) + MAX_FRAME_GAP, "{sim:?}");
    }

    // -- reader thread ----------------------------------------------------

    /// Append `lines` lines to `path`, one every `every`, until told to stop.
    fn spawn_writer(
        path: PathBuf,
        every: Duration,
        stop: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            use std::io::Write as _;
            let mut i = 0u32;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(every);
                i += 1;
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&path) {
                    let _ = writeln!(f, "line {i}");
                }
            }
        })
    }

    struct Rig {
        panes: [Pane; 2],
        dirty: Arc<AtomicBool>,
        sigint: Arc<AtomicBool>,
        reader: Option<ReaderThread>,
        writers: Vec<std::thread::JoinHandle<()>>,
        writers_stop: Arc<AtomicBool>,
    }

    impl Rig {
        /// Two empty log files with writers appending to them, and a real
        /// reader thread ingesting into real panes — everything `run_app`
        /// has except the terminal.
        fn start(dir: &std::path::Path, write_every: Duration) -> Self {
            let (out, err) = (dir.join("run.out"), dir.join("run.err"));
            std::fs::write(&out, b"").unwrap();
            std::fs::write(&err, b"").unwrap();
            let (out_pane, out_tail) = Pane::seeded("stdout", out.clone());
            let (err_pane, err_tail) = Pane::seeded("stderr", err.clone());
            let dirty = Arc::new(AtomicBool::new(false));
            let sigint = Arc::new(AtomicBool::new(false));
            let reader = ReaderThread::spawn(
                [out_tail, err_tail],
                [Arc::clone(&out_pane.content), Arc::clone(&err_pane.content)],
                Arc::clone(&dirty),
                Arc::clone(&sigint),
            );
            let writers_stop = Arc::new(AtomicBool::new(false));
            let writers = vec![
                spawn_writer(out, write_every, Arc::clone(&writers_stop)),
                spawn_writer(err, write_every * 2, Arc::clone(&writers_stop)),
            ];
            Rig {
                panes: [out_pane, err_pane],
                dirty,
                sigint,
                reader: Some(reader),
                writers,
                writers_stop,
            }
        }

        fn lines(&self, i: usize) -> usize {
            self.panes[i].lock().lines.len()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.writers_stop.store(true, Ordering::Relaxed);
            drop(self.reader.take());
            for w in self.writers.drain(..) {
                let _ = w.join();
            }
        }
    }

    #[test]
    fn a_slow_draw_does_not_delay_file_reading() {
        // The direct regression test for the reported bug. The main thread
        // is stuck for 600ms in what used to be the same loop as the poll —
        // a blocked write to a slow pty. Ingestion must carry on regardless.
        let tmp = tempfile::tempdir().unwrap();
        let rig = Rig::start(tmp.path(), Duration::from_millis(20));

        // Let the reader settle, then take a reading either side of a long,
        // blocking "draw".
        std::thread::sleep(Duration::from_millis(200));
        let before = rig.lines(STDOUT);
        std::thread::sleep(Duration::from_millis(600)); // the slow repaint
        let after = rig.lines(STDOUT);

        // The writer produced ~30 lines during that stall; the old design
        // would have read none of them until the draw returned. Ten is a
        // deliberately loose floor for a loaded CI box.
        assert!(
            after - before >= 10,
            "only {} lines ingested during a 600ms blocked draw (before {before}, after {after})",
            after - before
        );
        assert!(rig.dirty.load(Ordering::Acquire), "the reader never raised the dirty flag");
    }

    #[test]
    fn the_reader_keeps_both_panes_current() {
        let tmp = tempfile::tempdir().unwrap();
        let rig = Rig::start(tmp.path(), Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(400));
        assert!(rig.lines(STDOUT) > 0);
        assert!(rig.lines(STDERR) > 0);
        // Ingested in order and with the line assembly intact.
        assert_eq!(rig.panes[STDOUT].lock().lines[0], "line 1");
        assert!(rig.lines(STDOUT) > rig.lines(STDERR), "stderr is written half as often");
    }

    #[test]
    fn the_reader_thread_winds_down_promptly_on_ctrl_c() {
        // Interrupt contract: the shared SIGINT flag stops the reader, and
        // dropping the handle (what `run_app` does on every exit path) joins
        // it well inside the "wind-down feels instant" budget.
        let tmp = tempfile::tempdir().unwrap();
        let mut rig = Rig::start(tmp.path(), Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(100));
        rig.sigint.store(true, Ordering::Relaxed);
        let t = Instant::now();
        let reader = rig.reader.take().expect("reader still running");
        drop(reader);
        assert!(t.elapsed() < Duration::from_millis(250), "join took {:?}", t.elapsed());
    }

    #[test]
    fn dropping_the_handle_stops_the_reader_even_without_a_signal() {
        // The error/`?` path out of `run_app`: no SIGINT, but the thread
        // must still not outlive the TUI.
        let tmp = tempfile::tempdir().unwrap();
        let mut rig = Rig::start(tmp.path(), Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        drop(rig.reader.take());
        assert!(t.elapsed() < Duration::from_millis(250), "join took {:?}", t.elapsed());
        assert!(!rig.sigint.load(Ordering::Relaxed));
        // And it really stopped: the buffer stops growing while the writer
        // keeps writing.
        let after_stop = rig.lines(STDOUT);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(rig.lines(STDOUT), after_stop);
    }

    // -- manual/tmux smoke test -------------------------------------------
    //
    // `follow_tui` needs a real controlling terminal (raw mode, alternate
    // screen) — not something `cargo test`'s normal harness gives it — and
    // it blocks in its own event loop until `q`/Esc/Ctrl-C, so it can't run
    // as part of the automated suite. It's `#[ignore]`d for that reason,
    // not because it's untested: this was driven manually with `tmux`
    // (`send-keys` + `capture-pane`) against exactly this test to smoke-test
    // the real TUI end to end without adding any new CLI surface — see the
    // task's final report for that session's transcript. Kept here so the
    // same drive can be repeated by hand:
    //   cargo test --features "" tail::tui::tests::smoke_manual_tmux -- --ignored --exact --nocapture
    #[test]
    #[ignore = "interactive: needs a real tty, run manually inside tmux (see comment above)"]
    fn smoke_manual_tmux() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("run.out");
        let err = tmp.path().join("run.err");
        std::fs::write(&out, "seed stdout line 1\nseed stdout line 2\n").unwrap();
        std::fs::write(&err, "seed stderr line 1\n").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let (out2, err2) = (out.clone(), err.clone());
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let mut i = 0u32;
            while !writer_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                i += 1;
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&out2) {
                    let _ = writeln!(f, "stdout tick {i}");
                }
                if i.is_multiple_of(2)
                    && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&err2)
                {
                    let _ = writeln!(f, "stderr tick {i}");
                }
            }
        });

        let sources: [(&str, PathBuf); 2] = [("stdout", out), ("stderr", err)];
        let _ = follow_tui(&sources, "smoke test");

        stop.store(true, Ordering::Relaxed);
        let _ = writer.join();
    }
}
