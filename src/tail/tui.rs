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
//!
//! # Copying and searching
//!
//! Two things a plain `tail -f` gets for free from the terminal and a
//! full-screen view has to hand back deliberately: copying a line, and
//! finding one. Mouse capture is what takes copying away — the terminal
//! gives us the drag instead of selecting with it — so both routes are
//! offered. `m` releases capture outright, restoring the terminal's own
//! selection, which works everywhere and is the fallback of record;
//! otherwise a vim-style line selection (`v` then motions, or click-drag)
//! copies through [`clipboard`]'s OSC 52 path, which also survives the SSH
//! hop this tool is nearly always used across. Searching is vim's too:
//! `/`, `?`, `n`, `N` over a [`search::Query`], per pane, incremental as
//! you type, and centered on the hit when it lands.

use super::search::{self, Direction, Hit, Query};
use super::{LogTail, PollBackoff, SEED_BYTES, clipboard};
use crate::Res;
use anyhow::Context;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
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
/// How long a footer notice ("copied 12 lines", a rejected pattern, a
/// wrapped search) stays up before the footer goes back to being a key
/// hint.
const STATUS_TTL: Duration = Duration::from_secs(3);
/// Columns of context kept to the left of a search hit when the view has to
/// pan sideways to show it.
const HIT_MARGIN: u16 = 8;

/// Every match of the active pattern in a pane.
const MATCH_STYLE: Style = Style::new().bg(Color::Yellow).fg(Color::Black);
/// The one match `n`/`N` are currently sitting on, picked out from the rest.
const CURRENT_MATCH_STYLE: Style =
    Style::new().bg(Color::Magenta).fg(Color::White).add_modifier(Modifier::BOLD);
/// Lines inside a `v` selection. Reversing the whole line reads as a block
/// however the user's palette is set up, where a background colour might
/// not.
const SELECT_STYLE: Style = Style::new().add_modifier(Modifier::REVERSED);

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
// Modes, notices and view math — pure, unit-tested.
// ---------------------------------------------------------------------

/// What keystrokes mean right now. `Normal` is the live-tailing view;
/// anything else is a modal overlay the footer describes, which is what
/// keeps the bindings from fighting each other — `q` quits in `Normal` and
/// types a `q` into the pattern in `Search`, with no modifier gymnastics.
enum Mode {
    Normal,
    /// vim's line-visual mode over the focused pane: an anchor line, a
    /// moving cursor, and `y` to copy the span between them.
    Select,
    /// The `/` (or `?`) prompt is open in the footer.
    Search(SearchPrompt),
}

/// A search being typed. The pattern lives here rather than on the pane so
/// an abandoned search leaves nothing behind, and the `saved_*` fields hold
/// everything Esc has to put back: incremental search moves the view (and
/// replaces the pane's pattern) on every keystroke, and vim's contract is
/// that giving up returns you to exactly where you started.
struct SearchPrompt {
    dir: Direction,
    input: String,
    saved_offset: u16,
    saved_follow: bool,
    saved_search: Option<PaneSearch>,
    /// What to show beside the prompt: a `[3/17]` counter, `no match`, or
    /// the regex error for a pattern that is still half-typed. Kept as text
    /// because all three occupy the same spot and only one can be true.
    note: String,
}

/// A transient one-line notice in the footer: what a copy did, why a
/// pattern was rejected, that a search wrapped. Timed out rather than
/// sticky, so the footer reverts to being a key hint on its own.
struct Status {
    text: String,
    error: bool,
    until: Instant,
}

impl Status {
    fn expired(&self, now: Instant) -> bool {
        now >= self.until
    }
}

/// Post a footer notice, replacing any current one — the newest thing the
/// user did is always the thing worth telling them about.
fn note(status: &mut Option<Status>, text: impl Into<String>, error: bool) {
    *status = Some(Status { text: text.into(), error, until: Instant::now() + STATUS_TTL });
}

/// Inclusive line span of a selection, lowest first and clamped to `len`.
/// `None` for an empty buffer, so callers can't build a span over nothing.
fn selection_span(anchor: usize, cursor: usize, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let (lo, hi) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    Some((lo.min(len - 1), hi.min(len - 1)))
}

/// Vertical offset that brings `line` into a viewport `height` tall,
/// centered when there's room either side. Search jumps center rather than
/// scroll minimally: a hit glued to the top or bottom edge is a hit without
/// the context that makes it readable.
fn center_offset(line: usize, height: u16, max: u16) -> u16 {
    let half = usize::from(height / 2);
    u16::try_from(line.saturating_sub(half)).unwrap_or(u16::MAX).min(max)
}

/// Vertical offset that keeps `cursor` on screen while moving it, scrolling
/// by the minimum needed. Unlike a search jump this must *not* recenter —
/// holding `j` through a selection would make the text crawl under a fixed
/// cursor instead of the cursor walking down the text.
fn scroll_to_show(cursor: usize, offset: u16, height: u16, max: u16) -> u16 {
    let height = usize::from(height.max(1));
    let cursor16 = u16::try_from(cursor).unwrap_or(u16::MAX);
    let offset = if cursor16 < offset {
        cursor16
    } else if cursor + 1 > usize::from(offset) + height {
        u16::try_from(cursor + 1 - height).unwrap_or(u16::MAX)
    } else {
        offset
    };
    offset.min(max)
}

