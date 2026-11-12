//! Build attempts (`build submit` groundwork, not yet wired to a command):
//! one run of `make` for one config, recorded under
//! `configs/<name>/.cactup-builds/%04d/` so a queued build can be monitored
//! and a compute node can execute it without touching the global DB, the
//! installation registry, the MDB, or knobs (D11). Modeled closely on
//! `testsuite::{TestMeta, TestRun}` and `sim::restart::{RestartMeta, Restart}`.
//!
//! Unlike a simulation restart's `output-%04d`, there is no nested `.cactup/`
//! here — a build attempt's whole directory is already cactup's own, never a
//! user-visible workspace — and no `-active` symlink: a config has at most
//! one build in flight, so the highest-numbered attempt is always the
//! subject, and there is no simfactory `output-NNNN-active` contract to
//! preserve the way there is for restarts (§9.2).

use crate::build::{ConfigMeta, OptionlistSource, CONFIG};
use crate::format::{self, Format, Toml};
use crate::sim::restart::UniverseSpec;
use crate::walltime::Walltime;
use crate::Res;
use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// `build.toml`'s format: version 1, no migrations. Read and written by the
/// compute-node binary of a queued build, so it is `frozen_read`: a breaking
/// change ships its down-step. `legacy` is the rename of the attempt's own
/// `variant` key to `flavor` that shipped with config record schema 2,
/// without a bump of this record's version ([`build_legacy`]).
///
/// `[config-meta]` is a whole config record with its own version
/// ([`CONFIG`]), read through `ConfigMeta::deserialize_upgrading` and
/// written at `CONFIG.current` by `ConfigMeta::serialize_versioned`; it,
/// `[timestamps]`, `[reservation]` and `[universe]` are edited in place, so
/// keys a newer binary added inside them are put back. `[outcome]` is
/// replaced whole when a build ends or is stopped; `vars` and `knobs` are
/// open maps written whole at prepare.
pub static BUILD: Format<Toml> = Format {
    name: "build record",
    key: "schema",
    current: 1,
    upgrades: &[],
    downgrades: &[],
    down_floor: 1,
    frozen_read: true,
    legacy: Some(build_legacy),
    replace_whole: &["vars", "knobs", "outcome"],
};

/// An attempt recorded before the optionlist "variant" was renamed "flavor"
/// (config record schema 2) has its own `variant` key: the same rename,
/// kept indefinitely for the reason [`CONFIG`] gives. The embedded config
/// record is brought to `CONFIG.current` here too, so put-back compares it
/// in the shape its writer writes: compared at its on-disk schema 1, the
/// keys schema 2 renamed would look unknown and be put back beside their
/// new names.
fn build_legacy(doc: &mut Toml) -> Res<()> {
    if let Some(flavor) = doc.remove("variant") {
        doc.insert("flavor".to_owned(), flavor);
    }
    if let Some(config_meta) = doc.get_mut("config-meta").and_then(toml::Value::as_table_mut) {
        format::upgrade_embedded(config_meta, &CONFIG)?;
    }
    Ok(())
}

/// `[timestamps]` in `build.toml`, mirroring `testsuite::Timestamps` plus the
/// `started` point a build (unlike a one-shot test run) needs: submission and
/// the make invocation actually starting are distinct events once a build can
/// sit queued.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Timestamps {
    pub created: Option<DateTime<Utc>>,
    pub submitted: Option<DateTime<Utc>>,
    pub started: Option<DateTime<Utc>>,
    pub finished: Option<DateTime<Utc>>,
    /// When a stop cactup sent after the record's end, its status query
    /// having failed, settled the job: the scheduler took the stop, or
    /// refused it saying (by `unknown-job-pattern`) it does not know the
    /// job. The job is over from then on, and asked about again only after
    /// a later end (a requeued run's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_stopped: Option<DateTime<Utc>>,
}

/// `[outcome]` in `build.toml`: recorded once the `make` invocation ends.
/// `complete` is the completeness check's verdict (the same notion
/// `build::execute` uses today to decide a build actually finished, not just
/// that `make` exited), kept distinct from `exit_status` because a `make`
/// that exits 0 without actually finishing the build is exactly the case
/// this field exists to catch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BuildOutcomeRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
    pub complete: bool,
}

/// `[reservation]` in `build.toml`: the scheduler slot a build was queued
/// into. A foreground build reserves nothing — recording zeros or a
/// fabricated queue name would be a lie about a job that never went through
/// a scheduler — so `BuildMeta::reservation` is `None` for every build this
/// chunk produces; the submit chunk fills it in at submission time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Reservation {
    pub queue: String,
    pub nodes: u32,
    pub tasks: u32,
    pub tpn: u32,
    pub cpus: u32,
    #[serde(default)]
    pub gpus_per_task: u32,
    pub walltime: Walltime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<String>,
}

