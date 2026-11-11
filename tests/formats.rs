//! The versioned on-disk records through the real binary, as a queued job
//! meets them: the compute-node path (`sim run --sim-dir D --restart-id N`)
//! reads `simulation.toml` and `restart.toml`, writes `restart.toml` at its
//! start and its end, and must keep what a newer cactup put in them and
//! refuse what a newer cactup versioned past it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CACTUP: &str = env!("CARGO_BIN_EXE_cactup");

const SIMULATION: &str = "\
schema = 1
machine = \"workstation\"
simulation-id = \"simulation-x-workstation-host-user-20260501-120000-4242\"
sourcedir = \"/nonexistent/Cactus\"
configuration = \"sim\"
config-id = \"cfg-0123\"
build-id = \"build-0001\"
executable = \"/nonexistent/exe\"
optionlist = \"cactup-optionlist.cfg\"
thornlist = \"thornlist.th\"
parfile = \"x.par\"
alias = \"et\"
future-simulation-key = \"kept\"

[future-simulation-table]
n = 1
";

const RESTART: &str = "\
schema = 1
created = \"2026-05-01T12:00:00Z\"
nodes = 1
tasks = 1
tpn = 1
cpus = 1
queue = \"workq\"
walltime = \"01:00:00\"
checkpt-buffer = \"00:10:00\"
job-id = \"-1\"
terminated = false
future-restart-key = \"kept\"

[universe]
name = \"host\"
future-universe-key = 7

[future-restart-table]
n = 2

[vars]
NAME = \"x\"

[knobs]
";

/// A hand-built simulation directory with restart 0 and a trivial run-script
/// that leaves a marker, so a run that began can be told from one that did not.
struct Sim {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    dir: PathBuf,
}

impl Sim {
    fn new() -> Sim {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = tmp.path().join("x");
        let restart = dir.join("output-0000/.cactup");
        for d in [&home, &dir.join(".cactup/par"), &restart] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(dir.join(".cactup/simulation.toml"), SIMULATION).unwrap();
        fs::write(dir.join(".cactup/par/x.par"), "ActiveThorns = \"IOUtil\"\n").unwrap();
        fs::write(restart.join("restart.toml"), RESTART).unwrap();
        fs::write(restart.join("run-script"), "#!/bin/sh\ntouch ran\n").unwrap();
        Sim { _tmp: tmp, home, dir }
    }

    fn simulation(&self) -> PathBuf {
        self.dir.join(".cactup/simulation.toml")
    }

    fn restart(&self) -> PathBuf {
        self.dir.join("output-0000/.cactup/restart.toml")
    }

    fn ran(&self) -> bool {
        self.dir.join("output-0000/ran").exists()
    }

    /// The compute-node invocation a generated submit script makes.
    fn run(&self) -> Output {
        Command::new(CACTUP)
            .args(["sim", "run", "x", "--sim-dir"])
            .arg(&self.dir)
            .args(["--restart-id", "0"])
            .env("CACTUP_HOME", &self.home)
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }
}

fn text(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn table(path: &Path) -> toml::Table {
    toml::from_str(&text(path)).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_run_keeps_the_keys_it_does_not_know() {
    let sim = Sim::new();
    let simulation_before = text(&sim.simulation());
    let out = sim.run();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(sim.ran(), "the run-script did not run");

    // The start and finish writes: this binary's own bookkeeping landed, and
    // the keys it does not model are still there, in their places.
    let restart = table(&sim.restart());
    assert_eq!(restart.keys().next().map(String::as_str), Some("schema"));
    assert_eq!(restart["schema"].as_integer(), Some(1));
    assert!(restart.contains_key("started") && restart.contains_key("finished"), "{restart:?}");
    assert_eq!(restart["terminated"].as_bool(), Some(true));
    assert_eq!(restart["future-restart-key"].as_str(), Some("kept"));
    assert_eq!(restart["future-restart-table"]["n"].as_integer(), Some(2));
    assert_eq!(restart["universe"]["future-universe-key"].as_integer(), Some(7));

    // The compute path never writes simulation.toml: it is as it was, with
    // the unknown keys in it.
    assert_eq!(text(&sim.simulation()), simulation_before);
    assert!(text(&sim.simulation()).contains("future-simulation-key"));
}

#[test]
fn a_newer_restart_record_stops_the_run_before_it_starts() {
    let sim = Sim::new();
    let newer = RESTART.replacen("schema = 1", "schema = 2", 1);
    fs::write(sim.restart(), &newer).unwrap();
    let out = sim.run();
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("written by a newer cactup") && err.contains("upgrade cactup"), "{err}");
    assert!(err.contains("restart.toml"), "{err}");
    assert!(!sim.ran(), "the run-script ran");
    assert_eq!(text(&sim.restart()), newer, "the refused file was rewritten");
}

#[test]
fn a_newer_simulation_record_stops_the_run_before_it_starts() {
    let sim = Sim::new();
    let newer = SIMULATION.replacen("schema = 1", "schema = 2", 1);
    fs::write(sim.simulation(), &newer).unwrap();
    let out = sim.run();
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("written by a newer cactup") && err.contains("simulation.toml"), "{err}");
    assert!(!sim.ran(), "the run-script ran");
    assert_eq!(text(&sim.simulation()), newer);
    assert_eq!(text(&sim.restart()), RESTART, "the restart was touched");
}