/// Horizontal pan that brings char column `col` into a `width`-wide
/// viewport, keeping [`HIT_MARGIN`] columns of context to its left when it
/// has to move at all. A column that is already on screen returns `hscroll`
/// untouched — landing on a hit must not jog a pane sideways for nothing.
fn pan_to(col: usize, hscroll: u16, width: u16, max_hscroll: u16) -> u16 {
    let lo = usize::from(hscroll);
    if col >= lo && col < lo + usize::from(width) {
        return hscroll;
    }
    let target = col.saturating_sub(usize::from(HIT_MARGIN));
    u16::try_from(target).unwrap_or(u16::MAX).min(max_hscroll)
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
        PaneContent { exists: false, lines: VecDeque::new(), open: false, tab_col: 0, evicted: 0 }
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

/// A pane's live search: what to look for, where in the buffer we are, and
/// which way `n` travels. Per pane on purpose — stdout and stderr get
/// independent searches, which is most of the point of having two panes.
struct PaneSearch {
    query: Query,
    dir: Direction,
    hit: Option<Hit>,
    /// `(ordinal, total)` as of the last search step, for the `[3/17]`
    /// counter. Recomputed when the pattern or the hit moves, never per
    /// frame: scanning ten thousand lines is cheap but not free, and a
    /// count that lags the newest arrivals is exactly what vim shows too.
    count: (usize, usize),
}

/// One pane: the shared content above, plus the view state that only the
/// main thread ever touches.
struct Pane {
    label: String,
    path: PathBuf,
    content: Arc<Mutex<PaneContent>>,
    scroll: ScrollState,
    hscroll: u16,
    /// `(anchor, cursor)` line indices while a `v` selection is up. Held on
    /// the pane rather than in [`Mode::Select`] so rendering needs to know
    /// nothing about modes: a pane draws a selection exactly when it has
    /// one, and leaving select mode clears it.
    sel: Option<(usize, usize)>,
    search: Option<PaneSearch>,
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
            sel: None,
            search: None,
        };
        (pane, tail)
    }

    fn lock(&self) -> MutexGuard<'_, PaneContent> {
        lock(&self.content)
    }

    /// Fold any lines the reader has evicted since the last call into this
    /// pane's line indices, so a paused view keeps showing the same text as
    /// the buffer slides out from under it. Draining the counter makes this
    /// idempotent, so it's safe to call as often as the loop likes.
    fn apply_evictions(&mut self) {
        let evicted = std::mem::take(&mut self.lock().evicted);
        shift_view(&mut self.scroll, &mut self.sel, &mut self.search, evicted);
    }

    fn max_scroll(&self, inner_height: u16) -> u16 {
        (self.lock().lines.len() as u16).saturating_sub(inner_height)
    }

    fn max_line_width(&self) -> usize {
        max_line_width(&self.lock().lines)
    }

    fn len(&self) -> usize {
        lock(&self.content).lines.len()
    }

    /// Widest horizontal pan that still shows text, for an `inner_width`
    /// content area.
    fn max_hscroll(&self, inner_width: u16) -> u16 {
        let widest = self.max_line_width();
        u16::try_from(widest.saturating_sub(usize::from(inner_width))).unwrap_or(u16::MAX)
    }

    /// Pin the view on `hit` and record it as the current match: center it
    /// vertically, pan sideways only if its column is off screen, and
    /// release follow — a jump the user asked for must not be yanked away
    /// by the next line of output.
    fn focus_hit(&mut self, hit: Hit, inner: (u16, u16)) {
        let (inner_height, inner_width) = inner;
        let (len, col, widest) = {
            let content = lock(&self.content);
            let col = content.lines.get(hit.line).map_or(0, |l| char_col(l, hit.start));
            (content.lines.len(), col, max_line_width(&content.lines))
        };
        let max = u16::try_from(len).unwrap_or(u16::MAX).saturating_sub(inner_height);
        self.scroll.follow = false;
        self.scroll.offset = center_offset(hit.line, inner_height, max);
        let max_hscroll =
            u16::try_from(widest.saturating_sub(usize::from(inner_width))).unwrap_or(u16::MAX);
        self.hscroll = pan_to(col, self.hscroll, inner_width, max_hscroll);
        if let Some(search) = &mut self.search {
            search.hit = Some(hit);
        }
        self.refresh_count();
    }

    /// Recompute the `[3/17]` counter. Called after anything that moves the
    /// hit or changes the pattern, and nowhere else (see [`PaneSearch`]).
    fn refresh_count(&mut self) {
        let Some(search) = &self.search else { return };
        let count = match search.hit {
            Some(hit) => search::hit_ordinal(&search.query, &lock(&self.content).lines, hit),
            None => (0, 0),
        };
        if let Some(search) = &mut self.search {
            search.count = count;
        }
    }

    /// Where a search starts when there is no current hit: the top of what
    /// is on screen going forward, the bottom going backward, so the first
    /// hit landed on is the first one not already read.
    fn view_origin(&self, dir: Direction, inner_height: u16) -> (usize, usize) {
        match dir {
            Direction::Forward => (usize::from(self.scroll.offset), 0),
            Direction::Backward => {
                let last = usize::from(self.scroll.offset)
                    .saturating_add(usize::from(inner_height))
                    .min(self.len())
                    .saturating_sub(1);
                (last, usize::MAX)
            }
        }
    }

    /// Clear the search on this pane: pattern, highlight and hit together.
    fn clear_search(&mut self) {
        self.search = None;
    }
}

/// Slide every line index a pane's view holds down by `by` evicted lines:
/// the scroll offset, the selection, and the current search hit all name
/// positions in a buffer whose front just moved. Indices that fall off the
/// front saturate at 0 rather than wrapping — the text they named is gone,
/// and the oldest surviving line is the honest answer.
///
/// Taken field-by-field rather than as `&mut Pane` so it can also be called
/// with the content lock held, which is how [`render_pane`] folds in an
/// eviction and reads the lines it caused in one critical section.
fn shift_view(
    scroll: &mut ScrollState,
    sel: &mut Option<(usize, usize)>,
    search: &mut Option<PaneSearch>,
    by: usize,
) {
    if by == 0 {
        return;
    }
    scroll.offset = scroll.offset.saturating_sub(u16::try_from(by).unwrap_or(u16::MAX));
    if let Some((anchor, cursor)) = sel {
        *anchor = anchor.saturating_sub(by);
        *cursor = cursor.saturating_sub(by);
    }
    if let Some(search) = search
        && let Some(hit) = &mut search.hit
    {
        hit.line = hit.line.saturating_sub(by);
    }
}

/// Char column of byte offset `at` within `line`. Tolerates an offset that
/// no longer lands on a char boundary, which the last line of a pane can
/// produce all by itself: the producer may extend it (mid-multibyte-char)
/// between a search finding a hit and the frame that shows it, and slicing
/// there would panic inside the TUI.
fn char_col(line: &str, at: usize) -> usize {
    line.char_indices().take_while(|(i, _)| *i < at).count()
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
        crossterm::terminal::SetTitle(format!(
            "cactup log — {}",
            sanitize(subject.as_bytes(), &mut 0)
        ))
    );

    let mut focused = STDOUT;
    // Cached pane content-rects from the last draw, used to size PgUp/PgDn
    // and to hit-test mouse events between redraws.
    let mut layout = (Rect::default(), Rect::default());
    let mut mode = Mode::Normal;
    let mut status: Option<Status> = None;
    // Mouse capture is on until the user hands the mouse back to the
    // terminal with `m`.
    let mut mouse = true;
    let mut drag: Option<DragStart> = None;

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
    draw(terminal, &mut panes, focused, &mut layout, &mode, &status, mouse)?;
    pacer.note_frame(started, Instant::now());

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        for pane in &mut panes {
            pane.apply_evictions();
        }
        // A notice that has timed out is a view change like any other: drop
        // it, and the frame that follows puts the key hint back.
        if status.as_ref().is_some_and(|s| s.expired(Instant::now())) {
            status = None;
            view_dirty = true;
        }

        // Block for input until the next frame is due (or TICK, whichever
        // comes first, so Ctrl-C is still noticed promptly). When nothing is
        // dirty there's no frame pending, so wait out the whole tick.
        let dirty = view_dirty || content_dirty.load(Ordering::Acquire);
        let timeout = if dirty { pacer.due_in(Instant::now()).min(TICK) } else { TICK };
        if event::poll(timeout).context("polling terminal events")? {
            match event::read().context("reading a terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(
                        key,
                        &mut panes,
                        &mut focused,
                        layout,
                        &mut mode,
                        &mut status,
                        &mut mouse,
                    )? {
                        return Ok(());
                    }
                    view_dirty = true;
                }
                Event::Mouse(event) => {
                    handle_mouse(event, &mut panes, &mut focused, layout, &mut mode, &mut drag);
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
            draw(terminal, &mut panes, focused, &mut layout, &mode, &status, mouse)?;
            // Timed from `now` (before the draw) to the moment it lands, so
            // the pacer sees the true cost of a frame, blocking write and
            // all, and paces the next one off its completion.
            pacer.note_frame(now, Instant::now());
        }
    }
}