/// `<attempt dir>/build.toml`. The frozen `[vars]` table is the same additive
/// device as `restart.toml`'s (§9.3): it lets the compute-node build path
/// rebuild its whole substitution context from disk (D11).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BuildMeta {
    pub attempt_id: u32,
    pub config: String,
    /// Which optionlist this attempt was built from — the machine's flavor, or
    /// the `--optionlist` file (§7.8). Flattened, as in [`ConfigMeta`].
    #[serde(flatten)]
    pub optionlist_source: OptionlistSource,
    pub machine: String,
    pub alias: String,
    pub config_dir: PathBuf,
    pub cactus_root: PathBuf,
    /// NOT derivable from `cactus_root`: `fetch::source_heads` takes the
    /// *installation* root and joins the thornlist's own recorded root
    /// (which need not match `cactus_root`'s `!DEFINE ROOT`), so a compute
    /// node re-deriving source heads needs this frozen separately.
    pub install_root: PathBuf,
    pub submitted: bool,
    /// The scheduler's job id, `restart::NO_JOB_ID` when submission failed,
    /// or the pid for a foreground build.
    pub job_id: String,
    // `job-ids = "pids"` (§10): where `job-id`'s process started.
    /// On a machine whose job ids are pids: the host the job's process was
    /// started on, recorded with its id. Absent on a batch scheduler's
    /// machine, and in a record an older cactup made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_host: Option<String>,
    /// With `job-host`: that process's start time, from `/proc/<pid>/stat`.
    /// Absent with `job-host` set when the process had ended already when
    /// cactup read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_start: Option<u64>,
    /// Last observed live status letter (cached; the live query is the
    /// source of truth, mirroring `TestMeta::status`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The scheduler slot this build was queued into; `None` for a
    /// foreground build (this chunk always leaves it `None` — see
    /// [`Reservation`]'s doc comment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation: Option<Reservation>,
    /// The human-readable reason this build is happening ("optionlist
    /// changed", …), for `build show`.
    pub decision: String,
    /// Whether the rebuild decision was a from-scratch one, as `prepare` saw
    /// it. `execute` may upgrade this on its own if a re-probe shows the
    /// flesh has since moved (`FLESH_NOT_AS_BUILT`) — see `execute`'s doc
    /// comment — and updates this field to match before it stores the
    /// outcome, so a later `build show` reflects what actually happened, not
    /// just what was decided hours earlier on the login node.
    pub full_rebuild: bool,
    /// The machine's resolved `make` invocation for this attempt (post
    /// `@MAKEJOBS@`/env substitution, e.g. `"make -j4"`), frozen at `prepare`
    /// time because resolving `[build].make` needs the MDB (D11). `execute`
    /// reuses it verbatim to drive the exact same `make` if it needs to run
    /// an extra `<name>-realclean` step of its own (the upgrade described on
    /// `full_rebuild` above). `None` only for a `--virtual-executable`
    /// build, which never runs `make` at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    /// This attempt's build-phase environment setup (§6.1), frozen alongside
    /// `make` for the same D11 reason — re-resolving `machine.meta`'s
    /// `[environment]`/universe overrides in `execute` would mean reading
    /// the MDB there. Empty for a `--virtual-executable` build.
    #[serde(default)]
    pub build_env: String,
    /// The build's provisioning steps (`provision`, then `build-provision`,
    /// §6.1), as the build script runs them after `build_env`, each in a
    /// subshell: frozen for the same D11 reason. Kept apart from
    /// `build_env` because they cannot change the environment the build
    /// runs in: not part of the build cache's key, never run by `cache
    /// doctor`. Empty when the machine has none, and for a
    /// `--virtual-executable` build.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub build_provisioning: String,
    /// `--virtual-executable`'s source path (§7.7), canonicalized at
    /// `prepare` time; `execute` copies it into place instead of running
    /// `make`. Not run through the build universe or a spawned shell — a
    /// plain in-process file copy, exactly as before this split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_executable: Option<PathBuf>,
    /// The frozen run universe; absent = host context (§4.8). A config's
    /// recorded universe is only a *name* — the executing node must
    /// re-wrap the build command without reading the MDB (D11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universe: Option<UniverseSpec>,
    /// The fully-formed config metadata to store on success — `built` is
    /// unset until then (execute stamps it). Carries its own version key.
    #[serde(
        deserialize_with = "ConfigMeta::deserialize_upgrading",
        serialize_with = "ConfigMeta::serialize_versioned"
    )]
    pub config_meta: ConfigMeta,
    #[serde(default)]
    pub vars: IndexMap<String, toml::Value>,
    /// The effective knob snapshot as of `prepare` (§5, §6.1), frozen for
    /// the same D11 reason as `vars`: `@KNOB(…)@` in a build submit script
    /// must resolve on the compute node without the global DB.
    #[serde(default)]
    pub knobs: IndexMap<String, String>,
    #[serde(default)]
    pub timestamps: Timestamps,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<BuildOutcomeRecord>,
}

