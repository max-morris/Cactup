//! Restarts: the numbered `output-%04d` dirs, the active symlink (§9.2), the
//! per-restart `.cactup/restart.toml` metadata (§9.3), and the stale-restart
//! reaper (§8.3).

use crate::database::SCHEMA;
use crate::installation::write_toml;
use crate::lock::{LinkLock, HEARTBEAT_STALE_SECS};
use crate::mdb::Universe;
use crate::scheduler::{JobStatus, Scheduler};
use crate::sim::Simulation;
use crate::template::{VarSet, VarValue};
use crate::walltime::Walltime;
use crate::Res;
use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use colored::Colorize;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_schema() -> u32 {
    SCHEMA
}

/// Job id recorded when submission failed / produced no id (§8.3).
pub const NO_JOB_ID: &str = "-1";

/// The resolved run universe frozen into `restart.toml` (§4.8/§9.3): the name
/// plus the *raw* wrapper spec, so the compute node re-expands it against the
/// frozen `[vars]` without re-reading the MDB.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct UniverseSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapper_argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapper: Option<String>,
}

impl UniverseSpec {
    pub fn from_universe(name: &str, u: &Universe) -> UniverseSpec {
        UniverseSpec {
            name: name.to_owned(),
            wrapper_argv: u.wrapper_argv.clone(),
            wrapper: u.wrapper.clone(),
        }
    }

    /// Env keys are deliberately absent (env is baked into the generated
    /// scripts — §6.1); an identity spec (both wrappers None, §4.8) rebuilds
    /// an identity universe.
    pub fn to_universe(&self) -> Universe {
        Universe {
            wrapper_argv: self.wrapper_argv.clone(),
            wrapper: self.wrapper.clone(),
            environment: Default::default(),
        }
    }
}

/// `output-%04d/.cactup/restart.toml` (§9.3): submit/run keys plus the frozen
/// §6.3 variable set (additive — lets the compute-node path rebuild its whole
/// substitution context from disk, per D11).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RestartMeta {
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub created: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<DateTime<Utc>>,
    pub nodes: u32,
    pub tasks: u32,
    pub tpn: u32,
    pub cpus: u32,
    pub queue: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<String>,
    /// The hard scheduler wall for THIS job (§8.8).
    pub walltime: Walltime,
    /// The chosen checkpoint buffer (§8.8).
    pub checkpt_buffer: Walltime,
    /// `-1` = failed/unknown (§8.3).
    pub job_id: String,
    /// The job id this restart's job depends on (pre-submitted chain, §8.3.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chained_job_id: Option<String>,
    /// Last observed status letter (display cache only; the live query is
    /// the source of truth — §8.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Set by run completion / `stop` / `clean` (§8.6).
    #[serde(default)]
    pub terminated: bool,
    /// The frozen run universe; absent = host context (§4.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universe: Option<UniverseSpec>,
    /// The frozen §6.3 variable set (run-phase `ENV_SETUP`).
    #[serde(default)]
    pub vars: IndexMap<String, toml::Value>,
}

/// One restart on disk.
#[derive(Debug)]
pub struct Restart {
    pub id: u32,
    pub dir: PathBuf,
    pub meta: RestartMeta,
}

impl Restart {
    pub fn load(sim_dir: &Path, id: u32) -> Res<Restart> {
        let dir = restart_dir(sim_dir, id);
        let path = dir.join(".cactup").join("restart.toml");
        if !path.is_file() {
            bail!("restart {} has no metadata at {}", dir_name(id), path.display());
        }
        // RestartMeta has no Default; read + schema-guard manually.
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let meta: RestartMeta =
            toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;
        if meta.schema > SCHEMA {
            bail!(
                "{} has schema {} but this cactup understands at most schema {SCHEMA}; \
                 please upgrade cactup",
                path.display(),
                meta.schema
            );
        }
        Ok(Restart { id, dir, meta })
    }

    pub fn store(&self) -> Res<()> {
        write_toml(&self.dir.join(".cactup").join("restart.toml"), &self.meta)
    }

    pub fn cactup_dir(&self) -> PathBuf {
        self.dir.join(".cactup")
    }

    pub fn running_lock_path(&self) -> PathBuf {
        self.cactup_dir().join("running.lock")
    }

    pub fn heartbeat_path(&self) -> PathBuf {
        self.cactup_dir().join("heartbeat")
    }

    /// Touch the heartbeat file's mtime (§9.3); called every HEARTBEAT_SECS
    /// by a live run. Best-effort. A plain write stamps the mtime with the
    /// *fileserver's* clock — the same domain `age_secs` measures against —
    /// where set_modified(now) would inject this node's local clock.
    pub fn touch_heartbeat(&self) {
        let _ = fs::write(self.heartbeat_path(), b"");
    }
}