/// Handle one key press. Returns `true` if the app should quit.
///
/// Dispatch is by mode first and binding second, which is what lets the `/`
/// prompt accept `q`, `j` or `/` as plain text: while it is open the pane's
/// own bindings simply aren't reachable. Only Ctrl-C outranks the mode.
fn handle_key(
    key: KeyEvent,
    panes: &mut [Pane; 2],
    focused: &mut usize,
    layout: (Rect, Rect),
    mode: &mut Mode,
    status: &mut Option<Status>,
    mouse: &mut bool,
) -> Res<bool> {
    // The interrupt contract doesn't get a modal exemption: Ctrl-C means
    // "stop" from inside a half-typed pattern too.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(true);
    }
    let rect = if *focused == STDOUT { layout.0 } else { layout.1 };
    // (height, width) of the pane's *content* area, borders excluded — what
    // every page, centering and pan calculation below is relative to.
    let inner = (rect.height.saturating_sub(2), rect.width.saturating_sub(2));
    if matches!(mode, Mode::Search(_)) {
        search_key(key, &mut panes[*focused], mode, inner, status);
        return Ok(false);
    }
    if matches!(mode, Mode::Select) {
        return select_key(key, &mut panes[*focused], mode, inner, status);
    }
    normal_key(key, panes, focused, mode, inner, status, mouse)
}

/// Live-tailing bindings: scroll, pan, focus, and the entry points into the
/// search prompt and line selection.
fn normal_key(
    key: KeyEvent,
    panes: &mut [Pane; 2],
    focused: &mut usize,
    mode: &mut Mode,
    inner: (u16, u16),
    status: &mut Option<Status>,
    mouse: &mut bool,
) -> Res<bool> {
    let (inner_height, inner_width) = inner;
    let page = inner_height.saturating_sub(1).max(1);
    // The two bindings that aren't about the focused pane.
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => {
            *focused = 1 - *focused;
            return Ok(false);
        }
        KeyCode::Char('m') => {
            *mouse = !*mouse;
            set_mouse_capture(*mouse)?;
            let msg: &str = if *mouse {
                "mouse captured — wheel scrolls, drag selects lines"
            } else {
                "mouse released — select with your terminal as usual, m to take it back"
            };
            note(status, msg, false);
            return Ok(false);
        }
        _ => {}
    }
    let pane = &mut panes[*focused];
    let max = pane.max_scroll(inner_height);
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Esc => {
            // vim's `:nohlsearch` reflex: Esc dismisses what's up before it
            // means "quit", so clearing a search can't drop you out of the
            // TUI by accident.
            if pane.search.is_some() {
                pane.clear_search();
            } else {
                return Ok(true);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => pane.scroll.up(1),
        KeyCode::Down | KeyCode::Char('j') => pane.scroll.down(1, max),
        KeyCode::PageUp => pane.scroll.up(page),
        KeyCode::PageDown => pane.scroll.down(page, max),
        KeyCode::Home => pane.scroll.top(),
        KeyCode::Char('g') => pane.scroll.top(),
        KeyCode::End | KeyCode::Char('G') => pane.scroll.bottom(max),
        KeyCode::Left | KeyCode::Char('h') => pane.hscroll = pane.hscroll.saturating_sub(PAN_STEP),
        KeyCode::Right | KeyCode::Char('l') => {
            pane.hscroll = (pane.hscroll + PAN_STEP).min(pane.max_hscroll(inner_width));
        }
        KeyCode::Char('/') => *mode = open_prompt(pane, Direction::Forward),
        KeyCode::Char('?') => *mode = open_prompt(pane, Direction::Backward),
        KeyCode::Char('n') => step_along(pane, false, inner, status),
        KeyCode::Char('N') => step_along(pane, true, inner, status),
        KeyCode::Char('v') => {
            if start_select(pane, inner_height) {
                *mode = Mode::Select;
            } else {
                note(status, "nothing to select yet", true);
            }
        }
        KeyCode::Char('y') => copy_view(pane, inner_height, status)?,
        KeyCode::Char('Y') => copy_span(pane, None, "the pane", status)?,
        _ => {}
    }
    Ok(false)
}

/// One keystroke in line-selection mode. Motions move the cursor and extend
/// the selection from its anchor — vim's visual mode, where there is no way
/// to move without extending; press Esc and `v` again to start elsewhere.
fn select_key(
    key: KeyEvent,
    pane: &mut Pane,
    mode: &mut Mode,
    inner: (u16, u16),
    status: &mut Option<Status>,
) -> Res<bool> {
    let (inner_height, inner_width) = inner;
    let page = inner_height.saturating_sub(1).max(1);
    let len = pane.len();
    let Some((anchor, cursor)) = pane.sel else {
        // Nothing to be selecting: the buffer emptied out under us.
        leave_select(pane, mode);
        return Ok(false);
    };
    let last = len.saturating_sub(1);
    let mut cursor = cursor.min(last);
    match key.code {
        // `q` still quits, from every mode: a cancel key that only sometimes
        // exits the app is worse than losing a selection.
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Esc | KeyCode::Char('v') => {
            leave_select(pane, mode);
            return Ok(false);
        }
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let span = selection_span(anchor, cursor, len);
            copy_span(pane, span, "the selection", status)?;
            leave_select(pane, mode);
            return Ok(false);
        }
        KeyCode::Up | KeyCode::Char('k') => cursor = cursor.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => cursor = (cursor + 1).min(last),
        KeyCode::PageUp => cursor = cursor.saturating_sub(usize::from(page)),
        KeyCode::PageDown => cursor = (cursor + usize::from(page)).min(last),
        KeyCode::Home | KeyCode::Char('g') => cursor = 0,
        KeyCode::End | KeyCode::Char('G') => cursor = last,
        // Panning doesn't touch the selection — a long line still has to be
        // readable before you decide to copy it.
        KeyCode::Left | KeyCode::Char('h') => {
            pane.hscroll = pane.hscroll.saturating_sub(PAN_STEP);
            return Ok(false);
        }
        KeyCode::Right | KeyCode::Char('l') => {
            pane.hscroll = (pane.hscroll + PAN_STEP).min(pane.max_hscroll(inner_width));
            return Ok(false);
        }
        _ => return Ok(false),
    }
    pane.sel = Some((anchor.min(last), cursor));
    let max = pane.max_scroll(inner_height);
    pane.scroll.offset = scroll_to_show(cursor, pane.scroll.offset, inner_height, max);
    Ok(false)
}

