//! The shared build cache (spec D15, §18): cactup stands in front of
//! Cactus's object compiles as a compiler wrapper (see [`wrapper`]), so an
//! object some installation of this instance already built need not be
//! compiled again.
//!
//! This module is the build-side plumbing. `prepare` calls [`stage`] to
//! freeze one build's cache settings into its attempt directory; the build
//! script then asks [`probe`] whether the wrapper can be used where the
//! build actually runs and, if so, points `make` at the makefile fragment
//! the probe wrote. Nothing here touches the optionlist or `config-data`:
//! the compilers a configuration records stay the real ones, so a `make`
//! run by hand later builds exactly as it would without cactup.
//!
//! Three of the §18.1 rules already bind everything in this module:
//!
//! - **Fail open.** A build must never fail, or compile anything differently,
//!   because of the cache. Whatever the wrapper or the probe cannot do, or
//!   does not fully understand, ends in the real compiler running exactly as
//!   `make` asked for it.
//! - **Hermetic on the compute node (D11).** The wrapper and the probe read
//!   only the attempt's own frozen [`BuildConf`] and the configuration
//!   directory named in it: never the global DB, the registry, the MDB or
//!   knobs.
//! - **Quiet.** The wrapper's stdout and stderr are the compiler's. It adds
//!   nothing of its own to a compile that runs.

pub mod compile;
pub mod environment;
pub mod event;
pub mod fortran;
pub mod hash;
pub mod identity;
pub mod key;
pub mod lookup;
pub mod platform;
pub mod probe;
pub mod specs;
pub mod store;
pub mod upkeep;
pub mod wrapper;

use crate::build::sh_quote;
use crate::database::Database;
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// The verb the injected makefile fragment runs the wrapper by:
/// `cactup __cc <config.toml> <compiler> <shell> <args…>`. Dispatched before
/// clap ever sees the command line, so it is in no help output.
pub const WRAP_VERB: &str = "__cc";

/// The verb the build script runs the probe by: `cactup __cc-probe
/// <config.toml>`.
pub const PROBE_VERB: &str = "__cc-probe";

/// The probe's exit status for "the cache cannot be used here, and the
/// reason is already printed" — as opposed to the probe not running at all
/// (126/127 from the shell), which the build script reports itself.
pub const PROBE_DECLINED: i32 = 3;

/// What the cache does for a build (knob `build-cache`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Not involved: the build script is exactly what it is without the cache.
    #[default]
    Off,
    /// Wrap the compilers, work out the key each compile would be cached
    /// under, and log it; but serve nothing and store nothing.
    Record,
    /// Serve what the store has, and publish what is compiled (§18.8).
    Serve,
    /// Serve, but check every hit by compiling anyway (§18.8).
    Audit,
}

impl Mode {
    const ALL: [Self; 4] = [Self::Off, Self::Record, Self::Serve, Self::Audit];

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Record => "record",
            Self::Serve => "serve",
            Self::Audit => "audit",
        }
    }

    pub fn parse(s: &str) -> Res<Self> {
        match Self::ALL.into_iter().find(|mode| mode.name() == s) {
            Some(mode) => Ok(mode),
            None => bail!("invalid build-cache value \"{s}\" (valid: off, record, serve, audit)"),
        }
    }

    /// Does this mode use the store?
    pub fn serves(self) -> bool {
        matches!(self, Self::Serve | Self::Audit)
    }

    /// The effective `build-cache` setting, read leniently like
    /// `update::autoupdate`: a stored value that no longer parses means
    /// `off` rather than a failed build. Resolved on the login node only
    /// (D11): [`stage`] freezes it.
    pub fn from_db(db: &Database) -> Self {
        db.knob_or_default("build-cache").and_then(|v| Self::parse(&v).ok()).unwrap_or_default()
    }
}

/// Knob validator (§5): `build-cache` stores the name form.
pub fn validate_mode(value: &str) -> Res<String> {
    Ok(Mode::parse(value.trim())?.name().to_owned())
}

/// Knob validator (§5): `build-cache-size`, a size (`200G`): above it, a
/// serving build says so (§18.9).
pub fn validate_size(value: &str) -> Res<String> {
    let value = value.trim();
    upkeep::parse_size(value).map_err(|e| anyhow::anyhow!("invalid build-cache-size: {e:#}"))?;
    Ok(value.to_owned())
}

/// The effective `build-cache-size`, read leniently: anything that is not a
/// size means none. Resolved on the login node only (D11).
pub fn size_limit_from_db(db: &Database) -> Option<u64> {
    db.knob("build-cache-size").and_then(|value| upkeep::parse_size(value).ok())
}

/// Knob validator (§5): `build-cache-relocate` is `yes` or `no`.
pub fn validate_relocate(value: &str) -> Res<String> {
    match value.trim() {
        "yes" => Ok("yes".to_owned()),
        "no" => Ok("no".to_owned()),
        other => bail!("invalid build-cache-relocate value \"{other}\" (valid: yes, no)"),
    }
}

/// The effective `build-cache-relocate`, read leniently: anything but `no`
/// is the default, `yes`. Resolved on the login node only (D11).
pub fn relocate_from_db(db: &Database) -> bool {
    db.knob("build-cache-relocate") != Some("no")
}

