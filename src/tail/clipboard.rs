//! Clipboard writes for the log-follow TUI's vim-style "yank selected
//! lines" feature, via the OSC 52 terminal escape sequence.
//!
//! OSC 52 (`ESC ] 52 ; c ; <base64> ESC \`) asks the terminal *emulator*
//! itself to put the given text on the system clipboard. That's the only
//! option available here: cactup is a MUSL static binary (`CLAUDE.md`'s
//! static-linking contract) and every real clipboard crate (`arboard`,
//! `copypasta`, ...) ultimately links X11/Wayland/Win32 shared libraries,
//! which the build cannot carry. OSC 52 needs nothing but bytes on stdout,
//! so it costs zero dependencies — and, unlike every X11/Wayland API, it
//! works unmodified over SSH into a login node, which is exactly how this
//! tool is normally run: the escape sequence rides the same pty all the way
//! back to the user's local terminal, which is the thing actually holding
//! the clipboard.
//!
//! The known limitation: OSC 52 clipboard writes are opt-in on many
//! terminals (xterm needs `allowWindowOps`; some terminals refuse it
//! outright) and a terminal that refuses it simply discards the sequence —
//! there is no ack, no error, nothing to detect. A copy can therefore
//! silently do nothing. That is exactly why the TUI's yank binding must
//! also offer a fallback: turn mouse capture off (so the terminal's own
//! click-drag selection works) and let the user copy with their terminal's
//! native selection instead.

use crate::Res;
use anyhow::Context;
use std::io::Write;

/// Cap on the number of bytes of text we will try to push through OSC 52.
pub(crate) const MAX_CLIP_BYTES: usize = 100_000;

/// Outcome of a copy attempt: how much actually went out.
pub(crate) struct Copied {
    pub(crate) lines: usize,
    pub(crate) truncated: bool,
}

/// Whether a multiplexer between us and the real terminal needs the
/// sequence wrapped to get through it. This tool runs almost exclusively
/// inside tmux or screen on a login node, and the two want opposite
/// treatment — see [`wrap_from_env`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrap {
    /// Send the OSC 52 sequence as-is.
    None,
    /// Wrap it in GNU screen's DCS passthrough, chunked (see [`screen_wrap`]).
    Screen,
}

/// Pure decision logic behind [`detect_wrap`], taking the two env vars as
/// plain `Option<&str>` so it's unit-testable without touching the process
/// environment (`std::env::set_var` is `unsafe` as of the 2024 edition this
/// crate is on, so tests must not reach for it).
///
/// `$TMUX` being set at all — regardless of its value, which is a
/// socket-path/pid triple nobody parses — means the process is running
/// inside a tmux pane. Screen leaves no equally reliable variable of its
/// own (`$STY` is screen's session name, but only screen sets a `$TERM`
/// starting with `screen`, and that's true whether or not `$STY` happens to
/// survive into this process), so `$TERM`'s prefix is the check used here.
///
/// Tmux gets **no** wrapping, which is not an oversight. tmux understands
/// OSC 52 natively: with `set-clipboard` at its default `external` it
/// forwards an application's sequence out to the real terminal, and at `on`
/// it also sets its own paste buffer — so a bare sequence works on a stock
/// tmux. Its DCS passthrough (`ESC P tmux; …`) does the opposite of what's
/// wanted here: it hands the bytes to the outer terminal *uninterpreted*,
/// so tmux's own buffer is never set, and it is refused outright unless
/// `allow-passthrough on` has been turned on by hand — off by default since
/// tmux 3.3. Wrapping would therefore trade a mechanism that works out of
/// the box for one that needs the user to configure something first.
/// (Verified against tmux 3.5a: wrapped, `show-buffer` stays empty; bare,
/// the yanked lines land in the buffer.)
///
/// Tmux is still checked *first*, because tmux's own default `$TERM` is
/// `screen`/`screen-256color`: inside tmux the screen branch below would
/// otherwise match and wrap a sequence tmux would then never unwrap.
fn wrap_from_env(tmux: Option<&str>, term: Option<&str>) -> Wrap {
    if tmux.is_some() {
        Wrap::None
    } else if term.is_some_and(|t| t.starts_with("screen")) {
        Wrap::Screen
    } else {
        Wrap::None
    }
}