/// Start a one-line selection on the newest visible line and pause follow —
/// a selection whose lines scroll away under it is unusable. `false` if
/// there is nothing in the pane to select yet.
fn start_select(pane: &mut Pane, inner_height: u16) -> bool {
    let len = pane.len();
    if len == 0 {
        return false;
    }
    let bottom = usize::from(pane.scroll.offset)
        .saturating_add(usize::from(inner_height))
        .min(len)
        .saturating_sub(1);
    pane.scroll.follow = false;
    pane.sel = Some((bottom, bottom));
    true
}

/// Drop the selection and return to the live-tailing bindings.
fn leave_select(pane: &mut Pane, mode: &mut Mode) {
    pane.sel = None;
    *mode = Mode::Normal;
}

/// One keystroke while the `/` prompt is open: either it edits the pattern
/// or it ends the prompt.
fn search_key(
    key: KeyEvent,
    pane: &mut Pane,
    mode: &mut Mode,
    inner: (u16, u16),
    status: &mut Option<Status>,
) {
    let Mode::Search(prompt) = mode else { return };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            abandon_search(pane, prompt);
            *mode = Mode::Normal;
        }
        KeyCode::Enter => {
            commit_search(pane, prompt, inner, status);
            *mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            if prompt.input.pop().is_none() {
                // Backspacing off the front of an empty pattern is how vim
                // leaves the prompt, so it is how you leave this one.
                abandon_search(pane, prompt);
                *mode = Mode::Normal;
            } else {
                preview_search(pane, prompt, inner);
            }
        }
        KeyCode::Char('u') if ctrl => {
            prompt.input.clear();
            preview_search(pane, prompt, inner);
        }
        KeyCode::Char(c) if !ctrl => {
            prompt.input.push(c);
            preview_search(pane, prompt, inner);
        }
        _ => {}
    }
}

/// Open the `/` (or `?`) prompt, stashing everything Esc has to undo. The
/// pane's current search comes along with it: the preview owns the
/// highlight from here until the prompt closes, one way or the other.
fn open_prompt(pane: &mut Pane, dir: Direction) -> Mode {
    Mode::Search(SearchPrompt {
        dir,
        input: String::new(),
        saved_offset: pane.scroll.offset,
        saved_follow: pane.scroll.follow,
        saved_search: pane.search.take(),
        note: String::new(),
    })
}

/// Put back everything an abandoned search disturbed — the previous pattern
/// and hit, and the scroll position the preview moved.
fn abandon_search(pane: &mut Pane, prompt: &mut SearchPrompt) {
    pane.search = prompt.saved_search.take();
    pane.scroll.offset = prompt.saved_offset;
    pane.scroll.follow = prompt.saved_follow;
}

/// Re-run the search on every keystroke — vim's `incsearch` — so a pattern
/// can be judged before it is committed. A pattern that doesn't compile yet
/// (every prefix of `ERROR[0-9]` is one) shows no hit and raises no alarm:
/// the reason sits quietly beside the prompt instead of flashing a notice.
///
/// The view is rewound to where the prompt was opened before each attempt,
/// so the origin can't creep forward by a hit per keystroke.
fn preview_search(pane: &mut Pane, prompt: &mut SearchPrompt, inner: (u16, u16)) {
    pane.search = None;
    pane.scroll.offset = prompt.saved_offset;
    pane.scroll.follow = prompt.saved_follow;
    if prompt.input.is_empty() {
        prompt.note.clear();
        return;
    }
    let query = match Query::new(&prompt.input) {
        Ok(query) => query,
        Err(msg) => {
            prompt.note = msg;
            return;
        }
    };
    let from = pane.view_origin(prompt.dir, inner.0);
    let found = query.find_from_inclusive(&lock(&pane.content).lines, from, prompt.dir);
    pane.search = Some(PaneSearch { query, dir: prompt.dir, hit: None, count: (0, 0) });
    match found {
        Some(found) => {
            pane.focus_hit(found.hit, inner);
            let (ordinal, total) = pane.search.as_ref().map_or((0, 0), |s| s.count);
            prompt.note = format!("[{ordinal}/{total}]");
        }
        None => prompt.note = "no match".to_string(),
    }
}

/// Accept the typed pattern. An empty or uncompilable one leaves the pane
/// exactly as the prompt found it. A valid pattern with no match anywhere
/// is still *kept*: these are live logs, and the line being waited for may
/// simply not have been written yet, so `n` can ask again later.
fn commit_search(
    pane: &mut Pane,
    prompt: &mut SearchPrompt,
    inner: (u16, u16),
    status: &mut Option<Status>,
) {
    let query = match Query::new(&prompt.input) {
        Ok(query) => query,
        Err(msg) => {
            abandon_search(pane, prompt);
            note(status, msg, true);
            return;
        }
    };
    // Search from where the prompt was opened rather than from wherever the
    // preview drifted to, so committing lands on the hit already on screen
    // instead of the one after it.
    pane.scroll.offset = prompt.saved_offset;
    pane.scroll.follow = prompt.saved_follow;
    pane.search = Some(PaneSearch { query, dir: prompt.dir, hit: None, count: (0, 0) });
    step_search(pane, prompt.dir, inner, status);
}

/// `n` / `N`: step along the pane's search direction, or against it.
fn step_along(pane: &mut Pane, reverse: bool, inner: (u16, u16), status: &mut Option<Status>) {
    let Some(search) = &pane.search else {
        note(status, "no search yet — press / to start one", true);
        return;
    };
    let dir = if reverse { search.dir.reverse() } else { search.dir };
    step_search(pane, dir, inner, status);
}

/// Move to the next hit in `dir`, wrapping around the buffer and saying so
/// the way vim does. With no hit yet the search starts from what's on
/// screen, not from the top of a ten-thousand-line buffer.
fn step_search(pane: &mut Pane, dir: Direction, inner: (u16, u16), status: &mut Option<Status>) {
    let (found, pattern) = {
        let Some(search) = &pane.search else {
            note(status, "no search yet — press / to start one", true);
            return;
        };
        let content = lock(&pane.content);
        let found = match search.hit {
            Some(hit) => search.query.find(&content.lines, (hit.line, hit.start), dir),
            None => {
                let from = match dir {
                    Direction::Forward => (usize::from(pane.scroll.offset), 0),
                    Direction::Backward => {
                        let last = usize::from(pane.scroll.offset)
                            .saturating_add(usize::from(inner.0))
                            .min(content.lines.len())
                            .saturating_sub(1);
                        (last, usize::MAX)
                    }
                };
                search.query.find_from_inclusive(&content.lines, from, dir)
            }
        };
        (found, search.query.pattern().to_string())
    };
    match found {
        None => note(status, format!("pattern not found: {pattern}"), true),
        Some(found) => {
            pane.focus_hit(found.hit, inner);
            if found.wrapped {
                note(status, wrap_notice(dir), false);
            }
        }
    }
}

fn wrap_notice(dir: Direction) -> &'static str {
    match dir {
        Direction::Forward => "search hit BOTTOM, continuing at TOP",
        Direction::Backward => "search hit TOP, continuing at BOTTOM",
    }
}