impl BuildMeta {
    /// The recorded job, as the scheduler is asked about it, with when the
    /// record was made (its submission, else its creation).
    pub fn job(&self) -> crate::scheduler::JobRef<'_> {
        crate::scheduler::JobRef::new(
            &self.job_id,
            self.job_host.as_deref(),
            self.job_start,
            self.timestamps.submitted.or(self.timestamps.created),
        )
    }

    /// Record `job` (a submitted one, or a foreground run's pid) as this
    /// attempt's job.
    pub fn set_job(&mut self, job: &crate::scheduler::JobRecord) {
        self.job_id = job.id.clone();
        self.job_host = job.host.clone();
        self.job_start = job.start;
    }
}

/// A located build attempt.
#[derive(Debug)]
pub struct BuildAttempt {
    pub id: u32,
    pub dir: PathBuf,
    pub meta: BuildMeta,
}

impl BuildAttempt {
    /// `configs/<name>/.cactup-builds` (the attempts root).
    // Pinned foundation API (build submit groundwork); only tests call these
    // so far — the command that consumes them lands later.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn builds_dir(config_dir: &Path) -> PathBuf {
        config_dir.join(".cactup-builds")
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dir_name(id: u32) -> String {
        format!("{id:04}")
    }

    pub fn attempt_dir(config_dir: &Path, id: u32) -> PathBuf {
        Self::builds_dir(config_dir).join(Self::dir_name(id))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn meta_path(dir: &Path) -> PathBuf {
        dir.join("build.toml")
    }

    pub fn script_path(&self) -> PathBuf {
        self.dir.join("build-script")
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn submit_script_path(&self) -> PathBuf {
        self.dir.join("submit-script")
    }

    /// The rendered native optionlist staged for this attempt's `make`
    /// invocation (`options=`) — installed into `configs/<name>/` under the
    /// same filename only once `execute` succeeds (see the module doc: a
    /// prepared-but-never-run attempt must not touch the live config).
    pub fn optionlist_path(&self) -> PathBuf {
        self.dir.join("cactup-optionlist.cfg")
    }

    /// Touched by the build script once its configure step has succeeded:
    /// which attempt configured the config last (the newest of these) is
    /// what `cache doctor` checks a config's flags against. Nothing reads it
    /// on the build path.
    pub fn configured_stamp_path(&self) -> PathBuf {
        self.dir.join(CONFIGURED_STAMP)
    }

    /// This attempt's optionlist *source* snapshot (verbatim TOML, pre-render)
    /// — the §7.8 rebuild-decision diff input, installed alongside
    /// [`Self::optionlist_path`] on success.
    pub fn optionlist_snapshot_path(&self) -> PathBuf {
        self.dir.join("cactup-optionlist.toml")
    }

    /// This attempt's processed thornlist (toggles applied), staged for
    /// `make`'s `THORNLIST=` — installed on success like
    /// [`Self::optionlist_path`].
    pub fn thornlist_path(&self) -> PathBuf {
        self.dir.join(crate::build::THORNLIST_PROCESSED)
    }

    /// This attempt's thornlist *source* snapshot — installed on success like
    /// [`Self::optionlist_snapshot_path`].
    pub fn thornlist_snapshot_path(&self) -> PathBuf {
        self.dir.join(crate::build::THORNLIST_SNAPSHOT)
    }

    pub fn out_path(&self) -> PathBuf {
        self.dir.join("build.out")
    }

    pub fn err_path(&self) -> PathBuf {
        self.dir.join("build.err")
    }

    pub fn running_lock_path(&self) -> PathBuf {
        self.dir.join("running.lock")
    }

    pub fn heartbeat_path(&self) -> PathBuf {
        self.dir.join("heartbeat")
    }

    /// The build cache's files for this attempt (`crate::objcache`): its
    /// frozen settings, the makefile fragment that wraps the compilers, and
    /// the log of wrapped compiles. Absent when the cache is off.
    pub fn cc_dir(&self) -> PathBuf {
        self.dir.join("cc")
    }

    /// Every attempt id present as a `%04d` directory under `.cactup-builds`,
    /// ascending. One `read_dir`, junk entries (non-`%04d`, non-directories)
    /// ignored — mirrors `sim::restart::list_ids`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn scan(config_dir: &Path) -> Res<Vec<u32>> {
        let dir = Self::builds_dir(config_dir);
        let mut ids = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            other => other.with_context(|| format!("Failed to read '{}'", dir.display()))?,
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(id) = parse_attempt_dir_name(name) && entry.file_type()?.is_dir() {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// The newest attempt id, if any. There is deliberately no "active"
    /// notion beyond this — see the module doc.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latest_id(config_dir: &Path) -> Res<Option<u32>> {
        Ok(Self::scan(config_dir)?.last().copied())
    }

    pub fn next_id(config_dir: &Path) -> Res<u32> {
        let next = match Self::scan(config_dir)?.last() {
            Some(&max) => max + 1,
            None => 0,
        };
        if next > 9999 {
            bail!("Maximum number of build attempts reached");
        }
        Ok(next)
    }

    /// Open the build attempt at `<config_dir>/.cactup-builds/<id>`
    /// (schema-guarded).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(config_dir: &Path, id: u32) -> Res<BuildAttempt> {
        let dir = Self::attempt_dir(config_dir, id);
        let meta = Self::read_meta(&dir)?;
        Ok(BuildAttempt { id, dir, meta })
    }

    /// The `build.toml` in attempt directory `dir` (schema-guarded).
    fn read_meta(dir: &Path) -> Res<BuildMeta> {
        let path = Self::meta_path(dir);
        format::read(&path, &BUILD)?.ok_or_else(|| missing(&path))
    }

    /// The lock of attempt directory `dir`'s record, waited for: the
    /// submitter holds it while it submits and records the job, and the
    /// compute side's writes take it ([`Self::store_meta_keeping_job`]), so
    /// a job that starts at once never writes back a copy read before its
    /// job id (and, where job ids are pids, its host and start time) was
    /// recorded.
    pub fn record_lock(dir: &Path) -> Res<crate::lock::LinkLock> {
        crate::lock::LinkLock::acquire_wait(
            &dir.join("record.lock"),
            std::time::Duration::from_secs(crate::lock::HANDOFF_WAIT_SECS),
        )
    }

    /// [`Self::store_meta`] under [`Self::record_lock`], keeping the job the
    /// submitter recorded on disk (its id, host, start time, `submitted` and
    /// when): what `execute` writes, which may run before the submitter has
    /// recorded them.
    pub fn store_meta_keeping_job(&mut self) -> Res<()> {
        let _lock = Self::record_lock(&self.dir)?;
        let path = Self::meta_path(&self.dir);
        let (disk, on_disk) = format::read_for_write::<BuildMeta, _>(&path, &BUILD)?;
        let disk = disk.ok_or_else(|| missing(&path))?;
        if disk.submitted {
            self.meta.submitted = true;
            self.meta.job_id = disk.job_id;
            self.meta.job_host = disk.job_host;
            self.meta.job_start = disk.job_start;
            self.meta.timestamps.submitted = disk.timestamps.submitted;
        }
        format::write_over(&path, &self.meta, &BUILD, &on_disk)
    }

    /// Create a fresh attempt directory and write its metadata: a record
    /// made anew, so nothing of one already there is kept.
    pub fn create(dir: PathBuf, meta: BuildMeta) -> Res<BuildAttempt> {
        fs::create_dir_all(&dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
        let attempt = BuildAttempt { id: meta.attempt_id, dir, meta };
        format::replace(&Self::meta_path(&attempt.dir), &attempt.meta, &BUILD)?;
        Ok(attempt)
    }

    /// Store an edit of the record on disk (keys this binary does not model
    /// are kept).
    pub fn store_meta(&self) -> Res<()> {
        format::write(&Self::meta_path(&self.dir), &self.meta, &BUILD)
    }

    /// Touch the heartbeat file's mtime; called periodically by a live
    /// build. Best-effort. A plain write stamps the mtime with the
    /// *fileserver's* clock — the same domain staleness checks measure
    /// against — where `set_modified(now)` would inject this node's local
    /// clock (mirrors `Restart::touch_heartbeat`).
    pub fn touch_heartbeat(&self) {
        let _ = fs::write(self.heartbeat_path(), b"");
    }
}

/// An attempt directory without its `build.toml`.
fn missing(path: &Path) -> anyhow::Error {
    anyhow::anyhow!("Failed to read '{}': the build record is missing", path.display())
}

/// Parse a `%04d` attempt directory name (>=4 ASCII digits, like
/// `sim::restart`'s `output-%04d` parser minus the prefix — a build attempt
/// dir carries no other name component).
/// The file in an attempt's directory its build script writes once its
/// configure step has succeeded ([`BuildAttempt::configured_stamp_path`]).
pub const CONFIGURED_STAMP: &str = "configured";

fn parse_attempt_dir_name(name: &str) -> Option<u32> {
    if name.len() < 4 || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::restart::{freeze_knobs, freeze_vars, thaw_vars};
    use crate::template::VarSet;

    fn sample_meta() -> BuildMeta {
        let mut vars = VarSet::new();
        vars.set("MAKEJOBS", 4i64);
        vars.set("SOURCEBASEDIR", "/home/user/Cactus");

        BuildMeta {
            attempt_id: 0,
            config: "sim".to_owned(),
            optionlist_source: OptionlistSource::Flavor("generic".to_owned()),
            machine: "mike".to_owned(),
            alias: "et-dev".to_owned(),
            config_dir: PathBuf::from("/home/user/Cactus/configs/sim"),
            cactus_root: PathBuf::from("/home/user/Cactus"),
            install_root: PathBuf::from("/home/user/et"),
            submitted: false,
            job_id: "12345".to_owned(),
            job_host: None,
            job_start: None,
            status: Some("R".to_owned()),
            reservation: Some(Reservation {
                queue: "checkpt".to_owned(),
                nodes: 1,
                tasks: 1,
                tpn: 1,
                cpus: 4,
                gpus_per_task: 0,
                walltime: Walltime(3600),
                allocation: None,
            }),
            decision: "optionlist changed".to_owned(),
            full_rebuild: true,
            make: Some("make -j4".to_owned()),
            build_env: "module load gcc".to_owned(),
            build_provisioning: String::new(),
            virtual_executable: None,
            universe: Some(UniverseSpec {
                name: "et-sif".to_owned(),
                wrapper_argv: Some(vec!["singularity".to_owned(), "exec".to_owned()]),
                wrapper: None,
            }),
            config_meta: sample_config_meta(),
            vars: freeze_vars(&vars),
            knobs: freeze_knobs(&vars),
            timestamps: Timestamps {
                created: Some(Utc::now()),
                submitted: None,
                started: None,
                finished: None,
                job_stopped: None,
            },
            outcome: None,
        }
    }

    fn sample_config_meta() -> ConfigMeta {
        toml::from_str(
            r#"
            schema = 2
            name = "sim"
            flavor = "generic"
            thornlist = "sim.th"
            machine = "mike"
            coerce-run-universe = true
            config-id = "cfg-1"
            build-id = "build-1"
            "#,
        )
        .unwrap()
    }

    /// An attempt recorded by a cactup from before the flavor rename — its
    /// own `variant` key and its embedded schema-1 config record — opens as
    /// the current shape, so `build list/show/log` keep working on it.
    #[test]
    fn opens_an_attempt_recorded_before_the_flavor_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path();
        let meta = sample_meta();
        let dir = BuildAttempt::attempt_dir(config_dir, meta.attempt_id);
        BuildAttempt::create(dir.clone(), meta).unwrap();

        // Rewrite what was stored into the old shape.
        let path = BuildAttempt::meta_path(&dir);
        let mut table: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let flavor = table.remove("flavor").unwrap();
        table.insert("variant".to_owned(), flavor);
        let cfg = table.get_mut("config-meta").unwrap().as_table_mut().unwrap();
        cfg.insert("schema".to_owned(), toml::Value::Integer(1));
        let flavor = cfg.remove("flavor").unwrap();
        cfg.insert("variant".to_owned(), flavor);
        cfg.remove("runs-on");
        cfg.insert("gpu".to_owned(), toml::Value::Boolean(true));
        cfg.insert("compatible-queues".to_owned(), toml::Value::Array(vec!["gpu".into()]));
        fs::write(&path, toml::to_string(&table).unwrap()).unwrap();

        let reopened = BuildAttempt::open(config_dir, 0).unwrap();
        assert_eq!(reopened.meta.optionlist_source, OptionlistSource::Flavor("generic".to_owned()));
        let cfg = &reopened.meta.config_meta;
        assert_eq!(cfg.optionlist_source, OptionlistSource::Flavor("generic".to_owned()));
        assert_eq!(cfg.runs_on.queues, ["gpu"]);
        assert!(cfg.runs_on.gpu);
    }

    #[test]
    fn numbering_ignores_junk_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path();
        assert_eq!(BuildAttempt::next_id(config_dir).unwrap(), 0);
        assert_eq!(BuildAttempt::latest_id(config_dir).unwrap(), None);

        fs::create_dir_all(BuildAttempt::attempt_dir(config_dir, 0)).unwrap();
        fs::create_dir_all(BuildAttempt::attempt_dir(config_dir, 1)).unwrap();
        // Junk: not %04d, and a plain file that looks like one.
        fs::create_dir_all(BuildAttempt::builds_dir(config_dir).join("notanid")).unwrap();
        fs::write(BuildAttempt::builds_dir(config_dir).join("0002"), b"not a dir").unwrap();

        assert_eq!(BuildAttempt::scan(config_dir).unwrap(), vec![0, 1]);
        assert_eq!(BuildAttempt::latest_id(config_dir).unwrap(), Some(1));
        assert_eq!(BuildAttempt::next_id(config_dir).unwrap(), 2);
    }

