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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Was the preprocessed text the same after the compile as before it?
    /// Only then would the object have been stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable: Option<bool>,
    /// How much preprocessed text the key covers, and how big the object is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_bytes: Option<u64>,
    /// Wall-clock milliseconds: computing the key, the compile itself, and
    /// checking the text again afterward.
    pub key_ms: u64,
    pub compile_ms: u64,
    pub recheck_ms: u64,
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

/// Every event in the log at `path`. A line that does not parse (a write
/// cut short by a full disk) is skipped: the rest is still worth reading.
pub fn read(path: &Path) -> Res<Vec<Event>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect())
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
            stable: None,
            text_bytes: None,
            object_bytes: Some(1024),
            key_ms: 1,
            compile_ms: 250,
            recheck_ms: 0,
        };
        plain.append(&log);
        plain.append(&log);
        std::fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"{\"compiler\":\"cut sho").unwrap();
        assert_eq!(read(&log).unwrap(), [plain.clone(), plain]);
        assert!(read(&tmp.path().join("missing")).is_err());
    }
}
