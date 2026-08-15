//! `cactup wisdom` (spec §16): one random message from the compiled-in
//! corpus — cactup feature tips mixed with attributed words of wisdom —
//! plus the random after-any-command printing and the validators/renderers
//! for the `wisdom-frequency` and `wisdom-kind` knobs (§5).

use super::Ctx;
use crate::database::Database;
use crate::Res;
use anyhow::bail;
use colored::Colorize;
use std::io::IsTerminal;

// `TIPS`/`ZENS`: the corpus, parsed and validated from
// `resources/wisdom.txt` by `build.rs` (shared parser: `wisdom_parse.rs`).
// A malformed corpus is a *build* failure, so no runtime path here can see
// a bad entry.
include!(concat!(env!("OUT_DIR"), "/wisdom_gen.rs"));

/// When `wisdom-kind` is `all`, the chance (in percent) that the pick is a
/// zen entry rather than a feature tip. Dev-time tunable, not a knob.
const ZEN_PERCENT: u32 = 25;

/// How often the random post-command wisdom fires (§5, `wisdom-frequency`).
/// Stored on disk as the ordinal, always rendered as the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WisdomFrequency {
    Off = 0,
    Rare = 1,
    Normal = 2,
    Chatty = 3,
    Always = 4,
}

impl WisdomFrequency {
    const ALL: [WisdomFrequency; 5] =
        [Self::Off, Self::Rare, Self::Normal, Self::Chatty, Self::Always];

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Rare => "rare",
            Self::Normal => "normal",
            Self::Chatty => "chatty",
            Self::Always => "always",
        }
    }

    /// Parse the name form (the ordinal is a storage detail, not accepted).
    pub fn parse(s: &str) -> Res<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.name() == s)
            .ok_or_else(|| valid_values_error("wisdom-frequency", s, &Self::ALL.map(Self::name)))
    }

    pub fn from_ordinal(n: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|f| *f as u8 == n)
    }

    /// `Some(d)` means "fire with probability 1/d"; `None` means never.
    fn denominator(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Rare => Some(15),
            Self::Normal => Some(8),
            Self::Chatty => Some(4),
            Self::Always => Some(1),
        }
    }
}

/// Which entries are eligible (§5, `wisdom-kind`): `relevant` = feature
/// tips only, `all` = tips mixed with zen entries. Stored as the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WisdomKind {
    Relevant,
    All,
}

impl WisdomKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Relevant => "relevant",
            Self::All => "all",
        }
    }

    pub fn parse(s: &str) -> Res<Self> {
        [Self::Relevant, Self::All]
            .into_iter()
            .find(|k| k.name() == s)
            .ok_or_else(|| valid_values_error("wisdom-kind", s, &["relevant", "all"]))
    }
}

fn valid_values_error(knob: &str, got: &str, valid: &[&str]) -> anyhow::Error {
    anyhow::anyhow!("invalid {knob} value \"{got}\" (valid: {})", valid.join(", "))
}

/// Knob validator (§5): name → stored ordinal string.
pub fn validate_frequency(value: &str) -> Res<String> {
    Ok((WisdomFrequency::parse(value)? as u8).to_string())
}

/// Knob renderer (§5): stored ordinal string → name. Anything unparseable
/// renders as-is (no compatibility machinery — the set-path validates).
pub fn render_frequency(stored: &str) -> String {
    stored
        .parse::<u8>()
        .ok()
        .and_then(WisdomFrequency::from_ordinal)
        .map(|f| f.name().to_owned())
        .unwrap_or_else(|| stored.to_owned())
}

/// Knob validator (§5): `wisdom-kind` stores the name form directly.
pub fn validate_kind(value: &str) -> Res<String> {
    Ok(WisdomKind::parse(value)?.name().to_owned())
}

/// The effective knob values, parsed leniently: an unreadable stored value
/// falls back to the default rather than failing whatever command ran.
fn knob_settings(db: &Database) -> (WisdomFrequency, WisdomKind) {
    let frequency = db
        .knob_or_default("wisdom-frequency")
        .and_then(|v| v.parse::<u8>().ok())
        .and_then(WisdomFrequency::from_ordinal)
        .unwrap_or(WisdomFrequency::Normal);
    let kind = db
        .knob_or_default("wisdom-kind")
        .and_then(|v| WisdomKind::parse(&v).ok())
        .unwrap_or(WisdomKind::All);
    (frequency, kind)
}

