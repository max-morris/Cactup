//! Self-update settings: the `autoupdate`, `update-url` and `mdb-url` knobs
//! (§5) and their defaults. These are maintenance knobs — they steer the
//! binary itself, never a job — so they are kept out of the knob snapshot
//! frozen into restart/build/test metadata.

use crate::database::Database;
use crate::Res;
use anyhow::bail;

/// Where distribution builds look for new releases (`latest.json` and the
/// binaries) and where the documentation lives: the `update-url` default.
pub const DEFAULT_UPDATE_URL: &str = "https://max-morris.github.io/Cactup";

/// The git repository whose `mdb` branch carries the published machine
/// database: the `mdb-url` default.
pub const DEFAULT_MDB_URL: &str = "https://github.com/max-morris/Cactup.git";

/// What a distribution build does when a newer release exists (knob
/// `autoupdate`): install it and carry on in it, only say so, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoUpdate {
    Auto,
    Notify,
    Off,
}

impl AutoUpdate {
    const ALL: [Self; 3] = [Self::Auto, Self::Notify, Self::Off];

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Notify => "notify",
            Self::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Res<Self> {
        match Self::ALL.into_iter().find(|mode| mode.name() == s) {
            Some(mode) => Ok(mode),
            None => bail!("invalid autoupdate value \"{s}\" (valid: auto, notify, off)"),
        }
    }
}

/// Knob validator (§5): `autoupdate` stores the name form.
pub fn validate_autoupdate(value: &str) -> Res<String> {
    Ok(AutoUpdate::parse(value.trim())?.name().to_owned())
}

/// Knob validator (§5): `update-url` must be an http(s) URL; stored without
/// a trailing `/`, since paths are appended to it.
pub fn validate_update_url(value: &str) -> Res<String> {
    let url = value.trim().trim_end_matches('/');
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
    match rest {
        Some(host) if !host.is_empty() && !url.contains(char::is_whitespace) => Ok(url.to_owned()),
        _ => bail!("invalid update-url \"{value}\": expected an http:// or https:// URL"),
    }
}

/// Knob validator (§5): `mdb-url` is anything git can fetch from (an
/// https URL, an ssh remote, a local path), so it is only required to be
/// non-empty.
pub fn validate_mdb_url(value: &str) -> Res<String> {
    let url = value.trim();
    if url.is_empty() {
        bail!("mdb-url cannot be empty (`cactup knob delete mdb-url` restores the default)");
    }
    Ok(url.to_owned())
}

/// The effective `autoupdate` setting, read leniently: a stored value that
/// no longer parses means the default, `auto`, rather than failing whatever
/// command is running.
#[cfg_attr(not(test), allow(dead_code))] // read by the self-update check
pub fn autoupdate(db: &Database) -> AutoUpdate {
    db.knob_or_default("autoupdate")
        .and_then(|v| AutoUpdate::parse(&v).ok())
        .unwrap_or(AutoUpdate::Auto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoupdate_values() {
        assert_eq!(validate_autoupdate("notify").unwrap(), "notify");
        assert_eq!(validate_autoupdate(" off ").unwrap(), "off");
        for mode in AutoUpdate::ALL {
            assert_eq!(AutoUpdate::parse(mode.name()).unwrap(), mode);
        }
        let err = validate_autoupdate("sometimes").unwrap_err().to_string();
        assert!(err.contains("auto, notify, off"), "{err}");
    }

    #[test]
    fn update_url_values() {
        assert_eq!(
            validate_update_url("https://example.org/cactup/").unwrap(),
            "https://example.org/cactup"
        );
        assert_eq!(validate_update_url("http://127.0.0.1:8080//").unwrap(), "http://127.0.0.1:8080");
        for bad in ["", "example.org", "ftp://example.org", "https://", "https://a b"] {
            assert!(validate_update_url(bad).is_err(), "{bad:?} accepted");
        }
        assert_eq!(validate_update_url(DEFAULT_UPDATE_URL).unwrap(), DEFAULT_UPDATE_URL);
    }

    #[test]
    fn mdb_url_values() {
        assert_eq!(validate_mdb_url(" /srv/git/cactup.git ").unwrap(), "/srv/git/cactup.git");
        assert_eq!(
            validate_mdb_url("git@github.com:max-morris/Cactup.git").unwrap(),
            "git@github.com:max-morris/Cactup.git"
        );
        assert!(validate_mdb_url("  ").is_err());
    }

    #[test]
    fn defaults_and_lenient_read() {
        let mut db = Database::new();
        assert_eq!(autoupdate(&db), AutoUpdate::Auto);
        assert_eq!(db.knob_or_default("update-url").as_deref(), Some(DEFAULT_UPDATE_URL));
        assert_eq!(db.knob_or_default("mdb-url").as_deref(), Some(DEFAULT_MDB_URL));

        db.set_knob("autoupdate", "notify".to_owned());
        assert_eq!(autoupdate(&db), AutoUpdate::Notify);
        // A value that no longer parses falls back to the default.
        db.set_knob("autoupdate", "garbage".to_owned());
        assert_eq!(autoupdate(&db), AutoUpdate::Auto);
    }

    #[test]
    fn maintenance_knobs_stay_out_of_the_snapshot() {
        let mut db = Database::new();
        db.set_knob("autoupdate", "off".to_owned());
        db.set_knob("update-url", "https://example.org".to_owned());
        db.set_knob("allocation", "hpc_xxx".to_owned());
        let snapshot = db.knob_snapshot();
        for knob in ["autoupdate", "update-url", "mdb-url"] {
            assert!(crate::database::knob_spec(knob).is_some(), "{knob} is a standard knob");
            assert!(!snapshot.contains_key(knob), "{knob} leaked into the snapshot");
        }
        assert_eq!(snapshot.get("allocation").map(String::as_str), Some("hpc_xxx"));
    }
}
