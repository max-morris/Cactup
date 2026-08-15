//! Full-screen side-by-side TUI for `FollowMode::Both` when stdout is a
//! terminal: stdout live-tails in the left pane, stderr in the right, each
//! independently scrollable. [`crate::tail::follow_combined`] is the
//! non-TTY fallback ([`follow_tui`] is only ever entered after that check).
//!
//! This module is split into two halves on purpose: plain data/state
//! (`ScrollState`, `Pane`, `sanitize`) that's exercised directly by the unit
//! tests at the bottom, and the terminal/event-loop plumbing around it that
//! isn't practical to unit-test and is kept as thin as possible instead.

use super::{LogTail, PollBackoff};
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Lines retained per pane; oldest are dropped once this is exceeded.
const MAX_LINES: usize = 10_000;
/// Trailing bytes of an existing file read to seed a pane at startup;
/// bounds startup memory/IO against multi-GB sim logs.
const SEED_BYTES: u64 = 4 * 1024 * 1024;
/// Columns panned per Left/Right keypress or horizontal wheel notch.
const PAN_STEP: u16 = 8;
/// Lines scrolled per mouse-wheel notch.
const WHEEL_STEP: u16 = 3;
/// Input-poll tick. A file poll is only actually performed once
/// `PollBackoff`'s interval has elapsed since the last one (see the loop in
/// [`run_app`]) — this tick just keeps keyboard/mouse input responsive.
const TICK: Duration = Duration::from_millis(50);

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

/// One pane's buffered content and view state.
struct Pane {
    label: String,
    path: PathBuf,
    tail: LogTail,
    /// Whether the underlying file has been seen to exist yet — before that
    /// the pane shows a "waiting for ..." placeholder.
    exists: bool,
    lines: VecDeque<String>,
    /// True if the last entry in `lines` has no trailing newline yet, i.e.
    /// the next ingest should extend it rather than start a new line.
    open: bool,
    tab_col: usize,
    scroll: ScrollState,
    hscroll: u16,
}

impl Pane {
    /// Seed a pane from the trailing [`SEED_BYTES`] of `path` (if it exists
    /// yet), capped at [`MAX_LINES`], and position its `LogTail` to pick up
    /// exactly where the seed read left off. Only the tail is read — sim
    /// logs can run to tens of GB, far past what the pane can retain anyway.
    fn seeded(label: &str, path: PathBuf) -> Self {
        use std::io::{Read, Seek, SeekFrom};
        let mut pane = Pane {
            label: label.to_string(),
            tail: LogTail::new(path.clone(), 0),
            path,
            exists: false,
            lines: VecDeque::new(),
            open: false,
            tab_col: 0,
            scroll: ScrollState::new(),
            hscroll: 0,
        };
        if let Ok(mut f) = std::fs::File::open(&pane.path) {
            pane.exists = true;
            let start = f.metadata().map_or(0, |m| m.len().saturating_sub(SEED_BYTES));
            let mut content = Vec::new();
            if f.seek(SeekFrom::Start(start)).is_ok() && f.read_to_end(&mut content).is_ok() {
                let mut seed = &content[..];
                if start > 0 {
                    // Started mid-file: drop the leading partial line.
                    seed = match seed.iter().position(|&b| b == b'\n') {
                        Some(i) => &seed[i + 1..],
                        None => &[],
                    };
                }
                pane.ingest(seed);
                pane.tail = LogTail::new(pane.path.clone(), start + content.len() as u64);
            }
        }
        pane
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
            // Keep the current view stable as the oldest line drops off.
            self.scroll.offset = self.scroll.offset.saturating_sub(1);
        }
    }

    fn max_scroll(&self, inner_height: u16) -> u16 {
        (self.lines.len() as u16).saturating_sub(inner_height)
    }
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
    let panes = [
        Pane::seeded(sources[0].0, sources[0].1.clone()),
        Pane::seeded(sources[1].0, sources[1].1.clone()),
    ];
    let result = run_app(&mut terminal, panes, subject, &stop);
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

