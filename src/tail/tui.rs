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
//! finding one.
//!
//! The mouse is the TUI's from the first frame, because selecting inside a
//! split view is the one thing the terminal *cannot* do for us: it has no
//! idea the screen is two panes and a footer, so a drag it owns lights up
//! one rectangle straight across all three and copies the two panes
//! interleaved column-wise. Captured, a click focuses the pane under it and
//! a drag selects that pane's text from the character it pressed on to the
//! one under the pointer — the field out of the line, not the whole line.
//! A drag that runs off an edge scrolls the pane after it ([`autoscroll`]),
//! faster the further out it goes, so a selection can reach past what is on
//! screen the way it would in the terminal's own scrollback.
//!
//! Selections therefore come in two grains, and [`Selection`] carries which
//! one it is: a drag makes `Grain::Chars` and `v` makes `Grain::Lines`,
//! since the unit a log line is *read* in is the line but the unit it is
//! copied in usually isn't. Everything downstream — the highlight
//! ([`selected_bytes`]), the clipboard ([`copy_selection`]), the footer's
//! count ([`selection_size`]) — reads the grain rather than assuming one,
//! and the keyboard motions that extend a selection preserve it.
//!
//! What capture costs is the terminal's own double-click, triple-click and
//! native copy, which is the copy route that needs nothing from us and
//! works where OSC 52 is refused or a multiplexer strips it. So it is a
//! toggle: `m` hands the mouse back (most terminals also lend it out for a
//! single Shift-drag), `ALT_SCROLL_ON` keeps the wheel scrolling while it
//! is gone, and the footer says which side holds it.
//!
//! The TUI's own copying goes through [`clipboard`]'s OSC 52 path (which
//! survives the SSH hop this tool is nearly always used across): `y` yanks
//! the view, `Y` the pane, and a selection of either grain. Ctrl-Shift-C
//! does the same as `y` wherever the terminal lets us see it as its own
//! chord — see [`enable_rich_keys`]. Searching is vim's: `/`, `?`, `n`,
//! `N` over a [`search::Query`], per pane, incremental as you type, and
//! centered on the hit when it lands.

use super::search::{self, Direction, Hit, Query};
use super::{LogTail, PollBackoff, SEED_BYTES, clipboard};
use crate::Res;
use anyhow::Context;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
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
/// How often a drag held off the edge of its pane scrolls the view another
/// step. The mouse reports movement and nothing else, so a pointer parked
/// out there has to be driven by the clock; [`TICK`] bounds how promptly
/// that can happen, so there is no point asking for finer than it.
const AUTOSCROLL_TICK: Duration = Duration::from_millis(50);
/// Most cells one autoscroll step moves, however far outside its pane the
/// pointer has been thrown.
const AUTOSCROLL_MAX: u16 = 8;
/// Longest the main loop ever blocks in one `event::poll`. File polling
/// happens on the reader thread, so this only bounds two things: how long a
/// keypress can sit unnoticed, and how long a Ctrl-C takes to wind the TUI
/// down (the interrupt contract, spec §2.4).
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
/// The selected characters. Reversing them reads as a block however the
/// user's palette is set up, where a background colour might not — and it
/// leaves the background free for a search match to keep using (see
/// [`render_line`]).
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

/// One end of a selection: which retained line, and how far into it.
///
/// `col` counts *characters* — not bytes, and not screen cells — which is
/// the unit `hscroll`, [`char_col`] and [`max_line_width`] already work in,
/// so a column derived from a mouse event is directly comparable with one
/// stored here. It is deliberately *not* clamped to the line's length: a
/// press in the blank space right of a short line is a real position, and
/// which end of the selection it turns out to be is what decides its
/// meaning (see [`selected_bytes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    line: usize,
    col: usize,
}

/// How much of the lines a selection reaches across is actually in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grain {
    /// Whole lines; `col` is ignored. `v`'s vim line-visual selection,
    /// where the unit that matters is the log line.
    Lines,
    /// Exactly the characters between the two ends, both included. What a
    /// mouse drag means — pulling one field out of one line is the gesture
    /// worth capturing the mouse for, since the terminal's own selection
    /// cannot do it across a split screen.
    Chars,
}

/// A standing selection in one pane: two ends, and what they enclose.
///
/// The ends are held in *gesture* order rather than document order —
/// `anchor` is where it began, `cursor` is the end that moves — because
/// that is what extending has to preserve. [`Selection::ordered`] is the
/// document-order view that rendering and copying want.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: Point,
    cursor: Point,
    grain: Grain,
}

impl Selection {
    /// The whole of line `line`, and nothing else — what `v` opens with.
    fn line(line: usize) -> Self {
        let at = Point { line, col: 0 };
        Self { anchor: at, cursor: at, grain: Grain::Lines }
    }

    /// From the character at `anchor` to the character at `cursor`,
    /// inclusive — what a drag makes.
    fn chars(anchor: Point, cursor: Point) -> Self {
        Self { anchor, cursor, grain: Grain::Chars }
    }

    /// The two ends in document order, lowest first.
    fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.cursor { (self.anchor, self.cursor) } else { (self.cursor, self.anchor) }
    }

    /// Inclusive span of lines the selection touches in a buffer of `len`
    /// lines, or `None` when there are none.
    fn line_span(&self, len: usize) -> Option<(usize, usize)> {
        selection_span(self.anchor.line, self.cursor.line, len)
    }

    /// Slide both ends down by `by` evicted lines — see [`shift_view`].
    fn shifted(self, by: usize) -> Self {
        Self {
            anchor: Point { line: self.anchor.line.saturating_sub(by), ..self.anchor },
            cursor: Point { line: self.cursor.line.saturating_sub(by), ..self.cursor },
            ..self
        }
    }
}

/// The byte range of `text` — the retained line at `index` — that `sel`
/// covers, or `None` when the line is outside it entirely. Bytes rather
/// than characters because slicing the line, for the screen and for the
/// clipboard alike, is what the answer is for; a line inside the selection
/// that contributes no characters still answers `Some` of an empty range,
/// so a blank line in the middle of a selection copies as a blank line.
///
/// A `Grain::Chars` end past the end of its own line resolves to that
/// line's end, which is what makes a drag through the ragged right edge of
/// a log behave: the *low* end past its line contributes nothing (the press
/// landed in blank space right of the text), while the *high* end past its
/// line takes the line to its end.
fn selected_bytes(text: &str, index: usize, sel: &Selection, len: usize) -> Option<(usize, usize)> {
    let last = len.checked_sub(1)?;
    let (lo, hi) = sel.ordered();
    let (lo_line, hi_line) = (lo.line.min(last), hi.line.min(last));
    if index < lo_line || index > hi_line {
        return None;
    }
    if sel.grain == Grain::Lines {
        return Some((0, text.len()));
    }
    let start = if index == lo_line { byte_of_char(text, lo.col) } else { 0 };
    // Inclusive of the character under the cursor: vim's charwise rule, and
    // the one a terminal drag follows too — the cell being pointed at is in.
    let end = if index == hi_line { byte_of_char(text, hi.col.saturating_add(1)) } else { text.len() };
    // `max` for the one case that can invert: both ends on lines past the
    // buffer's end, clamped onto the same line from opposite sides.
    Some((start, end.max(start)))
}