    #[test]
    fn round_trip_through_create_and_open() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path();
        let meta = sample_meta();
        let dir = BuildAttempt::attempt_dir(config_dir, meta.attempt_id);
        BuildAttempt::create(dir.clone(), meta.clone()).unwrap();

        let reopened = BuildAttempt::open(config_dir, 0).unwrap();
        assert_eq!(reopened.id, 0);
        assert_eq!(reopened.dir, dir);
        assert_eq!(reopened.meta.config, meta.config);
        assert_eq!(reopened.meta.optionlist_source, meta.optionlist_source);
        assert_eq!(reopened.meta.machine, meta.machine);
        assert_eq!(reopened.meta.job_id, meta.job_id);
        assert_eq!(reopened.meta.decision, meta.decision);
        assert_eq!(reopened.meta.full_rebuild, meta.full_rebuild);
        assert_eq!(reopened.meta.config_meta.config_id, meta.config_meta.config_id);

        // The frozen universe and vars survive the round trip intact.
        assert_eq!(reopened.meta.universe.as_ref().unwrap().name, "et-sif");
        let thawed = thaw_vars(&reopened.meta.vars, &reopened.meta.knobs).unwrap();
        let original = thaw_vars(&meta.vars, &meta.knobs).unwrap();
        assert_eq!(thawed.get("MAKEJOBS"), original.get("MAKEJOBS"));
        assert_eq!(thawed.get("SOURCEBASEDIR"), original.get("SOURCEBASEDIR"));

