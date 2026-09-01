//! Build attempts (`build submit` groundwork, not yet wired to a command):
//! one run of `make` for one config, recorded under
//! `configs/<name>/.cactup-builds/%04d/` so a queued build can be monitored
//! and a compute node can execute it without touching the global DB, the
//! installation registry, the MDB, or knobs (D11). Modelled closely on
//! `testsuite::{TestMeta, TestRun}` and `sim::restart::{RestartMeta, Restart}`.
//!
//! Unlike a simulation restart's `output-%04d`, there is no nested `.cactup/`
//! here — a build attempt's whole directory is already cactup's own, never a
//! user-visible workspace — and no `-active` symlink: a config has at most
//! one build in flight, so the highest-numbered attempt is always the
//! subject, and there is no simfactory `output-NNNN-active` contract to
//! preserve the way there is for restarts (§9.2).

use crate::build::{ConfigMeta, OptionlistSource};
use crate::database::SCHEMA;
use crate::installation::write_toml;
use crate::sim::restart::UniverseSpec;
use crate::walltime::Walltime;
use crate::Res;
use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_schema() -> u32 {
    SCHEMA
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
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub attempt_id: u32,
    pub config: String,
    /// Which optionlist this attempt was built from — the machine variant, or
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
    /// unset until then (execute stamps it).
    pub config_meta: ConfigMeta,
    #[serde(default)]
    pub vars: IndexMap<String, toml::Value>,
    #[serde(default)]
    pub timestamps: Timestamps,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<BuildOutcomeRecord>,
}

/// A located build attempt.
#[derive(Debug)]
pub struct BuildAttempt {
    // Pinned foundation API (build submit groundwork); only tests read this
    // field so far — the command that consumes it lands later.
    #[cfg_attr(not(test), allow(dead_code))]
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

    /// Every attempt id present as a `%04d` directory under `.cactup-builds`,
    /// ascending. One `read_dir`, junk entries (non-`%04d`, non-directories)
    /// ignored — mirrors `sim::restart::list_ids`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn scan(config_dir: &Path) -> Res<Vec<u32>> {
        let dir = Self::builds_dir(config_dir);
        let mut ids = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            other => other.with_context(|| format!("Failed to read {}", dir.display()))?,
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
            bail!("maximum number of build attempts reached");
        }
        Ok(next)
    }

    /// Open the build attempt at `<config_dir>/.cactup-builds/<id>`
    /// (schema-guarded).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(config_dir: &Path, id: u32) -> Res<BuildAttempt> {
        let dir = Self::attempt_dir(config_dir, id);
        let path = Self::meta_path(&dir);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let meta: BuildMeta =
            toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;
        if meta.schema > SCHEMA {
            bail!(
                "{} has schema {} but this cactup understands at most schema {SCHEMA}; \
                 please upgrade cactup",
                path.display(),
                meta.schema
            );
        }
        Ok(BuildAttempt { id, dir, meta })
    }

    /// Create a fresh attempt directory and write its metadata.
    pub fn create(dir: PathBuf, meta: BuildMeta) -> Res<BuildAttempt> {
        fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        let attempt = BuildAttempt { id: meta.attempt_id, dir, meta };
        attempt.store_meta()?;
        Ok(attempt)
    }

    pub fn store_meta(&self) -> Res<()> {
        write_toml(&Self::meta_path(&self.dir), &self.meta)
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

/// Parse a `%04d` attempt directory name (>=4 ASCII digits, like
/// `sim::restart`'s `output-%04d` parser minus the prefix — a build attempt
/// dir carries no other name component).
fn parse_attempt_dir_name(name: &str) -> Option<u32> {
    if name.len() < 4 || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::restart::{freeze_vars, thaw_vars};
    use crate::template::VarSet;

    fn sample_meta() -> BuildMeta {
        let mut vars = VarSet::new();
        vars.set("MAKEJOBS", 4i64);
        vars.set("SOURCEBASEDIR", "/home/user/Cactus");

        BuildMeta {
            schema: SCHEMA,
            attempt_id: 0,
            config: "sim".to_owned(),
            optionlist_source: OptionlistSource::Variant("generic".to_owned()),
            machine: "mike".to_owned(),
            alias: "et-dev".to_owned(),
            config_dir: PathBuf::from("/home/user/Cactus/configs/sim"),
            cactus_root: PathBuf::from("/home/user/Cactus"),
            install_root: PathBuf::from("/home/user/et"),
            submitted: false,
            job_id: "12345".to_owned(),
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
            virtual_executable: None,
            universe: Some(UniverseSpec {
                name: "et-sif".to_owned(),
                wrapper_argv: Some(vec!["singularity".to_owned(), "exec".to_owned()]),
                wrapper: None,
            }),
            config_meta: sample_config_meta(),
            vars: freeze_vars(&vars),
            timestamps: Timestamps {
                created: Some(Utc::now()),
                submitted: None,
                started: None,
                finished: None,
            },
            outcome: None,
        }
    }

    fn sample_config_meta() -> ConfigMeta {
        toml::from_str(
            r#"
            schema = 1
            name = "sim"
            variant = "generic"
            thornlist = "sim.th"
            machine = "mike"
            coerce-run-universe = true
            config-id = "cfg-1"
            build-id = "build-1"
            "#,
        )
        .unwrap()
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
        let thawed = thaw_vars(&reopened.meta.vars).unwrap();
        let original = thaw_vars(&meta.vars).unwrap();
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
        fs::create_dir_all(&dir).unwrap();
        let mut meta = sample_meta();
        meta.schema = SCHEMA + 1000;
        fs::write(BuildAttempt::meta_path(&dir), toml::to_string_pretty(&meta).unwrap()).unwrap();

        let err = format!("{:#}", BuildAttempt::open(config_dir, 0).unwrap_err());
        assert!(err.contains("please upgrade cactup"), "unexpected error: {err}");
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
