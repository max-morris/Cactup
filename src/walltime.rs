//! Canonical walltime handling (spec §8.5): grammar `(DD-)?HH:MM:SS`, stored
//! internally as a total number of seconds. All comparisons (queue ceilings)
//! and the chaining division operate on this seconds value.

// Consumed by the Phase-2/3 streams (SCHED, SIM); some accessors unused until then.

use crate::Res;
use anyhow::bail;

/// A walltime, internally a total number of seconds (§8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Walltime(pub u64);

/// Deserialize from the canonical string form, so `max-walltime` keys in
/// `meta.toml` (§4.2) parse straight to seconds with a grammar error naming
/// the bad value.
impl<'de> serde::Deserialize<'de> for Walltime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Walltime::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Serialize as the canonical string form (round-trips with Deserialize), so
/// `restart.toml` (§9.3) stores human-readable walltimes.
impl serde::Serialize for Walltime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.canonical())
    }
}

fn parse_field(s: &str, what: &str, original: &str) -> Res<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid walltime '{original}': {what} is not a number (expected (DD-)?HH:MM:SS)");
    }
    Ok(s.parse::<u64>()?)
}

impl Walltime {
    /// Parse the grammar `(DD-)?HH:MM:SS`, allowing leading (more-significant)
    /// fields to be elided when they would be zero: `SS`, `MM:SS`, and
    /// `HH:MM:SS` are all accepted, so `10:00` is 10 minutes and `90` is 90
    /// seconds. A `DD-` day prefix still requires the full `HH:MM:SS`. The
    /// most-significant present field may overflow its usual bound (`72:00:00`
    /// is 72 hours, `90:00` is 90 minutes); trailing fields are always `< 60`,
    /// and `HH` must be `< 24` when a `DD-` prefix is present.
    pub fn parse(s: &str) -> Res<Walltime> {
        let trimmed = s.trim();
        let (days, rest, has_day_prefix) = match trimmed.split_once('-') {
            Some((d, rest)) => (parse_field(d, "the DD day prefix", s)?, rest, true),
            None => (0, trimmed, false),
        };

        let fields: Vec<&str> = rest.split(':').collect();
        // Elided leading fields default to zero; the trailing field is always SS.
        let (hh, mm, ss) = match fields.as_slice() {
            [hh, mm, ss] => (
                parse_field(hh, "the HH field", s)?,
                parse_field(mm, "the MM field", s)?,
                parse_field(ss, "the SS field", s)?,
            ),
            [mm, ss] if !has_day_prefix => {
                (0, parse_field(mm, "the MM field", s)?, parse_field(ss, "the SS field", s)?)
            }
            [ss] if !has_day_prefix => (0, 0, parse_field(ss, "the SS field", s)?),
            _ => bail!("invalid walltime '{s}': expected (DD-)?HH:MM:SS"),
        };

        if has_day_prefix && hh > 23 {
            bail!("invalid walltime '{s}': HH must be < 24 when a DD- day prefix is present");
        }
        // Trailing fields are bounded; the most-significant present field may
        // overflow (it absorbs the elided components' magnitude).
        let ss_bounded = fields.len() >= 2;
        let mm_bounded = fields.len() >= 3;
        if (mm_bounded && mm > 59) || (ss_bounded && ss > 59) {
            bail!("invalid walltime '{s}': trailing MM/SS fields must be < 60");
        }

        Ok(Walltime(days * 86400 + hh * 3600 + mm * 60 + ss))
    }

    // Pinned foundation API; callers in this crate construct `Walltime(secs)`
    // directly (the tuple field is crate-visible).
    #[allow(dead_code)]
    pub fn from_seconds(secs: u64) -> Walltime {
        Walltime(secs)
    }

    pub fn total_seconds(&self) -> u64 {
        self.0
    }

    /// Total whole minutes (truncating) — the `@WALLTIME_MINUTES@` value.
    pub fn total_minutes(&self) -> u64 {
        self.0 / 60
    }

    /// Total whole hours (truncating) — the `@WALLTIME_HOURS@` value.
    pub fn total_hours(&self) -> u64 {
        self.0 / 3600
    }

    /// Canonical string form: `DD-HH:MM:SS` when ≥ 1 day, else `HH:MM:SS`
    /// (a zero `DD-` prefix is elided, §8.5).
    pub fn canonical(&self) -> String {
        let d = self.0 / 86400;
        let h = (self.0 % 86400) / 3600;
        let m = (self.0 % 3600) / 60;
        let s = self.0 % 60;
        if d > 0 {
            format!("{d:02}-{h:02}:{m:02}:{s:02}")
        } else {
            format!("{h:02}:{m:02}:{s:02}")
        }
    }

    /// The `@WALLTIME_HH@` component: TOTAL hours (days folded in), so a
    /// template writing `@WALLTIME_HH@:@WALLTIME_MM@:@WALLTIME_SS@` (the
    /// PBS-style reconstruction) always names the full wall, even when the
    /// canonical form would use a `DD-` prefix.
    pub fn component_hours(&self) -> u64 {
        self.0 / 3600
    }
    /// The `@WALLTIME_MM@` component (minutes within the hour).
    pub fn component_minutes(&self) -> u64 {
        (self.0 % 3600) / 60
    }
    /// The `@WALLTIME_SS@` component (seconds within the minute).
    pub fn component_seconds(&self) -> u64 {
        self.0 % 60
    }
}

impl std::fmt::Display for Walltime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.canonical())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_day_forms() {
        assert_eq!(Walltime::parse("1:00:00").unwrap().0, 3600);
        assert_eq!(Walltime::parse("72:00:00").unwrap().0, 72 * 3600);
        assert_eq!(Walltime::parse("00-1:00:00").unwrap().0, 3600);
        assert_eq!(Walltime::parse("3-00:30:15").unwrap().0, 3 * 86400 + 30 * 60 + 15);
        assert_eq!(Walltime::parse("365-00:00:00").unwrap().0, 365 * 86400);
    }

    #[test]
    fn parses_elided_leading_fields() {
        // MM:SS and SS forms, with the most-significant field free to overflow.
        assert_eq!(Walltime::parse("10:00").unwrap().0, 600);
        assert_eq!(Walltime::parse("90").unwrap().0, 90);
        assert_eq!(Walltime::parse("00:30").unwrap().0, 30);
        assert_eq!(Walltime::parse("90:00").unwrap().0, 90 * 60);
        assert_eq!(Walltime::parse("100").unwrap().0, 100);
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["", ":00", "1:2:3:4", "aa:00:00", "1:60:00", "1:00:60", "10:60", "1-25:00:00", "-1:00:00", "1-", "3-30:00", "3-90"] {
            assert!(Walltime::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn canonical_forms() {
        assert_eq!(Walltime(3600).canonical(), "01:00:00");
        assert_eq!(Walltime(72 * 3600).canonical(), "03-00:00:00");
        assert_eq!(Walltime(90).canonical(), "00:01:30");
    }

    #[test]
    fn components_fold_days_into_hours() {
        let w = Walltime::parse("3-01:02:03").unwrap();
        assert_eq!(w.component_hours(), 73);
        assert_eq!(w.component_minutes(), 2);
        assert_eq!(w.component_seconds(), 3);
        assert_eq!(w.total_minutes(), 73 * 60 + 2);
        assert_eq!(w.total_hours(), 73);
    }
}
