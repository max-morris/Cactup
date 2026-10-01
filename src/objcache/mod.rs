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

pub mod probe;
pub mod wrapper;

use crate::build::sh_quote;
use crate::database::Database;
use crate::Res;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// The verb the injected makefile fragment runs the wrapper by:
/// `cactup __cc <config.toml> <args…>`, with the compiler command in the
/// environment (`wrapper::CMD_ENV`). Dispatched before clap ever sees the
/// command line, so it is in no help output.
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
    /// Wrap the compilers and log each compile, but serve nothing and store
    /// nothing.
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

/// A build with the cache staged: the shell text `prepare` splices into the
/// build script.
#[derive(Debug)]
pub struct Staged {
    cactup: PathBuf,
    cc_dir: PathBuf,
    config_dir: PathBuf,
}

/// Freeze `mode` for one build into `cc_dir` (`<attempt>/cc`). `None` when
/// the cache is off: nothing is written, and the build script comes out
/// byte for byte what it is without this module.
pub fn stage(cc_dir: &Path, mode: Mode, cactup: &str, config_dir: &Path) -> Res<Option<Staged>> {
    if mode == Mode::Off {
        return Ok(None);
    }
    let conf = BuildConf { mode, cactup: PathBuf::from(cactup), config_dir: config_dir.to_owned() };
    fs::create_dir_all(cc_dir).with_context(|| format!("Failed to create {}", cc_dir.display()))?;
    let path = conf_path(cc_dir);
    let text = toml::to_string(&conf).context("Failed to serialize the build-cache configuration")?;
    fs::write(&path, text).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(Some(Staged { cactup: conf.cactup, cc_dir: cc_dir.to_owned(), config_dir: conf.config_dir }))
}

impl Staged {
    /// The build-script step that decides, where the build actually runs,
    /// whether the wrapper is used: it leaves `CACTUP_CC_MAKEFILES` naming
    /// the injection fragment, or empty with one line on stderr saying why
    /// not. It can only ever turn the cache off, never fail the build —
    /// inside a container universe the cactup binary or the attempt
    /// directory may simply not be visible.
    ///
    /// The self-test runs the build's own frozen `make` command twice under
    /// the fragment: once where and how Cactus's object sub-makes run (in
    /// the configuration's `build` directory, with `CCTK_TARGET` set), and
    /// once as any other make below the build would (see
    /// `probe::selftest_wrapped_mk`). Its output goes to
    /// `<attempt>/cc/selftest.log`.
    pub fn probe_step(&self, make: &str) -> String {
        let cactup = sh_quote(&self.cactup);
        let inject = sh_quote(&inject_path(&self.cc_dir));
        let log = sh_quote(&self.cc_dir.join("selftest.log"));
        format!(
            "CACTUP_CC_MAKEFILES=\n\
             if {cactup} {PROBE_VERB} {conf}; then\n\
             \x20 if ( MAKEFILES={inject}; export MAKEFILES\n\
             \x20      cd {build} && {make} -s -f {wrapped} CCTK_TARGET=cactup-selftest SRCDIR=. &&\n\
             \x20      cd {cc} && {make} -s -f {untouched} CCTK_TARGET=cactup-selftest SRCDIR=. ) > {log} 2>&1; then\n\
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

    #[test]
    fn mode_knob_accepts_exactly_its_names() {
        assert_eq!(validate_mode(" record ").unwrap(), "record");
        assert_eq!(validate_mode("off").unwrap(), "off");
        let err = validate_mode("on").unwrap_err().to_string();
        assert!(err.contains("valid: off, record"), "{err}");
    }

    #[test]
    fn off_stages_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc");
        let staged = stage(&cc, Mode::Off, "/opt/cactup/bin/cactup-abc1234", tmp.path()).unwrap();
        assert!(staged.is_none());
        assert!(!cc.exists(), "an off build must not even create the directory");
    }

    #[test]
    fn staged_configuration_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc");
        let config = tmp.path().join("Cactus/configs/sim");
        stage(&cc, Mode::Record, "/opt/cactup/bin/cactup-abc1234", &config).unwrap().unwrap();

        let conf = BuildConf::load(&conf_path(&cc)).unwrap();
        assert_eq!(
            conf,
            BuildConf {
                mode: Mode::Record,
                cactup: PathBuf::from("/opt/cactup/bin/cactup-abc1234"),
                config_dir: config,
            }
        );
        assert!(BuildConf::load(&cc.join("missing.toml")).is_err());
    }

    #[test]
    fn the_script_steps_quote_their_paths() {
        let staged = Staged {
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
                "cd '/work/cfg/build' && make -j8 -s -f '/work/cfg/.cactup-builds/0003/cc/selftest/wrapped.mk' \
                 CCTK_TARGET=cactup-selftest SRCDIR=. &&"
            ),
            "{probe}"
        );
        assert!(probe.contains("[ $cactup_cc_status -eq 3 ] || echo"), "{probe}");

        let build = staged.build_step("make -j8", "sim");
        assert!(build.contains("export MAKEFILES; make -j8 sim )"), "{build}");
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
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cactup = tmp.path().join("cactup-abc");
        if let Some(body) = probe {
            fs::write(&cactup, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&cactup, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config_dir = tmp.path().join("cfg");
        let cc_dir = config_dir.join(".cactup-builds/0000/cc");
        fs::create_dir_all(selftest_dir(&cc_dir)).unwrap();
        fs::create_dir_all(config_dir.join("build")).unwrap();
        let inject = inject_path(&cc_dir).display().to_string();
        let staged = Staged { cactup, cc_dir, config_dir };
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

    /// A build "make" that prints the `MAKEFILES` it was given.
    const SHOW: &str = r#"sh -c 'echo "make $1 with [$MAKEFILES]"' --"#;

    #[test]
    fn the_build_reads_the_fragment_only_when_probe_and_selftest_both_pass() {
        let ran = run_steps(Some("exit 0"), "true", SHOW, None);
        assert_eq!((ran.stdout.as_str(), ran.stderr.as_str()), ("make sim with [<inject.mk>]\nafter the build\n", ""));

        // A MAKEFILES the user already had stays in front.
        let ran = run_steps(Some("exit 0"), "true", SHOW, Some("/home/me/extra.mk"));
        assert_eq!(ran.stdout, "make sim with [/home/me/extra.mk <inject.mk>]\nafter the build\n");
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

        // The probe is fine but the self-test fails with this make.
        let ran = run_steps(Some("exit 0"), "false", SHOW, Some("/home/me/extra.mk"));
        assert_eq!(ran.stdout, "make sim with [/home/me/extra.mk]\nafter the build\n");
        assert!(ran.stderr.starts_with("cactup: build cache off for this build: its self-test failed"), "{}", ran.stderr);
        assert_eq!(ran.stderr.lines().count(), 1, "{}", ran.stderr);
        assert!(ran.success);
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
