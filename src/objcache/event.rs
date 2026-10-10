//! The log of a build's wrapped compiles: `<attempt>/cc/events.jsonl`, one
//! JSON object per line, written by the wrapper and read by `cactup cache
//! report`.

use super::key::Parts;
use crate::Res;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// One wrapped compile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The compiler as the recipe named it.
    pub compiler: String,
    /// The object, relative to the configuration's `build` directory
    /// (`Boundary/ScalarBoundary.c.o`): the name one compile has in every
    /// build of every configuration. `None` if the command line has no
    /// `-o`, or one that leads elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// The key this compile would be cached under, and what it is made of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Parts>,
    /// Why there is no key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_cached: Option<String>,
    /// Is the key free of where the installation is and what the
    /// configuration is called (`key::PathMap`)? Only such a key can be
    /// shared with another installation or configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relocatable: Option<bool>,
    /// Were the preprocessed text and the files behind it the same after
    /// the compile as before it? Only then would the object have been
    /// stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable: Option<bool>,
    /// How much preprocessed text the key covers, how many files it was
    /// made from, and how big the object is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_bytes: Option<u64>,
    /// Wall-clock milliseconds: computing the key, the compile itself, and
    /// checking afterward (with the lookups made for it right before the
    /// compile).
    pub key_ms: u64,
    pub compile_ms: u64,
    pub recheck_ms: u64,
    /// How the check after a Fortran compile was made (§18.10): by this
    /// many lookups of a path, before and after the compile, or by running
    /// the dependency run again, for the reason given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookups: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_by_compiler: Option<String>,
    /// In a serving build (§18.8): found in the store (served, or in audit
    /// mode checked), or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    /// What the store said besides "no entry": why an entry was not used,
    /// or why the object was not published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// Did this compile add an entry to the store?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<bool>,
    /// In audit mode, what checking a hit found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<Audit>,
    /// Milliseconds restoring from the store, and publishing to it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub serve_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub publish_ms: u64,
}

/// Whether the store had the compile's object (§18.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Hit,
    Miss,
}

/// What audit mode found when it compiled a hit anyway (§18.8). The log
/// spells these in kebab case, and the build script counts them by that
/// spelling (`Staged::build_step`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Audit {
    /// The compile made the stored object, and the dependency file the hit
    /// would have written.
    Same,
    /// The compile made another object, twice over: the entry was wrong.
    WrongHit,
    /// The object was right, the dependency file a hit writes was not.
    WrongDependencyFile,
    /// The compile made another object, and another the second time.
    NotDeterministic,
    /// The compile failed, where the entry says it succeeded.
    CompileFailed,
    /// The objects differed, and the second compile, which would have told
    /// a wrong hit from a compiler that is not deterministic, did not end
    /// well (killed, crashed, failed): nothing can be said.
    SecondCompileFailed,
    /// The files the compile read changed while it ran: nothing can be
    /// said about the hit.
    InputsChanged,
}

impl Audit {
    pub fn name(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::WrongHit => "wrong hit",
            Self::WrongDependencyFile => "wrong dependency file",
            Self::NotDeterministic => "not deterministic",
            Self::CompileFailed => "the compile failed",
            Self::SecondCompileFailed => "the second compile failed",
            Self::InputsChanged => "its inputs changed meanwhile",
        }
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Event {
    /// Best-effort: the log is for measurement, and a compile that ran is
    /// never failed over it. One `write` of one whole line in append mode,
    /// so the parallel compiles of a build do not interleave their lines.
    pub fn append(&self, path: &Path) {
        let Ok(mut line) = serde_json::to_string(self) else { return };
        line.push('\n');
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// Every event in the log at `path`, and the number of lines that are not
/// one: a write cut short by a full disk, or a log some other cactup wrote
/// (the format is not kept between versions). The rest is still worth
/// reading, as long as the reader is told.
pub fn read(path: &Path) -> Res<(Vec<Event>, usize)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let lines = text.lines().count();
    let events: Vec<Event> = text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    let unreadable = lines - events.len();
    Ok((events, unreadable))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("events.jsonl");
        let plain = Event {
            compiler: "gfortran".into(),
            unit: Some("T/a.F90.o".into()),
            exit: Some(0),
            signal: None,
            key: None,
            parts: None,
            not_cached: Some("Fortran is not cached yet".into()),
            relocatable: None,
            stable: None,
            text_bytes: None,
            files: None,
            object_bytes: Some(1024),
            key_ms: 1,
            compile_ms: 250,
            recheck_ms: 0,
            ..Default::default()
        };
        plain.append(&log);
        plain.append(&log);
        std::fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"{\"compiler\":\"cut sho").unwrap();
        assert_eq!(read(&log).unwrap(), (vec![plain.clone(), plain], 1));
        assert!(read(&tmp.path().join("missing")).is_err());
    }
}