/// The store's directory in an install home: beside the installations,
/// whose builds already do their I/O there, and hidden, as no alias is.
const STORE_NAME: &str = ".cactup-build-cache";

/// Where the store is (§18.7), and what said so, for people to read.
pub struct StoreRoot {
    pub path: PathBuf,
    pub from: String,
    /// A place that could not be resolved on the way (an `@ENV(…)@` unset
    /// here): passed over for the next, and said.
    pub warnings: Vec<String>,
}

/// The store's root (§18.7): the `build-cache-home` knob, else the
/// machine's `[paths] build-cache-home`, else `.cactup-build-cache` in the
/// install home (the `install-home` knob, else the machine's, else
/// `$CACTUP_HOME/cacti`). Knobs are read leniently like [`Mode::from_db`] (a
/// value that is not an absolute path is no value), and a machine's place
/// that cannot be resolved here is passed over with a warning, never a
/// reason to fail the build. Resolved on the login node only (D11):
/// [`stage`] freezes it.
pub fn store_root(db: &Database, machine: Option<&crate::mdb::Machine>) -> StoreRoot {
    let mut warnings = Vec::new();
    for key in ["build-cache-home", "install-home"] {
        let knob = db.knob(key).filter(|value| Path::new(value).is_absolute());
        let (place, from) = match (knob, machine) {
            (Some(knob), _) => (Ok(Some(knob.to_owned())), format!("the {key} knob")),
            (None, Some(machine)) => (machine.meta.path_for(db, key), format!("machine {}'s {key}", machine.name)),
            (None, None) => continue,
        };
        match place {
            Ok(Some(place)) if Path::new(&place).is_absolute() => {
                return match key {
                    "install-home" => StoreRoot {
                        path: Path::new(&place).join(STORE_NAME),
                        from: format!("beside the installations, {from}"),
                        warnings,
                    },
                    _ => StoreRoot { path: PathBuf::from(place), from, warnings },
                };
            }
            Ok(Some(place)) => warnings.push(format!("{from} \"{place}\" is not an absolute path")),
            Ok(None) => {}
            Err(e) => warnings.push(format!("{e:#}")),
        }
    }
    StoreRoot {
        path: crate::CACTUP_ROOT.join("cacti").join(STORE_NAME),
        from: "beside the installations, by default".to_owned(),
        warnings,
    }
}

/// One build's cache settings (§18.2), frozen by [`stage`] as
/// `<attempt>/cc/config.toml` and read back by the probe and by every
/// wrapper invocation of that build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BuildConf {
    pub mode: Mode,
    /// The cactup that wraps this build's compiles: the versioned binary
    /// (`freeze::frozen_cactup`), so an update mid-build changes nothing.
    pub cactup: PathBuf,
    pub config_dir: PathBuf,
    pub cactus_root: PathBuf,
    /// The cactup machine this build was prepared for. Objects are keyed by
    /// it, so two machines sharing one filesystem never share objects.
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universe: Option<String>,
    /// SHA-256 of the build-phase environment setup the build script runs:
    /// an edit to a machine's modules keys every object differently.
    pub build_env_digest: String,
    /// The store's root (§18.7, [`store_root`]).
    pub store: PathBuf,
    /// Are keys made with the path map where it holds (§18.8, knob
    /// `build-cache-relocate`)?
    pub relocate: bool,
    /// Above this many bytes, the store is said to be large (§18.9, knob
    /// `build-cache-size`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_limit: Option<u64>,
}

impl BuildConf {
    /// Read a frozen configuration. Any failure is the caller's cue to leave
    /// the cache out of it.
    pub fn load(path: &Path) -> Res<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
    }
}

/// `<attempt>/cc/config.toml`.
pub fn conf_path(cc_dir: &Path) -> PathBuf {
    cc_dir.join("config.toml")
}

/// The makefile fragment that puts the wrapper in front of the compilers,
/// written by the probe and read by `make` through `MAKEFILES`.
pub fn inject_path(cc_dir: &Path) -> PathBuf {
    cc_dir.join("inject.mk")
}

/// Two throwaway makefiles the build script runs to check that this `make`
/// takes the fragment the way it is meant.
pub fn selftest_dir(cc_dir: &Path) -> PathBuf {
    cc_dir.join("selftest")
}

/// One line per wrapped compile.
pub fn events_path(cc_dir: &Path) -> PathBuf {
    cc_dir.join("events.jsonl")
}

