//! CLI surface (spec §3). Global flags + the full command tree; topology
//! flags per §8.5, compute-node flags per §8.3.1, restart flags per §8.8.

use crate::walltime::Walltime;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// clap adapter for the canonical §8.5 walltime grammar.
fn parse_walltime(s: &str) -> Result<Walltime, String> {
    Walltime::parse(s).map_err(|e| e.to_string())
}

/// `-j` / `--make-jobs` value: an explicit count, or `max` for "all threads
/// available in whatever universe/context the build runs in" (§7.6). `Max` is
/// resolved at build time (as a shell `$(nproc)`) so it reflects the wrapped
/// build context — e.g. an `srun`/`singularity` allocation — not the login node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MakeJobs {
    Count(u32),
    Max,
}

/// clap adapter for `-j`: a positive integer or the word `max`.
fn parse_make_jobs(s: &str) -> Result<MakeJobs, String> {
    if s.eq_ignore_ascii_case("max") {
        return Ok(MakeJobs::Max);
    }
    match s.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(MakeJobs::Count(n)),
        _ => Err(format!("expected a positive integer or \"max\", got \"{s}\"")),
    }
}

#[derive(Parser, Debug)]
#[command(name = "cactup")]
#[command(version)]
#[command(about = "The best way to install Cactus", long_about = None)]
pub(crate) struct Args {
    #[clap(flatten)]
    pub globals: GlobalOpts,
    #[clap(subcommand)]
    pub command: Commands,
}

#[derive(clap::Args, Debug)]
pub(crate) struct GlobalOpts {
    #[clap(short, long, global = true)]
    pub verbose: bool,
    /// Print every shell command cactup runs to stderr as it happens (submit,
    /// build, run, discovery, …) — useful for diagnosing scheduler failures.
    #[clap(long, global = true)]
    pub trace: bool,
    #[clap(long, global = true, default_value = "https://bitbucket.org/einsteintoolkit/manifest.git")]
    pub manifest_url: String,
    /// Override the system MDB location (§2.2; mainly for testing).
    #[clap(long, global = true, value_name = "PATH")]
    pub mdb_path: Option<PathBuf>,
    /// Use this machine, skipping discovery entirely (§4.3).
    #[clap(long, global = true, value_name = "NAME")]
    pub machine: Option<String>,
    /// Target this installation for one command instead of the active one.
    #[clap(long, global = true, value_name = "ALIAS")]
    pub installation: Option<String>,
    /// Hostname to use for machine discovery (§4.3; overrides ~/.hostname
    /// and the system FQDN).
    #[clap(long, global = true, value_name = "HOSTNAME")]
    pub hostname: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// List available Einstein Toolkit releases
    Releases {
        #[clap(short, long, help = "List all releases instead of only the few most recent.")]
        all: bool,
    },
    /// List Einstein Toolkit installations on this machine
    List,
    /// Show the active installation, or a named one, in detail
    Show { alias: Option<String> },
    /// Set the active Einstein Toolkit installation
    Use {
        #[clap(help = "The alias of the installation to activate.")]
        alias: String,
    },
    /// Install an Einstein Toolkit release
    Install(InstallArgs),
    /// Remove an installation (§3.1)
    Uninstall {
        #[clap(help = "The alias of the installation to remove.")]
        alias: String,
        #[clap(short, long, help = "Do not ask for confirmation.")]
        force: bool,
    },
    /// Manage Cactus configurations in the active installation (§7)
    #[clap(subcommand)]
    Config(ConfigCommand),
    /// Alias for `config build`
    Build(ConfigBuildArgs),
    /// Manage simulations (§8)
    #[clap(subcommand)]
    Sim(SimCommand),
    /// Run thorn test suites against a built config (§11)
    #[clap(subcommand)]
    Test(TestCommand),
    /// Print or set machine-global default values (§5)
    Knob {
        /// The knob to print or set; omit to print all knobs.
        name: Option<String>,
        /// The value to set; omit to print the knob.
        value: Option<String>,
    },
    /// Inspect and manage machine definitions (§4)
    #[clap(subcommand)]
    Machine(MachineCommand),
}

