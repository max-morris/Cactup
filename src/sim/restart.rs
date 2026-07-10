//! Restarts: the numbered `output-%04d` dirs, the active symlink (§9.2), the
//! per-restart `.cactup/restart.toml` metadata (§9.3), checkpoint recovery
//! (§8.8), and the stale-restart reaper (§8.3).

use crate::database::SCHEMA;
use crate::installation::write_toml;
use crate::lock::{LinkLock, HEARTBEAT_STALE_SECS};
use crate::mdb::Universe;
use crate::scheduler::{JobStatus, Scheduler};
use crate::sim::Simulation;
use crate::template::{VarSet, VarValue};
use crate::walltime::Walltime;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use colored::Colorize;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

    pub fn to_universe(&self) -> Universe {
        Universe {
            kind: None,
            wrapper_argv: self.wrapper_argv.clone(),
            wrapper: self.wrapper.clone(),
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
    /// Whether this restart recovers checkpoints (§8.8).
    pub checkpointing: bool,
    /// Recovery hint: newest checkpoint-bearing restart at submit time; the
    /// compute node re-scans backward from it (§8.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_restart_id: Option<u32>,
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
    /// by a live run. Best-effort.
    pub fn touch_heartbeat(&self) {
        let path = self.heartbeat_path();
        let done = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now()));
        if done.is_err() {
            let _ = fs::write(&path, b"");
        }
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

/// All restart ids, sorted ascending.
pub fn list_ids(sim_dir: &Path) -> Res<Vec<u32>> {
    let mut ids = Vec::new();
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
        }
    }
    ids.sort_unstable();
    Ok(ids)
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
    let mut found = Vec::new();
    for entry in fs::read_dir(sim_dir)
        .with_context(|| format!("Failed to read simulation directory {}", sim_dir.display()))?
    {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(base) = name.strip_suffix("-active") {
            if let Some(id) = parse_dir_name(base) {
                found.push(id);
            }
        }
    }
    match found.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        several => bail!(
            "simulation at {} has more than one active restart ({}) — this must be repaired by hand",
            sim_dir.display(),
            several.iter().map(|id| dir_name(*id)).collect::<Vec<_>>().join(", ")
        ),
    }
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