/// What a serving or auditing build does after its build step, whether it
/// succeeded or not (§18.9): record the keys it found in the store, so that
/// `cache gc` knows they are in use, and say in one line if the store is
/// larger than `build-cache-size`. Best-effort: a failure costs a line, never
/// the build. Reads the attempt's own frozen settings and the store, nothing
/// else (D11).
pub fn after_build(cc_dir: &Path) {
    use colored::Colorize;
    let Ok(conf) = BuildConf::load(&conf_path(cc_dir)) else { return };
    if !conf.mode.serves() {
        return;
    }
    let Ok(store) = store::Store::new(&conf.store, &conf.machine) else { return };
    let events = event::read(&events_path(cc_dir)).map(|(events, _)| events).unwrap_or_default();
    let keys: Vec<String> =
        events.iter().filter(|e| e.outcome == Some(event::Outcome::Hit)).filter_map(|e| e.key.clone()).collect();
    if let Err(e) = upkeep::log_use(&store, &keys) {
        eprintln!("{} build cache: could not record which entries this build used: {e:#}", "warning:".yellow().bold());
    }
    if let Some(limit) = conf.size_limit {
        // Never a walk of the store inside a build: the size last measured,
        // plus what this build added.
        let published: u64 = events.iter().filter(|e| e.published == Some(true)).filter_map(|e| e.object_bytes).sum();
        match upkeep::add_to_size(&conf.store, published) {
            Ok(None) => println!(
                "{} build-cache-size is set, but the size of the build cache in {} is not known yet; \
                 `cactup cache stats` measures it",
                "note:".bold(),
                conf.store.display(),
            ),
            Ok(Some(bytes)) if bytes > limit => println!(
                "{} the build cache in {} holds about {}, more than build-cache-size ({}); \
                 `cactup cache gc --unused-for 30d` removes what no build has used in 30 days",
                "note:".bold(),
                conf.store.display(),
                upkeep::human(bytes),
                upkeep::human(limit),
            ),
            Ok(_) => {}
            Err(e) => eprintln!("{} build cache: could not keep the size of {}: {e:#}", "warning:".yellow().bold(), conf.store.display()),
        }
    }
}

/// What `prepare` knows about the build it is staging.
pub struct StageInputs<'a> {
    pub cactup: &'a str,
    pub config_dir: &'a Path,
    pub cactus_root: &'a Path,
    pub machine: &'a str,
    pub universe: Option<&'a str>,
    pub build_env: &'a str,
    pub store: &'a Path,
    pub relocate: bool,
    pub size_limit: Option<u64>,
}

/// A build with the cache staged: the shell text `prepare` splices into the
/// build script.
#[derive(Debug)]
pub struct Staged {
    mode: Mode,
    cactup: PathBuf,
    cc_dir: PathBuf,
    config_dir: PathBuf,
}

/// Freeze `mode` for one build into `cc_dir` (`<attempt>/cc`). `None` when
/// the cache is off: nothing is written, and the build script comes out
/// byte for byte what it is without this module.
pub fn stage(cc_dir: &Path, mode: Mode, inputs: &StageInputs) -> Res<Option<Staged>> {
    if mode == Mode::Off {
        return Ok(None);
    }
    let conf = BuildConf {
        mode,
        cactup: PathBuf::from(inputs.cactup),
        config_dir: inputs.config_dir.to_owned(),
        cactus_root: inputs.cactus_root.to_owned(),
        machine: inputs.machine.to_owned(),
        universe: inputs.universe.map(str::to_owned),
        build_env_digest: hash::bytes_digest(inputs.build_env.as_bytes()),
        store: inputs.store.to_owned(),
        relocate: inputs.relocate,
        size_limit: inputs.size_limit,
    };
    fs::create_dir_all(cc_dir).with_context(|| format!("Failed to create {}", cc_dir.display()))?;
    let path = conf_path(cc_dir);
    let text = toml::to_string(&conf).context("Failed to serialize the build-cache configuration")?;
    fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(Some(Staged { mode, cactup: conf.cactup, cc_dir: cc_dir.to_owned(), config_dir: conf.config_dir }))
}

impl Staged {
    /// The build-script step that decides, where the build actually runs,
    /// whether the wrapper is used: it leaves `CACTUP_CC_MAKEFILES` naming
    /// the injection fragment, or empty with one line on stderr saying why
    /// not. It can only ever turn the cache off, never fail the build —
    /// inside a container universe the cactup binary or the attempt
    /// directory may simply not be visible.
    ///
    /// The self-test runs the build's own frozen `make` command three times
    /// under the fragment: once where and how Cactus's object sub-makes run
    /// (in the configuration's `build` directory, with `CCTK_TARGET` set),
    /// and twice as other makes below the build do — in that directory
    /// without `CCTK_TARGET`, and with it somewhere else — so that each
    /// half of the fragment's guard is tried on its own (see
    /// `probe::selftest_wrapped_mk`). Each run leaves a file behind when
    /// all its checks ran and passed, and those three files are the pass: a
    /// make that exits 0 having done nothing proves nothing. The output
    /// goes to `<attempt>/cc/selftest.log`.
    pub fn probe_step(&self, make: &str) -> String {
        let cactup = sh_quote(&self.cactup);
        let inject = sh_quote(&inject_path(&self.cc_dir));
        let log = sh_quote(&self.cc_dir.join("selftest.log"));
        format!(
            "CACTUP_CC_MAKEFILES=\n\
             if {cactup} {PROBE_VERB} {conf}; then\n\
             \x20 if ( MAKEFILES={inject}; export MAKEFILES\n\
             \x20      cd {build} && {make} -s -f {wrapped} all CCTK_TARGET=cactup-selftest SRCDIR=. &&\n\
             \x20      {make} -s -f {untouched} all RUN=elsewhere SRCDIR=. &&\n\
             \x20      cd {cc} && {make} -s -f {untouched} all RUN=untouched CCTK_TARGET=cactup-selftest SRCDIR=. ) > {log} 2>&1 &&\n\
             \x20    [ -f {wrapped_passed} ] && [ -f {elsewhere_passed} ] && [ -f {untouched_passed} ]; then\n\
             \x20   CACTUP_CC_MAKEFILES={inject}\n\
             \x20 else\n\
             \x20   echo 'cactup: build cache off for this build: its self-test failed with this make (see '{log}')' >&2\n\
             \x20 fi\n\
             else\n\
             \x20 cactup_cc_status=$?\n\
             \x20 [ $cactup_cc_status -eq {PROBE_DECLINED} ] || echo 'cactup: build cache off for this build: '{cactup}\" could not check it here (exit status $cactup_cc_status)\" >&2\n\
             fi",
            conf = sh_quote(&conf_path(&self.cc_dir)),
            build = sh_quote(&self.config_dir.join("build")),
            cc = sh_quote(&self.cc_dir),
            wrapped = sh_quote(&probe::selftest_wrapped(&self.cc_dir)),
            untouched = sh_quote(&probe::selftest_untouched(&self.cc_dir)),
            wrapped_passed = sh_quote(&probe::selftest_passed(&self.cc_dir, "wrapped")),
            elsewhere_passed = sh_quote(&probe::selftest_passed(&self.cc_dir, "elsewhere")),
            untouched_passed = sh_quote(&probe::selftest_passed(&self.cc_dir, "untouched")),
        )
    }

