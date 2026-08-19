//! Test-suite subsystem (spec §11, D3): first-class `cactup test …` tree
//! with its own output root (test-home §11.5) and a simplified one-shot run
//! model (no restarts/recovery/chaining — §11.6). Test runs target any built
//! config (default: the active config) — there is no separate test-config
//! kind.
//!
//! A test run is named after its config (the CLI has no run-name argument —
//! §11.3): one run dir per config under `<test-home>/<config>/<config>/`,
//! and re-running allocates the next `results-%04d` set in the same dir.

pub mod manage;
pub mod run;

use crate::database::SCHEMA;
use crate::installation::{write_toml, Installation};
use crate::lock::LinkLock;
use crate::sim::restart::UniverseSpec;
use crate::walltime::Walltime;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_schema() -> u32 {
    SCHEMA
}

/// `[results]` in `test.toml` (§11.8): the parsed harness summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ResultsSummary {
    pub passed: u32,
    pub failed: u32,
    /// The results-%04d this summary belongs to.
    pub results_id: u32,
}

/// `[timestamps]` in `test.toml` (§11.8).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Timestamps {
    pub created: Option<DateTime<Utc>>,
    pub submitted: Option<DateTime<Utc>>,
    pub finished: Option<DateTime<Utc>>,
}

/// `<TestName>/.cactup/test.toml` (§11.8). The frozen `[vars]` table is the
/// same additive device as `restart.toml`'s (§9.3): it lets the compute-node
/// `test run --test-dir` rebuild its substitution context from disk (D11).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TestMeta {
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub name: String,
    pub config: String,
    pub config_id: String,
    pub build_id: String,
    pub machine: String,
    pub alias: String,
    /// `test-<name>-<machine>-<hostname>-<user>-<timestamp>-<pid>` (§11.7).
    pub test_run_id: String,
    /// `all` or the space-joined resolved selector list (§11.3).
    pub select: String,
    pub queue: String,
    pub nodes: u32,
    pub tasks: u32,
    pub tpn: u32,
    pub cpus: u32,
    /// GPUs per task (§8.5); 0 on a non-GPU run.
    #[serde(default)]
    pub gpus_per_task: u32,
    pub walltime: Walltime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<String>,
    /// `-1` = failed/unknown, or the interactive runner's pid.
    pub job_id: String,
    /// Last observed live status letter (cached; the live query rules — §8.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The frozen run universe; absent = host context (§4.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universe: Option<UniverseSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<ResultsSummary>,
    #[serde(default)]
    pub timestamps: Timestamps,
    #[serde(default)]
    pub vars: IndexMap<String, toml::Value>,
}

/// A located test run.
#[derive(Debug)]
pub struct TestRun {
    pub name: String,
    pub dir: PathBuf,
    pub meta: TestMeta,
}

impl TestRun {
    pub fn cactup_dir(&self) -> PathBuf {
        self.dir.join(".cactup")
    }

    pub fn meta_path(dir: &Path) -> PathBuf {
        dir.join(".cactup").join("test.toml")
    }

    pub fn running_lock_path(&self) -> PathBuf {
        self.cactup_dir().join("running.lock")
    }

    pub fn heartbeat_path(&self) -> PathBuf {
        self.cactup_dir().join("heartbeat")
    }