#[derive(clap::Args, Debug)]
pub(crate) struct InstallArgs {
    #[clap(help = "The release to install. If unspecified, the most recent release will be installed.")]
    pub release: Option<String>,
    #[clap(short, long, help = "The unique name of the installation. If unspecified, the release name will be used.")]
    pub alias: Option<String>,
    #[clap(short, long, help = "Assume default answers to all unspecified flags instead of prompting.")]
    pub silent: bool,
    #[clap(long, help = "The prefix to install to.")]
    pub install_prefix: Option<String>,
    #[clap(long, help = "Skip creating a symlink.")]
    pub no_symlink: bool,
    #[clap(long, help = "The directory in which to create the symlink.")]
    pub symlink_prefix: Option<String>,
    #[clap(long, help = "The name of the symlink to create.")]
    pub symlink_name: Option<String>,
}

/// `--universe U | --no-universe` (§4.8), shared by build/submit/run commands.
#[derive(clap::Args, Debug)]
pub(crate) struct UniverseFlags {
    /// Run inside the named universe (§4.8), overriding any default.
    #[clap(long, value_name = "UNIVERSE", conflicts_with = "no_universe")]
    pub universe: Option<String>,
    /// Force the host context, overriding any default universe.
    #[clap(long)]
    pub no_universe: bool,
}

/// The §8.5 TOPOLOGY flag set, shared by `sim submit/run` and `test run/submit`.
#[derive(clap::Args, Debug)]
pub(crate) struct TopologyFlags {
    /// Account/allocation to charge (default: knob).
    #[clap(short, long, value_name = "ACCOUNT")]
    pub allocation: Option<String>,
    /// Scheduler queue (default: knob, then the machine's default queue).
    #[clap(short, long, value_name = "QUEUE")]
    pub queue: Option<String>,
    /// Notification address (default: knob).
    #[clap(short, long, value_name = "ADDRESS")]
    pub mail: Option<String>,
    /// Notification type (default: knob, `all`).
    #[clap(short = 'M', long, value_name = "TYPE")]
    pub mail_type: Option<String>,
    /// Node count (default: 1).
    #[clap(short, long, value_name = "N")]
    pub nodes: Option<u32>,
    /// Total tasks (MPI ranks; default: the script variant's `tasks` setting,
    /// else nodes * tasks-per-node — but 2 for `test run`/`test submit`).
    #[clap(short = 'T', long, value_name = "N")]
    pub tasks: Option<u32>,
    /// Tasks per node (default: fill the node, floor(MAX_TASKS_PER_NODE / cpus)).
    #[clap(short, long, value_name = "N")]
    pub tpn: Option<u32>,
    /// CPUs (threads) per task (default: 1).
    #[clap(short, long, value_name = "N")]
    pub cpus: Option<u32>,
    /// Use GPUs (default: inferred from the queue's gpu flag).
    #[clap(short, long)]
    pub gpu: bool,
    /// Job name (default: the simulation name).
    #[clap(short, long, value_name = "NAME")]
    pub job_name: Option<String>,
    /// Total walltime for the whole simulation; chained into per-job segments
    /// when it exceeds the queue ceiling (§8.8).
    #[clap(short, long, value_name = "(DD-)?HH:MM:SS", value_parser = parse_walltime)]
    pub wall_time: Option<Walltime>,
    /// stdout filename (default: template default).
    #[clap(short, long, value_name = "FILE")]
    pub out: Option<String>,
    /// stderr filename (default: template default).
    #[clap(short, long, value_name = "FILE")]
    pub err: Option<String>,
}

/// Build flags for `config build` (§7.1, §7.6, §7.7).
#[derive(clap::Args, Debug)]
pub(crate) struct BuildOpts {
    /// Rebuild even if the config is already built.
    #[clap(short, long)]
    pub force: bool,
    /// Thornlist path (default: <Cactus root>/thornlists/einsteintoolkit.th).
    #[clap(long, value_name = "PATH")]
    pub thornlist: Option<PathBuf>,
    /// Optionlist variant (required iff the machine has more than one — §4.4).
    #[clap(long, value_name = "VARIANT")]
    pub variant: Option<String>,
    #[clap(flatten)]
    pub universe: UniverseFlags,
    /// Debug build.
    #[clap(long)]
    pub debug: bool,
    /// Optimized build.
    #[clap(long)]
    pub optimize: bool,
    /// Unsafe (fast-math style) build.
    #[clap(long = "unsafe")]
    pub unsafe_build: bool,
    /// Profiling build.
    #[clap(long)]
    pub profile: bool,
    /// Force reconfiguration before building.
    #[clap(long)]
    pub reconfig: bool,
    /// Clean the config before building.
    #[clap(long)]
    pub clean: bool,
    /// Parallel make jobs: a number, or `max` for all threads available in the
    /// build context (default: machine make-jobs, else 1 — §7.6).
    #[clap(long, short = 'j', value_name = "N|max", value_parser = parse_make_jobs)]
    pub make_jobs: Option<MakeJobs>,
    /// Copy a prebuilt cactus_<config> into place, skipping configure/make (§7.7).
    #[clap(long, alias = "virtual", value_name = "EXE")]
    pub virtual_executable: Option<PathBuf>,
}