/// Freeze a VarSet into the TOML `[vars]` table (types preserved).
pub fn freeze_vars(vars: &VarSet) -> IndexMap<String, toml::Value> {
    vars.iter()
        .map(|(name, value)| {
            let v = match value {
                VarValue::Str(s) => toml::Value::String(s.clone()),
                VarValue::Int(i) => toml::Value::Integer(*i),
                VarValue::Bool(b) => toml::Value::Boolean(*b),
            };
            (name.to_owned(), v)
        })
        .collect()
}

/// Rebuild the VarSet from a frozen `[vars]` table. Non-scalar values (which
/// cactup never writes) are an error naming the key.
pub fn thaw_vars(frozen: &IndexMap<String, toml::Value>) -> Res<VarSet> {
    let mut vars = VarSet::new();
    for (name, value) in frozen {
        match value {
            toml::Value::String(s) => vars.set(name, s.as_str()),
            toml::Value::Integer(i) => vars.set(name, *i),
            toml::Value::Boolean(b) => vars.set(name, *b),
            other => bail!("restart.toml var {name} has unsupported type {}", other.type_str()),
        }
    }
    Ok(vars)
}

/// `output-%04d` (§9.1).
pub fn dir_name(id: u32) -> String {
    format!("output-{id:04}")
}

pub fn restart_dir(sim_dir: &Path, id: u32) -> PathBuf {
    sim_dir.join(dir_name(id))
}