/// Byte offset of character `col` in `line`, or the line's length when the
/// line is shorter than that. The inverse of [`char_col`], and equally
/// unbothered by a line the producer is still extending mid-character.
fn byte_of_char(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(at, _)| at)
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
        self.cost = if sample > self.cost { sample } else { (self.cost * 3 + sample) / 4 };
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
    /// What is selected in this pane, if anything. Held on the pane rather
    /// than in [`Mode::Select`] so rendering needs to know nothing about
    /// modes: a pane draws a selection exactly when it has one, and leaving
    /// select mode clears it.
    sel: Option<Selection>,
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
    sel: &mut Option<Selection>,
    search: &mut Option<PaneSearch>,
    by: usize,
) {
    if by == 0 {
        return;
    }
    scroll.offset = scroll.offset.saturating_sub(u16::try_from(by).unwrap_or(u16::MAX));
    if let Some(sel) = sel {
        *sel = sel.shifted(by);
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

/// Turn the terminal's alternate-scroll mode on/off (DECSET 1007).
///
/// Armed for the stretch after `m`, when the terminal has the mouse back
/// and our own wheel handling never sees a notch: in the alternate screen
/// the terminal turns notches into ↑/↓ presses, which land in the
/// normal-mode bindings like any other arrow key. Inert while capture is
/// on (a terminal that sees mouse reporting enabled stops synthesising the
/// keys), so it is simply left set for the whole session; terminals that
/// don't implement it ignore the sequence either way.
const ALT_SCROLL_ON: &[u8] = b"\x1b[?1007h";
const ALT_SCROLL_OFF: &[u8] = b"\x1b[?1007l";

/// Set while the kitty keyboard protocol's disambiguation flag is pushed —
/// see [`enable_rich_keys`]. A static because [`restore_terminal`], which
/// has to pop exactly what was pushed, also runs from the panic hook, where
/// there is no app state to consult.
static RICH_KEYS: AtomicBool = AtomicBool::new(false);

/// Whether this terminal reports Ctrl-Shift-C as a chord of its own, which
/// is what makes [`is_copy_chord`] reachable and the footer's mention of it
/// honest.
fn rich_keys() -> bool {
    RICH_KEYS.load(Ordering::Relaxed)
}

/// Ask for the kitty keyboard protocol's `DISAMBIGUATE_ESCAPE_CODES`, so
/// Ctrl-Shift-C can be told apart from Ctrl-C.
///
/// Legacy key encoding has no way to spell Ctrl-Shift-C: the terminal sends
/// the same 0x03 byte it sends for Ctrl-C, and an app that quits on Ctrl-C
/// — as the interrupt contract requires — necessarily quits on the copy
/// chord too. The disambiguating flag is the fix: with it, Ctrl-Shift-C
/// arrives as `CSI 99;6u`, a distinct event [`is_copy_chord`] can match.
///
/// Best-effort: a push the terminal rejects (or never understood) leaves
/// `RICH_KEYS` clear, and the chord stays unreachable and unadvertised.
/// Only this one flag is asked for — key-release and alternate-key
/// reporting would change events the TUI already handles. Must run *after*
/// the switch to the alternate screen: the flag stack is per-screen, so a
/// push on the main screen is not the one [`restore_terminal`] pops.
fn enable_rich_keys(stdout: &mut std::io::Stdout) {
    let pushed = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    RICH_KEYS.store(pushed.is_ok(), Ordering::Relaxed);
}

fn setup_terminal() -> Res<Terminal<Backend>> {
    enable_raw_mode().context("enabling raw mode")?;
    // Detection before the screen is cleared, deliberately: it waits on a
    // reply the terminal may never send (crossterm gives up after two
    // seconds), and on the rare terminal that stays silent a pause on the
    // shell's own screen looks far less like a hang than a blank one.
    let rich = supports_keyboard_enhancement().unwrap_or(false);
    let mut stdout = std::io::stdout();
    // Capture from the first frame: a side-by-side view can't leave
    // selection to the terminal, which knows nothing about the split and
    // drags a rectangle straight across both panes and the footer. `m`
    // hands the mouse back for the terminals where that is the only copy
    // route (see [`set_mouse_capture`]).
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("entering the alternate screen")?;
    let _ = stdout.write_all(ALT_SCROLL_ON);
    if rich {
        enable_rich_keys(&mut stdout);
    }
    Terminal::new(CrosstermBackend::new(stdout)).context("creating the ratatui terminal")
}

/// Best-effort: leave raw mode / the alternate screen / mouse capture,
/// ignoring errors since this runs on every exit path, including after a
/// panic or mid-error, when the terminal may already be in a mixed state.
fn restore_terminal() {
    let mut stdout = std::io::stdout();
    // `swap` so the panic hook and the normal exit path can both run this
    // without popping a second time off someone else's stack.
    if RICH_KEYS.swap(false, Ordering::Relaxed) {
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = stdout.write_all(ALT_SCROLL_OFF);
    let _ = disable_raw_mode();
    let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
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
    let mut mode = Mode::Normal;
    let mut status: Option<Status> = None;
    // The TUI owns the mouse until the user hands it back with `m`: the
    // pane is the unit a click and a drag mean something in, and only we
    // know where the panes are.
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

        // A drag parked off the edge of its pane keeps scrolling: the mouse
        // reports movement only, so once the pointer stops out there
        // nothing else would move the view after it. Skipped while the
        // search prompt is up, which owns both the view and the mode a
        // selection would take (`handle_mouse` drops the gesture for the
        // same reason).
        if let Some(start) = &mut drag
            && !matches!(mode, Mode::Search(_))
        {
            let now = Instant::now();
            if now >= start.due {
                start.due = now + AUTOSCROLL_TICK;
                let rect = pane_rect(start.pane, layout);
                if autoscroll(&mut panes[start.pane], rect, start.pos) {
                    extend_drag(&mut panes[start.pane], rect, start.at, start.pos, &mut mode);
                    view_dirty = true;
                }
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
/// own bindings simply aren't reachable. Only the two chords outrank the
/// mode: Ctrl-Shift-C copies and Ctrl-C quits.
fn handle_key(
    key: KeyEvent,
    panes: &mut [Pane; 2],
    focused: &mut usize,
    layout: (Rect, Rect),
    mode: &mut Mode,
    status: &mut Option<Status>,
    mouse: &mut bool,
) -> Res<bool> {
    let rect = pane_rect(*focused, layout);
    // (height, width) of the pane's *content* area, borders excluded — what
    // every page, centering and pan calculation below is relative to.
    let inner = (rect.height.saturating_sub(2), rect.width.saturating_sub(2));
    // Ctrl-Shift-C first, because the alternative is reading it as the
    // Ctrl-C below and quitting on the user's copy. It is checked before
    // the modes for the same reason `y` isn't enough on its own: the chord
    // has to mean "copy" everywhere, including mid-pattern. Terminals that
    // keep the chord for their own copy never send it here, and those that
    // can't spell it (no `rich_keys`) send a bare Ctrl-C that we cannot
    // tell apart — there, the terminal's own selection is the copy route.
    if is_copy_chord(&key) {
        return copy_current(&mut panes[*focused], mode, inner.0, status).map(|()| false);
    }
    // The interrupt contract doesn't get a modal exemption: Ctrl-C means
    // "stop" from inside a half-typed pattern too.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(true);
    }
    if matches!(mode, Mode::Search(_)) {
        search_key(key, &mut panes[*focused], mode, inner, status);
        return Ok(false);
    }
    if matches!(mode, Mode::Select) {
        return select_key(key, &mut panes[*focused], mode, inner, status);
    }
    normal_key(key, panes, focused, mode, inner, status, mouse)
}

/// Is this Ctrl-Shift-C, in either spelling a terminal may use for it?
///
/// With `DISAMBIGUATE_ESCAPE_CODES` alone the chord arrives as the base key
/// plus both modifiers (`Char('c')` + CONTROL | SHIFT). A terminal that
/// volunteers the shifted codepoint too — crossterm reads it and clears
/// SHIFT — makes it `Char('C')` + CONTROL. Both are the same keypress.
fn is_copy_chord(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && match key.code {
            KeyCode::Char('c') => key.modifiers.contains(KeyModifiers::SHIFT),
            KeyCode::Char('C') => true,
            _ => false,
        }
}

/// Copy whatever the current mode means by "copy" — the selection while one
/// is being made, otherwise the view. That is `y`'s meaning in each mode,
/// and Ctrl-Shift-C is bound to it rather than to a mode of its own.
fn copy_current(
    pane: &mut Pane,
    mode: &mut Mode,
    inner_height: u16,
    status: &mut Option<Status>,
) -> Res<()> {
    if matches!(mode, Mode::Select)
        && let Some(sel) = pane.sel
    {
        copy_selection(pane, &sel, status)?;
        leave_select(pane, mode);
        return Ok(());
    }
    copy_view(pane, inner_height, status)
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
                "mouse captured — click focuses a pane, drag selects its text; \
                 your terminal's own selection is off until m"
            } else {
                "mouse released — double-click, drag and copy with your terminal \
                 as usual (it can't see the panes), m to take it back"
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

/// One keystroke in selection mode. Motions move the cursor and extend the
/// selection from its anchor — vim's visual mode, where there is no way to
/// move without extending; press Esc and `v` again to start elsewhere.
///
/// The motions here are all vertical, and they leave the grain alone: they
/// extend a `v` selection by whole lines and a dragged one by characters
/// from the same column, so picking up where the mouse left off doesn't
/// silently coarsen what it selected.
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
    let Some(sel) = pane.sel else {
        // Nothing to be selecting: the buffer emptied out under us.
        leave_select(pane, mode);
        return Ok(false);
    };
    let last = len.saturating_sub(1);
    let mut line = sel.cursor.line.min(last);
    match key.code {
        // `q` still quits, from every mode: a cancel key that only sometimes
        // exits the app is worse than losing a selection.
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Esc | KeyCode::Char('v') => {
            leave_select(pane, mode);
            return Ok(false);
        }
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            copy_selection(pane, &sel, status)?;
            leave_select(pane, mode);
            return Ok(false);
        }
        KeyCode::Up | KeyCode::Char('k') => line = line.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => line = (line + 1).min(last),
        KeyCode::PageUp => line = line.saturating_sub(usize::from(page)),
        KeyCode::PageDown => line = (line + usize::from(page)).min(last),
        KeyCode::Home | KeyCode::Char('g') => line = 0,
        KeyCode::End | KeyCode::Char('G') => line = last,
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
    pane.sel = Some(Selection {
        anchor: Point { line: sel.anchor.line.min(last), ..sel.anchor },
        cursor: Point { line, ..sel.cursor },
        ..sel
    });
    let max = pane.max_scroll(inner_height);
    pane.scroll.offset = scroll_to_show(line, pane.scroll.offset, inner_height, max);
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
    pane.sel = Some(Selection::line(bottom));
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

/// The unit a copy notice counts in. Lines for everything that copies
/// whole ones; characters where a line count would be useless — a mouse
/// selection inside one line is honestly "1 line" and tells the user
/// nothing about whether two characters went out or two hundred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counted {
    Lines,
    Chars,
}

/// Copy `span` of a pane's lines — or the whole pane, for `None`.
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
    copy_text(&lines, what, Counted::Lines, status)
}

/// Copy exactly what `sel` covers: whole lines for `Grain::Lines`, and for
/// `Grain::Chars` the first and last clipped to the two ends.
fn copy_selection(pane: &Pane, sel: &Selection, status: &mut Option<Status>) -> Res<()> {
    let lines = selection_text(pane, sel);
    // Counted in characters only where the whole selection sits inside one
    // line; once it spans lines, lines are what the user is thinking in.
    let counted =
        if sel.grain == Grain::Chars && lines.len() == 1 { Counted::Chars } else { Counted::Lines };
    copy_text(&lines, "the selection", counted, status)
}

/// The text a selection covers, one entry per line it touches.
fn selection_text(pane: &Pane, sel: &Selection) -> Vec<String> {
    let content = lock(&pane.content);
    let len = content.lines.len();
    let Some((lo, hi)) = sel.line_span(len) else { return Vec::new() };
    content
        .lines
        .iter()
        .enumerate()
        .skip(lo)
        .take(hi + 1 - lo)
        .filter_map(|(index, line)| {
            selected_bytes(line, index, sel, len).map(|(from, to)| line[from..to].to_string())
        })
        .collect()
}

/// Push `lines` out through OSC 52 and report what went, in `counted`'s
/// unit.
///
/// A terminal that refuses OSC 52 pastes looks identical from in here to one
/// that accepted it (see [`clipboard`]), so the notice says what was *sent*;
/// `m` is the documented way out when it turns out the terminal dropped it.
fn copy_text(lines: &[String], what: &str, counted: Counted, status: &mut Option<Status>) -> Res<()> {
    if lines.is_empty() {
        note(status, "nothing to copy yet", true);
        return Ok(());
    }
    let copied = clipboard::copy_lines(lines)?;
    let (count, unit) = match counted {
        Counted::Lines => (copied.lines, if copied.lines == 1 { "line" } else { "lines" }),
        Counted::Chars => (copied.chars, if copied.chars == 1 { "character" } else { "characters" }),
    };
    let msg = if copied.truncated {
        format!(
            "copied {count} {unit} of {what} — cut off at {} KB, past what terminals accept",
            clipboard::MAX_CLIP_BYTES / 1000
        )
    } else {
        format!("copied {count} {unit} of {what}")
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

/// Take the mouse for the TUI, or hand it back to the terminal.
///
/// Capture is what makes click-to-focus and drag-to-select work, and it is
/// the default because those are the gestures that respect the split:
/// the terminal's own selection knows nothing about the panes and drags one
/// rectangle across both of them and the footer. What capture costs is that
/// same terminal-side selection with its double-click, triple-click and
/// native copy — the copy route that works even where OSC 52 is refused or
/// a multiplexer strips it. So it stays a toggle, `m` releases it, and
/// `ALT_SCROLL_ON` keeps the wheel scrolling while it is released. (Most
/// terminals also give it back for one gesture under Shift-drag, without
/// the toggle.)
fn set_mouse_capture(on: bool) -> Res<()> {
    let mut out = std::io::stdout();
    if on {
        execute!(out, EnableMouseCapture).context("enabling mouse capture")
    } else {
        execute!(out, DisableMouseCapture).context("disabling mouse capture")
    }
}

/// Where the left button went down, if it is still down: the anchor a drag
/// selects from. A click on its own only focuses the pane and clears what
/// was selected — it takes actual movement to *make* a selection, so
/// click-to-focus stays a click, not a one-character selection.
struct DragStart {
    pane: usize,
    at: Point,
    /// Where the pointer was last seen, anywhere on the screen. Kept
    /// because the mouse falls silent the moment it stops moving: this is
    /// the position [`autoscroll`] goes on extending from while the button
    /// is held outside the pane.
    pos: (u16, u16),
    /// When the next autoscroll step is due.
    due: Instant,
}

fn handle_mouse(
    event: MouseEvent,
    panes: &mut [Pane; 2],
    focused: &mut usize,
    layout: (Rect, Rect),
    mode: &mut Mode,
    drag: &mut Option<DragStart>,
) {
    // A half-typed pattern is the one thing the mouse must not disturb: the
    // prompt belongs to the pane that opened it, and `SearchPrompt` holds
    // the view state Esc puts back. Moving focus or starting a selection
    // out from under it would strand both, so while it is open the mouse
    // does nothing at all — the pane a wheel notch would scroll is the
    // pane being searched, and it is already scrolling to the hits. Any
    // gesture in flight is abandoned with it, so it can't resume later
    // against an anchor from before the prompt.
    if matches!(mode, Mode::Search(_)) {
        *drag = None;
        return;
    }
    // The left button's own events are settled before the pane hit-test
    // below, because a gesture in flight owns them wherever the pointer has
    // got to: it belongs to the pane the press landed in, and running out
    // of that pane — into the other one, or over the footer — should keep
    // extending to the edge of its text, as any editor does, rather than
    // freeze the selection where it last crossed the pane. Out there the
    // pane also scrolls after the pointer, which is the clock's job rather
    // than this function's — see [`autoscroll`]. The release is hoisted
    // with it so that letting go out there still ends the gesture, instead
    // of leaving an anchor standing for the next event to extend.
    if event.kind == MouseEventKind::Drag(MouseButton::Left) {
        if let Some(start) = drag {
            start.pos = (event.column, event.row);
            let rect = pane_rect(start.pane, layout);
            extend_drag(&mut panes[start.pane], rect, start.at, start.pos, mode);
        }
        return;
    }
    // The selection outlives the gesture — `y` comes after the release.
    // Bare motion ends it too: with button-event tracking the terminal
    // reports movement only while a button is down, so a `Moved` is proof
    // the release happened somewhere we couldn't see it (the pointer left
    // the window). Without that the anchor would stand and the autoscroll
    // clock would go on scrolling toward a pointer that isn't there.
    if matches!(event.kind, MouseEventKind::Up(MouseButton::Left) | MouseEventKind::Moved) {
        *drag = None;
        return;
    }
    let pos = (event.column, event.row);
    let hit = if in_rect(pos, layout.0) {
        Some(STDOUT)
    } else if in_rect(pos, layout.1) {
        Some(STDERR)
    } else {
        None
    };
    let Some(i) = hit else { return };
    let rect = pane_rect(i, layout);
    let inner_height = rect.height.saturating_sub(2);
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            *focused = i;
            // Pressing down begins a new gesture, so whatever the last one
            // left highlighted goes: a click that only moved focus must not
            // leave a stale selection lit in the pane behind it, and
            // `Mode::Select` is global while `sel` is per-pane, so a click
            // into the other pane would otherwise leave the mode pointing
            // at a pane with nothing selected.
            clear_selections(panes, mode);
            *drag = point_at(&panes[i], rect, event.column, event.row).map(|at| DragStart {
                pane: i,
                at,
                pos: (event.column, event.row),
                due: Instant::now() + AUTOSCROLL_TICK,
            });
        }
        // Any other button: focus, and nothing else. Unlike the left one it
        // starts no gesture of ours, so a middle-click paste or a
        // right-click inside the focused pane leaves a selection standing.
        MouseEventKind::Down(_) => {
            focus_pane(i, focused, panes, mode);
            *drag = None;
        }
        MouseEventKind::ScrollUp => {
            focus_pane(i, focused, panes, mode);
            panes[i].scroll.up(WHEEL_STEP);
        }
        MouseEventKind::ScrollDown => {
            focus_pane(i, focused, panes, mode);
            let max = panes[i].max_scroll(inner_height);
            panes[i].scroll.down(WHEEL_STEP, max);
        }
        _ => {}
    }
}

/// Pull `row` onto one of `rect`'s content rows, for a drag that has left
/// the pane vertically. Rows outside stay outside for a pane too short to
/// have any — [`line_at`] rejects those anyway.
fn clamp_row(rect: Rect, row: u16) -> u16 {
    if rect.height < 3 {
        return row;
    }
    row.clamp(rect.y + 1, rect.y + rect.height - 2)
}

/// Extend the drag anchored at `start` to the pointer at `pos`, which may
/// be anywhere on the screen: a gesture in flight owns the pointer wherever
/// it has got to, so a position off the pane means the near edge of its
/// text (see [`clamp_row`] and [`point_at`]'s column clamp), never
/// "nowhere".
fn extend_drag(pane: &mut Pane, rect: Rect, start: Point, pos: (u16, u16), mode: &mut Mode) {
    let (column, row) = pos;
    if let Some(at) = point_at(pane, rect, column, clamp_row(rect, row)) {
        // Dragging is a selection gesture; with capture on the terminal
        // never sees it, so the TUI has to mean it.
        pane.scroll.follow = false;
        pane.sel = Some(Selection::chars(start, at));
        *mode = Mode::Select;
    }
}

/// Scroll `pane` one step after a drag that has run off `rect`'s edges, and
/// say whether the view actually moved.
///
/// Clamping the pointer back onto the text is only half of what a selection
/// running off the edge should do — the other half is that the pane follows
/// it, the way the terminal's own selection scrolls the scrollback when a
/// drag reaches the top of the window. It can't be driven by mouse events,
/// which stop the instant the pointer stops moving even though the button
/// is still down and still outside; the main loop calls this every
/// [`AUTOSCROLL_TICK`] instead, for as long as a drag is in flight, and
/// re-extends the selection whenever it answers `true`.
///
/// A step that moves nothing is no reason to stop calling: a pane pinned at
/// the bottom of its buffer starts moving again on its own as the log grows
/// under it.
fn autoscroll(pane: &mut Pane, rect: Rect, pos: (u16, u16)) -> bool {
    let (dx, dy) = autoscroll_delta(rect, pos);
    if (dx, dy) == (0, 0) {
        return false;
    }
    let before = (pane.scroll.offset, pane.hscroll);
    if dy != 0 {
        let max = pane.max_scroll(rect.height.saturating_sub(2));
        if dy < 0 {
            pane.scroll.up(cells(dy));
        } else {
            pane.scroll.down(cells(dy), max);
            // Reaching the bottom re-arms follow, which would slide the
            // text out from under the selection being made. The gesture in
            // flight outranks it; `End` is how the user asks for it back.
            pane.scroll.follow = false;
        }
    }
    if dx != 0 {
        let max = pane.max_hscroll(rect.width.saturating_sub(2));
        pane.hscroll = if dx < 0 {
            pane.hscroll.saturating_sub(cells(dx))
        } else {
            pane.hscroll.saturating_add(cells(dx)).min(max)
        };
    }
    (pane.scroll.offset, pane.hscroll) != before
}

/// One autoscroll step for a pointer at `pos`: signed columns and rows to
/// move the view by, both zero while it is still over `rect`'s text.
///
/// Speed is the overshoot itself, so a pointer a cell past the edge crawls
/// (and stays steerable, a character at a time) while one thrown well clear
/// of the pane travels — the acceleration every terminal and editor gives a
/// drag-selection, without needing to track how long it has been out there.
fn autoscroll_delta(rect: Rect, pos: (u16, u16)) -> (i32, i32) {
    let (x, y) = pos;
    let span = |lo: u16, len: u16| (lo + 1, lo.saturating_add(len).saturating_sub(2));
    // A pane too small to have any content cells has no edges to run off.
    let dx = if rect.width < 3 { 0 } else { overshoot(span(rect.x, rect.width), x) };
    let dy = if rect.height < 3 { 0 } else { overshoot(span(rect.y, rect.height), y) };
    (dx, dy)
}

/// How far `at` is outside the inclusive range `lo..=hi`, signed toward the
/// end it left and capped at [`AUTOSCROLL_MAX`]; zero inside.
fn overshoot((lo, hi): (u16, u16), at: u16) -> i32 {
    let cap = i32::from(AUTOSCROLL_MAX);
    if at < lo {
        -i32::from(lo - at).min(cap)
    } else if at > hi {
        i32::from(at - hi).min(cap)
    } else {
        0
    }
}

/// The magnitude of one axis of an [`autoscroll_delta`], as the cell count
/// the scroll and pan helpers take. Capped at [`AUTOSCROLL_MAX`] there, so
/// the fallback is unreachable.
fn cells(delta: i32) -> u16 {
    u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX)
}

/// Move focus to pane `i` on a gesture that isn't itself a selection.
///
/// Focus moving is what makes an existing selection untenable: `Mode::Select`
/// is global and its bindings, its footer count and `copy_current` all read
/// the *focused* pane's `sel`, so focus must never land on a pane that has
/// none. Wheeling over the other pane therefore ends the selection, the
/// same as Esc would.
fn focus_pane(i: usize, focused: &mut usize, panes: &mut [Pane; 2], mode: &mut Mode) {
    if i != *focused {
        *focused = i;
        clear_selections(panes, mode);
    }
}

/// Drop any selection either pane is holding and leave `Mode::Select`.
///
/// The mouse equivalent of Esc, and the reason it takes both panes: `sel`
/// lives on the pane so rendering needs to know nothing about modes, but
/// the mode itself is global, so the two can only be cleared together.
fn clear_selections(panes: &mut [Pane; 2], mode: &mut Mode) {
    for pane in panes {
        pane.sel = None;
    }
    *mode = Mode::Normal;
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

/// Buffer position under a mouse event at `(column, row)` in `rect`:
/// [`line_at`]'s line, plus how far into it the column lands.
///
/// The column is measured in characters from the line's start, so the
/// pane's horizontal pan is added back in — what is drawn in the leftmost
/// content cell is character `hscroll`, not character 0. Unlike the row it
/// clamps rather than failing on a border cell: the borders are one cell of
/// slop either side of the text, and a press that lands on one means the
/// near edge of the text, not "nowhere".
fn point_at(pane: &Pane, rect: Rect, column: u16, row: u16) -> Option<Point> {
    let line = line_at(pane, rect, row)?;
    let rightmost = rect.width.saturating_sub(3);
    let within = column.saturating_sub(rect.x.saturating_add(1)).min(rightmost);
    Some(Point { line, col: usize::from(pane.hscroll) + usize::from(within) })
}

/// The rect pane `i` was last drawn in.
fn pane_rect(i: usize, layout: (Rect, Rect)) -> Rect {
    if i == STDOUT { layout.0 } else { layout.1 }
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
        let sel = pane.sel;
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
            .map(|(index, line)| {
                let selected = sel.and_then(|sel| selected_bytes(line, index, &sel, count));
                render_line(line, index, selected, query, current)
            })
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
        block = block.title(Line::from(Span::styled(label, Style::new().fg(Color::Yellow))).centered());
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

/// Build one display line: the selected characters reversed, the active
/// pattern's matches picked out inside it.
///
/// `sel` is the byte range of *this* line that the selection covers — the
/// caller resolves it (see [`selected_bytes`]), which is what keeps this a
/// pure function of one string and one range.
///
/// The two highlights are independent and can overlap part-way through a
/// word, so rather than walking one and nesting the other the line is cut
/// at every edge of either, and each piece styled from what covers it.
/// Match positions are recomputed per visible line per frame rather than
/// cached: forty short lines against one regex is nothing beside the
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
    let hits = query.map(|q| q.hits_in(text)).unwrap_or_default();
    // An empty range is a line inside the selection that contributes no
    // characters — real, and worth a blank line to the clipboard, but
    // nothing to draw.
    let sel = sel.filter(|(from, to)| from < to);
    if hits.is_empty() && sel.is_none() {
        return Line::from(Span::raw(text.to_string()));
    }
    let mut cuts: Vec<usize> = Vec::with_capacity(hits.len() * 2 + 4);
    cuts.push(0);
    cuts.push(text.len());
    for &(start, end) in &hits {
        cuts.push(start);
        cuts.push(end);
    }
    if let Some((from, to)) = sel {
        cuts.push(from);
        cuts.push(to);
    }
    // Char boundaries only: an offset the producer has invalidated by
    // extending the line mid-character is a cut we can do without, and
    // slicing there would panic inside the TUI (see `char_col`).
    cuts.retain(|&at| at <= text.len() && text.is_char_boundary(at));
    cuts.sort_unstable();
    cuts.dedup();
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(cuts.len());
    // Hits are ordered and disjoint, so one cursor walks them alongside the
    // pieces instead of each piece rescanning the whole list.
    let mut next = 0;
    for piece in cuts.windows(2) {
        let (from, to) = (piece[0], piece[1]);
        while next < hits.len() && hits[next].1 <= from {
            next += 1;
        }
        let hit = hits.get(next).filter(|&&(start, end)| from >= start && to <= end);
        let selected = sel.is_some_and(|(start, end)| from >= start && to <= end);
        let is_current = hit.is_some_and(|&(start, _)| {
            current.is_some_and(|hit| hit.line == index && hit.start == start)
        });
        // Inside the selection the reverse video is already spoken for, so
        // a match there is marked by weight instead of colour — two
        // backgrounds fighting over one cell reads as neither.
        let style = match (selected, hit.is_some(), is_current) {
            (true, _, true) => SELECT_STYLE.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            (true, true, false) => SELECT_STYLE.add_modifier(Modifier::UNDERLINED),
            (true, false, false) => SELECT_STYLE,
            (false, _, true) => CURRENT_MATCH_STYLE,
            (false, true, false) => MATCH_STYLE,
            (false, false, false) => Style::new(),
        };
        spans.push(Span::styled(text[from..to].to_string(), style));
    }
    if spans.is_empty() {
        // Nothing but zero-width matches on an empty line.
        spans.push(Span::raw(text.to_string()));
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
        Line::from(Span::styled(
            format!(
                "{} selected · j/k G g extend · {} copy · Esc cancel · q quit",
                pane.sel.map_or_else(|| "nothing".to_string(), |sel| selection_size(pane, &sel)),
                copy_keys(rich_keys())
            ),
            dim,
        ))
    } else {
        Line::from(Span::styled(hint(area.width, mouse, rich_keys()), dim))
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// How much is selected, phrased in the unit the selection was made in:
/// characters where the whole of it sits inside one line, lines otherwise.
/// Matches what the copy notice will go on to say it sent (see
/// [`copy_selection`]) — a footer counting lines followed by a notice
/// counting characters reads as two different copies.
fn selection_size(pane: &Pane, sel: &Selection) -> String {
    let content = lock(&pane.content);
    let len = content.lines.len();
    let Some((lo, hi)) = sel.line_span(len) else { return "nothing".to_string() };
    if sel.grain == Grain::Chars && lo == hi {
        let chars = content
            .lines
            .get(lo)
            .and_then(|line| {
                selected_bytes(line, lo, sel, len).map(|(from, to)| line[from..to].chars().count())
            })
            .unwrap_or(0);
        let plural = if chars == 1 { "character" } else { "characters" };
        return format!("{chars} {plural}");
    }
    let lines = hi + 1 - lo;
    let plural = if lines == 1 { "line" } else { "lines" };
    format!("{lines} {plural}")
}

/// How to spell the copy binding for the user: Ctrl-Shift-C is only worth
/// naming where the terminal can actually deliver it (see
/// [`enable_rich_keys`]) — advertising it elsewhere would be advertising a
/// key that quits.
fn copy_keys(rich: bool) -> &'static str {
    if rich { "y ^⇧C" } else { "y" }
}

/// The key hint, in two lengths. On a narrow terminal the long one would be
/// truncated mid-word, so the short one keeps the bindings that can't be
/// guessed — search and copy — and drops the ones an arrow key finds by
/// itself.
fn hint(width: u16, mouse: bool, rich: bool) -> String {
    let mouse_key = if mouse { "m free mouse" } else { "m grab mouse" };
    let copy = copy_keys(rich);
    let full = format!(
        "Tab focus · ↑/↓ PgUp/PgDn scroll · ←/→ pan · End follow · / search · n/N hits · \
         drag or v select · {copy} copy view · Y copy pane · {mouse_key} · q quit"
    );
    if usize::from(width) >= full.chars().count() {
        full
    } else {
        format!("/ search · n/N hits · drag/v select · {copy} copy · {mouse_key} · q quit")
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

    // -- is_copy_chord ------------------------------------------------------

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn copy_chord_matches_ctrl_shift_c_as_the_base_key_plus_both_modifiers() {
        let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        assert!(is_copy_chord(&key(KeyCode::Char('c'), ctrl_shift)));
    }

    #[test]
    fn copy_chord_matches_the_shifted_spelling_a_terminal_may_send_instead() {
        // Alternate-key reporting hands crossterm 'C' and clears SHIFT.
        assert!(is_copy_chord(&key(KeyCode::Char('C'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn copy_chord_does_not_swallow_plain_ctrl_c() {
        assert!(!is_copy_chord(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn copy_chord_needs_control() {
        assert!(!is_copy_chord(&key(KeyCode::Char('C'), KeyModifiers::SHIFT)));
        assert!(!is_copy_chord(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
    }

    // -- hint -------------------------------------------------------------

    #[test]
    fn hint_uses_the_full_text_when_it_fits() {
        let full = hint(u16::MAX, false, false);
        assert!(full.contains("Tab focus"));
    }

    #[test]
    fn hint_switches_to_the_short_form_when_narrow() {
        let short = hint(10, false, false);
        assert!(!short.contains("Tab focus"));
        assert!(short.contains("search"));
    }

    #[test]
    fn hint_names_the_copy_chord_only_where_the_terminal_can_send_it() {
        assert!(hint(u16::MAX, false, true).contains("^⇧C"));
        assert!(hint(10, false, true).contains("^⇧C"));
        assert!(!hint(u16::MAX, false, false).contains("^⇧C"));
        assert!(!hint(10, false, false).contains("^⇧C"));
    }

    #[test]
    fn hint_reflects_mouse_capture_state() {
        assert!(hint(u16::MAX, true, false).contains("m free mouse"));
        assert!(hint(u16::MAX, false, false).contains("m grab mouse"));
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

    /// A whole-line selection from `anchor` to `cursor` — `v`'s shape,
    /// which is what most of these tests only need a pair of lines for.
    fn lines_sel(anchor: usize, cursor: usize) -> Selection {
        Selection {
            anchor: Point { line: anchor, col: 0 },
            cursor: Point { line: cursor, col: 0 },
            grain: Grain::Lines,
        }
    }

    /// A character selection from `(anchor_line, anchor_col)` to
    /// `(cursor_line, cursor_col)` — what a drag makes.
    fn chars_sel(anchor: (usize, usize), cursor: (usize, usize)) -> Selection {
        Selection::chars(
            Point { line: anchor.0, col: anchor.1 },
            Point { line: cursor.0, col: cursor.1 },
        )
    }

    #[test]
    fn shift_view_by_zero_is_a_no_op() {
        let mut scroll = ScrollState { offset: 5, follow: false };
        let mut sel = Some(lines_sel(3, 7));
        let mut search = Some(PaneSearch {
            query: Query::new("x").unwrap(),
            dir: Direction::Forward,
            hit: Some(Hit { line: 4, start: 0, end: 1 }),
            count: (1, 1),
        });
        shift_view(&mut scroll, &mut sel, &mut search, 0);
        assert_eq!(scroll.offset, 5);
        assert_eq!(sel, Some(lines_sel(3, 7)));
        assert_eq!(search.as_ref().unwrap().hit.unwrap().line, 4);
    }

    #[test]
    fn shift_view_slides_scroll_selection_and_search_hit_together() {
        let mut scroll = ScrollState { offset: 10, follow: false };
        let mut sel = Some(lines_sel(8, 12));
        let mut search = Some(PaneSearch {
            query: Query::new("x").unwrap(),
            dir: Direction::Forward,
            hit: Some(Hit { line: 9, start: 0, end: 1 }),
            count: (1, 1),
        });
        shift_view(&mut scroll, &mut sel, &mut search, 3);
        assert_eq!(scroll.offset, 7);
        assert_eq!(sel, Some(lines_sel(5, 9)));
        assert_eq!(search.as_ref().unwrap().hit.unwrap().line, 6);
    }

    #[test]
    fn shift_view_saturates_at_zero_instead_of_wrapping() {
        let mut scroll = ScrollState { offset: 2, follow: false };
        let mut sel = Some(lines_sel(1, 3));
        let mut search: Option<PaneSearch> = None;
        shift_view(&mut scroll, &mut sel, &mut search, 10);
        assert_eq!(scroll.offset, 0);
        assert_eq!(sel, Some(lines_sel(0, 0)));
    }

    #[test]
    fn shift_view_tolerates_no_selection_and_no_search() {
        let mut scroll = ScrollState { offset: 5, follow: false };
        let mut sel: Option<Selection> = None;
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

    // -- point_at / clamp_row -----------------------------------------------

    /// A pane holding one ten-character line, for the column math.
    fn alphabet_pane() -> Pane {
        let mut content = test_content();
        content.ingest(b"abcdefghij\n");
        test_pane(content)
    }

    #[test]
    fn point_at_maps_a_column_to_a_character_offset_past_the_border() {
        let pane = alphabet_pane();
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        // Column 0 is the left border, so column 1 holds character 0.
        assert_eq!(point_at(&pane, rect, 1, 1), Some(Point { line: 0, col: 0 }));
        assert_eq!(point_at(&pane, rect, 4, 1), Some(Point { line: 0, col: 3 }));
    }

    #[test]
    fn point_at_adds_the_panes_horizontal_pan_back_in() {
        let mut pane = alphabet_pane();
        pane.hscroll = 20;
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        assert_eq!(point_at(&pane, rect, 1, 1), Some(Point { line: 0, col: 20 }));
    }

    #[test]
    fn point_at_clamps_a_column_on_either_border_to_the_nearest_text() {
        let pane = alphabet_pane();
        // Content columns are 5..=14, i.e. characters 0..=9.
        let rect = Rect { x: 4, y: 0, width: 12, height: 5 };
        assert_eq!(point_at(&pane, rect, 4, 1), Some(Point { line: 0, col: 0 })); // left border
        assert_eq!(point_at(&pane, rect, 0, 1), Some(Point { line: 0, col: 0 })); // further left
        assert_eq!(point_at(&pane, rect, 15, 1), Some(Point { line: 0, col: 9 })); // right border
        assert_eq!(point_at(&pane, rect, 99, 1), Some(Point { line: 0, col: 9 }));
    }

    #[test]
    fn point_at_is_none_wherever_line_at_is() {
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        assert_eq!(point_at(&test_pane(test_content()), rect, 2, 1), None); // empty pane
        assert_eq!(point_at(&alphabet_pane(), rect, 2, 0), None); // top border
    }

    #[test]
    fn clamp_row_pulls_a_drag_that_left_the_pane_back_onto_its_text() {
        // Content rows are 4..=9.
        let rect = Rect { x: 0, y: 3, width: 20, height: 8 };
        assert_eq!(clamp_row(rect, 0), 4);
        assert_eq!(clamp_row(rect, 6), 6);
        assert_eq!(clamp_row(rect, 40), 9);
    }

    #[test]
    fn clamp_row_leaves_a_pane_with_no_text_rows_alone() {
        let rect = Rect { x: 0, y: 0, width: 20, height: 2 };
        assert_eq!(clamp_row(rect, 7), 7);
    }

    // -- autoscroll ----------------------------------------------------------

    /// A pane holding `n` numbered lines padded to `width` characters —
    /// enough text for a drag to scroll and pan through.
    fn scrollable_pane(n: usize, width: usize) -> Pane {
        let mut content = test_content();
        for i in 0..n {
            content.ingest(format!("{i:width$}\n").as_bytes());
        }
        test_pane(content)
    }

    #[test]
    fn autoscroll_delta_is_zero_while_the_pointer_is_over_the_text() {
        // Content cells are columns 5..=14, rows 4..=9.
        let rect = Rect { x: 4, y: 3, width: 12, height: 8 };
        assert_eq!(autoscroll_delta(rect, (5, 4)), (0, 0));
        assert_eq!(autoscroll_delta(rect, (14, 9)), (0, 0));
        assert_eq!(autoscroll_delta(rect, (9, 6)), (0, 0));
    }

    #[test]
    fn autoscroll_delta_grows_with_the_overshoot_and_stops_at_the_cap() {
        let rect = Rect { x: 4, y: 3, width: 12, height: 8 };
        assert_eq!(autoscroll_delta(rect, (4, 6)), (-1, 0)); // one cell left
        assert_eq!(autoscroll_delta(rect, (0, 6)), (-5, 0));
        assert_eq!(autoscroll_delta(rect, (15, 10)), (1, 1)); // past both far edges
        let cap = i32::from(AUTOSCROLL_MAX);
        assert_eq!(autoscroll_delta(rect, (999, 999)), (cap, cap));
        assert_eq!(autoscroll_delta(rect, (0, 0)), (-5, -4)); // out of the top corner
    }

    #[test]
    fn autoscroll_delta_leaves_a_pane_with_no_text_cells_alone() {
        let rect = Rect { x: 0, y: 0, width: 2, height: 2 };
        assert_eq!(autoscroll_delta(rect, (9, 9)), (0, 0));
    }

    #[test]
    fn a_drag_below_a_pane_scrolls_it_down_to_the_end_and_no_further() {
        let mut pane = scrollable_pane(20, 4);
        // Content rows are 1..=3; row 5 is two below the last of them.
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        assert!(autoscroll(&mut pane, rect, (5, 5)));
        assert_eq!(pane.scroll.offset, 2);
        assert!(!pane.scroll.follow, "a drag must not leave the view following");
        for _ in 0..20 {
            autoscroll(&mut pane, rect, (5, 5));
        }
        assert_eq!(pane.scroll.offset, pane.max_scroll(3)); // 20 lines, 3 rows
        assert!(!autoscroll(&mut pane, rect, (5, 5)), "nothing left to scroll to");
        assert!(!pane.scroll.follow, "hitting the bottom must not re-arm follow mid-drag");
    }

    #[test]
    fn a_drag_above_a_pane_scrolls_it_up_to_the_top_and_no_further() {
        let mut pane = scrollable_pane(20, 4);
        pane.scroll.offset = 10;
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        assert!(autoscroll(&mut pane, rect, (5, 0)));
        assert_eq!(pane.scroll.offset, 9);
        for _ in 0..20 {
            autoscroll(&mut pane, rect, (5, 0));
        }
        assert_eq!(pane.scroll.offset, 0);
        assert!(!autoscroll(&mut pane, rect, (5, 0)));
    }

    #[test]
    fn a_drag_off_the_side_of_a_pane_pans_it_within_the_widest_line() {
        let mut pane = scrollable_pane(4, 40);
        // Content columns are 1..=10; column 14 is four past the last.
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        assert!(autoscroll(&mut pane, rect, (14, 2)));
        assert_eq!(pane.hscroll, 4);
        for _ in 0..20 {
            autoscroll(&mut pane, rect, (14, 2));
        }
        assert_eq!(pane.hscroll, pane.max_hscroll(10)); // 40 columns, 10 shown
        assert!(!autoscroll(&mut pane, rect, (14, 2)), "no text left to pan to");
        // ...and back the other way, a column at a time.
        assert!(autoscroll(&mut pane, rect, (0, 2)));
        assert_eq!(pane.hscroll, 29);
    }

    #[test]
    fn a_drag_into_a_corner_scrolls_both_ways_at_once() {
        let mut pane = scrollable_pane(20, 40);
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        assert!(autoscroll(&mut pane, rect, (13, 5)));
        assert_eq!((pane.scroll.offset, pane.hscroll), (2, 3));
    }

    #[test]
    fn autoscroll_does_nothing_while_the_pointer_is_still_inside() {
        let mut pane = scrollable_pane(20, 40);
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        assert!(!autoscroll(&mut pane, rect, (5, 2)));
        assert_eq!((pane.scroll.offset, pane.hscroll), (0, 0));
        assert!(pane.scroll.follow, "hovering inside is not a scroll, so follow stands");
    }

    #[test]
    fn an_autoscrolled_drag_extends_the_selection_to_the_line_it_scrolled_to() {
        let mut pane = scrollable_pane(20, 4);
        let rect = Rect { x: 0, y: 0, width: 12, height: 5 };
        let start = Point { line: 0, col: 0 };
        let pos = (5, 5);
        assert!(autoscroll(&mut pane, rect, pos));
        let mut mode = Mode::Normal;
        extend_drag(&mut pane, rect, start, pos, &mut mode);
        // Offset 2 puts line 4 on the last content row, which is where the
        // clamped pointer lands.
        assert_eq!(pane.sel.map(|s| s.cursor.line), Some(4));
        assert!(matches!(mode, Mode::Select));
    }

    // -- Selection / byte_of_char / selected_bytes ---------------------------

    /// Render `text` under `sel` and spell the highlight back out with `[]`
    /// around the reversed run — the readable form of a character-granular
    /// selection in an assertion.
    fn marked(text: &str, sel: Option<(usize, usize)>) -> String {
        let mut out = String::new();
        for span in &render_line(text, 0, sel, None, None).spans {
            if span.style.add_modifier.contains(Modifier::REVERSED) {
                out.push('[');
                out.push_str(&span.content);
                out.push(']');
            } else {
                out.push_str(&span.content);
            }
        }
        out
    }

    #[test]
    fn byte_of_char_counts_chars_not_bytes() {
        assert_eq!(byte_of_char("héllo", 0), 0);
        assert_eq!(byte_of_char("héllo", 1), 1);
        // The two-byte é sits between characters 1 and 2.
        assert_eq!(byte_of_char("héllo", 2), 3);
        assert_eq!(byte_of_char("héllo", 3), 4);
    }

    #[test]
    fn byte_of_char_past_the_end_of_the_line_is_its_length() {
        assert_eq!(byte_of_char("abc", 3), 3);
        assert_eq!(byte_of_char("abc", 99), 3);
        assert_eq!(byte_of_char("", 0), 0);
    }

    #[test]
    fn ordered_puts_a_backwards_drag_into_document_order() {
        assert_eq!(
            chars_sel((7, 2), (3, 9)).ordered(),
            (Point { line: 3, col: 9 }, Point { line: 7, col: 2 })
        );
        // Same line, cursor left of the anchor.
        assert_eq!(
            chars_sel((4, 8), (4, 2)).ordered(),
            (Point { line: 4, col: 2 }, Point { line: 4, col: 8 })
        );
    }

    #[test]
    fn a_line_selection_covers_every_byte_of_every_line_it_spans() {
        let sel = lines_sel(1, 2);
        assert_eq!(selected_bytes("abc", 0, &sel, 4), None);
        assert_eq!(selected_bytes("abc", 1, &sel, 4), Some((0, 3)));
        assert_eq!(selected_bytes("abcdef", 2, &sel, 4), Some((0, 6)));
        assert_eq!(selected_bytes("abc", 3, &sel, 4), None);
    }

    #[test]
    fn a_character_selection_inside_one_line_includes_both_ends() {
        // Columns 2..=4 of "abcdefg" are "cde": the character under the
        // pointer is in, as it is in vim's charwise visual.
        let sel = chars_sel((0, 2), (0, 4));
        assert_eq!(selected_bytes("abcdefg", 0, &sel, 1), Some((2, 5)));
        assert_eq!(marked("abcdefg", selected_bytes("abcdefg", 0, &sel, 1)), "ab[cde]fg");
    }

    #[test]
    fn a_character_selection_reads_the_same_dragged_either_way() {
        let there = chars_sel((0, 2), (0, 4));
        let back = chars_sel((0, 4), (0, 2));
        assert_eq!(selected_bytes("abcdefg", 0, &there, 1), selected_bytes("abcdefg", 0, &back, 1));
    }

    #[test]
    fn a_character_selection_clips_only_its_first_and_last_lines() {
        let sel = chars_sel((1, 4), (3, 1));
        assert_eq!(selected_bytes("zero", 0, &sel, 5), None);
        assert_eq!(selected_bytes("one-tail", 1, &sel, 5), Some((4, 8))); // col 4 to the end
        assert_eq!(selected_bytes("two", 2, &sel, 5), Some((0, 3))); // a middle line, whole
        assert_eq!(selected_bytes("three", 3, &sel, 5), Some((0, 2))); // through col 1
        assert_eq!(selected_bytes("four", 4, &sel, 5), None);
    }

    #[test]
    fn a_high_end_past_its_line_takes_that_line_to_its_end() {
        // Dragging out past the ragged right edge of a log selects to the
        // end of the line, which is what any editor does.
        let sel = chars_sel((0, 1), (0, 99));
        assert_eq!(selected_bytes("abc", 0, &sel, 1), Some((1, 3)));
    }

    #[test]
    fn a_low_end_past_its_line_contributes_no_characters() {
        // Pressing in the blank space right of a short line and dragging
        // down: the line is in the selection but gives it nothing, which
        // copies as the blank leading line an editor would give you.
        let sel = chars_sel((0, 99), (1, 2));
        assert_eq!(selected_bytes("abc", 0, &sel, 2), Some((3, 3)));
        assert_eq!(selected_bytes("defgh", 1, &sel, 2), Some((0, 3)));
    }

    #[test]
    fn a_character_selection_slices_multibyte_text_on_char_boundaries() {
        // "héllo" is six bytes; columns 1..=2 are "él", bytes 1..4.
        let sel = chars_sel((0, 1), (0, 2));
        assert_eq!(selected_bytes("héllo", 0, &sel, 1), Some((1, 4)));
        assert_eq!(marked("héllo", selected_bytes("héllo", 0, &sel, 1)), "h[él]lo");
    }

    #[test]
    fn selected_bytes_on_an_empty_buffer_is_none() {
        assert_eq!(selected_bytes("", 0, &chars_sel((0, 0), (0, 0)), 0), None);
    }

    #[test]
    fn selected_bytes_cannot_come_out_inverted_by_clamping() {
        // Both ends past the buffer's end land on its last line from
        // opposite sides, which would otherwise read as end-before-start.
        let sel = chars_sel((9, 4), (12, 1));
        assert_eq!(selected_bytes("abcdef", 1, &sel, 2), Some((4, 4)));
    }

    // -- render_line's two overlapping highlights ---------------------------

    #[test]
    fn a_selection_and_a_match_can_overlap_part_way_through_a_word() {
        let query = Query::new("cde").unwrap();
        // The selection covers "bcd", the match "cde": the line is cut at
        // both edges of both, and the pieces inside the selection keep its
        // reverse video while the match there is marked by an underline.
        let line = render_line("abcdef", 0, Some((1, 4)), Some(&query), None);
        let pieces: Vec<(&str, Style)> =
            line.spans.iter().map(|span| (span.content.as_ref(), span.style)).collect();
        assert_eq!(
            pieces,
            vec![
                ("a", Style::new()),
                ("b", SELECT_STYLE),
                ("cd", SELECT_STYLE.add_modifier(Modifier::UNDERLINED)),
                ("e", MATCH_STYLE),
                ("f", Style::new()),
            ]
        );
    }

    #[test]
    fn the_current_match_inside_a_selection_is_marked_by_weight_not_colour() {
        let query = Query::new("cd").unwrap();
        let current = Some(Hit { line: 0, start: 2, end: 4 });
        let line = render_line("abcdef", 0, Some((0, 6)), Some(&query), current);
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::REVERSED)),
            "every piece of a fully selected line stays reversed"
        );
        let hit = &line.spans[1];
        assert_eq!(hit.content.as_ref(), "cd");
        assert!(hit.style.add_modifier.contains(Modifier::BOLD | Modifier::UNDERLINED));
        assert_eq!(hit.style.bg, None, "no background to fight the selection's reverse video");
    }

    #[test]
    fn an_empty_selected_range_draws_nothing() {
        assert_eq!(marked("abc", Some((2, 2))), "abc");
    }

    // -- selection_text / selection_size ------------------------------------

    fn phonetic_pane() -> Pane {
        let mut content = test_content();
        content.ingest(b"alpha\nbravo\ncharlie\n");
        test_pane(content)
    }

    #[test]
    fn selection_text_clips_the_first_and_last_lines_of_a_character_selection() {
        let pane = phonetic_pane();
        let sel = chars_sel((0, 2), (2, 3));
        assert_eq!(selection_text(&pane, &sel), vec!["pha", "bravo", "char"]);
    }

    #[test]
    fn selection_text_of_a_line_selection_is_the_lines_entire() {
        let pane = phonetic_pane();
        assert_eq!(selection_text(&pane, &lines_sel(0, 1)), vec!["alpha", "bravo"]);
    }

    #[test]
    fn selection_size_counts_characters_inside_one_line_and_lines_across_them() {
        let pane = phonetic_pane();
        assert_eq!(selection_size(&pane, &chars_sel((0, 1), (0, 3))), "3 characters");
        assert_eq!(selection_size(&pane, &chars_sel((0, 1), (0, 1))), "1 character");
        assert_eq!(selection_size(&pane, &chars_sel((0, 1), (2, 1))), "3 lines");
        // `v`'s selection is counted in lines even when it is only one.
        assert_eq!(selection_size(&pane, &Selection::line(1)), "1 line");
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
        let sim =
            simulate_frames(Duration::from_millis(100), Duration::from_secs(1), Duration::from_secs(60));
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
        let short =
            simulate_frames(Duration::from_millis(100), Duration::from_secs(1), Duration::from_secs(30));
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
        assert_eq!(pane.sel, Some(Selection::line(14)));
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
        pane.sel = Some(lines_sel(1, 2));
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