        // The reservation and the frozen make/env round-trip too.
        let reservation = reopened.meta.reservation.as_ref().unwrap();
        assert_eq!(reservation.queue, "checkpt");
        assert_eq!(reservation.nodes, 1);
        assert_eq!(reopened.meta.make, meta.make);
        assert_eq!(reopened.meta.build_env, meta.build_env);
    }

    #[test]
    fn open_rejects_a_future_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path();
        let dir = BuildAttempt::attempt_dir(config_dir, 0);
        BuildAttempt::create(dir.clone(), sample_meta()).unwrap();
        let path = BuildAttempt::meta_path(&dir);
        let text = fs::read_to_string(&path).unwrap().replacen("schema = 1\n", "schema = 1001\n", 1);
        fs::write(&path, &text).unwrap();

        let err = format!("{:#}", BuildAttempt::open(config_dir, 0).unwrap_err());
        assert!(err.contains("upgrade cactup") && err.contains("build.toml"), "unexpected error: {err}");
    }

    /// A build record whose embedded config record a newer cactup wrote is
    /// refused when opened, naming the file, and left as it was.
    #[test]
    fn open_rejects_a_future_embedded_config_record() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path();
        let dir = BuildAttempt::attempt_dir(config_dir, 0);
        BuildAttempt::create(dir.clone(), sample_meta()).unwrap();
        let path = BuildAttempt::meta_path(&dir);
        let text = fs::read_to_string(&path).unwrap();
        let text = text.replace("[config-meta]\nschema = 2\n", "[config-meta]\nschema = 3\n");
        fs::write(&path, &text).unwrap();

        let err = format!("{:#}", BuildAttempt::open(config_dir, 0).unwrap_err());
        assert!(
            err.contains("build.toml") && err.contains("an embedded config record was written by a newer cactup"),
            "unexpected error: {err}"
        );
        assert!(!err.contains('§'), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    /// A newer cactup rewriting the embedded config record after this one
    /// opened the attempt: neither the edit (`store_meta`) nor a record made
    /// anew over it (`create`) steps it down to this binary's schema.
    #[test]
    fn writes_refuse_a_future_embedded_config_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BuildAttempt::attempt_dir(tmp.path(), 0);
        let attempt = BuildAttempt::create(dir.clone(), sample_meta()).unwrap();
        let path = BuildAttempt::meta_path(&dir);
        let text = fs::read_to_string(&path).unwrap();
        let text = text.replace("[config-meta]\nschema = 2\n", "[config-meta]\nschema = 3\nnew-v3-key = 1\n");
        fs::write(&path, &text).unwrap();

        let err = format!("{:#}", attempt.store_meta().unwrap_err());
        assert!(err.contains("build.toml") && err.contains("newer cactup"), "unexpected error: {err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let err = format!("{:#}", BuildAttempt::create(dir, sample_meta()).unwrap_err());
        assert!(err.contains("newer cactup"), "unexpected error: {err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    /// Every field set, every collection non-empty, and no `..`: a new field
    /// fails to compile here, and the shape pin then says what to do. The
    /// two optionlist sources take the two values of the enum.
    fn exhaustive_sample() -> BuildMeta {
        let time = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let [flavor, optionlist] = crate::build::every_optionlist_source();
        BuildMeta {
            attempt_id: 3,
            config: "sim".to_owned(),
            optionlist_source: flavor,
            machine: "workstation".to_owned(),
            alias: "et".to_owned(),
            config_dir: PathBuf::from("/home/user/et/Cactus/configs/sim"),
            cactus_root: PathBuf::from("/home/user/et/Cactus"),
            install_root: PathBuf::from("/home/user/et"),
            submitted: true,
            job_id: "1234".to_owned(),
            job_host: Some("login1".to_owned()),
            job_start: Some(99887766),
            status: Some("R".to_owned()),
            reservation: Some(Reservation {
                queue: "gpu".to_owned(),
                nodes: 1,
                tasks: 2,
                tpn: 2,
                cpus: 8,
                gpus_per_task: 1,
                walltime: Walltime(3600),
                allocation: Some("proj1".to_owned()),
            }),
            decision: "optionlist changed".to_owned(),
            full_rebuild: true,
            make: Some("make -j8".to_owned()),
            build_env: "module load gcc".to_owned(),
            build_provisioning: "spack install foo".to_owned(),
            virtual_executable: Some(PathBuf::from("/home/user/bin/cactus_sim")),
            universe: Some(UniverseSpec {
                name: "et-sif".to_owned(),
                wrapper_argv: Some(["apptainer", "exec", "img.sif", "@COMMAND@"].map(String::from).to_vec()),
                wrapper: Some("apptainer exec img.sif @COMMAND@".to_owned()),
            }),
            config_meta: ConfigMeta::exhaustive_sample(optionlist),
            vars: [
                ("MAKEJOBS".to_owned(), toml::Value::Integer(8)),
                ("SOURCEBASEDIR".to_owned(), toml::Value::String("/home/user/et/Cactus".to_owned())),
            ]
            .into_iter()
            .collect(),
            knobs: [("queue".to_owned(), "gpu".to_owned())].into_iter().collect(),
            timestamps: Timestamps {
                created: Some(time("2026-05-01T12:00:00Z")),
                submitted: Some(time("2026-05-01T12:01:00Z")),
                started: Some(time("2026-05-01T12:05:00Z")),
                finished: Some(time("2026-05-01T13:00:00Z")),
                job_stopped: Some(time("2026-05-01T13:01:00Z")),
            },
            outcome: Some(BuildOutcomeRecord { exit_status: Some(0), complete: true }),
        }
    }

    #[test]
    fn build_format_generic_tests() {
        use crate::format::testing::*;
        let fixtures = fixture_dir("build");
        descriptor_is_consistent(&BUILD);
        assert!(BUILD.frozen_read);
        shape_is_pinned(&BUILD, &fixtures, &exhaustive_sample());
        every_fixture_loads::<BuildMeta, _>(&BUILD, &fixtures);
        newer_is_refused::<BuildMeta, _>(&BUILD, "build.toml", "attempt-id = \"many\"\n", &exhaustive_sample());
        unknown_keys_survive(
            &BUILD,
            "build.toml",
            &exhaustive_sample(),
            &["config-meta", "runs-on"],
            |m: &mut BuildMeta| m.timestamps.job_stopped = None,
            &["timestamps", "job-stopped"],
        );
    }

    /// The attempts an older cactup recorded before the flavor rename (its
    /// own `variant` key, its embedded config record at schema 1, with and
    /// without that record's `schema` key) open as what they recorded, and
    /// once stored again hold only the current spelling of each key.
    #[test]
    fn attempts_recorded_before_the_flavor_rename_open_and_store_cleanly() {
        for name in ["v1-variant.toml", "v1-variant-embedded-noschema.toml"] {
            let tmp = tempfile::tempdir().unwrap();
            let dir = BuildAttempt::attempt_dir(tmp.path(), 0);
            fs::create_dir_all(&dir).unwrap();
            let path = BuildAttempt::meta_path(&dir);
            fs::copy(crate::format::testing::fixture_dir("build").join(name), &path).unwrap();
            let original = fs::read_to_string(&path).unwrap();
            let mut attempt = BuildAttempt::open(tmp.path(), 0).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), original, "{name}: a read rewrote it");
            let cuda = OptionlistSource::Flavor("cuda".to_owned());
            assert_eq!(attempt.meta.optionlist_source, cuda, "{name}");
            let cfg = &attempt.meta.config_meta;
            assert_eq!(cfg.optionlist_source, cuda, "{name}");
            assert_eq!(cfg.runs_on.queues, ["gpu", "gpu-debug"], "{name}");
            assert!(cfg.runs_on.gpu, "{name}");

            attempt.meta.status = Some("C".to_owned());
            attempt.store_meta_keeping_job().unwrap();
            let doc: Toml = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(doc["schema"].as_integer(), Some(1), "{name}");
            assert!(!doc.contains_key("variant") && doc["flavor"].as_str() == Some("cuda"), "{name}");
            let cfg = doc["config-meta"].as_table().unwrap();
            assert_eq!(cfg.keys().next().map(String::as_str), Some("schema"), "{name}");
            assert_eq!(cfg["schema"].as_integer(), Some(2), "{name}");
            for old in ["variant", "gpu", "compatible-queues"] {
                assert!(!cfg.contains_key(old), "{name}: {old} came back: {cfg:?}");
            }
            assert_eq!(cfg["flavor"].as_str(), Some("cuda"), "{name}");
            assert_eq!(cfg["runs-on"]["gpu"].as_bool(), Some(true), "{name}");
        }
    }

    /// Every config record fixture, embedded as a build record's
    /// `[config-meta]`, reads as the same record it reads as standalone.
    #[test]
    fn config_fixtures_load_embedded_as_they_load_standalone() {
        let host: Toml = toml::from_str(
            &fs::read_to_string(crate::format::testing::fixture_dir("build").join("current.toml")).unwrap(),
        )
        .unwrap();
        let config_fixtures = crate::format::testing::fixture_dir("config");
        for entry in fs::read_dir(&config_fixtures).unwrap() {
            let entry = entry.unwrap();
            let standalone = ConfigMeta::load_path(&entry.path()).unwrap().unwrap();
            let embedded: Toml = toml::from_str(&fs::read_to_string(entry.path()).unwrap()).unwrap();
            let mut doc = host.clone();
            doc.insert("config-meta".to_owned(), toml::Value::Table(embedded));
            let tmp = tempfile::tempdir().unwrap();
            let dir = BuildAttempt::attempt_dir(tmp.path(), 0);
            fs::create_dir_all(&dir).unwrap();
            fs::write(BuildAttempt::meta_path(&dir), toml::to_string_pretty(&doc).unwrap()).unwrap();
            let attempt = BuildAttempt::open(tmp.path(), 0).unwrap();
            assert_eq!(
                toml::Table::try_from(&attempt.meta.config_meta).unwrap(),
                toml::Table::try_from(&standalone).unwrap(),
                "{}",
                entry.path().display()
            );
        }
    }

    /// What `execute` writes keeps what a newer cactup added to the record
    /// meanwhile, at the top level, inside the embedded `[config-meta]` and
    /// inside its `[runs-on]`, and the job the submitter recorded.
    #[test]
    fn store_meta_keeping_job_keeps_unknown_keys_in_the_embedded_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BuildAttempt::attempt_dir(tmp.path(), 0);
        let mut attempt = BuildAttempt::create(dir.clone(), sample_meta()).unwrap();
        let path = BuildAttempt::meta_path(&dir);
        let mut doc: Toml = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        doc.insert("future-top".to_owned(), toml::Value::Integer(1));
        doc.insert("submitted".to_owned(), toml::Value::Boolean(true));
        doc.insert("job-id".to_owned(), toml::Value::String("777".to_owned()));
        let cfg = doc.get_mut("config-meta").unwrap().as_table_mut().unwrap();
        cfg.insert("future-config".to_owned(), toml::Value::Integer(2));
        let mut runs_on = toml::Table::new();
        runs_on.insert("future-runs-on".to_owned(), toml::Value::Integer(3));
        cfg.insert("runs-on".to_owned(), toml::Value::Table(runs_on));
        fs::write(&path, toml::to_string_pretty(&doc).unwrap()).unwrap();

        attempt.meta.outcome = Some(BuildOutcomeRecord { exit_status: Some(0), complete: true });
        attempt.store_meta_keeping_job().unwrap();
        let doc: Toml = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["future-top"].as_integer(), Some(1), "{doc:?}");
        assert_eq!(doc["job-id"].as_str(), Some("777"));
        assert_eq!(doc["outcome"]["complete"].as_bool(), Some(true));
        let cfg = doc["config-meta"].as_table().unwrap();
        assert_eq!(cfg["future-config"].as_integer(), Some(2), "{cfg:?}");
        assert_eq!(cfg["runs-on"]["future-runs-on"].as_integer(), Some(3), "{cfg:?}");
        assert_eq!(cfg["schema"].as_integer(), Some(2));
    }

    /// Moving the attempt (and its embedded record) from a flavor to an
    /// `--optionlist` file never leaves both keys in either.
    #[test]
    fn switching_the_optionlist_source_never_leaves_both_keys_in_a_build_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BuildAttempt::attempt_dir(tmp.path(), 0);
        fs::create_dir_all(&dir).unwrap();
        let path = BuildAttempt::meta_path(&dir);
        fs::copy(crate::format::testing::fixture_dir("build").join("v1-variant.toml"), &path).unwrap();
        let mut attempt = BuildAttempt::open(tmp.path(), 0).unwrap();
        let file = OptionlistSource::Optionlist("/x/my.cfg".to_owned());
        attempt.meta.optionlist_source = file.clone();
        attempt.meta.config_meta.optionlist_source = file;
        attempt.store_meta().unwrap();
        let doc: Toml = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        for table in [&doc, doc["config-meta"].as_table().unwrap()] {
            assert!(!table.contains_key("flavor") && !table.contains_key("variant"), "{table:?}");
            assert_eq!(table["optionlist"].as_str(), Some("/x/my.cfg"));
        }
    }

    #[test]
    fn attempt_paths_are_flat_files_under_the_attempt_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path();
        let dir = BuildAttempt::attempt_dir(config_dir, 0);
        let attempt = BuildAttempt::create(dir.clone(), sample_meta()).unwrap();

        assert_eq!(attempt.script_path(), dir.join("build-script"));
        assert_eq!(attempt.submit_script_path(), dir.join("submit-script"));
        assert_eq!(attempt.out_path(), dir.join("build.out"));
        assert_eq!(attempt.err_path(), dir.join("build.err"));
        assert_eq!(attempt.running_lock_path(), dir.join("running.lock"));
        assert_eq!(attempt.heartbeat_path(), dir.join("heartbeat"));
    }

    #[test]
    fn touch_heartbeat_creates_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path();
        let dir = BuildAttempt::attempt_dir(config_dir, 0);
        let attempt = BuildAttempt::create(dir, sample_meta()).unwrap();
        assert!(!attempt.heartbeat_path().is_file());
        attempt.touch_heartbeat();
        assert!(attempt.heartbeat_path().is_file());
    }
}
