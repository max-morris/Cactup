//! CLI surface (spec §3). Global flags + the full command tree; topology
//! flags per §8.5, compute-node flags per §8.3.1, restart flags per §8.8.

use crate::walltime::Walltime;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

// §8.5
/// clap adapter for the canonical walltime grammar.
fn parse_walltime(s: &str) -> Result<Walltime, String> {
    Walltime::parse(s).map_err(|e| e.to_string())
}

// §7.6
/// `-j` / `--make-jobs` value: an explicit count, or `max` for "all threads
/// available in whatever universe/context the build runs in". `Max` is
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
    // §2.2
    /// Override the system MDB location (mainly for testing).
    #[clap(long, global = true, value_name = "PATH")]
    pub mdb_path: Option<PathBuf>,
    // §4.3
    /// Use this machine, skipping discovery entirely.
    #[clap(long, global = true, value_name = "NAME")]
    pub machine: Option<String>,
    /// Target this installation for one command instead of the active one.
    #[clap(long, global = true, value_name = "ALIAS")]
    pub installation: Option<String>,
    // §4.3
    /// Hostname to use for machine discovery (overrides ~/.hostname
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
    /// List Einstein Toolkit installations on this machine (top-level
    /// equivalent of `installation list`)
    List,
    /// Show cactup's current state: active installation, active config, and machine
    Show,
    /// Set the active Einstein Toolkit installation (top-level equivalent of
    /// `installation use`)
    Use {
        #[clap(help = "The alias of the installation to activate.")]
        alias: String,
    },
    /// Install an Einstein Toolkit release
    Install(InstallArgs),
    // §3.1
    /// Remove an installation
    Uninstall {
        #[clap(help = "The alias of the installation to remove.")]
        alias: String,
        #[clap(short, long, help = "Do not ask for confirmation.")]
        force: bool,
    },
    /// Manage Einstein Toolkit installations
    #[clap(subcommand)]
    Installation(InstallationCommand),
    /// Short for `installation`
    #[clap(subcommand)]
    Inst(InstallationCommand),
    // §7
    /// Manage Cactus configurations in the active installation
    #[clap(subcommand)]
    Config(ConfigCommand),
    /// Alias for `config build`
    Build(ConfigBuildArgs),
    // §8
    /// Manage simulations
    #[clap(subcommand)]
    Sim(SimCommand),
    // §11
    /// Run thorn test suites against a built config
    #[clap(subcommand)]
    Test(TestCommand),
    // §5
    /// Print or set machine-global default values
    Knob {
        /// The knob to print or set; omit to print all knobs.
        name: Option<String>,
        /// The value to set; omit to print the knob.
        value: Option<String>,
    },
    // §4
    /// Inspect and manage machine definitions
    #[clap(subcommand)]
    Machine(MachineCommand),
    // §16, §5
    /// Print one random piece of cactup wisdom (--help for how to configure)
    ///
    /// Wisdom may also appear spontaneously after running a command. Two machine-global knobs
    /// control that:
    ///
    ///   wisdom-frequency  How often the after-command wisdom fires:
    ///                     off (never), rare (1 command in 15), normal
    ///                     (1 in 8, the default), chatty (1 in 4), or
    ///                     always.
    ///   wisdom-kind       Which entries are eligible, here and after
    ///                     commands: relevant (cactup feature tips only)
    ///                     or all (the default: tips mixed with attributed
    ///                     words of wisdom).
    ///
    /// Set them with `cactup knob`:
    ///
    ///   cactup knob wisdom-frequency chatty
    ///   cactup knob wisdom-kind relevant
    #[clap(verbatim_doc_comment)]
    Wisdom,
}

#[derive(Subcommand, Debug)]
pub(crate) enum InstallationCommand {
    /// List Einstein Toolkit installations on this machine
    List,
    /// Show the active installation, or a named one, in detail
    Show { alias: Option<String> },
    /// Set the active Einstein Toolkit installation
    Use { alias: String },
    /// Re-run the component fetch: update repos, adopt a new thornlist or release
    Refetch(RefetchArgs),
    /// Show how the source trees have diverged from the last fetch
    Delta {
        /// Installation to inspect (default: the active one).
        alias: Option<String>,
    },
}