/// Copy `span` of a pane's lines — or the whole pane, for `None` — and
/// report what went out.
///
/// A terminal that refuses OSC 52 pastes looks identical from in here to one
/// that accepted it (see [`clipboard`]), so the notice says what was *sent*;
/// `m` is the documented way out when it turns out the terminal dropped it.
fn copy_span(
    pane: &Pane,
    span: Option<(usize, usize)>,
    what: &str,
    status: &mut Option<Status>,
) -> Res<()> {
    let lines: Vec<String> = {
        let content = lock(&pane.content);
        match span {
            Some((lo, hi)) => content.lines.iter().skip(lo).take(hi + 1 - lo).cloned().collect(),
            None => content.lines.iter().cloned().collect(),
        }
    };
    if lines.is_empty() {
        note(status, "nothing to copy yet", true);
        return Ok(());
    }
    let copied = clipboard::copy_lines(&lines)?;
    let plural = if copied.lines == 1 { "line" } else { "lines" };
    let msg = if copied.truncated {
        format!(
            "copied {} {plural} of {what} — cut off at {} KB, past what terminals accept",
            copied.lines,
            clipboard::MAX_CLIP_BYTES / 1000
        )
    } else {
        format!("copied {} {plural} of {what}", copied.lines)
    };
    note(status, msg, false);
    Ok(())
}

/// `y` in Normal mode: copy exactly what is on screen in the focused pane.
/// "Copy what I'm looking at" is the common case, and shouldn't need a
/// selection made first.
fn copy_view(pane: &Pane, inner_height: u16, status: &mut Option<Status>) -> Res<()> {
    let len = pane.len();
    let lo = usize::from(pane.scroll.offset).min(len);
    let hi = lo.saturating_add(usize::from(inner_height)).min(len);
    if hi <= lo {
        note(status, "nothing to copy yet", true);
        return Ok(());
    }
    copy_span(pane, Some((lo, hi - 1)), "the view", status)
}

/// Hand the mouse to the terminal, or take it back.
///
/// Capture is what makes the wheel and click-to-focus work, and at the same
/// time what stops the terminal's own click-drag selection — the one copy
/// route that works in every terminal, including those that refuse OSC 52
/// and those behind a multiplexer that strips it. So it's a toggle, not a
/// setting.
fn set_mouse_capture(on: bool) -> Res<()> {
    let mut out = std::io::stdout();
    if on {
        execute!(out, EnableMouseCapture).context("enabling mouse capture")
    } else {
        execute!(out, DisableMouseCapture).context("disabling mouse capture")
    }
}

/// Where the left button went down, if it is still down: the anchor a drag
/// would select from. A click on its own only focuses a pane — it takes
/// actual movement to begin a selection, so click-to-focus keeps behaving
/// exactly as it did.
struct DragStart {
    pane: usize,
    line: usize,
}

fn handle_mouse(
    event: MouseEvent,
    panes: &mut [Pane; 2],
    focused: &mut usize,
    layout: (Rect, Rect),
    mode: &mut Mode,
    drag: &mut Option<DragStart>,
) {
    let pos = (event.column, event.row);
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
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            *focused = i;
            *drag = line_at(&panes[i], rect, event.row).map(|line| DragStart { pane: i, line });
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(start) = drag
                && start.pane == i
                && let Some(line) = line_at(&panes[i], rect, event.row)
            {
                // Dragging is a selection gesture; with capture on the
                // terminal never sees it, so the TUI has to mean it.
                panes[i].scroll.follow = false;
                panes[i].sel = Some((start.line, line));
                *mode = Mode::Select;
            }
        }
        // The selection outlives the gesture — `y` comes after the release.
        MouseEventKind::Up(MouseButton::Left) => *drag = None,
        MouseEventKind::Down(_) => {
            *focused = i;
            *drag = None;
        }
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

/// Retained-line index under mouse row `row` in `rect`, or `None` if that
/// row is a border or the pane is still empty.
///
/// A row inside the text area but below the last retained line clamps to
/// that last line rather than reporting `None`: those rows are blank only
/// because the buffer is shorter than the viewport, and a drag that runs
/// off the end of the text should select *to* the end — the same thing any
/// editor does — instead of leaving the selection frozen wherever it last
/// crossed real text.
fn line_at(pane: &Pane, rect: Rect, row: u16) -> Option<usize> {
    if rect.height < 3 || row <= rect.y || row + 1 >= rect.y + rect.height {
        return None;
    }
    let len = pane.len();
    if len == 0 {
        return None;
    }
    let index = usize::from(pane.scroll.offset) + usize::from(row - rect.y - 1);
    Some(index.min(len - 1))
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
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let cols = Layout::default()
        .direction(ratatui::layout::Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    (cols[0], cols[1], rows[1])
}

fn draw(
    terminal: &mut Terminal<Backend>,
    panes: &mut [Pane; 2],
    focused: usize,
    layout: &mut (Rect, Rect),
    mode: &Mode,
    status: &Option<Status>,
    mouse: bool,
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
            render_footer(frame, footer, mode, status, panes, focused, mouse);
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
        // borrows only `pane.content`, leaving the view state free to move.
        let mut content = lock(&pane.content);
        // Fold in anything evicted since the loop last looked, so every
        // index resolved below — the offset, the selection, the search hit —
        // is resolved against the buffer as it is right now.
        let evicted = std::mem::take(&mut content.evicted);
        shift_view(&mut pane.scroll, &mut pane.sel, &mut pane.search, evicted);
        let count = content.lines.len();
        let max = (count as u16).saturating_sub(inner_height);
        let offset = pane.scroll.resolve(max);
        let sel = pane.sel.and_then(|(anchor, cursor)| selection_span(anchor, cursor, count));
        let query = pane.search.as_ref().map(|search| &search.query);
        let current = pane.search.as_ref().and_then(|search| search.hit);
        // Hand the Paragraph only the visible slice — vertical scrolling is
        // done here by slicing at `offset` (building all MAX_LINES Lines per
        // frame just for Paragraph to skip them is wasted work), horizontal
        // panning is left to `scroll`.
        let visible: Vec<Line> = content
            .lines
            .iter()
            .enumerate()
            .skip(offset as usize)
            .take(inner_height as usize)
            .map(|(index, line)| render_line(line, index, sel, query, current))
            .collect();
        (content.exists, count, visible)
    };
    let max = (line_count as u16).saturating_sub(inner_height);
    let offset = pane.scroll.offset;

    let indicator =
        if pane.scroll.follow { "● live".to_string() } else { format!("⏸ +{}", max - offset) };
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
    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(format!("{} — {file_name}", pane.label)).left_aligned())
        .title(Line::from(indicator).right_aligned());
    // A pane's active pattern belongs on the pane, not in the footer: the
    // two panes are searched independently, so which one a pattern applies
    // to has to be visible at a glance.
    if let Some(search) = &pane.search {
        let (ordinal, total) = search.count;
        let label = if total == 0 {
            format!("/{} · no match", search.query.pattern())
        } else {
            format!("/{} [{ordinal}/{total}]", search.query.pattern())
        };
        block =
            block.title(Line::from(Span::styled(label, Style::new().fg(Color::Yellow))).centered());
    }

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

