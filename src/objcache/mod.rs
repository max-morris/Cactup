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
//!   only the attempt's own frozen [`BuildConf`], the configuration
//!   directory and the cache root named in it: never the global DB, the
//!   registry, the MDB or knobs.
//! - **Quiet.** The wrapper's stdout and stderr are the compiler's. It adds
//!   nothing of its own to a compile that runs.

pub mod probe;
pub mod wrapper;

use crate::database::Database;
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// The verb the injected makefile fragment runs the wrapper by:
/// `cactup __cc <config.toml> <VAR>:<n> <compiler…> <args…>`. Dispatched
/// before clap ever sees the command line, so it is in no help output.
pub const WRAP_VERB: &str = "__cc";

/// The verb the build script runs the probe by: `cactup __cc-probe
/// <config.toml>`.
pub const PROBE_VERB: &str = "__cc-probe";

/// The file name under which cactup is the wrapper with no verb at all, for
/// callers that can only name a program (cargo's `RUSTC_WRAPPER`): `cactup-cc
/// <compiler> <args…>`, with the configuration named by [`CONF_ENV`].
pub const WRAP_ARGV0: &str = "cactup-cc";

/// Names the [`BuildConf`] for the [`WRAP_ARGV0`] form.
pub const CONF_ENV: &str = "CACTUP_CC_CONF";

/// The [`BuildConf`] format this binary writes and reads. A wrapper handed
/// any other version runs the real compiler: the cactup that staged a build
/// and the one wrapping its compiles are the same frozen binary, so a
/// mismatch means something is off, and guessing is not an option.
pub const CONF_VERSION: u32 = 1;

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
    /// Wrap the compilers and log what each compile would have been keyed
    /// by, but serve nothing and store nothing.
    Record,
}

impl Mode {
    const ALL: [Self; 2] = [Self::Off, Self::Record];

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Record => "record",
        }
    }

    pub fn parse(s: &str) -> Res<Self> {
        match Self::ALL.into_iter().find(|mode| mode.name() == s) {
            Some(mode) => Ok(mode),
            None => bail!("invalid build-cache value \"{s}\" (valid: off, record)"),
        }
    }
}

/// Knob validator (§5): `build-cache` stores the name form.
pub fn validate_mode(value: &str) -> Res<String> {
    Ok(Mode::parse(value.trim())?.name().to_owned())
}

/// Knob validator (§5): `build-cache-dir` is an absolute directory. It is
/// frozen into every build as written, and a build may run on another node
/// or in another working directory, where a relative path would name
/// somewhere else.
pub fn validate_dir(value: &str) -> Res<String> {
    let dir = crate::shell::expand_path(value.trim());
    if dir.is_empty() {
        bail!("build-cache-dir cannot be empty (`cactup knob delete build-cache-dir` restores the default)");
    }
    if !Path::new(&dir).is_absolute() {
        bail!("invalid build-cache-dir \"{value}\": expected an absolute path");
    }
    Ok(dir)
}

/// Where the cache lives unless `build-cache-dir` says otherwise.
pub fn default_dir() -> PathBuf {
    crate::CACTUP_ROOT.join("cache")
}

/// The cache settings in force for a build, resolved on the login node from
/// the knobs. They never reach the compute node as knobs (D11): [`stage`]
/// freezes them.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub mode: Mode,
    pub root: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self { mode: Mode::Off, root: default_dir() }
    }
}

impl Settings {
    /// Read leniently, like `update::autoupdate`: a stored value that no
    /// longer parses means the default rather than a failed build.
    pub fn from_db(db: &Database) -> Self {
        let mode = db
            .knob_or_default("build-cache")
            .and_then(|v| Mode::parse(&v).ok())
            .unwrap_or_default();
        let root = db
            .knob_or_default("build-cache-dir")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .unwrap_or_else(default_dir);
        Self { mode, root }
    }
}