    /// Open the test run at `dir` (detection = `.cactup/test.toml` exists,
    /// mirroring D10).
    pub fn open(dir: &Path) -> Res<TestRun> {
        let path = Self::meta_path(dir);
        if !path.is_file() {
            bail!("{} is not a cactup test run ({} does not exist)", dir.display(), path.display());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let meta: TestMeta =
            toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;
        if meta.schema > SCHEMA {
            bail!(
                "{} has schema {} but this cactup understands at most schema {SCHEMA}; \
                 please upgrade cactup",
                path.display(),
                meta.schema
            );
        }
        Ok(TestRun { name: meta.name.clone(), dir: dir.to_owned(), meta })
    }

    /// Locate a test run by name through the `tests.toml` registry (§11.8).
    pub fn locate(inst: &Installation, name: &str) -> Res<TestRun> {
        let registry = inst.tests()?;
        let entry = registry.tests.get(name).ok_or_else(|| {
            anyhow!(
                "no test run named \"{name}\" in installation \"{}\" (see `cactup test list`)",
                inst.alias
            )
        })?;
        if !entry.dir.is_dir() {
            bail!(
                "test run \"{name}\" is registered at {} but that directory is missing;\n\
                 run `cactup test delete {name}` to drop the stale registry entry",
                entry.dir.display()
            );
        }
        TestRun::open(&entry.dir)
    }

    /// Per-test-run mutual exclusion (results-set allocation, metadata RMW).
    pub fn lock(&self) -> Res<LinkLock> {
        LinkLock::acquire(&self.cactup_dir().join("test.lock"))
    }

    pub fn store_meta(&self) -> Res<()> {
        write_toml(&Self::meta_path(&self.dir), &self.meta)
    }

    /// `log.txt` in the same `[LOG:…]` format as a simulation's (§11.5, §12).
    pub fn log(&self, command: &str, message: &str) {
        let ts = Utc::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("[LOG:{ts}] {command}::{message}\n");
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("log.txt"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }
}

/// `test-<name>-<machine>-<hostname>-<user>-<timestamp>-<pid>` (§11.7 — the
/// §9.1 simulation-id format with the `test-` prefix).
pub fn test_run_id(name: &str, machine: &str, hostname: &str) -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    let ts = Utc::now().format("%Y.%m.%d-%H.%M.%S");
    format!("test-{name}-{machine}-{hostname}-{user}-{ts}-{}", std::process::id())
}

/// `results-%04d` (§11.5) — the numbered, newest-active result sets, reusing
/// the §9.2 active-symlink mechanics against the `results-` prefix.
pub fn results_name(id: u32) -> String {
    format!("results-{id:04}")
}

pub fn results_dir(run_dir: &Path, id: u32) -> PathBuf {
    run_dir.join(results_name(id))
}

fn parse_results_name(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("results-")?;
    if digits.len() < 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

pub fn list_results_ids(run_dir: &Path) -> Res<Vec<u32>> {
    let mut ids = Vec::new();
    let entries = match fs::read_dir(run_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
        other => other.with_context(|| format!("Failed to read {}", run_dir.display()))?,
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = parse_results_name(name) && entry.file_type()?.is_dir() {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

pub fn next_results_id(run_dir: &Path) -> Res<u32> {
    Ok(match list_results_ids(run_dir)?.last() {
        Some(&max) => max + 1,
        None => 0,
    })
}

/// Scan for `results-(\d+)-active`: zero ⇒ none, more than one ⇒ fatal (§9.2
/// mechanics).
pub fn active_results_id(run_dir: &Path) -> Res<Option<u32>> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(run_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        other => other.with_context(|| format!("Failed to read {}", run_dir.display()))?,
    };
    for entry in entries {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(base) = name.strip_suffix("-active")
            && let Some(id) = parse_results_name(base)
        {
            found.push(id);
        }
    }
    match found.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        several => bail!(
            "test run at {} has more than one active result set ({:?}) — repair by hand",
            run_dir.display(),
            several
        ),
    }
}

/// Deactivate whatever result set is active, then activate `id` (a RELATIVE
/// symlink). Unlike a restart chain there is no cross-process handoff — the
/// caller holds the per-run lock.
pub fn activate_results(run_dir: &Path, id: u32) -> Res<()> {
    if let Some(existing) = active_results_id(run_dir)? {
        if existing == id {
            return Ok(());
        }
        fs::remove_file(run_dir.join(format!("{}-active", results_name(existing))))?;
    }
    let link = run_dir.join(format!("{}-active", results_name(id)));
    std::os::unix::fs::symlink(results_name(id), &link)
        .with_context(|| format!("Failed to create active symlink {}", link.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_naming_and_activation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert_eq!(results_name(3), "results-0003");
        assert_eq!(next_results_id(dir).unwrap(), 0);

        fs::create_dir(results_dir(dir, 0)).unwrap();
        fs::create_dir(results_dir(dir, 1)).unwrap();
        assert_eq!(list_results_ids(dir).unwrap(), vec![0, 1]);
        assert_eq!(next_results_id(dir).unwrap(), 2);

        assert_eq!(active_results_id(dir).unwrap(), None);
        activate_results(dir, 0).unwrap();
        assert_eq!(active_results_id(dir).unwrap(), Some(0));
        // Re-activation moves the link (newest-active — §11.5).
        activate_results(dir, 1).unwrap();
        assert_eq!(active_results_id(dir).unwrap(), Some(1));
        let target = fs::read_link(dir.join("results-0001-active")).unwrap();
        assert_eq!(target, PathBuf::from("results-0001"), "relative link");
        // Idempotent.
        activate_results(dir, 1).unwrap();
        assert_eq!(active_results_id(dir).unwrap(), Some(1));
    }

    #[test]
    fn test_run_id_format() {
        let id = test_run_id("et-tests", "mel5", "mel5.host");
        assert!(id.starts_with("test-et-tests-mel5-mel5.host-"), "{id}");
    }
}