/// Build one display line: search matches picked out inside it, and the
/// whole line reversed when it falls inside the selection.
///
/// Match positions are recomputed per visible line per frame rather than
/// cached. Forty short lines against one regex is nothing beside the
/// terminal write that follows, and it means a highlight can never disagree
/// with the text drawn under it — a pane's last line grows under the view
/// whenever the producer is mid-line.
fn render_line(
    text: &str,
    index: usize,
    sel: Option<(usize, usize)>,
    query: Option<&Query>,
    current: Option<Hit>,
) -> Line<'static> {
    let selected = sel.is_some_and(|(lo, hi)| index >= lo && index <= hi);
    let base = if selected { SELECT_STYLE } else { Style::new() };
    let hits = query.map(|q| q.hits_in(text)).unwrap_or_default();
    if hits.is_empty() {
        return Line::from(Span::styled(text.to_string(), base));
    }
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(hits.len() * 2 + 1);
    let mut at = 0;
    for (start, end) in hits {
        if start > at {
            spans.push(Span::styled(text[at..start].to_string(), base));
        }
        let is_current = current.is_some_and(|hit| hit.line == index && hit.start == start);
        // On a selected line the reverse video already belongs to the
        // selection, so a match there is marked by weight instead of
        // colour — two backgrounds fighting over one cell reads as neither.
        let style = if selected && is_current {
            base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else if selected {
            base.add_modifier(Modifier::UNDERLINED)
        } else if is_current {
            CURRENT_MATCH_STYLE
        } else {
            MATCH_STYLE
        };
        spans.push(Span::styled(text[start..end].to_string(), style));
        at = end;
    }
    if at < text.len() {
        spans.push(Span::styled(text[at..].to_string(), base));
    }
    Line::from(spans)
}