/// One build's cache settings (§18.2), frozen by [`stage`] as
/// `<attempt>/cc/config.toml` and read back by the probe and by every
/// wrapper invocation of that build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BuildConf {
    pub version: u32,
    pub mode: Mode,
    /// The cactup that wraps this build's compiles: the versioned binary
    /// (`freeze::frozen_cactup`), so an update mid-build changes nothing.
    pub cactup: PathBuf,
    pub cache_root: PathBuf,
    pub cactus_root: PathBuf,
    pub config_dir: PathBuf,
    /// The cactup machine this build was prepared for. Objects are keyed by
    /// it, so two machines sharing one filesystem never share objects.
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universe: Option<String>,
    /// SHA-256 of the build-phase environment setup the build script runs:
    /// an edit to a machine's modules keys every object differently.
    pub build_env_digest: String,
}

impl BuildConf {
    /// Read and check a frozen configuration. Any failure — unreadable,
    /// unparsable, another [`CONF_VERSION`] — is the caller's cue to leave
    /// the cache out of it.
    pub fn load(path: &Path) -> Res<Self> {
        #[derive(Deserialize)]
        struct Versioned {
            version: u32,
        }
        let text = fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
        let Versioned { version } =
            toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;
        if version != CONF_VERSION {
            bail!(
                "{} is build-cache configuration version {version}, and this cactup reads version {CONF_VERSION}",
                path.display()
            );
        }
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
/// scopes the injected variables the way the fragment needs.
pub fn selftest_dir(cc_dir: &Path) -> PathBuf {
    cc_dir.join("selftest")
}

/// One line per wrapped compile.
pub fn events_path(cc_dir: &Path) -> PathBuf {
    cc_dir.join("events.jsonl")
}

/// The lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// What `prepare` knows about the build it is staging.
pub struct StageInputs<'a> {
    pub cactup: &'a str,
    pub cactus_root: &'a Path,
    pub config_dir: &'a Path,
    pub machine: &'a str,
    pub universe: Option<&'a str>,
    pub build_env: &'a str,
}

/// A build with the cache staged: the shell text `prepare` splices into the
/// build script.
#[derive(Debug)]
pub struct Staged {
    cactup: PathBuf,
    cc_dir: PathBuf,
}

/// Freeze `settings` for one build into `cc_dir` (`<attempt>/cc`). `None`
/// when the cache is off: nothing is written, and the build script comes out
/// byte for byte what it is without this module.
pub fn stage(cc_dir: &Path, settings: &Settings, inputs: &StageInputs) -> Res<Option<Staged>> {
    if settings.mode == Mode::Off {
        return Ok(None);
    }
    let conf = BuildConf {
        version: CONF_VERSION,
        mode: settings.mode,
        cactup: PathBuf::from(inputs.cactup),
        cache_root: settings.root.clone(),
        cactus_root: inputs.cactus_root.to_owned(),
        config_dir: inputs.config_dir.to_owned(),
        machine: inputs.machine.to_owned(),
        universe: inputs.universe.map(str::to_owned),
        build_env_digest: sha256_hex(inputs.build_env.as_bytes()),
    };
    fs::create_dir_all(cc_dir).with_context(|| format!("Failed to create {}", cc_dir.display()))?;
    let path = conf_path(cc_dir);
    let text = toml::to_string(&conf).context("Failed to serialize the build-cache configuration")?;
    fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(Some(Staged { cactup: conf.cactup, cc_dir: cc_dir.to_owned() }))
}

/// Single-quote `s` for `/bin/sh`.
fn sh_quote(s: &Path) -> String {
    format!("'{}'", s.display().to_string().replace('\'', "'\\''"))
}

impl Staged {
    /// The build-script step that decides, where the build actually runs,
    /// whether the wrapper is used: it leaves `CACTUP_CC_MAKEFILES` naming
    /// the injection fragment, or empty with one line on stderr saying why
    /// not. It can only ever turn the cache off, never fail the build —
    /// inside a container universe the cactup binary or the attempt
    /// directory may simply not be visible.
    ///
    /// `make` is the build's own frozen `make` command, so the self-test
    /// exercises the very `make` that will read the fragment.
    pub fn probe_step(&self, make: &str) -> String {
        let cactup = sh_quote(&self.cactup);
        let inject = sh_quote(&inject_path(&self.cc_dir));
        let guarded = sh_quote(&probe::selftest_guarded(&self.cc_dir));
        let unguarded = sh_quote(&probe::selftest_unguarded(&self.cc_dir));
        format!(
            "CACTUP_CC_MAKEFILES=\n\
             if {cactup} {PROBE_VERB} {conf}; then\n\
             \x20 if ( MAKEFILES={inject}; export MAKEFILES; {make} -s -f {guarded} && {make} -s -f {unguarded} ) >/dev/null 2>&1; then\n\
             \x20   CACTUP_CC_MAKEFILES={inject}\n\
             \x20 else\n\
             \x20   echo 'cactup: build cache off for this build: this make cannot limit the compiler wrapper to object rules (GNU make 3.82 or later is needed)' >&2\n\
             \x20 fi\n\
             else\n\
             \x20 cactup_cc_status=$?\n\
             \x20 [ $cactup_cc_status -eq {PROBE_DECLINED} ] || echo 'cactup: build cache off for this build: '{cactup}\" could not check it here (exit status $cactup_cc_status)\" >&2\n\
             fi",
            conf = sh_quote(&conf_path(&self.cc_dir)),
        )
    }