/// Read `$TMUX`/`$TERM` from the real process environment and classify it;
/// see [`wrap_from_env`] for the actual logic.
fn detect_wrap() -> Wrap {
    wrap_from_env(std::env::var("TMUX").ok().as_deref(), std::env::var("TERM").ok().as_deref())
}

/// Standard base64 alphabet (RFC 4648 §4), padded with `=`.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A small hand-rolled base64 encoder so this module doesn't need to add a
/// crate for the one encoding OSC 52 requires — the payload is opaque
/// bytes, so `bytes` rather than `str` is the more general signature.
fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(BASE64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(BASE64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { BASE64_ALPHABET[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// Longest single GNU screen DCS passthrough string we ever emit. Older
/// screen builds truncate (some silently drop) a passthrough string much
/// longer than this, so a payload anywhere near [`MAX_CLIP_BYTES`] must be
/// split across several DCS strings rather than sent as one; 768 is
/// comfortably under every documented screen limit, with headroom to spare
/// for the two-byte introducer/terminator screen itself doesn't count
/// against the budget.
const SCREEN_CHUNK: usize = 768;

/// Wrap `inner` (a complete, already-built OSC 52 sequence) for GNU screen:
/// split across multiple `ESC P ... ESC \` DCS strings of at most
/// [`SCREEN_CHUNK`] bytes each, concatenated back to back. Screen passes
/// the contents of consecutive DCS strings straight through to the real
/// terminal, which reassembles them into the one OSC 52 sequence they
/// started as.
///
/// Chunking on raw byte offsets is safe here specifically because `inner`
/// is guaranteed pure ASCII (escape/control bytes plus the base64
/// alphabet) — there is no multi-byte UTF-8 sequence that a byte-offset cut
/// could ever split.
fn screen_wrap(inner: &str) -> String {
    debug_assert!(inner.is_ascii(), "OSC 52 sequences are ASCII; screen_wrap assumes byte == char");
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len().div_ceil(SCREEN_CHUNK) * 4);
    for chunk in bytes.chunks(SCREEN_CHUNK) {
        out.push_str("\x1bP");
        // Safe by the ASCII precondition above: any byte offset is a char
        // boundary.
        out.push_str(std::str::from_utf8(chunk).expect("ASCII OSC 52 sequence is valid UTF-8"));
        out.push_str("\x1b\\");
    }
    out
}

/// Build the escape sequence that asks the terminal to put `text` on the
/// system clipboard, wrapped for whatever multiplexer `wrap` says sits
/// between us and it. Pure and unit-tested; [`copy_lines`] is the only
/// caller that actually writes the result anywhere.
fn osc52_sequence(text: &str, wrap: Wrap) -> String {
    let payload = base64(text.as_bytes());
    // ST (`ESC \`) rather than BEL: BEL-terminated OSC is the older,
    // less consistently supported form for a payload this long, and ST is
    // what every terminal that implements OSC 52 clipboard writes expects.
    let inner = format!("\x1b]52;c;{payload}\x1b\\");
    match wrap {
        Wrap::None => inner,
        Wrap::Screen => screen_wrap(&inner),
    }
}

/// Truncate `text` to at most `max_bytes` bytes, always on a UTF-8 char
/// boundary — cutting mid-sequence would hand `base64` invalid UTF-8 to
/// mangle further and would corrupt the copy in a way that isn't even
/// visible until the paste. Terminals and multiplexers alike drop an
/// over-long OSC 52 payload *entirely* rather than accepting a prefix of
/// it, so sending a truncated selection is strictly better than sending one
/// byte over the limit and having the whole copy silently vanish.
fn truncate_to_char_boundary(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// Copy `lines` (joined with '\n', no trailing newline) to the system
/// clipboard via OSC 52, writing to stdout and flushing.
///
/// This deliberately writes straight to `stdout()` rather than through
/// ratatui: the sequence is not grid content — nothing about it should
/// appear as a cell on screen — so ratatui's buffer must never see it,
/// exactly like the `SetTitle` escape in `tui.rs`. It must be called while
/// the terminal is still in raw mode, from the TUI's own thread (the reader
/// thread never touches the terminal — see this module's sibling `tui.rs`
/// for that split), so the bytes land on the same stream the terminal is
/// already parsing escape sequences out of.
pub(crate) fn copy_lines(lines: &[String]) -> Res<Copied> {
    let joined = lines.join("\n");
    let (text, truncated) = truncate_to_char_boundary(&joined, MAX_CLIP_BYTES);
    let sequence = osc52_sequence(text, detect_wrap());
    let mut stdout = std::io::stdout();
    stdout.write_all(sequence.as_bytes()).context("writing the OSC 52 clipboard sequence")?;
    stdout.flush().context("flushing the OSC 52 clipboard sequence")?;
    // Report what actually went out rather than what was asked for: a
    // truncated copy sends fewer lines than it was handed, and the notice in
    // the TUI has to be able to say so honestly.
    let sent = if truncated {
        if text.is_empty() { 0 } else { text.matches('\n').count() + 1 }
    } else {
        lines.len()
    };
    Ok(Copied { lines: sent, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- base64 -------------------------------------------------------

    #[test]
    fn base64_empty_input() {
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn base64_one_byte_pads_with_two_equals() {
        // 'M' = 0b01001101 -> "TQ=="
        assert_eq!(base64(b"M"), "TQ==");
    }

    #[test]
    fn base64_two_bytes_pads_with_one_equals() {
        // "Ma" -> "TWE="
        assert_eq!(base64(b"Ma"), "TWE=");
    }

    #[test]
    fn base64_three_bytes_needs_no_padding() {
        // "Man" -> "TWFu"
        assert_eq!(base64(b"Man"), "TWFu");
    }

    #[test]
    fn base64_known_string_with_padding() {
        // A well-known reference vector, long enough to span several
        // 3-byte groups plus a final padded group.
        assert_eq!(base64(b"hello"), "aGVsbG8=");
    }

    // -- osc52_sequence -------------------------------------------------

    #[test]
    fn sequence_unwrapped_has_the_bare_osc_prefix_and_st_terminator() {
        let seq = osc52_sequence("hi", Wrap::None);
        assert!(seq.starts_with("\x1b]52;c;"), "{seq:?}");
        assert!(seq.ends_with("\x1b\\"), "{seq:?}");
        assert!(seq.contains(&base64(b"hi")));
    }

    #[test]
    fn sequence_inside_tmux_is_the_bare_one_not_a_dcs_passthrough() {
        // The regression this guards: wrapping in `ESC P tmux;` hands the
        // bytes to the outer terminal uninterpreted, so tmux never sets its
        // own paste buffer, and it needs `allow-passthrough on` (off by
        // default) even to be forwarded at all. See `wrap_from_env`.
        let seq = osc52_sequence("hi", wrap_from_env(Some("/tmp/tmux-1000/default,1,0"), None));
        assert!(seq.starts_with("\x1b]52;c;"), "{seq:?}");
        assert!(!seq.contains("tmux;"), "{seq:?}");
    }

    #[test]
    fn sequence_screen_wraps_a_short_payload_in_one_dcs_string() {
        let seq = osc52_sequence("hi", Wrap::Screen);
        assert_eq!(seq.matches("\x1bP").count(), 1);
        assert!(seq.starts_with("\x1bP"), "{seq:?}");
        assert!(seq.ends_with("\x1b\\"), "{seq:?}");
    }

    #[test]
    fn sequence_screen_chunks_a_long_payload_across_several_dcs_strings() {
        let text = "x".repeat(SCREEN_CHUNK * 3);
        let seq = osc52_sequence(&text, Wrap::Screen);

        let inner = format!("\x1b]52;c;{}\x1b\\", base64(text.as_bytes()));
        let expected_chunks = inner.len().div_ceil(SCREEN_CHUNK);
        assert!(expected_chunks > 1, "test payload wasn't long enough to force chunking");
        assert_eq!(seq.matches("\x1bP").count(), expected_chunks);

        // Reassembling every chunk reproduces the exact, unchunked inner
        // sequence — nothing was dropped, reordered, or corrupted at a
        // chunk boundary. `inner` always ends with its own `ESC \`
        // terminator, so the final chunk's content legitimately contains an
        // *embedded* `ESC \` immediately before the wrapper's own trailing
        // one; splitting on `ESC P` (which never otherwise occurs inside
        // `inner` — it only ever contains `ESC ]` and `ESC \`) and trimming
        // exactly the wrapper's own trailing terminator with
        // `strip_suffix` sidesteps that ambiguity, unlike scanning forward
        // for the first `ESC \`.
        let mut parts = seq.split("\x1bP");
        assert_eq!(parts.next(), Some(""), "seq must start with the first chunk's ESC P");
        let reassembled: String = parts
            .map(|chunk| chunk.strip_suffix("\x1b\\").expect("every DCS chunk is ST-terminated"))
            .collect();
        assert_eq!(reassembled, inner);
    }

    // -- wrap_from_env ----------------------------------------------------

    #[test]
    fn wrap_from_env_no_multiplexer() {
        assert_eq!(wrap_from_env(None, None), Wrap::None);
        assert_eq!(wrap_from_env(None, Some("xterm-256color")), Wrap::None);
    }

    #[test]
    fn wrap_from_env_leaves_tmux_unwrapped() {
        assert_eq!(wrap_from_env(Some("/tmp/tmux-1000/default,1234,0"), None), Wrap::None);
    }

    #[test]
    fn wrap_from_env_detects_screen_by_term_prefix() {
        assert_eq!(wrap_from_env(None, Some("screen")), Wrap::Screen);
        assert_eq!(wrap_from_env(None, Some("screen.xterm-256color")), Wrap::Screen);
    }

    #[test]
    fn wrap_from_env_does_not_screen_wrap_inside_tmux() {
        // tmux's own default $TERM is `screen`, so this is the common case,
        // not a corner: screen-wrapping in a tmux pane would produce a
        // sequence nothing ever unwraps.
        assert_eq!(wrap_from_env(Some("anything"), Some("screen")), Wrap::None);
    }

    // -- truncate_to_char_boundary -----------------------------------------

    #[test]
    fn truncate_leaves_short_text_untouched() {
        let (out, truncated) = truncate_to_char_boundary("hello", MAX_CLIP_BYTES);
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_never_splits_a_multibyte_char() {
        // '€' is 3 bytes, so a byte-count truncation done naively would land
        // mid-character almost everywhere.
        let text = "€".repeat(40_000); // 120,000 bytes, over MAX_CLIP_BYTES
        let (out, truncated) = truncate_to_char_boundary(&text, MAX_CLIP_BYTES);
        assert!(truncated);
        assert!(out.len() <= MAX_CLIP_BYTES);
        // 100_000 isn't a multiple of 3, so the naive cut point (byte
        // 100_000) is mid-character; the real cut must back up to the
        // nearest char boundary, i.e. 99_999 (33_333 whole '€'s).
        assert_eq!(out.len(), 99_999);
        assert!(out.chars().all(|c| c == '€'));
        assert_eq!(out.chars().count(), 33_333);
    }

    #[test]
    fn truncate_exactly_at_the_limit_is_not_truncated() {
        let text = "a".repeat(MAX_CLIP_BYTES);
        let (out, truncated) = truncate_to_char_boundary(&text, MAX_CLIP_BYTES);
        assert_eq!(out.len(), MAX_CLIP_BYTES);
        assert!(!truncated);
    }
}