    /// The build step itself: `make <target>`, reading the injection
    /// fragment when the probe step allowed it. The fragment goes on in a
    /// subshell, so it applies to this one `make` and to nothing after it,
    /// whatever shell syntax the machine's `make` command is.
    ///
    /// A build that went through the cache ends with one line saying how
    /// many compiles did: the fragment acts only where it recognizes
    /// Cactus's object sub-makes, and if it recognized none, a build that
    /// compiled a lot and recorded nothing must not look like one that
    /// worked.
    pub fn build_step(&self, make: &str, target: &str) -> String {
        let events = sh_quote(&events_path(&self.cc_dir));
        // Counted from the event log, one line per compile; a field is
        // counted by its spelling in the log's compact JSON.
        let count = |field: &str| format!("$(grep -c '{field}' {events} 2>/dev/null)");
        // The spellings are `event::Outcome`'s and `event::Audit`'s.
        let summary = match self.mode {
            Mode::Serve => format!(
                "\"cactup: build cache: $cactup_cc_count compiles, {} served from the cache, {} published, {} could not be (see cactup cache report)\"",
                count("\"outcome\":\"hit\""),
                count("\"published\":true"),
                count("\"published\":false"),
            ),
            Mode::Audit => format!(
                "\"cactup: build cache: $cactup_cc_count compiles, {} checked against the cache ({} wrong, {} failing to compile, {} not deterministic, {} with inputs that changed), {} published\"",
                count("\"outcome\":\"hit\""),
                count("\"audit\":\"wrong-"),
                // `compile-failed` and `second-compile-failed`.
                count("compile-failed\""),
                count("\"audit\":\"not-deterministic\""),
                count("\"audit\":\"inputs-changed\""),
                count("\"published\":true"),
            ),
            _ => "\"cactup: build cache: compiles recorded: $cactup_cc_count\"".to_owned(),
        };
        format!(
            "if [ -n \"$CACTUP_CC_MAKEFILES\" ]; then\n\
             \x20 ( MAKEFILES=\"${{MAKEFILES:+$MAKEFILES }}$CACTUP_CC_MAKEFILES\"; export MAKEFILES; {make} {target} )\n\
             \x20 cactup_cc_count=$(( $(cat {events} 2>/dev/null | wc -l) ))\n\
             \x20 if [ $cactup_cc_count -gt 0 ]; then\n\
             \x20   echo {summary}\n\
             \x20 else\n\
             \x20   echo 'cactup: build cache: no compile recorded (nothing needed compiling, or the cache did not apply to this build)'\n\
             \x20 fi\n\
             else\n\
             \x20 {make} {target}\n\
             fi"
        )
    }
}