    /// The build step itself: `make <target>`, reading the injection
    /// fragment when the probe step allowed it. The fragment goes on in a
    /// subshell, so it applies to this one `make` and to nothing after it,
    /// whatever shell syntax the machine's `make` command is.
    pub fn build_step(&self, make: &str, target: &str) -> String {
        format!(
            "if [ -n \"$CACTUP_CC_MAKEFILES\" ]; then\n\
             \x20 ( MAKEFILES=\"${{MAKEFILES:+$MAKEFILES }}$CACTUP_CC_MAKEFILES\"; export MAKEFILES; {make} {target} )\n\
             else\n\
             \x20 {make} {target}\n\
             fi"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(root: &'a Path, config: &'a Path) -> StageInputs<'a> {
        StageInputs {
            cactup: "/opt/cactup/bin/cactup-abc1234",
            cactus_root: root,
            config_dir: config,
            machine: "mel5",
            universe: Some("host"),
            build_env: "module load gcc\n",
        }
    }

    #[test]
    fn mode_knob_accepts_exactly_its_names() {
        assert_eq!(validate_mode(" record ").unwrap(), "record");
        assert_eq!(validate_mode("off").unwrap(), "off");
        let err = validate_mode("on").unwrap_err().to_string();
        assert!(err.contains("valid: off, record"), "{err}");
    }

    #[test]
    fn dir_knob_requires_an_absolute_path() {
        assert_eq!(validate_dir("/scratch/me/cache").unwrap(), "/scratch/me/cache");
        assert!(validate_dir("cache").unwrap_err().to_string().contains("absolute"));
        assert!(validate_dir("  ").unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn off_stages_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc");
        let staged = stage(&cc, &Settings::default(), &inputs(tmp.path(), tmp.path())).unwrap();
        assert!(staged.is_none());
        assert!(!cc.exists(), "an off build must not even create the directory");
    }

    #[test]
    fn staged_configuration_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc");
        let settings = Settings { mode: Mode::Record, root: PathBuf::from("/scratch/cache") };
        let root = tmp.path().join("Cactus");
        let config = root.join("configs/sim");
        stage(&cc, &settings, &inputs(&root, &config)).unwrap().unwrap();

        let conf = BuildConf::load(&conf_path(&cc)).unwrap();
        assert_eq!(conf.mode, Mode::Record);
        assert_eq!(conf.cache_root, Path::new("/scratch/cache"));
        assert_eq!(conf.config_dir, config);
        assert_eq!(conf.machine, "mel5");
        assert_eq!(conf.universe.as_deref(), Some("host"));
        assert_eq!(conf.build_env_digest, sha256_hex(b"module load gcc\n"));
    }

    #[test]
    fn another_configuration_version_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        // Only the version is checked before the rest is even looked at: a
        // future format need not resemble this one.
        fs::write(&path, format!("version = {}\nshape = \"unknown\"\n", CONF_VERSION + 1)).unwrap();
        let err = BuildConf::load(&path).unwrap_err().to_string();
        assert!(err.contains("reads version"), "{err}");
    }