#[cfg(test)]
impl BuildOpts {
    /// An all-defaults instance for unit tests (clap normally builds these).
    pub(crate) fn default_for_tests() -> BuildOpts {
        BuildOpts {
            force: false,
            thornlist: None,
            variant: None,
            universe: UniverseFlags { universe: None, no_universe: false },
            debug: false,
            optimize: false,
            unsafe_build: false,
            profile: false,
            reconfig: false,
            clean: false,
            make_jobs: None,
            virtual_executable: None,
        }
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct ConfigBuildArgs {
    /// The config to build (or rebuild, with -f).
    pub name: String,
    #[clap(flatten)]
    pub opts: BuildOpts,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCommand {
    /// Build (or rebuild with -f) a config in the active installation
    Build(ConfigBuildArgs),
    /// List configs in the active installation
    List,
    /// Show the active config, or a named one's stored metadata
    Show { name: Option<String> },
    /// Set the installation's active config
    Use { name: String },
    /// Remove a config build and its metadata (§7.1)
    Delete {
        name: String,
        #[clap(short, long, help = "Delete even if simulations were built from this config.")]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SimCommand {
    /// Create a simulation from a parfile (§8.2)
    Create {
        #[clap(short, long, help = "Replace an existing simulation of the same name.")]
        force: bool,
        /// The simulation name.
        sim: String,
        /// The parfile (.par, or computed .py — §6.2).
        parfile: PathBuf,
        /// Config to attach (default: active config).
        #[clap(long, value_name = "CONFIG")]
        config: Option<String>,
        /// Simulation directory (fixed at create time; default: under sim-home — §8.1).
        #[clap(long, value_name = "PATH")]
        sim_dir: Option<PathBuf>,
    },
    /// Submit a simulation to the queue (implicit create with a parfile — §8.3)
    Submit(SimStartArgs),
    /// Run a simulation interactively, bypassing the queue (§8.4)
    Run(SimRunArgs),
    /// Stop a running/queued simulation (§8.6)
    Stop {
        sim: String,
        #[clap(short, long, help = "Kill the job via the scheduler instead of the graceful TERMINATE trigger.")]
        force: bool,
    },
    /// Clean up a simulation's aborted restarts (§8.6)
    Clean { sim: String },
    /// Move a simulation to TRASH/ (§8.7)
    Delete {
        sim: String,
        #[clap(short, long, help = "Permanently delete instead of moving to TRASH/.")]
        force: bool,
    },
    /// List simulations
    List {
        #[clap(long, help = "Show extended per-simulation details.")]
        long: bool,
        #[clap(long, help = "List simulations across every installation (§8.1).")]
        all: bool,
    },
    /// Show one simulation in detail
    Show {
        sim: String,
        #[clap(long, help = "Show extended per-restart details.")]
        long: bool,
    },
    /// Print the active (or Nth) restart's output directory
    OutputDir {
        sim: String,
        #[clap(long, value_name = "N")]
        restart_id: Option<u32>,
    },
    /// Tail the simulation's stdout/stderr
    Log { sim: String },
}

/// Shared surface of `sim submit` and `sim run` (§3, §8.3, §8.8).
#[derive(clap::Args, Debug)]
pub(crate) struct SimStartArgs {
    /// The simulation name.
    pub sim: String,
    /// Parfile — triggers implicit create when the simulation doesn't exist (§8.3).
    pub parfile: Option<PathBuf>,
    /// Config for the implicit create (default: active config).
    #[clap(long, value_name = "CONFIG")]
    pub config: Option<String>,
    /// Bypass all nagging (implies --overwrite and --force-queue).
    #[clap(short, long)]
    pub force: bool,
    /// Replace an existing simulation on implicit create.
    #[clap(long)]
    pub overwrite: bool,
    /// Bypass the optionlist↔queue compatibility check (§4.4).
    #[clap(long)]
    pub force_queue: bool,
    #[clap(flatten)]
    pub universe: UniverseFlags,
    #[clap(flatten)]
    pub topology: TopologyFlags,
    /// Start the new restart cold, ignoring existing checkpoints (§8.8).
    #[clap(long)]
    pub no_recover: bool,
    /// Operate on a specific output-%04d instead of the latest (§8.8).
    #[clap(long, value_name = "N")]
    pub restart_id: Option<u32>,
    /// Override the checkpoint-hint buffer: @CHECKPOINT_WALLTIME@ = hard wall
    /// − buffer (default max(wall/24, 10 min) — §8.8).
    #[clap(long, value_name = "(DD-)?HH:MM:SS", value_parser = parse_walltime)]
    pub checkpt_buffer: Option<Walltime>,
}

#[derive(clap::Args, Debug)]
pub(crate) struct SimRunArgs {
    #[clap(flatten)]
    pub start: SimStartArgs,
    /// Launch under the debugger (@RUNDEBUG@/@DEBUGGER@).
    #[clap(long)]
    pub debug: bool,
    /// Compute-node path (§8.3.1): the absolute simulation directory, so the
    /// run needs neither the global DB nor the registry. Requires --restart-id.
    #[clap(long, value_name = "PATH", requires = "restart_id")]
    pub sim_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TestCommand {
    /// Run the test suite interactively (§11.6)
    Run(TestStartArgs),
    /// Submit the test suite to the queue (§11.6)
    Submit(TestStartArgs),
    /// Remove in-tree testsuite output the flesh harness left in the Cactus
    /// source tree (TEST/ at the Cactus root, configs/<cfg>/TEST) — cactup
    /// runs redirect it to test-home (§11.5)
    Clean,
    /// Manage test runs (§11.7)
    #[clap(subcommand)]
    Sim(TestSimCommand),
}

#[derive(clap::Args, Debug)]
pub(crate) struct TestStartArgs {
    /// Config whose testsuite to run (default: the active config).
    #[clap(long, value_name = "CONFIG")]
    pub config: Option<String>,
    /// Runscript variant override (§11.2).
    #[clap(long, value_name = "VARIANT")]
    pub variant: Option<String>,
    /// Bypass all nagging (implies --overwrite and --force-queue).
    #[clap(short, long)]
    pub force: bool,
    /// Replace an existing test run of the same name.
    #[clap(long)]
    pub overwrite: bool,
    /// Bypass the optionlist↔queue compatibility check (§4.4).
    #[clap(long)]
    pub force_queue: bool,
    #[clap(flatten)]
    pub universe: UniverseFlags,
    #[clap(flatten)]
    pub topology: TopologyFlags,
    /// Test selection: test names, thorns (arrangement/Thorn), or
    /// arrangements. Empty selects all tests (§11.3).
    pub tests: Vec<String>,
    /// Compute-node path (§11.6): the absolute test-run directory, so the
    /// run needs neither the global DB nor the registry.
    #[clap(long, value_name = "PATH", requires = "results_id")]
    pub test_dir: Option<PathBuf>,
    /// Compute-node path: the results-%04d id to drive.
    #[clap(long, value_name = "N", requires = "test_dir")]
    pub results_id: Option<u32>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TestSimCommand {
    /// List test runs
    List {
        #[clap(long)]
        long: bool,
        #[clap(long, help = "List test runs across every installation.")]
        all: bool,
    },
    /// Show one test run
    Show { name: String },
    /// Stop a queue-submitted test run
    Stop {
        name: String,
        #[clap(short, long)]
        force: bool,
    },
    /// Move a test run to the test-home TRASH/
    Delete {
        name: String,
        #[clap(short, long)]
        force: bool,
        #[clap(long, help = "Permanently remove instead of moving to TRASH/.")]
        purge: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum MachineCommand {
    /// List all machines in the MDB (replaces print-mdb / list-machines)
    List,
    /// Show the machine this host resolves to, or a named one (§4.3)
    Show {
        name: Option<String>,
        /// List the machine's optionlist variants with their details
        /// (description, compatible queues, universe, …) instead of the
        /// usual machine summary.
        #[clap(long)]
        variants: bool,
    },
    /// Persist a tuned local machine into the user MDB (§4.7)
    Create {
        /// Machine name (default: this host's short hostname).
        name: Option<String>,
        /// Clone BASE instead of `generic`; with no value, clone the
        /// currently-detected machine.
        #[clap(long, value_name = "BASE", num_args = 0..=1)]
        from_existing: Option<Option<String>>,
        /// Take defaults instead of prompting (successor to setup-silent).
        #[clap(long)]
        silent: bool,
        /// Skip generating discover.py (machine only selectable via --machine).
        #[clap(long)]
        no_discover: bool,
    },
    /// Delete a user-MDB machine (§4.7)
    Delete { name: String },
    /// Clear the cached detected machine (§4.3)
    Forget,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Args::command().debug_assert();
    }

    #[test]
    fn parses_representative_command_lines() {
        for argv in [
            vec!["cactup", "releases", "--all"],
            vec!["cactup", "list"],
            vec!["cactup", "show", "et"],
            vec!["cactup", "install", "ET_2025_05", "--silent"],
            vec!["cactup", "uninstall", "old", "-f"],
            vec!["cactup", "build", "sim", "--variant", "cuda", "--unsafe", "-j", "8"],
            vec!["cactup", "config", "build", "sim", "--universe", "et-sif"],
            vec!["cactup", "config", "delete", "sim", "-f"],
            vec![
                "cactup", "sim", "submit", "bbh", "bbh.par", "--config", "sim", "-n", "4", "-w",
                "2-00:00:00", "-q", "checkpt", "--force-queue",
            ],
            vec![
                "cactup", "sim", "run", "bbh", "--restart-id", "3", "--sim-dir", "/scratch/bbh",
                "--machine", "mel5", "--installation", "et", "--no-recover",
            ],
            vec!["cactup", "sim", "output-dir", "bbh", "--restart-id", "2"],
            vec!["cactup", "sim", "list", "--long", "--all"],
            vec!["cactup", "sim", "show", "bbh", "--long"],
            vec!["cactup", "test", "run", "-n", "1", "McLachlan/ML_BSSN", "TestArrangement"],
            vec![
                "cactup", "test", "run", "tests", "--test-dir", "/work/tests/sim-test/tests",
                "--results-id", "0", "--installation", "et", "--machine", "mel5",
            ],
            vec!["cactup", "test", "sim", "delete", "t1", "--purge"],
            vec!["cactup", "knob", "allocation", "hpc_xxx"],
            vec!["cactup", "machine", "create", "mylaptop", "--from-existing", "--silent"],
            vec!["cactup", "machine", "list"],
            vec!["cactup", "machine", "show", "--hostname", "mel5.host"],
        ] {
            if let Err(e) = Args::try_parse_from(&argv) {
                panic!("failed to parse {argv:?}: {e}");
            }
        }
    }

    #[test]
    fn make_jobs_accepts_number_or_max() {
        assert_eq!(parse_make_jobs("8"), Ok(MakeJobs::Count(8)));
        assert_eq!(parse_make_jobs("max"), Ok(MakeJobs::Max));
        assert_eq!(parse_make_jobs("MAX"), Ok(MakeJobs::Max));
        assert!(parse_make_jobs("0").is_err());
        assert!(parse_make_jobs("lots").is_err());
        // Wired through the parser end-to-end.
        assert!(Args::try_parse_from(["cactup", "build", "sim", "-j", "max"]).is_ok());
        assert!(Args::try_parse_from(["cactup", "build", "sim", "-j", "nope"]).is_err());
    }

    #[test]
    fn rejects_contradictory_flags() {
        // --universe and --no-universe are mutually exclusive (§4.8).
        assert!(Args::try_parse_from(["cactup", "build", "c", "--universe", "u", "--no-universe"]).is_err());
        // The compute-node --sim-dir is only meaningful with --restart-id (§8.3.1).
        assert!(Args::try_parse_from(["cactup", "sim", "run", "s", "--sim-dir", "/x"]).is_err());
        // Bad walltime grammar is rejected at parse time (§8.5).
        assert!(Args::try_parse_from(["cactup", "sim", "submit", "s", "-w", "1:99:00"]).is_err());
    }
}
