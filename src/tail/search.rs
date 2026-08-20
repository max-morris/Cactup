//! Vim-flavored `/` search for the log-follow TUI (`cactup sim log
//! --follow`), factored out of `tui.rs` so it can be unit-tested without a
//! terminal — no ratatui, no crossterm, just retained lines in and match
//! positions out. §8.5
//!
//! A search runs over one pane's retained lines (a `VecDeque<String>`,
//! capped at `MAX_LINES` — see `tui.rs`) and is recompiled from scratch on
//! every keystroke of incremental search rather than cached or indexed:
//! scanning up to 10k short lines with a compiled regex costs microseconds
//! to low milliseconds, comfortably inside one frame, so there is no payoff
//! for the complexity of an index or a background search thread.
//!
//! The pattern is a regex, not a literal, because these are logs: the
//! motivating use case is typing `ERROR|WARN` and having both light up.
//!
//! Case-folding is smart-case, matching the vim/most-editors convention:
//! a pattern with no uppercase ASCII letter searches case-insensitively, one
//! with any uppercase letter searches case-sensitively. The detection is a
//! plain scan of the pattern text for an uppercase ASCII letter, so an
//! escaped class like `\W` "reads" as uppercase and flips the query to
//! case-sensitive even though `W` there isn't a literal letter to match
//! case-sensitively against. That's a known simplification — a real regex
//! parse could tell the difference — but it matches what most editors'
//! smart-case does closely enough not to be worth the complexity here.

use regex::{Regex, RegexBuilder};
use std::collections::VecDeque;

/// Which way `n`/`N` and the initial jump travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Forward,
    Backward,
}

impl Direction {
    pub(crate) fn reverse(self) -> Direction {
        match self {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        }
    }
}

/// One match: which retained line, and the byte range within it. The range
/// always lands on `char` boundaries (it comes straight out of `regex`'s
/// match spans over a UTF-8 `str`), so callers can slice the line with it
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hit {
    pub(crate) line: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// A located hit plus whether finding it wrapped past the buffer end — vim's
/// "search hit BOTTOM, continuing at TOP" (and the reverse) notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) hit: Hit,
    pub(crate) wrapped: bool,
}

/// A compiled query plus the pattern text it came from, so the caller (the
/// footer) can redisplay what's being searched for without holding onto the
/// original string separately.
#[derive(Debug, Clone)]
pub(crate) struct Query {
    pattern: String,
    re: Regex,
}