#[cfg(test)]
/// Make the file at `path` executable, as a fresh file that a child
/// process (`install`) wrote. A test that writes an executable and runs it
/// at once fails now and then with "Text file busy": another test thread
/// that forks in between hands its child this process's open handle on the
/// file, and the kernel will not run a file open for writing. A file only
/// a child process ever had open for writing has no such handle here.
pub(crate) fn make_executable(path: &Path) {
    let fresh = path.with_extension("cactup-fresh");
    let installed = std::process::Command::new("install").arg("-m").arg("755").arg(path).arg(&fresh).status().unwrap();
    assert!(installed.success(), "install {}", path.display());
    fs::rename(&fresh, path).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `prepare` hands [`stage`], for a build of the configuration at
    /// `config_dir` wrapped by `cactup`.
    pub(super) fn inputs<'a>(cactup: &'a str, config_dir: &'a Path) -> StageInputs<'a> {
        StageInputs {
            cactup,
            config_dir,
            cactus_root: Path::new("/work/Cactus"),
            machine: "mel5",
            universe: Some("host"),
            build_env: "module load gcc\n",
            store: Path::new("/work/cache"),
            relocate: true,
            size_limit: None,
        }
    }

    /// After a serving build: the keys it found are logged, the size it
    /// added is kept; a recording build does neither.
    #[test]
    fn after_a_serving_build_its_uses_and_size_are_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc");
        let store = tmp.path().join("store");
        let mut stage_inputs = inputs("/opt/cactup/bin/cactup-abc1234", tmp.path());
        stage_inputs.store = &store;
        stage_inputs.size_limit = Some(1);
        stage(&cc, Mode::Serve, &stage_inputs).unwrap();
        let (hit, miss) = ("a".repeat(64), "b".repeat(64));
        for event in [
            event::Event { key: Some(hit.clone()), outcome: Some(event::Outcome::Hit), ..Default::default() },
            event::Event { key: Some(miss), outcome: Some(event::Outcome::Miss), published: Some(true), object_bytes: Some(4096), ..Default::default() },
        ] {
            event.append(&events_path(&cc));
        }
        after_build(&cc);
        let used = store.join(format!("v{}/mel5/used", store::FORMAT));
        let logs: Vec<_> = fs::read_dir(&used).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(logs.len(), 1);
        assert_eq!(fs::read_to_string(&logs[0]).unwrap(), format!("{hit}\n"));
        // Never measured: the build says so, and writes nothing that would
        // pass for a measurement.
        assert!(!store.join(format!("v{}/size", store::FORMAT)).exists());

        // A recording build: nothing.
        let record = tmp.path().join("record");
        stage(&record, Mode::Record, &stage_inputs).unwrap();
        event::Event { key: Some("c".repeat(64)), outcome: Some(event::Outcome::Hit), ..Default::default() }.append(&events_path(&record));
        after_build(&record);
        assert_eq!(fs::read_dir(&used).unwrap().count(), 1);
    }

    #[test]
    fn mode_knob_accepts_exactly_its_names() {
        assert_eq!(validate_mode(" record ").unwrap(), "record");
        assert_eq!(validate_mode("off").unwrap(), "off");
        let err = validate_mode("on").unwrap_err().to_string();
        assert!(err.contains("valid: off, record, serve, audit"), "{err}");
        assert_eq!(validate_mode("serve").unwrap(), "serve");
        assert_eq!(validate_relocate(" no ").unwrap(), "no");
        assert!(validate_relocate("off").is_err());
    }

    /// The knob, then the machine's place, then beside the installations.
    #[test]
    fn the_store_root_is_the_knobs_then_the_machines_then_the_install_homes() {
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent"),
        );
        let machine = |paths: &str| {
            let mut machine = mdb.load("generic").unwrap();
            machine.name = "m".to_owned();
            machine.meta.paths = toml::from_str(paths).unwrap();
            machine
        };
        let mut db = Database::new();
        let default = crate::CACTUP_ROOT.join("cacti").join(STORE_NAME);
        let root = store_root(&db, None);
        assert_eq!((root.path, root.warnings.len()), (default.clone(), 0));
        assert_eq!(store_root(&db, Some(&machine(""))).path, default);
        let installs = machine("install-home = \"/work/u\"");
        let root = store_root(&db, Some(&installs));
        assert_eq!(root.path, PathBuf::from("/work/u").join(STORE_NAME));
        assert_eq!(root.from, "beside the installations, machine m's install-home");
        let both = machine("install-home = \"/work/u\"\nbuild-cache-home = \"/project/u/cache\"");
        assert_eq!(store_root(&db, Some(&both)).path, PathBuf::from("/project/u/cache"));
        // A machine's place that cannot be resolved here is passed over,
        // and said; so is one that is not an absolute path.
        let unset = machine(
            "install-home = \"/work/u\"\nbuild-cache-home = \"@ENV(CACTUP_SURELY_UNSET_VARIABLE)@/cache\"",
        );
        let root = store_root(&db, Some(&unset));
        assert_eq!(root.path, PathBuf::from("/work/u").join(STORE_NAME));
        assert!(root.warnings[0].contains("build-cache-home"), "{:?}", root.warnings);
        let relative = machine("build-cache-home = \"cache\"");
        let root = store_root(&db, Some(&relative));
        assert_eq!(root.path, default);
        assert!(root.warnings[0].contains("not an absolute path"), "{:?}", root.warnings);
        // The install-home knob moves the default, the build-cache-home
        // knob wins over everything; a relative knob is no knob.
        db.set_knob("install-home", "/fast/u".to_owned());
        assert_eq!(store_root(&db, Some(&installs)).path, PathBuf::from("/fast/u").join(STORE_NAME));
        assert_eq!(store_root(&db, None).path, PathBuf::from("/fast/u").join(STORE_NAME));
        db.set_knob("build-cache-home", "relative".to_owned());
        assert_eq!(store_root(&db, Some(&both)).path, PathBuf::from("/project/u/cache"));
        db.set_knob("build-cache-home", "/mine".to_owned());
        let root = store_root(&db, Some(&both));
        assert_eq!((root.path, root.from.as_str()), (PathBuf::from("/mine"), "the build-cache-home knob"));
    }

    #[test]
    fn off_stages_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc");
        let staged = stage(&cc, Mode::Off, &inputs("/opt/cactup/bin/cactup-abc1234", tmp.path())).unwrap();
        assert!(staged.is_none());
        assert!(!cc.exists(), "an off build must not even create the directory");
    }

    #[test]
    fn staged_configuration_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc");
        let config = tmp.path().join("Cactus/configs/sim");
        stage(&cc, Mode::Record, &inputs("/opt/cactup/bin/cactup-abc1234", &config)).unwrap().unwrap();

        let conf = BuildConf::load(&conf_path(&cc)).unwrap();
        assert_eq!(
            conf,
            BuildConf {
                mode: Mode::Record,
                cactup: PathBuf::from("/opt/cactup/bin/cactup-abc1234"),
                config_dir: config,
                cactus_root: PathBuf::from("/work/Cactus"),
                machine: "mel5".to_owned(),
                universe: Some("host".to_owned()),
                build_env_digest: hash::bytes_digest(b"module load gcc\n"),
                store: PathBuf::from("/work/cache"),
                relocate: true,
                size_limit: None,
            }
        );
        assert!(BuildConf::load(&cc.join("missing.toml")).is_err());
    }

    #[test]
    fn the_script_steps_quote_their_paths() {
        let staged = Staged {
            mode: Mode::Record,
            cactup: PathBuf::from("/opt/it's here/cactup-abc"),
            cc_dir: PathBuf::from("/work/cfg/.cactup-builds/0003/cc"),
            config_dir: PathBuf::from("/work/cfg"),
        };
        let probe = staged.probe_step("make -j8");
        assert!(
            probe.contains(r"'/opt/it'\''s here/cactup-abc' __cc-probe '/work/cfg/.cactup-builds/0003/cc/config.toml'"),
            "{probe}"
        );
        assert!(
            probe.contains(
                "cd '/work/cfg/build' && make -j8 -s -f '/work/cfg/.cactup-builds/0003/cc/selftest/wrapped.mk' all \
                 CCTK_TARGET=cactup-selftest SRCDIR=. &&"
            ),
            "{probe}"
        );
        assert!(probe.contains("[ $cactup_cc_status -eq 3 ] || echo"), "{probe}");

        let build = staged.build_step("make -j8", "sim");
        assert!(build.contains("export MAKEFILES; make -j8 sim )"), "{build}");
        assert!(build.contains("$(cat '/work/cfg/.cactup-builds/0003/cc/events.jsonl' 2>/dev/null | wc -l)"), "{build}");
        assert!(build.trim_end().ends_with("fi"), "{build}");
    }

    /// What the two script steps did, run the way the build script runs them.
    struct Ran {
        stdout: String,
        stderr: String,
        success: bool,
    }

    /// Run the two script steps under `set -e` with stand-ins: `probe` is the
    /// body of a script standing in for cactup (`None`: a binary that is not
    /// there), `selftest_make` is the self-test's make, and the build's
    /// "make" is `build_make`.
    fn run_steps(probe: Option<&str>, selftest_make: &str, build_make: &str, makefiles: Option<&str>) -> Ran {
        let tmp = tempfile::tempdir().unwrap();
        let cactup = tmp.path().join("cactup-abc");
        if let Some(body) = probe {
            fs::write(&cactup, format!("#!/bin/sh\n{body}\n")).unwrap();
            crate::objcache::make_executable(&cactup);
        }
        let config_dir = tmp.path().join("cfg");
        let cc_dir = config_dir.join(".cactup-builds/0000/cc");
        fs::create_dir_all(selftest_dir(&cc_dir)).unwrap();
        fs::create_dir_all(config_dir.join("build")).unwrap();
        let inject = inject_path(&cc_dir).display().to_string();
        let staged = Staged { mode: Mode::Record, cactup, cc_dir, config_dir };
        let script = format!(
            "set -e\n{}\n{}\necho after the build\n",
            staged.probe_step(selftest_make),
            staged.build_step(build_make, "sim"),
        );
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script).env_remove("MAKEFILES");
        if let Some(makefiles) = makefiles {
            cmd.env("MAKEFILES", makefiles);
        }
        let out = cmd.output().unwrap();
        Ran {
            stdout: String::from_utf8_lossy(&out.stdout).replace(&inject, "<inject.mk>"),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            success: out.status.success(),
        }
    }

    /// A self-test "make" that passes: like the real one running a
    /// self-test makefile to the end, it leaves the run's `.passed` file
    /// (named by `RUN=`, else after the makefile).
    const PASSES: &str = r#"sh -c 'run=$(basename "$3" .mk); for a; do case $a in RUN=*) run=${a#RUN=};; esac; done; : > "$(dirname "$3")/$run.passed"' --"#;

    /// A build "make" that prints the `MAKEFILES` it was given.
    const SHOW: &str = r#"sh -c 'echo "make $1 with [$MAKEFILES]"' --"#;
    /// What the script says after a build in which no compile was recorded
    /// (the stand-in make compiles nothing).
    const NONE_RECORDED: &str = "cactup: build cache: no compile recorded \
                                 (nothing needed compiling, or the cache did not apply to this build)\n";

    #[test]
    fn the_build_reads_the_fragment_only_when_probe_and_selftest_both_pass() {
        let ran = run_steps(Some("exit 0"), PASSES, SHOW, None);
        assert_eq!(ran.stdout, format!("make sim with [<inject.mk>]\n{NONE_RECORDED}after the build\n"));
        assert_eq!(ran.stderr, "");

        // A MAKEFILES the user already had stays in front.
        let ran = run_steps(Some("exit 0"), PASSES, SHOW, Some("/home/me/extra.mk"));
        assert_eq!(ran.stdout, format!("make sim with [/home/me/extra.mk <inject.mk>]\n{NONE_RECORDED}after the build\n"));

        // A build whose compiles were logged says how many.
        let log_two = r#"sh -c 'printf "{}\n{}\n" >> "$(dirname "$MAKEFILES")/events.jsonl"' --"#;
        let ran = run_steps(Some("exit 0"), PASSES, log_two, None);
        assert_eq!(ran.stdout, "cactup: build cache: compiles recorded: 2\nafter the build\n");
    }

    /// A serving build ends its compile step with what the cache did, counted
    /// from the event log.
    #[test]
    fn a_serving_build_says_what_was_served_and_published() {
        let log = r#"sh -c 'events="$(dirname "$MAKEFILES")/events.jsonl"; printf "%s\n" "{\"outcome\":\"hit\"}" "{\"outcome\":\"miss\",\"published\":true}" "{\"outcome\":\"hit\",\"audit\":\"second-compile-failed\"}" "{}" >> "$events"' --"#;
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("cfg");
        let cc_dir = config_dir.join(".cactup-builds/0000/cc");
        fs::create_dir_all(&cc_dir).unwrap();
        for (mode, line) in [
            (Mode::Serve, "cactup: build cache: 4 compiles, 2 served from the cache, 1 published, 0 could not be (see cactup cache report)"),
            (Mode::Audit, "cactup: build cache: 4 compiles, 2 checked against the cache (0 wrong, 1 failing to compile, 0 not deterministic, 0 with inputs that changed), 1 published"),
        ] {
            let _ = fs::remove_file(events_path(&cc_dir));
            let staged = Staged { mode, cactup: PathBuf::from("/bin/true"), cc_dir: cc_dir.clone(), config_dir: config_dir.clone() };
            let script = format!("CACTUP_CC_MAKEFILES={}\n{}\n", sh_quote(&inject_path(&cc_dir)), staged.build_step(log, "sim"));
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env_remove("MAKEFILES").output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{line}\n"), "{}", String::from_utf8_lossy(&out.stderr));
        }
    }

    #[test]
    fn every_way_the_probe_can_fail_leaves_a_plain_build_and_one_line() {
        // Declined: the probe has said why itself, the script adds nothing.
        let declined = format!("echo 'cactup: build cache off for this build: reasons' >&2; exit {PROBE_DECLINED}");
        let ran = run_steps(Some(&declined), "true", SHOW, None);
        assert_eq!(ran.stdout, "make sim with []\nafter the build\n");
        assert_eq!(ran.stderr, "cactup: build cache off for this build: reasons\n");

        // Not there at all (a container that does not see the binary).
        let ran = run_steps(None, "true", SHOW, None);
        assert_eq!(ran.stdout, "make sim with []\nafter the build\n");
        let ours: Vec<&str> = ran.stderr.lines().filter(|l| l.starts_with("cactup: ")).collect();
        assert_eq!(ours.len(), 1, "{}", ran.stderr);
        assert!(ours[0].ends_with("could not check it here (exit status 127)"), "{}", ran.stderr);

        // Crashed, or anything else that is not the probe's own "declined".
        let ran = run_steps(Some("exit 1"), "true", SHOW, None);
        assert_eq!(ran.stdout, "make sim with []\nafter the build\n");
        assert!(ran.stderr.trim_end().ends_with("could not check it here (exit status 1)"), "{}", ran.stderr);

        // The probe is fine but the self-test fails with this make — or
        // "succeeds" without having run: a make that exits 0 and did nothing
        // has shown nothing.
        for selftest_make in ["false", "true"] {
            let ran = run_steps(Some("exit 0"), selftest_make, SHOW, Some("/home/me/extra.mk"));
            assert_eq!(ran.stdout, "make sim with [/home/me/extra.mk]\nafter the build\n", "{selftest_make}");
            assert!(
                ran.stderr.starts_with("cactup: build cache off for this build: its self-test failed"),
                "{selftest_make}: {}",
                ran.stderr
            );
            assert_eq!(ran.stderr.lines().count(), 1, "{}", ran.stderr);
            assert!(ran.success);
        }
    }

    /// The probe step as the build script runs it, with the real `make` and
    /// the fragment and self-test makefiles the real probe wrote — then
    /// with the fragment broken in each way the self-test exists to catch.
    /// A self-test that passes a broken fragment is worse than none: it is
    /// the only judge for a `make` nobody has tried.
    #[test]
    fn the_selftest_passes_the_real_fragment_and_fails_every_broken_one() {
        // Under the make on PATH and under every make named in
        // CACTUP_TEST_MAKES (colon-separated paths), as in tests/objcache.rs.
        let listed = std::env::var_os("CACTUP_TEST_MAKES").unwrap_or_default();
        let mut makes: Vec<PathBuf> = std::env::split_paths(&listed).filter(|p| !p.as_os_str().is_empty()).collect();
        makes.push(PathBuf::from("make"));
        makes.retain(|make| std::process::Command::new(make).arg("--version").output().is_ok_and(|o| o.status.success()));
        if makes.is_empty() {
            eprintln!("skipped: no make on this host");
        }
        for make in makes {
            selftest_judges_fragments(&make.display().to_string());
        }
    }

    fn selftest_judges_fragments(make: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let config_dir = root.join("Cactus/configs/sim");
        fs::create_dir_all(config_dir.join("config-data")).unwrap();
        fs::write(
            config_dir.join("config-data/make.config.rules"),
            "define COMPILE_C\ncurrent_wd=`$(GET_WD)` ; cd $(SCRATCH_BUILD) ; $(CC) $(CFLAGS) -c -o $@ $<\nendef\n",
        )
        .unwrap();
        // Stands in for cactup where the script and the wrapped recipe run
        // it: a probe that has nothing more to do, and a wrapper that can
        // wrap the self-test's compile.
        let cactup = root.join("cactup-abc");
        fs::write(&cactup, "#!/bin/sh\ncase \"$1 $3\" in '__cc-probe '|'__cc cactup:selftest') exit 0;; esac\nexit 1\n")
            .unwrap();
        crate::objcache::make_executable(&cactup);
        let cc_dir = config_dir.join(".cactup-builds/0000/cc");
        let staged = stage(&cc_dir, Mode::Record, &inputs(cactup.to_str().unwrap(), &config_dir)).unwrap().unwrap();
        let inject = inject_path(&cc_dir);

        // The fragment, rewritten by `breakage`, as the probe step judges it.
        let judge = |breakage: &dyn Fn(String) -> String| {
            assert_eq!(probe::run(Some(conf_path(&cc_dir).into())), 0);
            let fragment = breakage(fs::read_to_string(&inject).unwrap());
            fs::write(&inject, fragment).unwrap();
            let script = format!("set -e\n{}\necho \"use=[$CACTUP_CC_MAKEFILES]\"\n", staged.probe_step(make));
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env_remove("MAKEFILES").output().unwrap();
            let log = fs::read_to_string(cc_dir.join("selftest.log")).unwrap_or_default();
            (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned(), log)
        };

        let (stdout, stderr, log) = judge(&|fragment| fragment);
        assert_eq!((stdout, stderr), (format!("use=[{}]\n", inject.display()), String::new()), "{make}: {log}");

        let own_rule = format!("{}: ;\n", inject.display());
        let breakages: [(&str, &dyn Fn(String) -> String); 7] = [
            ("its recipes do not win over the rules file's", &|f| f.replace("override define", "define")),
            ("it acts wherever CCTK_TARGET is set", &|f| {
                let guard = f.lines().find(|l| l.starts_with("ifneq ($(findstring |")).unwrap().to_owned();
                f.replace(&guard, "ifneq (anywhere,)")
            }),
            ("it acts in the build directory whatever make runs there", &|f| f.replace("ifdef CCTK_TARGET\n", "ifdef MAKE\n")),
            ("it stays in MAKEFILE_LIST", &|f| f.replace("MAKEFILE_LIST := ", "CACTUP_UNUSED := ")),
            ("it stays in MAKEFILES", &|f| f.replace("MAKEFILES := ", "CACTUP_UNUSED := ")),
            ("a forwarding rule is run for it", &|f| f.replace(&own_rule, "")),
            // As on a host without `grep`: every thorn would stand down.
            ("its scan of a thorn's make fragments cannot run", &|f| f.replace("grep -Eq", "cactup-no-such-grep -Eq")),
        ];
        for (what, breakage) in breakages {
            let (stdout, stderr, log) = judge(breakage);
            assert_eq!(stdout, "use=[]\n", "{make}: {what}: the self-test passed a broken fragment\n{log}");
            assert!(stderr.starts_with("cactup: build cache off for this build: its self-test failed"), "{what}: {stderr}");
        }
        // A make told to ignore errors must not turn a failure into a pass.
        for flag in ["-i", "-k"] {
            assert_eq!(probe::run(Some(conf_path(&cc_dir).into())), 0);
            fs::write(&inject, fs::read_to_string(&inject).unwrap().replace("override define", "define")).unwrap();
            let script = format!("set -e\n{}\necho \"use=[$CACTUP_CC_MAKEFILES]\"\n", staged.probe_step(&format!("{make} {flag}")));
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).env_remove("MAKEFILES").output().unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), "use=[]\n", "{make} {flag}");
        }
    }

    #[test]
    fn a_failing_build_step_still_stops_the_script() {
        for probe in ["exit 0", "exit 3"] {
            let ran = run_steps(Some(probe), "true", "false", None);
            assert!(!ran.success, "{probe}");
            assert_eq!(ran.stdout, "", "{probe}: nothing after a failed make may run");
        }
    }
}