/// Parse an `output-NNNN` directory name (the preserved discovery rule §9.1).
fn parse_dir_name(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("output-")?;
    if digits.len() < 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The result of one directory scan of a simulation: which restarts exist and
/// which carry the `-active` marker (§9.2).
pub struct Scan {
    /// Restart ids present as `output-%04d` directories, ascending.
    pub ids: Vec<u32>,
    /// Ids carrying an `output-%04d-active` marker; more than one is a fault
    /// surfaced by [`Scan::active_id`].
    actives: Vec<u32>,
}

impl Scan {
    /// The active restart id, or the §9.2 hand-repair error when the
    /// simulation carries more than one active marker.
    pub fn active_id(&self, sim_dir: &Path) -> Res<Option<u32>> {
        match self.actives.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
            several => bail!(
                "simulation at {} has more than one active restart ({}) — this must be repaired by hand",
                sim_dir.display(),
                several.iter().map(|id| dir_name(*id)).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// The newest restart id, if any.
    pub fn latest(&self) -> Option<u32> {
        self.ids.last().copied()
    }
}

/// Scan `sim_dir` once for both the restart ids and the active marker —
/// `list_ids` + `active_id` in a single `read_dir`, which matters on a
/// networked filesystem when a command walks every simulation.
pub fn scan(sim_dir: &Path) -> Res<Scan> {
    let mut ids = Vec::new();
    let mut actives = Vec::new();
    for entry in fs::read_dir(sim_dir)
        .with_context(|| format!("Failed to read simulation directory {}", sim_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = parse_dir_name(name) {
            if entry.file_type()?.is_dir() {
                ids.push(id);
            }
        } else if let Some(base) = name.strip_suffix("-active") {
            if let Some(id) = parse_dir_name(base) {
                actives.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(Scan { ids, actives })
}

/// All restart ids, sorted ascending.
pub fn list_ids(sim_dir: &Path) -> Res<Vec<u32>> {
    Ok(scan(sim_dir)?.ids)
}

/// The next restart id; ids `0..9999`, `>9999 ⇒` the preserved error (§9.1).
pub fn next_id(sim_dir: &Path) -> Res<u32> {
    let next = match list_ids(sim_dir)?.last() {
        Some(&max) => max + 1,
        None => 0,
    };
    if next > 9999 {
        bail!("maximum number of restarts reached");
    }
    Ok(next)
}

/// Read the active restart via the symlink scan (§9.2): zero ⇒ none, more
/// than one ⇒ fatal. The symlink — never a stored field — is the truth.
pub fn active_id(sim_dir: &Path) -> Res<Option<u32>> {
    scan(sim_dir)?.active_id(sim_dir)
}

fn active_link(sim_dir: &Path, id: u32) -> PathBuf {
    sim_dir.join(format!("{}-active", dir_name(id)))
}

/// `makeActive()` (§9.2): create the RELATIVE `output-NNNN-active` symlink;
/// refuses if any active symlink already exists.
pub fn make_active(sim_dir: &Path, id: u32) -> Res<()> {
    if let Some(existing) = active_id(sim_dir)? {
        bail!(
            "cannot activate {}: {} is already active",
            dir_name(id),
            dir_name(existing)
        );
    }
    let link = active_link(sim_dir, id);
    std::os::unix::fs::symlink(dir_name(id), &link)
        .with_context(|| format!("Failed to create active symlink {}", link.display()))
}

/// Remove the active symlink, whichever restart it names (`clean`/`finish`).
pub fn deactivate(sim_dir: &Path) -> Res<Option<u32>> {
    let Some(id) = active_id(sim_dir)? else { return Ok(None) };
    let link = active_link(sim_dir, id);
    fs::remove_file(&link)
        .with_context(|| format!("Failed to remove active symlink {}", link.display()))?;
    Ok(Some(id))
}

/// The §8.3.2 chain handoff, called on the compute node under the per-sim
/// lock: unlink the predecessor's `-active`, then symlink-to-temp + rename
/// onto ours. The only externally observable transient is a zero-active
/// window (benign); never a two-active window.
pub fn handoff_active(sim_dir: &Path, id: u32) -> Res<()> {
    if let Some(existing) = active_id(sim_dir)? {
        if existing == id {
            return Ok(()); // already ours (e.g. first-of-chain, activated at submit)
        }
        let link = active_link(sim_dir, existing);
        fs::remove_file(&link)
            .with_context(|| format!("Failed to unlink predecessor {}", link.display()))?;
    }
    let temp = sim_dir.join(format!(
        ".cactup-active.{}.{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::os::unix::fs::symlink(dir_name(id), &temp)
        .with_context(|| format!("Failed to create temp symlink {}", temp.display()))?;
    fs::rename(&temp, active_link(sim_dir, id))
        .with_context(|| format!("Failed to activate {}", dir_name(id)))
}

/// The Cactus working dir of a restart: `<restart>/<parfile-stem>/` (§9.1).
pub fn workdir(sim: &Simulation, id: u32) -> PathBuf {
    restart_dir(&sim.dir, id).join(sim.parfile_stem())
}

// Checkpoint recovery is deliberately absent (§8.8): the parfile points Cactus
// at a recovery dir — typically one shared across restarts, outside every
// output-%04d — and Cactus loads the newest checkpoint there itself. cactup
// neither selects nor moves checkpoints, so there is no scan, no recovery
// source, and no PrepareCheckpointing port here.

/// Reaper throttles (§9.3): simfactory's "skip sims < 60 s old, re-clean
/// every 30 s", kept since the reaper runs on every submit.
const REAP_MIN_SIM_AGE_SECS: u64 = 60;
const REAP_RECLEAN_SECS: u64 = 30;

/// Age of `path`'s mtime, measured against the *fileserver's* clock: "now"
/// is minted as the mtime of a fresh temp file next to `path` — the same
/// trick lock.rs uses — because the node running the reaper and the NFS
/// server that stamped the file can disagree by more than the staleness
/// thresholds. A future mtime (writer's clock ahead) reads as age 0.
fn age_secs(path: &Path) -> Option<u64> {
    let mtime = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let temp = tempfile::Builder::new().prefix(".cactup-age.").tempfile_in(path.parent()?).ok()?;
    let fs_now = temp.as_file().metadata().and_then(|m| m.modified()).ok()?;
    Some(fs_now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0))
}

/// The §8.3 stale-active-restart reaper (port of simfactory `initRestart`).
/// Reaps — auto-`clean`s — the active restart only when the liveness protocol
/// says its run is truly dead: job status `U` AND `running.lock` unheld AND
/// heartbeat stale. A restart that finished cleanly (`terminated` recorded by
/// its run) skips the heartbeat wait — the marker is authoritative.
/// Returns true when a restart was reaped.
pub fn reap_stale(sim: &Simulation, sched: &Scheduler) -> Res<bool> {
    let Some(active) = active_id(&sim.dir)? else { return Ok(false) };

    // Throttles.
    let meta_path = sim.cactup_dir().join("simulation.toml");
    if age_secs(&meta_path).map(|a| a < REAP_MIN_SIM_AGE_SECS).unwrap_or(false) {
        return Ok(false);
    }
    let marker = sim.cactup_dir().join("last-cleaned");
    if age_secs(&marker).map(|a| a < REAP_RECLEAN_SECS).unwrap_or(false) {
        return Ok(false);
    }

    let restart = Restart::load(&sim.dir, active)?;

    // (a) live job status must be U (gone from the queue).
    let status = if restart.meta.job_id == NO_JOB_ID {
        JobStatus::Unknown
    } else {
        sched.get_status(&restart.meta.job_id)?
    };
    if !matches!(status, JobStatus::Unknown) {
        return Ok(false); // still in the queue → chaining, never reaping
    }
    // (b) the per-restart liveness marker must be unheld.
    if LinkLock::is_held_live(&restart.running_lock_path())? {
        return Ok(false);
    }
    // (c) heartbeat stale — unless the run itself recorded clean termination.
    if !restart.meta.terminated {
        if let Some(age) = age_secs(&restart.heartbeat_path()) {
            if age < HEARTBEAT_STALE_SECS {
                return Ok(false); // possibly a scheduler hiccup (transient U)
            }
        }
        // No heartbeat at all = the run never started = dead.
    }

    eprintln!(
        "{} reaping stale active restart {} (job {} gone)",
        "note:".yellow(),
        dir_name(active),
        restart.meta.job_id
    );
    crate::sim::manage::clean_active(sim)?;
    let _ = fs::write(&marker, b"");
    sim.log("reap", &format!("reaped stale active restart {}", dir_name(active)));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_names_and_discovery() {
        assert_eq!(dir_name(0), "output-0000");
        assert_eq!(dir_name(123), "output-0123");
        assert_eq!(parse_dir_name("output-0007"), Some(7));
        assert_eq!(parse_dir_name("output-12345"), Some(12345));
        assert_eq!(parse_dir_name("output-12"), None);
        assert_eq!(parse_dir_name("output-00x7"), None);
        assert_eq!(parse_dir_name("putput-0007"), None);
    }

    #[test]
    fn id_allocation_and_active_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert_eq!(next_id(dir).unwrap(), 0);
        fs::create_dir(restart_dir(dir, 0)).unwrap();
        fs::create_dir(restart_dir(dir, 1)).unwrap();
        assert_eq!(list_ids(dir).unwrap(), vec![0, 1]);
        assert_eq!(next_id(dir).unwrap(), 2);

        assert_eq!(active_id(dir).unwrap(), None);
        make_active(dir, 1).unwrap();
        assert_eq!(active_id(dir).unwrap(), Some(1));
        // Relative symlink (§9.2).
        let target = fs::read_link(dir.join("output-0001-active")).unwrap();
        assert_eq!(target, PathBuf::from("output-0001"));
        // Exactly-one-active: a second activation refuses.
        assert!(make_active(dir, 0).is_err());

        assert_eq!(deactivate(dir).unwrap(), Some(1));
        assert_eq!(active_id(dir).unwrap(), None);
        assert_eq!(deactivate(dir).unwrap(), None);
    }

    #[test]
    fn handoff_replaces_predecessor() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir(restart_dir(dir, 3)).unwrap();
        fs::create_dir(restart_dir(dir, 4)).unwrap();
        make_active(dir, 3).unwrap();

        handoff_active(dir, 4).unwrap();
        assert_eq!(active_id(dir).unwrap(), Some(4));
        // Idempotent when already ours.
        handoff_active(dir, 4).unwrap();
        assert_eq!(active_id(dir).unwrap(), Some(4));
        // Works with no predecessor at all.
        deactivate(dir).unwrap();
        handoff_active(dir, 4).unwrap();
        assert_eq!(active_id(dir).unwrap(), Some(4));
    }

    #[test]
    fn scan_reports_ids_and_active_together() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir(restart_dir(dir, 2)).unwrap();
        fs::create_dir(restart_dir(dir, 0)).unwrap();
        // A non-directory entry named like a restart is ignored.
        fs::write(restart_dir(dir, 1), b"not a dir").unwrap();
        make_active(dir, 0).unwrap();

        let scan = scan(dir).unwrap();
        assert_eq!(scan.ids, vec![0, 2]);
        assert_eq!(scan.latest(), Some(2));
        assert_eq!(scan.active_id(dir).unwrap(), Some(0));
    }

    #[test]
    fn scan_active_id_errors_on_two_actives_but_list_ids_still_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir(restart_dir(dir, 0)).unwrap();
        fs::create_dir(restart_dir(dir, 1)).unwrap();
        make_active(dir, 0).unwrap();
        // Force a second active marker directly (make_active refuses this).
        std::os::unix::fs::symlink(dir_name(1), dir.join("output-0001-active")).unwrap();

        assert!(scan(dir).unwrap().active_id(dir).is_err());
        assert_eq!(list_ids(dir).unwrap(), vec![0, 1]);
    }

    #[test]
    fn vars_freeze_thaw_roundtrip() {
        let mut vars = VarSet::new();
        vars.set("NODES", 4u64);
        vars.set("QUEUE", "checkpt");
        vars.set("GPU", false);
        let frozen = freeze_vars(&vars);
        let thawed = thaw_vars(&frozen).unwrap();
        assert_eq!(thawed.get("NODES"), Some(&VarValue::Int(4)));
        assert_eq!(thawed.get("QUEUE"), Some(&VarValue::Str("checkpt".into())));
        assert_eq!(thawed.get("GPU"), Some(&VarValue::Bool(false)));
    }
}