/// The bottom line, whose job depends on the mode. Precedence: the search
/// prompt is being typed into and must always win; a notice outranks the
/// key hint, which is the fallback when nothing else needs the row.
fn render_footer(
    frame: &mut ratatui::Frame,
    area: Rect,
    mode: &Mode,
    status: &Option<Status>,
    panes: &[Pane; 2],
    focused: usize,
    mouse: bool,
) {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let line = if let Mode::Search(prompt) = mode {
        let mark = match prompt.dir {
            Direction::Forward => '/',
            Direction::Backward => '?',
        };
        let mut spans = vec![
            Span::raw(format!("{mark}{}", prompt.input)),
            // Raw mode leaves us no real cursor down here, so the prompt
            // draws its own.
            Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
        ];
        if !prompt.note.is_empty() {
            spans.push(Span::styled(format!("  {}", prompt.note), dim));
        }
        Line::from(spans)
    } else if let Some(status) = status {
        let style =
            if status.error { Style::new().fg(Color::Red) } else { Style::new().fg(Color::Green) };
        Line::from(Span::styled(status.text.clone(), style))
    } else if matches!(mode, Mode::Select) {
        let pane = &panes[focused];
        let selected = pane
            .sel
            .and_then(|(anchor, cursor)| selection_span(anchor, cursor, pane.len()))
            .map_or(0, |(lo, hi)| hi + 1 - lo);
        let plural = if selected == 1 { "line" } else { "lines" };
        Line::from(Span::styled(
            format!("{selected} {plural} selected · j/k G g extend · y copy · Esc cancel · q quit"),
            dim,
        ))
    } else {
        Line::from(Span::styled(hint(area.width, mouse), dim))
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// The key hint, in two lengths. On a narrow terminal the long one would be
/// truncated mid-word, so the short one keeps the bindings that can't be
/// guessed — search and copy — and drops the ones an arrow key finds by
/// itself.
fn hint(width: u16, mouse: bool) -> String {
    let mouse_key = if mouse { "m free mouse" } else { "m grab mouse" };
    let full = format!(
        "Tab focus · ↑/↓ PgUp/PgDn scroll · ←/→ pan · End follow · / search · n/N hits · \
         v select · y copy view · Y copy pane · {mouse_key} · q quit"
    );
    if usize::from(width) >= full.chars().count() {
        full
    } else {
        format!("/ search · n/N hits · v select · y copy · {mouse_key} · q quit")
    }
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

    // -- Status / note ------------------------------------------------------

    #[test]
    fn status_is_not_expired_before_its_deadline() {
        let now = Instant::now();
        let status = Status { text: "hi".to_string(), error: false, until: now + STATUS_TTL };
        assert!(!status.expired(now));
        assert!(!status.expired(now + STATUS_TTL - Duration::from_millis(1)));
    }

    #[test]
    fn status_is_expired_at_and_after_its_deadline() {
        let now = Instant::now();
        let status = Status { text: "hi".to_string(), error: false, until: now + STATUS_TTL };
        assert!(status.expired(now + STATUS_TTL));
        assert!(status.expired(now + STATUS_TTL + Duration::from_secs(1)));
    }

    #[test]
    fn note_sets_a_status_that_is_not_yet_expired() {
        let mut status = None;
        note(&mut status, "copied 3 lines", false);
        let status = status.expect("note always leaves Some");
        assert_eq!(status.text, "copied 3 lines");
        assert!(!status.error);
        assert!(!status.expired(Instant::now()));
    }

    #[test]
    fn note_replaces_any_existing_status_rather_than_stacking() {
        let mut status = None;
        note(&mut status, "first", false);
        note(&mut status, "second", true);
        let status = status.expect("note always leaves Some");
        assert_eq!(status.text, "second");
        assert!(status.error);
    }

    // -- selection_span / center_offset / scroll_to_show / pan_to / char_col ------

    #[test]
    fn selection_span_normal_order_returns_the_span_as_is() {
        assert_eq!(selection_span(3, 7, 100), Some((3, 7)));
    }

    #[test]
    fn selection_span_reversed_order_normalizes_low_first() {
        // The cursor can end up above the anchor (selecting upward); the
        // span is always reported lowest-first regardless.
        assert_eq!(selection_span(7, 3, 100), Some((3, 7)));
    }

    #[test]
    fn selection_span_on_an_empty_buffer_is_none() {
        assert_eq!(selection_span(0, 0, 0), None);
    }

    #[test]
    fn selection_span_clamps_indices_past_len() {
        assert_eq!(selection_span(2, 50, 10), Some((2, 9)));
        assert_eq!(selection_span(50, 2, 10), Some((2, 9)));
    }

    #[test]
    fn center_offset_centers_with_room_either_side() {
        assert_eq!(center_offset(50, 10, 1000), 45);
    }

    #[test]
    fn center_offset_clamps_at_the_top_for_an_early_line() {
        assert_eq!(center_offset(2, 10, 1000), 0);
    }

    #[test]
    fn center_offset_clamps_at_max() {
        assert_eq!(center_offset(50, 10, 40), 40);
    }

    #[test]
    fn center_offset_handles_odd_and_even_heights() {
        assert_eq!(center_offset(50, 9, 1000), 46); // half = 9/2 = 4
        assert_eq!(center_offset(50, 10, 1000), 45); // half = 10/2 = 5
    }

    #[test]
    fn scroll_to_show_leaves_a_visible_cursor_alone() {
        // The key contract vs. center_offset: a cursor already on screen
        // must not recenter the view, or holding `j` would make the text
        // crawl under a fixed cursor instead of the cursor walking down it.
        assert_eq!(scroll_to_show(10, 5, 10, 1000), 5);
    }

    #[test]
    fn scroll_to_show_scrolls_up_minimally_when_the_cursor_is_above_the_view() {
        assert_eq!(scroll_to_show(3, 10, 10, 1000), 3);
    }

    #[test]
    fn scroll_to_show_scrolls_down_minimally_when_the_cursor_is_below_the_view() {
        assert_eq!(scroll_to_show(15, 0, 10, 1000), 6);
    }

    #[test]
    fn scroll_to_show_clamps_to_max() {
        assert_eq!(scroll_to_show(15, 0, 10, 4), 4);
    }

    #[test]
    fn scroll_to_show_treats_a_zero_height_as_one() {
        assert_eq!(scroll_to_show(5, 0, 0, 100), 5);
    }

    #[test]
    fn pan_to_leaves_an_onscreen_column_untouched() {
        assert_eq!(pan_to(15, 10, 20, 1000), 10);
    }

    #[test]
    fn pan_to_pans_back_when_the_column_is_left_of_the_view() {
        // Saturates at 0 rather than going negative.
        assert_eq!(pan_to(5, 20, 10, 1000), 0);
    }

    #[test]
    fn pan_to_pans_right_keeping_hit_margin_columns_of_context() {
        assert_eq!(pan_to(50, 0, 10, 1000), 50 - HIT_MARGIN);
    }

    #[test]
    fn pan_to_clamps_at_max_hscroll() {
        assert_eq!(pan_to(50, 0, 10, 20), 20);
    }

    #[test]
    fn char_col_counts_ascii_columns() {
        assert_eq!(char_col("hello", 3), 3);
    }

    #[test]
    fn char_col_counts_chars_not_bytes_for_multibyte_text() {
        // "café" is 5 bytes but 4 chars ('é' is 2 bytes); the byte offset
        // at the end of the string is column 4, not 5.
        assert_eq!(char_col("café", 5), 4);
    }

    #[test]
    fn char_col_does_not_panic_off_a_char_boundary() {
        // Byte 4 lands inside 'é' (which starts at byte 3), not on a char
        // boundary — slicing there would panic, but char_col only counts.
        assert_eq!(char_col("café", 4), 4);
    }

    #[test]
    fn char_col_clamps_an_offset_past_the_end_of_the_line() {
        assert_eq!(char_col("hi", 100), 2);
    }

    // -- wrap_notice ----------------------------------------------------------

    #[test]
    fn wrap_notice_forward_says_hit_bottom_continuing_at_top() {
        assert_eq!(wrap_notice(Direction::Forward), "search hit BOTTOM, continuing at TOP");
    }

    #[test]
    fn wrap_notice_backward_says_hit_top_continuing_at_bottom() {
        assert_eq!(wrap_notice(Direction::Backward), "search hit TOP, continuing at BOTTOM");
    }

    // -- hint -------------------------------------------------------------

    #[test]
    fn hint_uses_the_full_text_when_it_fits() {
        let full = hint(u16::MAX, false);
        assert!(full.contains("Tab focus"));
    }

    #[test]
    fn hint_switches_to_the_short_form_when_narrow() {
        let short = hint(10, false);
        assert!(!short.contains("Tab focus"));
        assert!(short.contains("search"));
    }

    #[test]
    fn hint_reflects_mouse_capture_state() {
        assert!(hint(u16::MAX, true).contains("m free mouse"));
        assert!(hint(u16::MAX, false).contains("m grab mouse"));
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
            sel: None,
            search: None,
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

    // -- shift_view ---------------------------------------------------------

    #[test]
    fn shift_view_by_zero_is_a_no_op() {
        let mut scroll = ScrollState { offset: 5, follow: false };
        let mut sel = Some((3, 7));
        let mut search = Some(PaneSearch {
            query: Query::new("x").unwrap(),
            dir: Direction::Forward,
            hit: Some(Hit { line: 4, start: 0, end: 1 }),
            count: (1, 1),
        });
        shift_view(&mut scroll, &mut sel, &mut search, 0);
        assert_eq!(scroll.offset, 5);
        assert_eq!(sel, Some((3, 7)));
        assert_eq!(search.as_ref().unwrap().hit.unwrap().line, 4);
    }

    #[test]
    fn shift_view_slides_scroll_selection_and_search_hit_together() {
        let mut scroll = ScrollState { offset: 10, follow: false };
        let mut sel = Some((8, 12));
        let mut search = Some(PaneSearch {
            query: Query::new("x").unwrap(),
            dir: Direction::Forward,
            hit: Some(Hit { line: 9, start: 0, end: 1 }),
            count: (1, 1),
        });
        shift_view(&mut scroll, &mut sel, &mut search, 3);
        assert_eq!(scroll.offset, 7);
        assert_eq!(sel, Some((5, 9)));
        assert_eq!(search.as_ref().unwrap().hit.unwrap().line, 6);
    }

    #[test]
    fn shift_view_saturates_at_zero_instead_of_wrapping() {
        let mut scroll = ScrollState { offset: 2, follow: false };
        let mut sel = Some((1, 3));
        let mut search: Option<PaneSearch> = None;
        shift_view(&mut scroll, &mut sel, &mut search, 10);
        assert_eq!(scroll.offset, 0);
        assert_eq!(sel, Some((0, 0)));
    }

    #[test]
    fn shift_view_tolerates_no_selection_and_no_search() {
        let mut scroll = ScrollState { offset: 5, follow: false };
        let mut sel: Option<(usize, usize)> = None;
        let mut search: Option<PaneSearch> = None;
        shift_view(&mut scroll, &mut sel, &mut search, 2);
        assert_eq!(scroll.offset, 3);
        assert_eq!(sel, None);
        assert!(search.is_none());
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

    // -- line_at ------------------------------------------------------------

    #[test]
    fn line_at_maps_a_row_to_a_buffer_line_honoring_the_border_and_scroll() {
        let mut content = test_content();
        for i in 0..20 {
            content.ingest(format!("line{i}\n").as_bytes());
        }
        let mut pane = test_pane(content);
        pane.scroll.offset = 5;
        let rect = Rect { x: 0, y: 0, width: 40, height: 10 };
        // Row 0 is the top border, so row 1 is the first text row.
        assert_eq!(line_at(&pane, rect, 1), Some(5));
        assert_eq!(line_at(&pane, rect, 3), Some(7));
    }

    #[test]
    fn line_at_returns_none_on_a_border_row() {
        let mut content = test_content();
        content.ingest(b"only line\n");
        let pane = test_pane(content);
        let rect = Rect { x: 0, y: 0, width: 40, height: 10 };
        assert_eq!(line_at(&pane, rect, 0), None); // top border
        assert_eq!(line_at(&pane, rect, 9), None); // bottom border
    }

    #[test]
    fn line_at_returns_none_for_an_empty_pane() {
        let pane = test_pane(test_content());
        let rect = Rect { x: 0, y: 0, width: 40, height: 10 };
        assert_eq!(line_at(&pane, rect, 1), None);
    }

    #[test]
    fn line_at_clamps_a_row_past_the_last_line_to_the_end_of_the_buffer() {
        let mut content = test_content();
        content.ingest(b"one\ntwo\nthree\n");
        let pane = test_pane(content);
        let rect = Rect { x: 0, y: 0, width: 40, height: 10 };
        // Row 8 would name index 7, but only 3 lines exist.
        assert_eq!(line_at(&pane, rect, 8), Some(2));
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

    // -- Pane::max_scroll / max_hscroll / len --------------------------------

    fn twenty_line_pane() -> Pane {
        let mut content = test_content();
        for i in 0..20 {
            content.ingest(format!("line{i}\n").as_bytes());
        }
        test_pane(content)
    }

    #[test]
    fn pane_len_and_max_scroll_reflect_the_retained_lines() {
        let pane = twenty_line_pane();
        assert_eq!(pane.len(), 20);
        assert_eq!(pane.max_scroll(6), 14);
    }

    #[test]
    fn pane_max_scroll_floors_at_zero_when_the_viewport_is_taller_than_the_content() {
        let mut content = test_content();
        content.ingest(b"one\ntwo\n");
        let pane = test_pane(content);
        assert_eq!(pane.max_scroll(50), 0);
    }

    #[test]
    fn pane_max_hscroll_is_the_widest_line_minus_the_viewport_width() {
        let mut content = test_content();
        content.ingest(b"short\n");
        content.ingest(format!("{}\n", "x".repeat(30)).as_bytes());
        let pane = test_pane(content);
        assert_eq!(pane.max_hscroll(10), 20);
    }

    // -- Pane::view_origin ----------------------------------------------------

    #[test]
    fn view_origin_forward_starts_at_the_top_of_the_visible_view() {
        let mut pane = twenty_line_pane();
        pane.scroll.offset = 5;
        assert_eq!(pane.view_origin(Direction::Forward, 10), (5, 0));
    }

    #[test]
    fn view_origin_backward_starts_at_the_last_visible_line() {
        let mut pane = twenty_line_pane();
        pane.scroll.offset = 5;
        assert_eq!(pane.view_origin(Direction::Backward, 10), (14, usize::MAX));
    }

    #[test]
    fn view_origin_clamps_against_a_buffer_shorter_than_the_viewport() {
        let mut content = test_content();
        for i in 0..3 {
            content.ingest(format!("line{i}\n").as_bytes());
        }
        let pane = test_pane(content);
        assert_eq!(pane.view_origin(Direction::Forward, 10), (0, 0));
        assert_eq!(pane.view_origin(Direction::Backward, 10), (2, usize::MAX));
    }

    // -- Pane::focus_hit / clear_search -----------------------------------

    #[test]
    fn focus_hit_releases_follow_and_centers_the_hit_line() {
        let mut content = test_content();
        for i in 0..40 {
            content.ingest(format!("line{i}\n").as_bytes());
        }
        let mut pane = test_pane(content);
        pane.scroll.follow = true;
        pane.focus_hit(Hit { line: 30, start: 0, end: 1 }, (10, 80));
        assert!(!pane.scroll.follow);
        // max = len(40) - inner_height(10) = 30; centered = 30 - 5 = 25.
        assert_eq!(pane.scroll.offset, 25);
    }

    #[test]
    fn focus_hit_pans_horizontally_only_when_the_hit_column_is_off_screen() {
        let mut content = test_content();
        content.ingest(b"short\n");
        content.ingest(format!("{}HIT\n", "x".repeat(100)).as_bytes());
        let mut pane = test_pane(content);
        pane.hscroll = 0;
        pane.focus_hit(Hit { line: 1, start: 100, end: 103 }, (10, 20));
        // widest = 103, max_hscroll = 103 - 20 = 83; pan_to(100, 0, 20, 83)
        // targets 100 - HIT_MARGIN = 92, clamped to 83.
        assert_eq!(pane.hscroll, 83);
    }

    #[test]
    fn focus_hit_leaves_hscroll_untouched_when_the_column_is_already_onscreen() {
        let mut content = test_content();
        content.ingest(b"short line\n");
        let mut pane = test_pane(content);
        pane.hscroll = 0;
        pane.focus_hit(Hit { line: 0, start: 2, end: 3 }, (10, 80));
        assert_eq!(pane.hscroll, 0);
    }

    #[test]
    fn focus_hit_stores_the_hit_on_the_panes_search() {
        let mut content = test_content();
        content.ingest(b"alpha\nbeta\n");
        let mut pane = test_pane(content);
        pane.search = Some(PaneSearch {
            query: Query::new("beta").unwrap(),
            dir: Direction::Forward,
            hit: None,
            count: (0, 0),
        });
        let hit = Hit { line: 1, start: 0, end: 4 };
        pane.focus_hit(hit, (10, 80));
        assert_eq!(pane.search.as_ref().unwrap().hit, Some(hit));
    }

    #[test]
    fn clear_search_drops_the_pattern_highlight_and_hit_together() {
        let mut content = test_content();
        content.ingest(b"alpha\n");
        let mut pane = test_pane(content);
        pane.search = Some(PaneSearch {
            query: Query::new("a").unwrap(),
            dir: Direction::Forward,
            hit: Some(Hit { line: 0, start: 0, end: 1 }),
            count: (1, 1),
        });
        pane.clear_search();
        assert!(pane.search.is_none());
    }

    // -- start_select / leave_select ---------------------------------------

    #[test]
    fn start_select_anchors_on_the_newest_visible_line_and_pauses_follow() {
        let mut pane = twenty_line_pane();
        pane.scroll.offset = 5;
        pane.scroll.follow = true;
        assert!(start_select(&mut pane, 10));
        // bottom = min(offset + height, len) - 1 = min(15, 20) - 1 = 14.
        assert_eq!(pane.sel, Some((14, 14)));
        assert!(!pane.scroll.follow);
    }

    #[test]
    fn start_select_on_an_empty_buffer_returns_false_and_sets_no_selection() {
        let mut pane = test_pane(test_content());
        assert!(!start_select(&mut pane, 10));
        assert_eq!(pane.sel, None);
    }

    #[test]
    fn leave_select_clears_the_selection_and_returns_to_normal_mode() {
        let mut pane = test_pane(test_content());
        pane.sel = Some((1, 2));
        let mut mode = Mode::Select;
        leave_select(&mut pane, &mut mode);
        assert_eq!(pane.sel, None);
        assert!(matches!(mode, Mode::Normal));
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