/// One uniformly random eligible entry. Under `all`, a zen entry is chosen
/// `ZEN_PERCENT`% of the time (falling back across empty pools).
fn pick(kind: WisdomKind) -> Option<&'static str> {
    let pool: &[&str] = match kind {
        WisdomKind::Relevant => TIPS,
        WisdomKind::All if TIPS.is_empty() => ZENS,
        WisdomKind::All if ZENS.is_empty() => TIPS,
        WisdomKind::All => {
            if fastrand::u32(..100) < ZEN_PERCENT {
                ZENS
            } else {
                TIPS
            }
        }
    };
    (!pool.is_empty()).then(|| pool[fastrand::usize(..pool.len())])
}

/// `cactup wisdom`: print one entry, plain, to stdout.
pub fn dispatch(ctx: &Ctx) -> Res<()> {
    let (_, kind) = knob_settings(&ctx.db.read()?);
    let Some(text) = pick(kind) else {
        bail!("the wisdom corpus is empty");
    };
    println!("{text}");
    Ok(())
}

/// The random post-command hook (§16): with the knob-controlled
/// probability, print one dimmed entry to stderr after a successful
/// interactive command. Decoration must never fail the command, so every
/// problem here is a silent no-op.
pub fn maybe_print(ctx: &Ctx) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let Ok(db) = ctx.db.read() else { return };
    let (frequency, kind) = knob_settings(&db);
    let Some(denominator) = frequency.denominator() else { return };
    if fastrand::u32(..denominator) != 0 {
        return;
    }
    let Some(text) = pick(kind) else { return };
    eprintln!();
    // Dim per line so a mid-entry reset can never undim the tail.
    for line in text.lines() {
        eprintln!("{}", line.dimmed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Corpus well-formedness (counts, no tabs, line width, zen
    // attribution) is enforced by build.rs — a bad wisdom.txt cannot
    // compile, so there is no test for it here.

    #[test]
    fn parse_handles_separator_comment_and_marker_edge_cases() {
        use crate::wisdom_parse::parse_wisdom;
        let (tips, zens) = parse_wisdom("%\n# c\nA\n%\n%\n!zen\nB\n— X\n%\n\n%\n# only a comment\n%");
        assert_eq!(tips, ["A"]);
        assert_eq!(zens, ["B\n— X"]);
        let (tips, zens) = parse_wisdom("# nothing but comments\n%\n#x");
        assert!(tips.is_empty() && zens.is_empty());
    }

    #[test]
    fn frequency_names_ordinals_and_errors() {
        for f in WisdomFrequency::ALL {
            assert_eq!(WisdomFrequency::parse(f.name()).unwrap(), f);
            assert_eq!(WisdomFrequency::from_ordinal(f as u8), Some(f));
        }
        assert_eq!(WisdomFrequency::parse("chatty").unwrap(), WisdomFrequency::Chatty);
        // The ordinal is a storage detail, not an accepted input.
        assert!(WisdomFrequency::parse("2").is_err());
        let err = format!("{:#}", WisdomFrequency::parse("banana").unwrap_err());
        assert!(err.contains("off, rare, normal, chatty, always"), "{err}");
        assert_eq!(WisdomFrequency::Off.denominator(), None);
        assert_eq!(WisdomFrequency::Always.denominator(), Some(1));
    }

    #[test]
    fn knob_validate_and_render_round_trip() {
        assert_eq!(validate_frequency("normal").unwrap(), "2");
        assert_eq!(render_frequency("2"), "normal");
        assert_eq!(render_frequency("junk"), "junk");
        assert_eq!(validate_kind("relevant").unwrap(), "relevant");
        assert!(validate_kind("everything").is_err());
    }

    #[test]
    fn pick_respects_kind() {
        for _ in 0..200 {
            let tip = pick(WisdomKind::Relevant).unwrap();
            assert!(TIPS.contains(&tip));
        }
        // Under `all`, both pools are reachable.
        let mut saw_zen = false;
        let mut saw_tip = false;
        for _ in 0..1000 {
            let entry = pick(WisdomKind::All).unwrap();
            saw_zen |= ZENS.contains(&entry);
            saw_tip |= TIPS.contains(&entry);
        }
        assert!(saw_zen && saw_tip);
    }
}