#[derive(clap::Args, Debug)]
pub(crate) struct RefetchArgs {
    /// Thornlist file to adopt and fetch (default: the installation's live thornlist).
    pub thornlist: Option<PathBuf>,
    /// Refetch to this Einstein Toolkit release tag (see `cactup releases`).
    #[clap(long, value_name = "TAG", conflicts_with = "thornlist")]
    pub release: Option<String>,
    /// Bypass all nagging (implies --overwrite-modified and --replace-thornlist; also skips the --prune confirmation).
    #[clap(short, long)]
    pub force: bool,
    /// Fetch over repos with local modifications (they are backed up first).
    /// The "all repos" form of `--overwrite`.
    #[clap(long)]
    pub overwrite_modified: bool,
    /// Fetch over these specific repos' local modifications (space- or comma-separated;
    /// repeatable). Names match a repo under `repos/` or any thorn it provides.
    #[clap(long, value_name = "NAMES")]
    pub overwrite: Vec<String>,
    /// Proceed even when the live thornlist has hand edits, discarding them (a snapshot is kept).
    #[clap(long)]
    pub replace_thornlist: bool,
    /// Remove repos and thorn symlinks the thornlist no longer mentions (asks once; -f skips the confirmation).
    #[clap(long)]
    pub prune: bool,
    /// Suppress the skipped-repos warning block (a one-line count is still printed). Unlike `install -s` this never assumes answers to prompts and never authorizes deletion.
    #[clap(short, long)]
    pub silent: bool,
    /// Print the full classification of what would happen and touch nothing.
    #[clap(short = 'n', long, conflicts_with = "silent")]
    pub dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub(crate) struct InstallArgs {
    #[clap(help = "The release to install. If unspecified, the most recent release will be installed.")]
    pub release: Option<String>,
    #[clap(long, value_name = "PATH", conflicts_with = "release", help = "Install from this thornlist file instead of a release (a \"custom installation\").")]
    pub thornlist: Option<PathBuf>,
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

// §4.8
/// `--universe U | --no-universe`, shared by build/submit/run commands.
#[derive(clap::Args, Debug)]
pub(crate) struct UniverseFlags {
    // §4.8
    /// Run inside the named universe, overriding any default.
    #[clap(long, value_name = "UNIVERSE", conflicts_with = "no_universe")]
    pub universe: Option<String>,
    /// Force the host context, overriding any default universe.
    #[clap(long)]
    pub no_universe: bool,
}

// §8.5
/// The TOPOLOGY flag set, shared by `sim submit/run` and `test run/submit`.
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
    /// Tasks per node (default: fill the node, floor(MAX_CPUS_PER_NODE / cpus)).
    #[clap(short, long, value_name = "N")]
    pub tpn: Option<u32>,
    /// CPUs (threads) per task (default: the machine/queue `default-cpus-per-task`, else 1).
    #[clap(short, long, value_name = "N")]
    pub cpus: Option<u32>,
    /// Use GPUs (default: inferred from the queue's gpu flag).
    #[clap(short, long)]
    pub gpu: bool,
    /// GPUs per task; GPU runs only (default: the machine/queue
    /// `default-gpus-per-task`, else 1).
    #[clap(short = 'G', long, value_name = "N")]
    pub gpus_per_task: Option<u32>,
    /// Job name (default: the simulation name).
    #[clap(short, long, value_name = "NAME")]
    pub job_name: Option<String>,
    // §8.8
    /// Total walltime for the whole simulation; chained into per-job segments
    /// when it exceeds the queue ceiling.
    #[clap(short, long, value_name = "(DD-)?HH:MM:SS", value_parser = parse_walltime)]
    pub wall_time: Option<Walltime>,
    /// stdout filename (default: template default).
    #[clap(short, long, value_name = "FILE")]
    pub out: Option<String>,
    /// stderr filename (default: template default).
    #[clap(short, long, value_name = "FILE")]
    pub err: Option<String>,
}

// §7.1, §7.6, §7.7
/// Build flags for `config build`.
#[derive(clap::Args, Debug)]
pub(crate) struct BuildOpts {
    /// Rebuild even if the config is already built.
    #[clap(short, long)]
    pub force: bool,
    /// Thornlist path (default: the one this config was last built from, else
    /// <Cactus root>/thornlists/installation-default.th).
    #[clap(long, value_name = "PATH")]
    pub thornlist: Option<PathBuf>,
    // §4.4
    /// Optionlist variant (required iff the machine has more than one).
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
    // §7.6
    /// Parallel make jobs: a number, or `max` for all threads available in the
    /// build context (default: machine make-jobs, else 1).
    #[clap(long, short = 'j', value_name = "N|max", value_parser = parse_make_jobs)]
    pub make_jobs: Option<MakeJobs>,
    // §7.7
    /// Copy a prebuilt cactus_<config> into place, skipping configure/make.
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
    // §7.1
    /// Remove a config build and its metadata
    Delete {
        name: String,
        #[clap(short, long, help = "Delete even if simulations were built from this config.")]
        force: bool,
    },
    /// Show how the source trees have diverged since this config was built
    Delta {
        /// Config to inspect (default: the active config).
        name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SimCommand {
    // §8.2
    /// Create a simulation from a parfile
    Create {
        #[clap(short, long, help = "Replace an existing simulation of the same name.")]
        force: bool,
        /// The simulation name.
        sim: String,
        // §6.2
        /// The parfile (.par, or computed .py).
        parfile: PathBuf,
        /// Config to attach (default: active config).
        #[clap(long, value_name = "CONFIG")]
        config: Option<String>,
        // §8.1
        /// Simulation directory (fixed at create time; default: under sim-home).
        #[clap(long, value_name = "PATH")]
        sim_dir: Option<PathBuf>,
    },
    // §8.3
    /// Submit a simulation to the queue (implicit create with a parfile)
    Submit(SimStartArgs),
    // §8.4
    /// Run a simulation interactively, bypassing the queue
    Run(SimRunArgs),
    // §8.6
    /// Stop a running/queued simulation
    Stop {
        sim: String,
        #[clap(short, long, help = "Kill the job via the scheduler instead of the graceful TERMINATE trigger.")]
        force: bool,
    },
    // §8.6
    /// Clean up a simulation's aborted restarts
    Clean { sim: String },
    // §8.7
    /// Move a simulation to TRASH/
    Delete {
        sim: String,
        #[clap(short, long, help = "Permanently delete instead of moving to TRASH/.")]
        force: bool,
    },
    /// List simulations
    List {
        #[clap(long, help = "Show extended per-simulation details.")]
        long: bool,
        // §8.1
        #[clap(long, help = "List simulations across every installation.")]
        all: bool,
    },
    /// Show one simulation in detail
    Show {
        sim: String,
        #[clap(long, help = "Show extended per-restart details.")]
        long: bool,
        #[clap(long, help = "Print only the active (or Nth) restart's output directory.")]
        output_dir: bool,
        #[clap(long, value_name = "N", requires = "output_dir", help = "With --output-dir, select the Nth restart instead of the active one.")]
        restart_id: Option<u32>,
    },
    /// Tail the simulation's stdout/stderr
    Log {
        sim: String,
        #[clap(short, long, conflicts_with_all = ["follow_out", "follow_err"], help = "Side-by-side live TUI of stdout and stderr, until Ctrl-C.")]
        follow: bool,
        #[clap(short = 'o', long, conflicts_with = "follow_err", help = "Stream only stdout (tail -f style), until Ctrl-C.")]
        follow_out: bool,
        #[clap(short = 'e', long, help = "Stream only stderr (tail -f style), until Ctrl-C.")]
        follow_err: bool,
    },
}

// §3, §8.3, §8.8
/// Shared surface of `sim submit` and `sim run`.
#[derive(clap::Args, Debug)]
pub(crate) struct SimStartArgs {
    /// The simulation name.
    pub sim: String,
    // §8.3
    /// Parfile — triggers implicit create when the simulation doesn't exist.
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
    // §4.4
    /// Bypass the optionlist↔queue compatibility check.
    #[clap(long)]
    pub force_queue: bool,
    /// Suppress the notice that the source tree has moved since this config
    /// was built. Never affects what runs — a run always uses the executable
    /// as it was built.
    #[clap(short, long)]
    pub silent: bool,
    #[clap(flatten)]
    pub universe: UniverseFlags,
    #[clap(flatten)]
    pub topology: TopologyFlags,
    // §8.8
    /// Override the checkpoint-hint buffer: @CHECKPOINT_WALLTIME@ = hard wall
    /// − buffer (default max(wall/24, 10 min)).
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
    // §8.3.1, §8.8
    /// Compute-node locator: load and run exactly this output-%04d.
    /// Internal plumbing baked into the generated submit-script as
    /// `--restart-id=@RESTART_ID@`; requires --sim-dir, which is the only path
    /// that honors it. Not a recovery knob — cactup does not steer recovery at
    /// all.
    #[clap(long, value_name = "N", requires = "sim_dir")]
    pub restart_id: Option<u32>,
    // §8.3.1
    /// Compute-node path: the absolute simulation directory, so the
    /// run needs neither the global DB nor the registry. Requires --restart-id.
    #[clap(long, value_name = "PATH", requires = "restart_id")]
    pub sim_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TestCommand {
    // §11.6
    /// Run the test suite interactively
    Run(TestStartArgs),
    // §11.6
    /// Submit the test suite to the queue
    Submit(TestStartArgs),
    // §11.5
    /// Remove in-tree testsuite output the flesh harness left in the Cactus
    /// source tree (TEST/ at the Cactus root, configs/<cfg>/TEST) — cactup
    /// runs redirect it to test-home
    Clean,
    // §11.7
    /// List test runs
    List {
        #[clap(long)]
        long: bool,
        #[clap(long, help = "List test runs across every installation.")]
        all: bool,
    },
    // §11.7
    /// Show one test run
    Show { name: String },
    /// Tail the test run's stdout/stderr
    Log {
        name: String,
        #[clap(short, long, conflicts_with_all = ["follow_out", "follow_err"], help = "Side-by-side live TUI of stdout and stderr, until Ctrl-C.")]
        follow: bool,
        #[clap(short = 'o', long, conflicts_with = "follow_err", help = "Stream only stdout (tail -f style), until Ctrl-C.")]
        follow_out: bool,
        #[clap(short = 'e', long, help = "Stream only stderr (tail -f style), until Ctrl-C.")]
        follow_err: bool,
    },
    // §11.7
    /// Stop a queue-submitted test run
    Stop {
        name: String,
        #[clap(short, long)]
        force: bool,
    },
    // §11.7
    /// Move a test run to the test-home TRASH/
    Delete {
        name: String,
        #[clap(short, long)]
        force: bool,
        #[clap(long, help = "Permanently remove instead of moving to TRASH/.")]
        purge: bool,
    },
}

#[derive(clap::Args, Debug)]
pub(crate) struct TestStartArgs {
    /// Config whose testsuite to run (default: the active config).
    #[clap(long, value_name = "CONFIG")]
    pub config: Option<String>,
    // §11.2
    /// Runscript variant override.
    #[clap(long, value_name = "VARIANT")]
    pub variant: Option<String>,
    /// Bypass all nagging (implies --overwrite and --force-queue).
    #[clap(short, long)]
    pub force: bool,
    /// Replace an existing test run of the same name.
    #[clap(long)]
    pub overwrite: bool,
    // §4.4
    /// Bypass the optionlist↔queue compatibility check.
    #[clap(long)]
    pub force_queue: bool,
    /// Suppress the notice that the source tree has moved since this config
    /// was built. Never affects what runs — a run always uses the executable
    /// as it was built.
    #[clap(short, long)]
    pub silent: bool,
    #[clap(flatten)]
    pub universe: UniverseFlags,
    #[clap(flatten)]
    pub topology: TopologyFlags,
    // §11.3
    /// Test selection: test names, thorns (arrangement/Thorn), or
    /// arrangements. Empty selects all tests.
    pub tests: Vec<String>,
    // §11.6
    /// Compute-node path: the absolute test-run directory, so the
    /// run needs neither the global DB nor the registry.
    #[clap(long, value_name = "PATH", requires = "results_id")]
    pub test_dir: Option<PathBuf>,
    /// Compute-node path: the results-%04d id to drive.
    #[clap(long, value_name = "N", requires = "test_dir")]
    pub results_id: Option<u32>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum MachineCommand {
    /// List all machines in the MDB (replaces print-mdb / list-machines)
    List,
    // §4.3
    /// Show the machine this host resolves to, or a named one
    Show {
        name: Option<String>,
        /// List the machine's optionlist variants with their details
        /// (description, compatible queues, universe, …) instead of the
        /// usual machine summary.
        #[clap(long)]
        variants: bool,
    },
    // §4.7
    /// Persist a tuned local machine into the user MDB
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
    // §4.7
    /// Delete a user-MDB machine
    Delete { name: String },
    // §4.3
    /// Clear the cached detected machine
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
            vec!["cactup", "show"],
            vec!["cactup", "installation", "show", "et"],
            vec!["cactup", "inst", "show", "et"],
            vec!["cactup", "inst", "list"],
            vec!["cactup", "installation", "use", "et"],
            vec!["cactup", "inst", "refetch"],
            vec!["cactup", "inst", "refetch", "--release", "ET_2026_11", "-f"],
            vec!["cactup", "installation", "refetch", "new.th", "--overwrite-modified", "--prune", "-n"],
            vec!["cactup", "inst", "refetch", "--overwrite", "SpacetimeX Cottonmouth", "-n"],
            vec!["cactup", "inst", "refetch", "--overwrite", "SpacetimeX", "--overwrite", "Cottonmouth", "-n"],
            vec!["cactup", "inst", "delta"],
            vec!["cactup", "installation", "delta", "et"],
            vec!["cactup", "config", "delta"],
            vec!["cactup", "config", "delta", "sim"],
            vec!["cactup", "install", "ET_2025_05", "--silent"],
            vec!["cactup", "install", "--thornlist", "my/list.th", "--silent"],
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
                "--machine", "mel5", "--installation", "et",
            ],
            // -s silences the source-divergence notice on both start paths.
            vec!["cactup", "sim", "submit", "bbh", "-s"],
            vec!["cactup", "test", "run", "-s"],
            vec!["cactup", "sim", "show", "bbh", "--output-dir", "--restart-id", "2"],
            vec!["cactup", "sim", "list", "--long", "--all"],
            vec!["cactup", "sim", "show", "bbh", "--long"],
            vec!["cactup", "test", "run", "-n", "1", "McLachlan/ML_BSSN", "TestArrangement"],
            vec![
                "cactup", "test", "run", "tests", "--test-dir", "/work/tests/sim-test/tests",
                "--results-id", "0", "--installation", "et", "--machine", "mel5",
            ],
            vec!["cactup", "test", "delete", "t1", "--purge"],
            vec!["cactup", "test", "log", "sim", "--follow"],
            vec!["cactup", "knob", "allocation", "hpc_xxx"],
            vec!["cactup", "machine", "create", "mylaptop", "--from-existing", "--silent"],
            vec!["cactup", "machine", "list"],
            vec!["cactup", "machine", "show", "--hostname", "mel5.host"],
            vec!["cactup", "knob", "wisdom-frequency", "chatty"],
            vec!["cactup", "wisdom"],
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
        // The compute-node pair is all-or-nothing (§8.3.1): --sim-dir needs a
        // locator, and run_compute is the only path that reads --restart-id, so
        // alone it would parse and then be ignored.
        assert!(Args::try_parse_from(["cactup", "sim", "run", "s", "--sim-dir", "/x"]).is_err());
        assert!(Args::try_parse_from(["cactup", "sim", "run", "s", "--restart-id", "3"]).is_err());
        // Bad walltime grammar is rejected at parse time (§8.5).
        assert!(Args::try_parse_from(["cactup", "sim", "submit", "s", "-w", "1:99:00"]).is_err());
        // --thornlist and a positional release are mutually exclusive (custom
        // vs. release installations).
        assert!(Args::try_parse_from(["cactup", "install", "ET_2025_05", "--thornlist", "x.th"]).is_err());
        // `refetch`'s --release and a positional thornlist are mutually
        // exclusive, same as `install`.
        assert!(
            Args::try_parse_from(["cactup", "inst", "refetch", "x.th", "--release", "ET_2026_11"]).is_err()
        );
        // --dry-run and --silent are mutually exclusive (§ RefetchArgs).
        assert!(Args::try_parse_from(["cactup", "inst", "refetch", "-n", "-s"]).is_err());
    }

    /// `--overwrite` must collect one value per occurrence without swallowing
    /// the positional THORNLIST that can follow it on the same command line.
    fn refetch_args(argv: &[&str]) -> RefetchArgs {
        match Args::try_parse_from(argv).unwrap_or_else(|e| panic!("failed to parse {argv:?}: {e}")).command {
            Commands::Inst(InstallationCommand::Refetch(args))
            | Commands::Installation(InstallationCommand::Refetch(args)) => args,
            other => panic!("expected a refetch command, got {other:?}"),
        }
    }

    #[test]
    fn overwrite_flag_parses_repeated_and_space_separated_values() {
        let args = refetch_args(&["cactup", "inst", "refetch", "--overwrite", "SpacetimeX Cottonmouth", "-n"]);
        assert_eq!(args.overwrite, vec!["SpacetimeX Cottonmouth".to_string()]);

        let args = refetch_args(&[
            "cactup", "inst", "refetch", "--overwrite", "SpacetimeX", "--overwrite", "Cottonmouth", "-n",
        ]);
        assert_eq!(args.overwrite, vec!["SpacetimeX".to_string(), "Cottonmouth".to_string()]);
    }

    #[test]
    fn overwrite_flag_does_not_swallow_the_positional_thornlist() {
        let args = refetch_args(&["cactup", "inst", "refetch", "--overwrite", "A", "new.th"]);
        assert_eq!(args.overwrite, vec!["A".to_string()]);
        assert_eq!(args.thornlist, Some(PathBuf::from("new.th")));
    }
}