impl Query {
    /// Compile `pattern` with smart-case (see the module doc). An empty
    /// pattern is rejected outright — an empty regex matches at every byte
    /// position, which would light up the whole buffer — rather than
    /// compiled as a technically-valid no-op regex. On a bad pattern,
    /// returns the regex error's message, already user-presentable and
    /// collapsed to one line for the footer.
    pub(crate) fn new(pattern: &str) -> Result<Query, String> {
        if pattern.is_empty() {
            return Err("empty search pattern".to_string());
        }
        let case_insensitive = !pattern.chars().any(|c| c.is_ascii_uppercase());
        let re = RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| e.to_string().lines().collect::<Vec<_>>().join(" "))?;
        Ok(Query { pattern: pattern.to_string(), re })
    }

    pub(crate) fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Every match in one line, in order, as byte ranges. Zero-width matches
    /// (e.g. `^`, `x*`) are included exactly once per position `regex`'s own
    /// iterator stops at — it already advances by one `char` rather than
    /// looping, so this never spins or splits a UTF-8 boundary.
    pub(crate) fn hits_in(&self, line: &str) -> Vec<(usize, usize)> {
        self.re.find_iter(line).map(|m| (m.start(), m.end())).collect()
    }

    /// Every hit across `lines`, in reading order: line ascending, then
    /// position within the line ascending. Recomputed on demand (see the
    /// module doc for why that's cheap enough not to cache).
    fn all_hits(&self, lines: &VecDeque<String>) -> Vec<Hit> {
        let mut hits = Vec::new();
        for (line, text) in lines.iter().enumerate() {
            for (start, end) in self.hits_in(text) {
                hits.push(Hit { line, start, end });
            }
        }
        hits
    }

    /// The nearest hit strictly after (Forward) / before (Backward) `from`,
    /// wrapping around the ends.
    pub(crate) fn find(
        &self,
        lines: &VecDeque<String>,
        from: (usize, usize),
        dir: Direction,
    ) -> Option<Found> {
        self.locate(lines, from, dir, false)
    }

    /// The first hit at or after (Forward) / at or before (Backward) `from`
    /// — used for incremental search while the user is still typing, where
    /// the cursor must not creep forward on every keystroke.
    pub(crate) fn find_from_inclusive(
        &self,
        lines: &VecDeque<String>,
        from: (usize, usize),
        dir: Direction,
    ) -> Option<Found> {
        self.locate(lines, from, dir, true)
    }

    /// Shared implementation of `find`/`find_from_inclusive`: build the flat
    /// ordered hit list, then walk it for the nearest hit on the requested
    /// side of `from` (strict or inclusive), wrapping to the opposite end if
    /// none qualifies. Wrapping to *itself* when there is exactly one hit
    /// sitting at `from` — vim's own behavior — falls straight out of this:
    /// the strict search finds nothing past `from`, so it wraps to the only
    /// hit there is, `wrapped: true`.
    fn locate(
        &self,
        lines: &VecDeque<String>,
        from: (usize, usize),
        dir: Direction,
        inclusive: bool,
    ) -> Option<Found> {
        if lines.is_empty() {
            return None;
        }
        let hits = self.all_hits(lines);
        if hits.is_empty() {
            return None;
        }
        let from = clamp_from(lines, from);
        match dir {
            Direction::Forward => {
                let found = hits.iter().find(|h| {
                    let key = (h.line, h.start);
                    if inclusive { key >= from } else { key > from }
                });
                match found {
                    Some(&hit) => Some(Found { hit, wrapped: false }),
                    None => Some(Found { hit: hits[0], wrapped: true }),
                }
            }
            Direction::Backward => {
                let found = hits.iter().rev().find(|h| {
                    let key = (h.line, h.start);
                    if inclusive { key <= from } else { key < from }
                });
                match found {
                    Some(&hit) => Some(Found { hit, wrapped: false }),
                    None => Some(Found {
                        hit: *hits.last().expect("checked non-empty above"),
                        wrapped: true,
                    }),
                }
            }
        }
    }
}

/// Clamp a `(line, byte-offset)` cursor to something safe to compare against
/// `lines`: an out-of-range line index clamps to the last line, and an
/// out-of-range column clamps to that line's byte length. `lines` must be
/// non-empty (callers check before reaching here). The clamped value is only
/// ever used as a comparison key against other `(line, start)` pairs, never
/// to slice a string, so it doesn't need to land on a `char` boundary.
fn clamp_from(lines: &VecDeque<String>, from: (usize, usize)) -> (usize, usize) {
    let last = lines.len() - 1;
    let line = from.0.min(last);
    let col = from.1.min(lines[line].len());
    (line, col)
}