    #[test]
    fn the_script_steps_quote_their_paths() {
        let staged = Staged {
            cactup: PathBuf::from("/opt/it's here/cactup-abc"),
            cc_dir: PathBuf::from("/work/cfg/.cactup-builds/0003/cc"),
        };
        let probe = staged.probe_step("make -j8");
        assert!(probe.contains(r"'/opt/it'\''s here/cactup-abc' __cc-probe '/work/cfg/.cactup-builds/0003/cc/config.toml'"), "{probe}");
        assert!(probe.contains("make -j8 -s -f '/work/cfg/.cactup-builds/0003/cc/selftest/lib/make/make.subdir'"), "{probe}");
        assert!(probe.contains("[ $cactup_cc_status -eq 3 ] || echo"), "{probe}");

        let build = staged.build_step("make -j8", "sim");
        assert!(build.contains("export MAKEFILES; make -j8 sim )"), "{build}");
        assert!(build.trim_end().ends_with("fi"), "{build}");
    }

    /// Run the two script steps the way the build script does (`set -e`),
    /// with a stand-in for cactup (`probe`: a script body, or `None` for a
    /// binary that is not there), `selftest_make` as the self-test's make,
    /// and a build "make" that prints the `MAKEFILES` it was given.
    /// Returns (stdout, stderr); the script itself must always succeed.
    fn run_steps(probe: Option<&str>, selftest_make: &str, makefiles: Option<&str>) -> (String, String) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cactup = tmp.path().join("cactup-abc");
        if let Some(body) = probe {
            fs::write(&cactup, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&cactup, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let staged = Staged { cactup, cc_dir: PathBuf::from("/attempt/cc") };
        let script = format!(
            "set -e\n{}\n{}\n",
            staged.probe_step(selftest_make),
            staged.build_step(r#"sh -c 'echo "make $1 with [$MAKEFILES]"' --"#, "sim"),
        );
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script).env_remove("MAKEFILES");
        if let Some(makefiles) = makefiles {
            cmd.env("MAKEFILES", makefiles);
        }
        let out = cmd.output().unwrap();
        let (stdout, stderr) =
            (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        assert!(out.status.success(), "the build script failed: {stdout}{stderr}");
        (stdout, stderr)
    }

    #[test]
    fn the_build_reads_the_fragment_only_when_probe_and_selftest_both_pass() {
        let (stdout, stderr) = run_steps(Some("exit 0"), "true", None);
        assert_eq!((stdout.as_str(), stderr.as_str()), ("make sim with [/attempt/cc/inject.mk]\n", ""));

        // A MAKEFILES the user already had stays in front.
        let (stdout, _) = run_steps(Some("exit 0"), "true", Some("/home/me/extra.mk"));
        assert_eq!(stdout, "make sim with [/home/me/extra.mk /attempt/cc/inject.mk]\n");
    }

    #[test]
    fn every_way_the_probe_can_fail_leaves_a_plain_build_and_one_line() {
        // Declined: the probe has said why itself, the script adds nothing.
        let declined = format!("echo 'cactup: build cache off for this build: reasons' >&2; exit {PROBE_DECLINED}");
        let (stdout, stderr) = run_steps(Some(&declined), "true", None);
        assert_eq!(stdout, "make sim with []\n");
        assert_eq!(stderr, "cactup: build cache off for this build: reasons\n");

        // Not there at all (a container that does not see the binary).
        let (stdout, stderr) = run_steps(None, "true", None);
        assert_eq!(stdout, "make sim with []\n");
        let ours: Vec<&str> = stderr.lines().filter(|l| l.starts_with("cactup: ")).collect();
        assert_eq!(ours.len(), 1, "{stderr}");
        assert!(ours[0].starts_with("cactup: build cache off for this build: "), "{stderr}");
        assert!(ours[0].ends_with("could not check it here (exit status 127)"), "{stderr}");

        // Crashed, or anything else that is not the probe's own "declined".
        let (stdout, stderr) = run_steps(Some("exit 1"), "true", None);
        assert_eq!(stdout, "make sim with []\n");
        assert!(stderr.trim_end().ends_with("could not check it here (exit status 1)"), "{stderr}");

        // The probe is fine but this make fails the self-test.
        let (stdout, stderr) = run_steps(Some("exit 0"), "false", Some("/home/me/extra.mk"));
        assert_eq!(stdout, "make sim with [/home/me/extra.mk]\n");
        assert!(stderr.contains("GNU make 3.82 or later is needed"), "{stderr}");
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
    }
}