fn run_app(
    terminal: &mut Terminal<Backend>,
    mut panes: [Pane; 2],
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
    let mut backoff = PollBackoff::new();
    let mut last_poll: Option<Instant> = None;
    // Cached pane content-rects from the last draw, used to size PgUp/PgDn
    // and to hit-test mouse events between redraws.
    let mut layout = (Rect::default(), Rect::default());

    draw(terminal, &mut panes, focused, &mut layout)?;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let due = match last_poll {
            None => true,
            Some(t) => t.elapsed() >= backoff.interval(),
        };
        let mut dirty = false;
        if due {
            let mut had_data = false;
            for pane in &mut panes {
                if let Some(buf) = pane.tail.poll() {
                    pane.exists = true;
                    pane.ingest(&buf);
                    had_data = true;
                    dirty = true;
                }
            }
            backoff.note(had_data);
            last_poll = Some(Instant::now());
        }

        if event::poll(TICK).context("polling terminal events")? {
            match event::read().context("reading a terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(key, &mut panes, &mut focused, layout) {
                        return Ok(());
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    handle_mouse(mouse, &mut panes, &mut focused, layout);
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }

        if dirty {
            draw(terminal, &mut panes, focused, &mut layout)?;
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
            let max_h = max_line_width(&pane.lines);
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
    let max = pane.max_scroll(inner_height);
    let offset = pane.scroll.resolve(max);

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

    if !pane.exists && pane.lines.is_empty() {
        let waiting = Line::from(Span::styled(
            format!("waiting for {}…", pane.path.display()),
            Style::new().add_modifier(Modifier::DIM),
        ));
        frame.render_widget(Paragraph::new(waiting).block(block), area);
        return;
    }

    // Hand the Paragraph only the visible slice — vertical scrolling is done
    // here by slicing at `offset` (building all MAX_LINES Lines per frame
    // just for Paragraph to skip them is wasted work), horizontal panning is
    // left to `scroll`.
    let text: Vec<Line> = pane
        .lines
        .iter()
        .skip(offset as usize)
        .take(inner_height as usize)
        .map(|l| Line::from(l.as_str()))
        .collect();
    let paragraph = Paragraph::new(text).block(block).scroll((0, pane.hscroll));
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

    // -- Pane::ingest partial-line carry-over -----------------------------

    fn test_pane() -> Pane {
        Pane {
            label: "stdout".to_string(),
            path: PathBuf::from("/nonexistent"),
            tail: LogTail::new(PathBuf::from("/nonexistent"), 0),
            exists: true,
            lines: VecDeque::new(),
            open: false,
            tab_col: 0,
            scroll: ScrollState::new(),
            hscroll: 0,
        }
    }

    #[test]
    fn ingest_splits_complete_lines() {
        let mut pane = test_pane();
        pane.ingest(b"one\ntwo\nthree\n");
        assert_eq!(pane.lines, VecDeque::from(["one".to_string(), "two".to_string(), "three".to_string()]));
        assert!(!pane.open);
    }

    #[test]
    fn ingest_carries_partial_line_to_next_poll() {
        let mut pane = test_pane();
        pane.ingest(b"partial line, no newline yet");
        assert_eq!(pane.lines.len(), 1);
        assert!(pane.open);
        pane.ingest(b" continues here\n");
        assert_eq!(pane.lines.len(), 1);
        assert_eq!(pane.lines[0], "partial line, no newline yet continues here");
        assert!(!pane.open);
    }

    #[test]
    fn ingest_partial_then_more_partial_then_newline() {
        let mut pane = test_pane();
        pane.ingest(b"a");
        pane.ingest(b"b");
        pane.ingest(b"c\nd");
        assert_eq!(pane.lines, VecDeque::from(["abc".to_string(), "d".to_string()]));
        assert!(pane.open);
    }

    #[test]
    fn ingest_enforces_line_cap_and_shifts_scroll() {
        let mut pane = test_pane();
        pane.scroll.follow = false;
        pane.scroll.offset = 5;
        for i in 0..(MAX_LINES + 10) {
            pane.ingest(format!("line{i}\n").as_bytes());
        }
        assert_eq!(pane.lines.len(), MAX_LINES);
        assert_eq!(pane.lines.front().unwrap(), "line10");
        // 10 lines were evicted; offset should have been shifted down but
        // never below 0.
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