/// Recoverable checkpoint files (`*chkpt.it_*`) in a working dir (§8.8).
pub fn checkpoint_files(workdir: &Path) -> Res<Vec<PathBuf>> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(workdir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        other => other.with_context(|| format!("Failed to read {}", workdir.display()))?,
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Skip half-written checkpoints (they are `clean`'s to delete, §8.6).
        if name.contains("chkpt.it_") && !name.contains("chkpt.tmp.it_") && entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// The newest checkpoint mtime in a restart's working dir, if any.
fn newest_checkpoint_mtime(sim: &Simulation, id: u32) -> Res<Option<SystemTime>> {
    let mut newest = None;
    for f in checkpoint_files(&workdir(sim, id))? {
        let mtime = fs::metadata(&f)?.modified()?;
        if newest.map(|n| mtime > n).unwrap_or(true) {
            newest = Some(mtime);
        }
    }
    Ok(newest)
}

/// The §8.8 backward scan: the newest restart at or before `upto` (or the
/// latest) that actually contains recoverable checkpoints. Deterministic and
/// prompt-free — used verbatim by the compute-node re-scan.
pub fn newest_with_checkpoints(sim: &Simulation, upto: Option<u32>) -> Res<Option<u32>> {
    for id in list_ids(&sim.dir)?
        .into_iter()
        .rev()
        .filter(|id| upto.map(|u| *id <= u).unwrap_or(true))
    {
        if !checkpoint_files(&workdir(sim, id))?.is_empty() {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Login-node recovery-source selection (§8.8): the backward scan, plus the
/// divergence prompt when id-order and checkpoint-mtime-order disagree (e.g.
/// after a manual `--restart-id` recovery created a newer branch in an older
/// restart). Never called on the compute-node path.
pub fn select_recovery_source(sim: &Simulation, upto: Option<u32>) -> Res<Option<u32>> {
    let mut bearing: Vec<(u32, SystemTime)> = Vec::new();
    for id in list_ids(&sim.dir)?
        .into_iter()
        .filter(|id| upto.map(|u| *id <= u).unwrap_or(true))
    {
        if let Some(mtime) = newest_checkpoint_mtime(sim, id)? {
            bearing.push((id, mtime));
        }
    }
    let Some(&(candidate, cand_mtime)) = bearing.last() else { return Ok(None) };

    let divergent = bearing[..bearing.len() - 1].iter().any(|&(_, m)| m > cand_mtime);
    if !divergent {
        return Ok(Some(candidate));
    }

    let ids: Vec<String> = bearing.iter().map(|(id, _)| id.to_string()).collect();
    eprintln!(
        "{} restart history is divergent: checkpoint-bearing restarts {} disagree with their \
         checkpoint timestamps (§8.8)",
        "warning:".yellow().bold(),
        ids.join(", ")
    );
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!(
            "cannot choose a recovery source non-interactively; rerun with --restart-id <N> \
             (checkpoint-bearing restarts: {})",
            ids.join(", ")
        );
    }
    let answer = crate::commands::prompt_with_default(
        "Recover from which restart?",
        &candidate.to_string(),
    )?;
    let chosen: u32 = answer
        .trim()
        .parse()
        .map_err(|_| anyhow!("\"{answer}\" is not a restart id"))?;
    if !bearing.iter().any(|(id, _)| *id == chosen) {
        bail!("restart {chosen} has no recoverable checkpoints (candidates: {})", ids.join(", "));
    }
    Ok(Some(chosen))
}

/// Port of `PrepareCheckpointing` (§8.8): hard-link (fall back to copy) every
/// checkpoint from the source restart's working dir into the current one.
/// No checkpoints ⇒ no-op (fresh start). Returns the number of files linked.
pub fn link_checkpoints(sim: &Simulation, from: u32, into: u32) -> Res<usize> {
    let src_dir = workdir(sim, from);
    let dst_dir = workdir(sim, into);
    let files = checkpoint_files(&src_dir)?;
    if files.is_empty() {
        return Ok(0);
    }
    fs::create_dir_all(&dst_dir)
        .with_context(|| format!("Failed to create working dir {}", dst_dir.display()))?;
    for src in &files {
        let dst = dst_dir.join(src.file_name().expect("checkpoint files have names"));
        if dst.exists() {
            continue;
        }
        if fs::hard_link(src, &dst).is_err() {
            fs::copy(src, &dst)
                .with_context(|| format!("Failed to copy checkpoint {}", src.display()))?;
        }
    }
    Ok(files.len())
}

/// Reaper throttles (§9.3): simfactory's "skip sims < 60 s old, re-clean
/// every 30 s", kept since the reaper runs on every submit.
const REAP_MIN_SIM_AGE_SECS: u64 = 60;
const REAP_RECLEAN_SECS: u64 = 30;

fn age_secs(path: &Path) -> Option<u64> {
    let mtime = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    SystemTime::now().duration_since(mtime).ok().map(|d| d.as_secs())
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

    fn sim_at(dir: &Path) -> Simulation {
        let mut meta = crate::sim::SimulationMeta::default();
        meta.parfile = "bbh.par".to_owned();
        Simulation { name: "bbh".to_owned(), dir: dir.to_owned(), meta }
    }

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
    fn backward_checkpoint_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let sim = sim_at(tmp.path());
        for id in 0..4 {
            fs::create_dir_all(workdir(&sim, id)).unwrap();
        }
        // 1 has checkpoints; 2 has only a half-written one; 3 has none.
        fs::write(workdir(&sim, 1).join("bbh.chkpt.it_100.h5"), b"c").unwrap();
        fs::write(workdir(&sim, 2).join("bbh.chkpt.tmp.it_200.h5"), b"c").unwrap();

        assert_eq!(newest_with_checkpoints(&sim, None).unwrap(), Some(1));
        assert_eq!(newest_with_checkpoints(&sim, Some(0)).unwrap(), None);
        // Linear history → silent selection, no divergence.
        assert_eq!(select_recovery_source(&sim, None).unwrap(), Some(1));
    }

    #[test]
    fn checkpoint_linking() {
        let tmp = tempfile::tempdir().unwrap();
        let sim = sim_at(tmp.path());
        fs::create_dir_all(workdir(&sim, 0)).unwrap();
        fs::create_dir_all(restart_dir(&sim.dir, 1)).unwrap();
        fs::write(workdir(&sim, 0).join("bbh.chkpt.it_50.h5"), b"data").unwrap();
        fs::write(workdir(&sim, 0).join("other.txt"), b"x").unwrap();

        assert_eq!(link_checkpoints(&sim, 0, 1).unwrap(), 1);
        let linked = workdir(&sim, 1).join("bbh.chkpt.it_50.h5");
        assert!(linked.is_file());
        assert!(!workdir(&sim, 1).join("other.txt").exists());
        // Hard link, not a copy (same filesystem).
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&linked).unwrap().nlink(), 2);
        // No checkpoints ⇒ no-op.
        assert_eq!(link_checkpoints(&sim, 1, 0).unwrap(), 1); // 1 now has the link
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