/// Ordinal of `hit` among all hits in `lines`, 1-based, and the total hit
/// count — for the `[3/17]` counter vim-with-`shortmess+=S` shows. `hit` not
/// actually being a hit of `q` over `lines` (shouldn't happen in practice)
/// reports ordinal `0` rather than panicking.
pub(crate) fn hit_ordinal(q: &Query, lines: &VecDeque<String>, hit: Hit) -> (usize, usize) {
    let hits = q.all_hits(lines);
    let ordinal = hits.iter().position(|h| *h == hit).map_or(0, |i| i + 1);
    (ordinal, hits.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> VecDeque<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // -- Direction ----------------------------------------------------------

    #[test]
    fn direction_reverses() {
        assert_eq!(Direction::Forward.reverse(), Direction::Backward);
        assert_eq!(Direction::Backward.reverse(), Direction::Forward);
    }

    // -- Query::new / smart-case / empty pattern -----------------------------

    #[test]
    fn empty_pattern_is_rejected() {
        let err = Query::new("").unwrap_err();
        assert_eq!(err, "empty search pattern");
    }

    #[test]
    fn bad_regex_reports_a_single_line_message() {
        let err = Query::new("(unclosed").unwrap_err();
        assert!(!err.is_empty());
        assert!(!err.contains('\n'), "error message should be single-line: {err:?}");
    }

    #[test]
    fn lowercase_pattern_is_case_insensitive() {
        let q = Query::new("error").unwrap();
        assert_eq!(q.hits_in("an ERROR occurred").len(), 1);
        assert_eq!(q.hits_in("an Error occurred").len(), 1);
    }

    #[test]
    fn uppercase_pattern_is_case_sensitive() {
        let q = Query::new("ERROR").unwrap();
        assert_eq!(q.hits_in("an ERROR occurred").len(), 1);
        assert_eq!(q.hits_in("an error occurred").len(), 0);
    }

    #[test]
    fn mixed_case_pattern_is_case_sensitive() {
        let q = Query::new("Error").unwrap();
        assert_eq!(q.hits_in("Error").len(), 1);
        assert_eq!(q.hits_in("error").len(), 0);
        assert_eq!(q.hits_in("ERROR").len(), 0);
    }

    #[test]
    fn escaped_uppercase_class_still_flips_to_case_sensitive() {
        // Known simplification (see module doc): `\W` is detected purely by
        // scanning the pattern text for an uppercase ASCII letter, so it
        // reads as uppercase even though it's an escape, not a literal
        // case-sensitive letter — flipping smart-case off here.
        let q = Query::new(r"a\Wb").unwrap();
        // Case-sensitive now, so this only matches the exact-case sequence.
        assert_eq!(q.hits_in("a b"), vec![(0, 3)]);
        assert!(q.hits_in("A B").is_empty());
    }

    #[test]
    fn pattern_accessor_returns_the_original_text() {
        let q = Query::new("ERROR|WARN").unwrap();
        assert_eq!(q.pattern(), "ERROR|WARN");
    }

    // -- hits_in: multiple hits per line, in order ---------------------------

    #[test]
    fn hits_in_finds_every_occurrence_in_order() {
        let q = Query::new("ab").unwrap();
        assert_eq!(q.hits_in("ab cd ab ef ab"), vec![(0, 2), (6, 8), (12, 14)]);
    }

    #[test]
    fn hits_in_alternation_pattern() {
        let q = Query::new("ERROR|WARN").unwrap();
        assert_eq!(q.hits_in("WARN: low disk, ERROR: out of space"), vec![(0, 4), (16, 21)]);
    }

    #[test]
    fn hits_in_no_match_is_empty() {
        let q = Query::new("zzz").unwrap();
        assert!(q.hits_in("nothing here").is_empty());
    }

    // -- find: within-line stepping before moving lines ----------------------

    #[test]
    fn forward_find_steps_within_a_line_before_moving_on() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x x", "x"]);
        let f1 = q.find(&ls, (0, 0), Direction::Forward).unwrap();
        assert_eq!(f1, Found { hit: Hit { line: 0, start: 2, end: 3 }, wrapped: false });
        let f2 = q.find(&ls, (f1.hit.line, f1.hit.start), Direction::Forward).unwrap();
        assert_eq!(f2, Found { hit: Hit { line: 1, start: 0, end: 1 }, wrapped: false });
    }

    #[test]
    fn backward_find_steps_within_a_line_in_reverse_order() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x", "x x"]);
        let f1 = q.find(&ls, (1, 3), Direction::Backward).unwrap();
        assert_eq!(f1, Found { hit: Hit { line: 1, start: 2, end: 3 }, wrapped: false });
        let f2 = q.find(&ls, (f1.hit.line, f1.hit.start), Direction::Backward).unwrap();
        assert_eq!(f2, Found { hit: Hit { line: 1, start: 0, end: 1 }, wrapped: false });
        let f3 = q.find(&ls, (f2.hit.line, f2.hit.start), Direction::Backward).unwrap();
        assert_eq!(f3, Found { hit: Hit { line: 0, start: 0, end: 1 }, wrapped: false });
    }

    // -- wrapping -------------------------------------------------------------

    #[test]
    fn forward_find_wraps_from_last_hit_to_first() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x", "y", "x"]);
        let last = q.find(&ls, (0, 0), Direction::Forward).unwrap();
        assert_eq!(last.hit, Hit { line: 2, start: 0, end: 1 });
        let wrapped = q.find(&ls, (last.hit.line, last.hit.start), Direction::Forward).unwrap();
        assert_eq!(wrapped, Found { hit: Hit { line: 0, start: 0, end: 1 }, wrapped: true });
    }

    #[test]
    fn backward_find_wraps_from_first_hit_to_last() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x", "y", "x"]);
        let wrapped = q.find(&ls, (0, 0), Direction::Backward).unwrap();
        assert_eq!(wrapped, Found { hit: Hit { line: 2, start: 0, end: 1 }, wrapped: true });
    }

    #[test]
    fn forward_find_wraps_to_itself_when_it_is_the_only_hit() {
        // The vim quirk called out in the spec: a single hit under the
        // cursor still reports a (wrapped) match on `n`, rather than `None`.
        let q = Query::new("only").unwrap();
        let ls = lines(&["nothing", "only one hit here", "nothing"]);
        let found = q.find(&ls, (1, 0), Direction::Forward).unwrap();
        assert_eq!(found, Found { hit: Hit { line: 1, start: 0, end: 4 }, wrapped: true });
    }

    #[test]
    fn backward_find_wraps_to_itself_when_it_is_the_only_hit() {
        let q = Query::new("only").unwrap();
        let ls = lines(&["nothing", "only one hit here", "nothing"]);
        let found = q.find(&ls, (1, 0), Direction::Backward).unwrap();
        assert_eq!(found, Found { hit: Hit { line: 1, start: 0, end: 4 }, wrapped: true });
    }

    #[test]
    fn find_returns_none_when_there_is_no_match_at_all() {
        let q = Query::new("zzz").unwrap();
        let ls = lines(&["a", "b", "c"]);
        assert!(q.find(&ls, (0, 0), Direction::Forward).is_none());
        assert!(q.find(&ls, (0, 0), Direction::Backward).is_none());
    }

    // -- find_from_inclusive: cursor must not creep forward ------------------

    #[test]
    fn inclusive_forward_finds_a_hit_exactly_at_from() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x x"]);
        let found = q.find_from_inclusive(&ls, (0, 2), Direction::Forward).unwrap();
        assert_eq!(found, Found { hit: Hit { line: 0, start: 2, end: 3 }, wrapped: false });
    }

    #[test]
    fn inclusive_backward_finds_a_hit_exactly_at_from() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x x"]);
        let found = q.find_from_inclusive(&ls, (0, 0), Direction::Backward).unwrap();
        assert_eq!(found, Found { hit: Hit { line: 0, start: 0, end: 1 }, wrapped: false });
    }

    #[test]
    fn exclusive_forward_does_not_find_a_hit_exactly_at_from() {
        // Contrast with the inclusive test above: plain `find` requires
        // strictly *after* `from`, so sitting on a hit doesn't count.
        let q = Query::new("x").unwrap();
        let ls = lines(&["x"]);
        let found = q.find(&ls, (0, 0), Direction::Forward).unwrap();
        assert!(found.wrapped); // only one hit, so it wraps back to itself
    }

    // -- zero-width matches: must not loop and must advance by one char ------

    #[test]
    fn zero_width_caret_matches_once_per_line() {
        let q = Query::new("^").unwrap();
        let ls = lines(&["abc", "def"]);
        assert_eq!(q.hits_in("abc"), vec![(0, 0)]);
        // Walking forward must terminate and visit each line's ^ once.
        let f1 = q.find(&ls, (0, 0), Direction::Forward).unwrap();
        assert_eq!(f1, Found { hit: Hit { line: 1, start: 0, end: 0 }, wrapped: false });
        let f2 = q.find(&ls, (f1.hit.line, f1.hit.start), Direction::Forward).unwrap();
        assert_eq!(f2, Found { hit: Hit { line: 0, start: 0, end: 0 }, wrapped: true });
    }

    #[test]
    fn zero_width_star_advances_one_char_at_a_time_without_looping() {
        // `x*` matches the empty string between every `x`-free position too;
        // regex's own iterator must step by one char, not spin forever or
        // yield adjacent duplicate empty matches.
        let q = Query::new("x*").unwrap();
        let hits = q.hits_in("axxb");
        // "" before a, "xx" at 1..3, then straight to "" at the very end (4):
        // an empty match immediately at the end of the preceding non-empty
        // match (position 3, right after "xx") is not re-emitted — the
        // standard zero-width-adjacent-to-a-match suppression regex itself
        // implements — so there is no separate (3, 3) entry.
        assert_eq!(hits, vec![(0, 0), (1, 3), (4, 4)]);
    }

    #[test]
    fn zero_width_matches_never_split_a_multibyte_char_boundary() {
        // 'é' and '→' are multi-byte in UTF-8; every reported offset must be
        // a valid char boundary so callers can slice with it safely, and the
        // walk must advance past each one instead of getting stuck.
        let q = Query::new("x*").unwrap();
        let line = "é→x";
        let hits = q.hits_in(line);
        assert_eq!(hits, vec![(0, 0), (2, 2), (5, 6)]);
        for &(start, end) in &hits {
            assert!(line.is_char_boundary(start), "{start} not a char boundary in {line:?}");
            assert!(line.is_char_boundary(end), "{end} not a char boundary in {line:?}");
        }
    }

    // -- multi-byte input: hit ranges land on char boundaries ----------------

    #[test]
    fn hit_ranges_land_on_char_boundaries_around_multibyte_text() {
        let q = Query::new("world").unwrap();
        let line = "héllo → world";
        let hits = q.hits_in(line);
        assert_eq!(hits.len(), 1);
        let (start, end) = hits[0];
        assert!(line.is_char_boundary(start));
        assert!(line.is_char_boundary(end));
        assert_eq!(&line[start..end], "world");
    }

    // -- out-of-range `from` / empty `lines`: clamp, don't panic -------------

    #[test]
    fn find_with_from_past_end_of_line_clamps_instead_of_panicking() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x", "y"]);
        // Column far past the line's actual length.
        let found = q.find(&ls, (0, 9_999), Direction::Forward);
        // Nothing strictly after that point on this line or the next line
        // ("y" doesn't match), so it wraps back to the only hit.
        assert_eq!(found, Some(Found { hit: Hit { line: 0, start: 0, end: 1 }, wrapped: true }));
    }

    #[test]
    fn find_with_line_index_past_the_end_clamps_instead_of_panicking() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x", "y"]);
        let found = q.find(&ls, (500, 0), Direction::Forward);
        assert_eq!(found, Some(Found { hit: Hit { line: 0, start: 0, end: 1 }, wrapped: true }));
    }

    #[test]
    fn find_over_empty_lines_is_none() {
        let q = Query::new("x").unwrap();
        let ls: VecDeque<String> = VecDeque::new();
        assert!(q.find(&ls, (0, 0), Direction::Forward).is_none());
        assert!(q.find(&ls, (0, 0), Direction::Backward).is_none());
        assert!(q.find_from_inclusive(&ls, (0, 0), Direction::Forward).is_none());
    }

    // -- hit_ordinal ------------------------------------------------------

    #[test]
    fn hit_ordinal_reports_position_and_total() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x x", "y", "x"]);
        let hits = q.hits_in("x x");
        let second_on_first_line = Hit { line: 0, start: hits[1].0, end: hits[1].1 };
        assert_eq!(hit_ordinal(&q, &ls, second_on_first_line), (2, 3));
        let on_last_line = Hit { line: 2, start: 0, end: 1 };
        assert_eq!(hit_ordinal(&q, &ls, on_last_line), (3, 3));
    }

    #[test]
    fn hit_ordinal_unknown_hit_reports_zero() {
        let q = Query::new("x").unwrap();
        let ls = lines(&["x"]);
        let not_a_real_hit = Hit { line: 5, start: 0, end: 1 };
        let (ordinal, total) = hit_ordinal(&q, &ls, not_a_real_hit);
        assert_eq!(ordinal, 0);
        assert_eq!(total, 1);
    }
}
