//! CLI surface (spec §3). Global flags + the full command tree; topology
//! flags per §8.5, compute-node flags per §8.3.1, restart flags per §8.8.

use crate::walltime::Walltime;
use clap::{ArgAction, Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

// §5.1
/// One `-K NAME=VALUE` knob override, validated at parse time: the name must
/// be a knob identifier and, for a standard knob, the value must pass that
/// knob's own validation. `value` is the stored form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KnobOverride {
    pub name: String,
    pub value: String,
}

/// clap adapter for `-K`: `NAME=VALUE` (the `NAME VALUE` spelling is folded
/// into this form by [`normalize_knob_args`] before clap sees it).
fn parse_knob_override(s: &str) -> Result<KnobOverride, String> {
    let Some((name, value)) = s.split_once('=') else {
        return Err(format!(
            "expected NAME=VALUE (or NAME VALUE), got \"{s}\"; e.g. -K allocation=hpc_xxx"
        ));
    };
    let value = crate::database::knob_stored_form(name, value).map_err(|e| e.to_string())?;
    Ok(KnobOverride { name: name.to_owned(), value })
}

/// A flag cactup once accepted and has since renamed. Retired flags are never
/// declared to clap, so no help text, completion, or "did you mean" can
/// mention them; `retired_flag_notice` catches them on the raw arguments
/// before clap parses anything, so the user gets the rename notice whatever
/// else is wrong with the line. Keep these indefinitely — at least one user
/// still types `--variant` from habit.
struct RetiredFlag {
    /// The exact subcommand path the flag was accepted under. `build` alone
    /// is the bare `cactup build [<name>]`; `build run`/`build submit` are
    /// listed separately, so `build log --variant` stays an ordinary error.
    path: &'static [&'static str],
    flag: &'static str,
    retirement: Retirement,
}

enum Retirement {
    /// Renamed: the notice rewrites the flag to its new spelling.
    Renamed(&'static str),
    /// Removed with nothing in its place: the notice says why and drops the
    /// flag (and its value) from the suggested command.
    Removed(&'static str),
}

const RETIRED_FLAGS: &[RetiredFlag] = &[
    RetiredFlag { path: &["build"], flag: "--variant", retirement: Retirement::Renamed("--flavor") },
    RetiredFlag { path: &["build", "run"], flag: "--variant", retirement: Retirement::Renamed("--flavor") },
    RetiredFlag { path: &["build", "submit"], flag: "--variant", retirement: Retirement::Renamed("--flavor") },
    RetiredFlag { path: &["test", "run"], flag: "--variant", retirement: Retirement::Removed(TEST_SCRIPTS_BY_QUEUE) },
    RetiredFlag { path: &["test", "submit"], flag: "--variant", retirement: Retirement::Removed(TEST_SCRIPTS_BY_QUEUE) },
    RetiredFlag { path: &["machine", "show"], flag: "--variants", retirement: Retirement::Renamed("--flavors") },
];

const TEST_SCRIPTS_BY_QUEUE: &str =
    "the queue now picks the test scripts; choose a different queue with -q";

/// The rename notice for a retired flag on this command line, if any: the
/// command line rewritten with the replacement, for the user to re-run.
/// `args` includes the program name, as `std::env::args_os` yields it.
pub(crate) fn retired_flag_notice(args: &[OsString]) -> Option<String> {
    use clap::CommandFactory;
    let args: Vec<String> = args.iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let before_passthrough = &args[..end];

    // The subcommand path, found the way clap finds it: walk the command
    // tree, skipping flags (and the value of each that takes one, by the
    // definitions of the command reached so far — global flags included, as
    // they may sit at any level) and descending while a word names a
    // subcommand. The first word that does not is a positional, so the path
    // ends there.
    let root = Args::command();
    let takes_value = |cmd: &clap::Command, token: &str| {
        cmd.get_arguments().chain(root.get_arguments()).any(|a| {
            a.get_action().takes_values()
                && (a.get_long().is_some_and(|l| token == format!("--{l}"))
                    || a.get_short().is_some_and(|c| token == format!("-{c}")))
        })
    };
    let mut path: Vec<&str> = Vec::new();
    let mut cmd = &root;
    let mut i = 0;
    while i < before_passthrough.len() {
        let token = before_passthrough[i].as_str();
        if token.starts_with('-') {
            i += if takes_value(cmd, token) { 2 } else { 1 };
            continue;
        }
        match cmd.find_subcommand(token) {
            Some(sub) => {
                path.push(sub.get_name());
                cmd = sub;
                i += 1;
            }
            None => break,
        }
    }

    let retired = RETIRED_FLAGS.iter().find(|r| {
        r.path == path.as_slice()
            && before_passthrough.iter().any(|a| a == r.flag || a.starts_with(&format!("{}=", r.flag)))
    })?;

    let mut rewritten = vec!["cactup".to_owned()];
    let mut k = 0;
    while k < args.len() {
        let a = &args[k];
        let spelled = k < end && a == retired.flag;
        let joined = k < end && a.starts_with(&format!("{}=", retired.flag));
        match &retired.retirement {
            Retirement::Renamed(new) if spelled => rewritten.push(new.to_string()),
            Retirement::Renamed(new) if joined => {
                rewritten.push(shell_quote(&format!("{new}{}", &a[retired.flag.len()..])))
            }
            // A removed flag goes, and so does its value when it was given
            // as the next word.
            Retirement::Removed(_) if spelled => {
                if args.get(k + 1).is_some_and(|v| k + 1 < end && !v.starts_with('-')) {
                    k += 1;
                }
            }
            Retirement::Removed(_) if joined => {}
            _ => rewritten.push(shell_quote(a)),
        }
        k += 1;
    }
    let what = match &retired.retirement {
        Retirement::Renamed(new) => format!("{} has been renamed {new}", retired.flag),
        Retirement::Removed(why) => format!("{} has been removed: {why}", retired.flag),
    };
    Some(format!("{what}. Run instead:\n  {}", rewritten.join(" ")))
}

/// Single-quote `s` for a shell when it holds anything but plain word
/// characters, so a rewritten command line can be pasted back as-is.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_=./:,@+%".contains(c));
    if plain { s.to_owned() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// Fold the two-token spelling of a knob override — `-K NAME VALUE`,
/// `-KNAME VALUE`, `--knob NAME VALUE` — into the one-token `-K NAME=VALUE`
/// clap parses. A `NAME` that already carries `=` is left alone, as is
/// everything after a bare `--`. Non-UTF-8 arguments pass through untouched.
pub(crate) fn normalize_knob_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut out = Vec::new();
    let mut args = args.into_iter().peekable();
    let mut passthrough = false;
    while let Some(arg) = args.next() {
        if passthrough {
            out.push(arg);
            continue;
        }
        let Some(s) = arg.to_str() else {
            out.push(arg);
            continue;
        };
        if s == "--" {
            passthrough = true;
            out.push(arg);
            continue;
        }
        // The flag and its NAME may be one token (`-Kfoo`) or two (`-K foo`,
        // `--knob foo`); either way, a NAME without `=` takes the next
        // token as its VALUE.
        let (prefix, name) = if s == "-K" || s == "--knob" {
            match args.peek().and_then(|n| n.to_str()) {
                Some(n) if !n.contains('=') && !n.starts_with('-') => {
                    let name = args.next().expect("peeked");
                    (format!("{s} "), name.to_str().expect("checked utf-8").to_owned())
                }
                _ => {
                    out.push(arg);
                    continue;
                }
            }
        } else if let Some(name) = s.strip_prefix("-K").filter(|n| !n.is_empty() && !n.contains('=')) {
            ("-K".to_owned(), name.to_owned())
        } else {
            out.push(arg);
            continue;
        };
        let Some(value) = args.peek().and_then(|v| v.to_str()).filter(|v| !v.starts_with('-')) else {
            // No value to pair with: hand clap the pieces and let it complain.
            match prefix.strip_suffix(' ') {
                Some(flag) => {
                    out.push(flag.into());
                    out.push(name.into());
                }
                None => out.push(format!("{prefix}{name}").into()),
            }
            continue;
        };
        let value = value.to_owned();
        args.next();
        match prefix.strip_suffix(' ') {
            Some(flag) => {
                out.push(flag.into());
                out.push(format!("{name}={value}").into());
            }
            None => out.push(format!("{prefix}{name}={value}").into()),
        }
    }
    out
}

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

impl std::fmt::Display for MakeJobs {
    /// The `-j` word that parses back to this value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MakeJobs::Count(n) => write!(f, "{n}"),
            MakeJobs::Max => f.write_str("max"),
        }
    }
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
#[command(version = crate::build_info::LONG_VERSION)]
#[command(about = "The best way to install Cactus", long_about = None)]
pub(crate) struct Args {
    #[clap(flatten)]
    pub globals: GlobalOpts,
    #[clap(subcommand)]
    pub command: Commands,
}

impl Args {
    /// This command's knob overlay (§5.1): every `-K NAME=VALUE`, plus `-a
    /// ACCOUNT` as `allocation=ACCOUNT`, since `-a` is shorthand for `-K
    /// allocation=…`. Both are equally explicit, so a command that gives
    /// both with different values is refused before anything happens; the
    /// same value given both ways is fine.
    pub fn knob_overlay(&self) -> anyhow::Result<indexmap::IndexMap<String, String>> {
        let mut overlay: indexmap::IndexMap<String, String> =
            self.globals.knob.iter().map(|k| (k.name.clone(), k.value.clone())).collect();
        if let Some(account) = self.command.topology().and_then(|t| t.allocation.as_deref()) {
            match overlay.get("allocation") {
                Some(other) if other != account => anyhow::bail!(
                    // `--allocation`: `cactup import` spells it only that way.
                    "--allocation {account} and -K allocation={other} disagree; give the allocation one way"
                ),
                _ => {
                    overlay.insert("allocation".to_owned(), account.to_owned());
                }
            }
        }
        Ok(overlay)
    }
}

#[derive(clap::Args, Debug, Clone)]
pub(crate) struct GlobalOpts {
    /// Show more detail
    #[clap(short, long, global = true)]
    pub verbose: bool,
    /// Print every shell command that cactup runs to stderr.
    #[clap(long, global = true)]
    pub trace: bool,
    /// Einstein Toolkit manifest repository to read releases from
    #[clap(long, global = true, default_value = "https://bitbucket.org/einsteintoolkit/manifest.git")]
    pub manifest_url: String,
    // §2.2
    /// Override the system MDB location.
    #[clap(long, global = true, value_name = "PATH")]
    pub mdb_path: Option<PathBuf>,
    // §4.3
    /// Use this machine, skipping discovery entirely.
    #[clap(long, global = true, value_name = "NAME")]
    pub machine: Option<String>,
    /// Target this installation for one command instead of the active one.
    #[clap(short = 'I', long, global = true, value_name = "ALIAS")]
    pub installation: Option<String>,
    // §4.3
    /// Hostname for machine discovery (default: ~/.hostname, else the system FQDN).
    #[clap(long, global = true, value_name = "HOSTNAME")]
    pub hostname: Option<String>,
    // §5.1
    /// Override a knob for this command only (repeatable)
    ///
    /// Give NAME=VALUE or NAME VALUE. Also applies to `@KNOB(name)@` in
    /// scripts, optionlists and parfiles. A custom knob need not exist yet.
    #[clap(
        short = 'K',
        long = "knob",
        global = true,
        value_name = "NAME=VALUE",
        value_parser = parse_knob_override,
        action = ArgAction::Append
    )]
    pub knob: Vec<KnobOverride>,
    /// Accept untrusted and unreviewed code without asking
    ///
    /// Covers a stack or package at a branch tip (@head, @master), or a
    /// package at an unpublished commit (#commit), from a source
    /// not trusted by Cactarium. Also asks Cactarium for versions awaiting
    /// review, and for entries awaiting their first review when your token
    /// (cactup login) is an admin's. Your token (CACTARIUM_TOKEN, else the
    /// one from cactup login) is sent by add, remove, claims, import,
    /// install and inst refetch, to the Cactarium server named by
    /// the cactarium-url knob, whatever it is. Without this option those
    /// commands send no token. Nobody has reviewed such code. It may fetch
    /// and build arbitrary code. The warning is still printed.
    #[clap(long, global = true)]
    pub risky_allow_untrusted_artifacts: bool,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// List available Einstein Toolkit releases
    Releases {
        #[clap(short, long, help = "List all releases instead of only the few most recent.")]
        all: bool,
    },
    /// List Einstein Toolkit installations on this machine (same as `installation list`)
    List,
    /// Show the active installation, config and machine
    Show,
    /// Set the active Einstein Toolkit installation (same as `installation use`)
    Use {
        #[clap(help = "The alias of the installation to activate.")]
        alias: String,
    },
    /// Install an Einstein Toolkit release
    Install(InstallArgs),
    // §8.9
    /// Reproduce a .sim file
    ///
    /// Installs its sources, builds them, creates the simulation, and
    /// optionally submits or runs it.
    // `-a` is `--alias` here, as in `install`: the flattened TOPOLOGY
    // allocation keeps its long form only.
    #[command(mut_arg("allocation", |a| a.short(None)))]
    Import(ImportArgs),
    // §8.11
    /// Add Cactarium packages to the active installation
    ///
    /// Each version is added at the commit recorded for it by Cactarium.
    Add {
        /// Packages to add: owner/name, or owner/name/Thorn for one thorn
        ///
        /// Repeat owner/name/Thorn for several thorns. Choose a version with
        /// @version or @track:range, or a commit with #commit (7 or more hex
        /// digits).
        #[clap(required = true, value_name = "PACKAGE")]
        packages: Vec<String>,
        /// Replace a repo already in the thornlist.
        #[clap(long)]
        replace: bool,
        /// Add even if the thornlist has hand edits (they are overwritten).
        #[clap(short, long)]
        force: bool,
        /// Move installed packages to the versions needed by the plan
        ///
        /// Without it, a plan that needs an installed package at another
        /// version is refused.
        #[clap(long)]
        upgrade: bool,
        /// Move every installed package to its newest fitting version
        #[clap(long)]
        upgrade_all: bool,
        /// Add only the packages named, without their dependencies
        ///
        /// Libraries are still placed.
        #[clap(long)]
        no_deps: bool,
        /// Ignore a package's claims for this command (repeatable)
        ///
        /// OWNER/PKG ignores all of them. OWNER/PKG:KIND:TARGET ignores one;
        /// KIND is dependency, incompatible-with or fork-of.
        #[clap(long, value_name = "OWNER/PKG[:KIND:TARGET]", action = ArgAction::Append)]
        ignore_claim: Vec<String>,
        /// Do not ask for confirmation
        ///
        /// Without it, cactup asks when the plan goes beyond the packages
        /// named.
        #[clap(short, long)]
        yes: bool,
        /// Show the plan and change nothing.
        #[clap(long)]
        dry_run: bool,
    },
    // §8.11
    /// Remove packages added with `cactup add`
    ///
    /// Any repo replaced by a package is restored. Packages brought in only for
    /// the ones named are removed too, after the plan is shown. Refused while
    /// another package depends on one named.
    Remove {
        /// The packages: owner/name.
        #[clap(required = true, value_name = "PACKAGE")]
        packages: Vec<String>,
        /// Remove even if the thornlist has hand edits (they are overwritten).
        #[clap(short, long)]
        force: bool,
        /// Remove only the packages named, without checking dependents
        ///
        /// Packages brought in for them stay. No Cactarium is needed.
        #[clap(long)]
        no_deps: bool,
        /// Do not ask for confirmation
        ///
        /// Without it, cactup asks when packages brought in for the ones named
        /// would go too.
        #[clap(short, long)]
        yes: bool,
        /// Show the plan and change nothing.
        #[clap(long)]
        dry_run: bool,
    },
    // §4.4 of the dependency plan
    /// Inspect and correct package claims
    ///
    /// A claim is what a package says about another: a dependency, an
    /// incompatibility, a fork. An override changes a claim for this
    /// installation, and applies to every add and remove in it.
    #[clap(subcommand)]
    Claims(ClaimsCommand),
    // Plan §4.7
    /// Write or register a package's cactarium.toml
    #[clap(subcommand)]
    Package(PackageCommand),
    // §8.10
    /// Work with Cactarium stacks
    #[clap(subcommand)]
    Stack(StackCommand),
    // §8.10
    /// Ask Cactarium to check a package or stack for new releases now
    Refresh {
        /// The package or stack: owner/name.
        target: String,
    },
    // §8.10
    /// Submit downloads to Cactarium for review
    #[clap(subcommand)]
    Download(DownloadCommand),
    // §8.10
    /// Store a Cactarium API token
    ///
    /// Create a token on the Cactarium settings page.
    Login {
        /// The token (default: asked for).
        #[clap(long, value_name = "TOKEN")]
        token: Option<String>,
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
    /// Build a config, or manage build attempts (--help for the build cache)
    ///
    /// The build cache reuses objects compiled by earlier builds. These
    /// machine-global knobs control it:
    ///
    ///   build-cache          serve (default): reuse and store objects
    ///                        off: do not use the cache
    ///                        audit: serve, and check each served object
    ///                        against a fresh compile
    ///                        record: only measure, change nothing
    ///   build-cache-home     absolute path of the cache
    ///                        (default: .cactup-build-cache in the install-home)
    ///   build-cache-relocate yes (default): share objects across installations
    ///                        no: share only within this installation
    ///   build-cache-size     size such as 200G. A build reports when the
    ///                        cache exceeds it. Only `cactup cache gc`
    ///                        removes objects.
    ///   build-cache-preserve-exact-warnings
    ///                        false (default): show the warnings of the
    ///                        original compile (rarely, about another
    ///                        config's headers)
    ///                        true: show this build's own warnings, share
    ///                        fewer objects between configs
    ///
    /// Set them with `cactup knob`, or for one build with -K:
    ///
    ///   cactup knob build-cache off
    ///   cactup -K build-cache=audit build
    #[command(args_conflicts_with_subcommands = true)]
    #[clap(verbatim_doc_comment)]
    Build {
        #[clap(flatten)]
        start: Box<BuildStartArgs>,
        #[clap(subcommand)]
        command: Option<BuildCommand>,
    },
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
    ///
    /// Standard knobs (allocation, queue, mail, …) always exist. Custom knobs
    /// are read by @KNOB(name)@ in parfiles, scripts and optionlists. Create
    /// one with -c, remove it with `knob delete`. Custom knob names use
    /// lowercase letters, digits and inner dashes, and start with a letter.
    #[command(args_conflicts_with_subcommands = true)]
    Knob {
        /// Knob to print or set (omit to print all).
        name: Option<String>,
        /// Value to set (omit to print the knob).
        value: Option<String>,
        /// Create a custom knob (needed the first time one is set).
        #[clap(short, long)]
        custom: bool,
        #[clap(subcommand)]
        command: Option<KnobCommand>,
    },
    // §4
    /// Inspect and manage machine definitions
    #[clap(subcommand)]
    Machine(MachineCommand),
    // §16, §5
    /// Print one random piece of cactup wisdom (--help for how to configure)
    ///
    /// Wisdom may also appear after a command. Two machine-global knobs
    /// control this:
    ///
    ///   wisdom-frequency  How often wisdom appears after a command:
    ///                     off (never), rare (1 command in 15), normal
    ///                     (1 in 8, the default), chatty (1 in 4), or
    ///                     always.
    ///   wisdom-kind       Which entries are shown: relevant (cactup
    ///                     tips only) or all (the default: tips and
    ///                     quotes).
    ///
    /// Set them with `cactup knob`:
    ///
    ///   cactup knob wisdom-frequency chatty
    ///   cactup knob wisdom-kind relevant
    #[clap(verbatim_doc_comment)]
    Wisdom,
    // §18.5
    /// Inspect and look after the build cache
    #[clap(subcommand)]
    Cache(CacheCommand),
    // §17, §5
    /// Update cactup and its machine database (--help for how to configure)
    ///
    /// Installs the newest published build in $CACTUP_HOME/bin (default
    /// ~/.cactup/bin) and refreshes the machine database. Older builds are
    /// kept for queued jobs. A cactup built from source does not update
    /// itself. Use git pull and cargo build.
    ///
    /// cactup also checks for a newer build before an interactive command,
    /// at most once a day. These machine-global knobs control updating:
    ///
    ///   autoupdate  auto (default): install a newer build and continue in it
    ///               notify: only report it
    ///               off: do not check
    ///   update-url  where builds are published
    ///               (default: https://max-morris.github.io/Cactup)
    ///               Must be https. http is allowed only for 127.0.0.1,
    ///               localhost or [::1].
    ///   mdb-url     git repository with the machine database on its mdb branch
    ///               (default: https://github.com/max-morris/Cactup.git)
    ///
    /// Set them with `cactup knob`:
    ///
    ///   cactup knob autoupdate notify
    #[clap(verbatim_doc_comment)]
    Update {
        /// Only report the installed and published builds.
        #[clap(long, conflicts_with = "prune")]
        check: bool,
        /// Also remove builds retired over 30 days ago
        ///
        /// Never removes the current or running build. A job still using a
        /// removed build cannot start its next restart.
        #[clap(long)]
        prune: bool,
    },
}

// §5
#[derive(Subcommand, Debug)]
pub(crate) enum KnobCommand {
    /// Delete a custom knob, or unset a standard one (back to its default)
    Delete {
        /// The knob to delete or unset.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum InstallationCommand {
    /// List Einstein Toolkit installations on this machine
    List,
    /// Show the active installation, or a named one, in detail
    Show { alias: Option<String> },
    /// Set the active Einstein Toolkit installation
    Use { alias: String },
    /// Update repos, or move to a new thornlist or release
    Refetch(RefetchArgs),
    /// Show how the source trees have diverged from the last fetch
    Delta {
        /// Installation to inspect (default: the active one).
        alias: Option<String>,
    },
    // §3.1
    /// Delete an installation with its configs and builds
    ///
    /// Simulation and test output is kept.
    Delete {
        /// The alias of the installation to delete.
        alias: String,
        /// Do not ask, and delete even while a build is running.
        #[clap(short, long)]
        force: bool,
    },
    // §3.1
    /// Copy an installation to a new name
    ///
    /// Configs, builds, simulations and test runs are not copied.
    Cp {
        /// The alias of the installation to copy.
        source: String,
        /// The alias of the new installation.
        new_name: String,
        /// Directory to copy into (default: <install-home>/<new-name>).
        #[clap(long, value_name = "PATH")]
        install_prefix: Option<String>,
        /// Link files to the source instead of copying them
        ///
        /// Convert them to real files later with `cactup inst reify`. The
        /// source cannot be deleted while a symbolic copy depends on it.
        #[clap(short, long)]
        symbolic: bool,
    },
    // §3.1
    /// Replace a symbolic copy's links with real files
    Reify {
        /// The alias of the symbolic copy.
        alias: String,
        /// Reify only these paths (default: everything)
        ///
        /// Paths are absolute, relative to the current directory, or relative
        /// to the Cactus directory (repos/flesh). Reifying everything makes
        /// the copy independent of its source.
        paths: Vec<std::path::PathBuf>,
    },
}

#[derive(clap::Args, Debug)]
pub(crate) struct RefetchArgs {
    /// Thornlist file to fetch (default: the live thornlist).
    pub thornlist: Option<PathBuf>,
    /// Move to this release (see `cactup releases`)
    ///
    /// An Einstein Toolkit release tag, `master` for the tip of the
    /// manifest's master branch, or a Cactarium stack release
    /// (owner/stack@range). A bare owner/stack takes the newest release on
    /// the installation's current track.
    #[clap(long, value_name = "RELEASE", conflicts_with = "thornlist")]
    pub release: Option<String>,
    /// Skip all confirmations and checks
    ///
    /// Implies --overwrite-modified and --replace-thornlist.
    #[clap(short, long)]
    pub force: bool,
    /// Fetch over local modifications in all repos (they are backed up first).
    #[clap(long)]
    pub overwrite_modified: bool,
    /// Fetch over local modifications in these repos (repeatable)
    ///
    /// Separate names with spaces or commas. A name may be a repo under
    /// `repos/` or one of its thorns.
    #[clap(long, value_name = "NAMES")]
    pub overwrite: Vec<String>,
    /// Discard hand edits to the thornlist (a snapshot is kept).
    #[clap(long)]
    pub replace_thornlist: bool,
    /// Remove repos and thorn links no longer in the thornlist
    ///
    /// Asks once. -f skips the confirmation.
    #[clap(long)]
    pub prune: bool,
    /// Print only a count of skipped repos
    ///
    /// Unlike `install -s`, never answers prompts.
    #[clap(short, long)]
    pub silent: bool,
    /// Show what would happen and change nothing.
    #[clap(short = 'n', long, conflicts_with = "silent")]
    pub dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub(crate) struct InstallArgs {
    /// What to install (default: the most recent release)
    ///
    /// A release, "master" for the tip of the manifest's master branch, or a
    /// Cactarium stack: owner/stack, owner/stack@version or
    /// owner/stack@track:range.
    pub release: Option<String>,
    #[clap(long, value_name = "PATH", conflicts_with = "release", help = "Install from this thornlist file instead of a release.")]
    pub thornlist: Option<PathBuf>,
    // §8.9
    #[clap(long, value_name = "FILE", conflicts_with_all = ["release", "thornlist"], help = "Install the sources recorded in a .sim file, at their pinned commits.")]
    pub simfile: Option<PathBuf>,
    #[clap(short, long, help = "The unique name of the installation (default: the release name).")]
    pub alias: Option<String>,
    #[clap(short, long, help = "Use defaults instead of prompting.")]
    pub silent: bool,
    #[clap(long, help = "The prefix to install to.")]
    pub install_prefix: Option<String>,
    #[clap(long, help = "Skip creating a symlink.")]
    pub no_symlink: bool,
    #[clap(long, help = "The directory in which to create the symlink.")]
    pub symlink_prefix: Option<String>,
    #[clap(long, help = "The name of the symlink to create.")]
    pub symlink_name: Option<String>,
    /// Set by `import`, which settles the .sim's inputs before installing: the
    /// install then has no inputs left to point out.
    #[clap(skip)]
    pub inputs_settled: bool,
}

// §4.8
/// `--universe U | --no-universe`, shared by build/submit/run commands.
#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct UniverseFlags {
    // §4.8
    /// Run inside the named universe, overriding any default.
    #[clap(long, value_name = "UNIVERSE", conflicts_with = "no_universe")]
    pub universe: Option<String>,
    /// Force the host context, overriding any default universe.
    #[clap(long)]
    pub no_universe: bool,
}

impl UniverseFlags {
    /// These flags as the command line that parses back to them.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(u) = &self.universe {
            args.extend(["--universe".to_owned(), u.clone()]);
        }
        if self.no_universe {
            args.push("--no-universe".to_owned());
        }
        args
    }
}

// §8.5
/// The TOPOLOGY flag set, shared by `sim submit/run` and `test run/submit`.
#[derive(clap::Args, Debug, Clone)]
pub(crate) struct TopologyFlags {
    /// Account to charge (default: the allocation knob)
    ///
    /// Shorthand for -K allocation=ACCOUNT. Outranks the simulation's own
    /// allocation knob and the global one. A different -K allocation= as
    /// well is an error.
    #[clap(short, long, value_name = "ACCOUNT")]
    pub allocation: Option<String>,
    /// Scheduler queue (default: the queue knob, else the machine's default).
    #[clap(short, long, value_name = "QUEUE")]
    pub queue: Option<String>,
    /// Notification address (default: the mail knob).
    #[clap(short, long, value_name = "ADDRESS")]
    pub mail: Option<String>,
    /// Notification type (default: the mail-type knob, else `all`).
    #[clap(short = 'M', long, value_name = "TYPE")]
    pub mail_type: Option<String>,
    /// Node count (default: as many as the tasks need)
    ///
    /// Default for a queued job: ceil(tasks / tasks-per-node), when --tpn is
    /// given or the machine declares the node size. Inside a job allocation:
    /// its own node count (all of it without --tasks). Otherwise 1.
    #[clap(short, long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub nodes: Option<u32>,
    /// Total tasks (MPI ranks)
    ///
    /// Default: the run or submit script's `tasks` setting, else nodes *
    /// tasks-per-node. `test run` and `test submit` default to 2.
    #[clap(short = 'T', long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub tasks: Option<u32>,
    /// Tasks per node (default: fill the node)
    ///
    /// Default: floor(MAX_CPUS_PER_NODE / cpus), and on a GPU run no more
    /// than the node's GPUs can feed. With --tasks, the tasks spread evenly
    /// over the nodes.
    #[clap(short, long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub tpn: Option<u32>,
    /// CPUs (threads) per task (default: `default-cpus-per-task`, else 1).
    #[clap(short, long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub cpus: Option<u32>,
    /// Use GPUs (default: inferred from the queue's gpu flag).
    #[clap(short, long)]
    pub gpu: bool,
    /// GPUs per task on a GPU run (default: `default-gpus-per-task`, else 1).
    #[clap(short = 'G', long, value_name = "N")]
    pub gpus_per_task: Option<u32>,
    /// Job name (default: the simulation name).
    // `-J` (not `-j`): `-j`/`--make-jobs` already claims `-j` in `BuildOpts`,
    // which `build`'s subcommands flatten alongside this struct — `-J` is
    // SLURM's own spelling for a job name, and free across this CLI.
    #[clap(short = 'J', long, value_name = "NAME")]
    pub job_name: Option<String>,
    // §8.8
    /// Walltime for the whole simulation
    ///
    /// Split into chained jobs when it exceeds the queue's limit.
    #[clap(short, long, value_name = "(DD-)?HH:MM:SS", value_parser = parse_walltime)]
    pub wall_time: Option<Walltime>,
    /// stdout filename (default: template default).
    #[clap(short, long, value_name = "FILE")]
    pub out: Option<String>,
    /// stderr filename (default: template default).
    #[clap(short, long, value_name = "FILE")]
    pub err: Option<String>,
}

impl TopologyFlags {
    /// These flags as the command line that parses back to them (long
    /// forms, so it reads the same under `import`, where `-a` is `--alias`).
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let mut add = |flag: &str, value: Option<String>| {
            if let Some(value) = value {
                args.extend([format!("--{flag}"), value]);
            }
        };
        add("allocation", self.allocation.clone());
        add("queue", self.queue.clone());
        add("mail", self.mail.clone());
        add("mail-type", self.mail_type.clone());
        add("nodes", self.nodes.map(|n| n.to_string()));
        add("tasks", self.tasks.map(|n| n.to_string()));
        add("tpn", self.tpn.map(|n| n.to_string()));
        add("cpus", self.cpus.map(|n| n.to_string()));
        add("gpus-per-task", self.gpus_per_task.map(|n| n.to_string()));
        add("job-name", self.job_name.clone());
        add("wall-time", self.wall_time.map(|w| w.canonical()));
        add("out", self.out.clone());
        add("err", self.err.clone());
        if self.gpu {
            args.push("--gpu".to_owned());
        }
        args
    }
}

// §7.1, §7.6, §7.7
/// Build flags for `cactup build`.
#[derive(clap::Args, Debug)]
pub(crate) struct BuildOpts {
    /// Rebuild from scratch, even if up to date
    ///
    /// Runs `make <config>-realclean` first. Implies --ignore-machine.
    #[clap(short, long)]
    pub force: bool,
    // §7.4
    /// Rebuild a config that was built for another machine, at your own risk.
    #[clap(long)]
    pub ignore_machine: bool,
    /// Thornlist to build (default: the one that this config was last built from)
    ///
    /// A config never built uses <Cactus root>/thornlists/installation-default.th.
    #[clap(long, value_name = "PATH")]
    pub thornlist: Option<PathBuf>,
    // §4.4
    /// Optionlist flavor (required if the machine has several and no default).
    #[clap(long, value_name = "FLAVOR")]
    pub flavor: Option<String>,
    // §4.4, §7.8
    /// Build with this optionlist file instead of the machine's
    ///
    /// An MDB-style .toml (with a [cactup] header), an [options]-only .toml,
    /// or a native Cactus .cfg.
    #[clap(long, value_name = "PATH", conflicts_with = "flavor")]
    pub optionlist: Option<PathBuf>,
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
    // §7.8
    /// Reconfigure and rebuild, even if up to date
    ///
    /// Incremental where possible. -f always rebuilds from scratch.
    #[clap(long)]
    pub reconfig: bool,
    /// Clean the config before building.
    #[clap(long)]
    pub clean: bool,
    // §7.6
    /// Parallel make jobs (default: the machine's make-jobs, else 1)
    ///
    /// A number, or `max` for all threads available in the build context.
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
            ignore_machine: false,
            thornlist: None,
            flavor: None,
            optionlist: None,
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

// §7, §8.3.1
/// Shared surface of the bare `cactup build`, `build run`, and `build submit`.
#[derive(clap::Args, Debug)]
pub(crate) struct BuildStartArgs {
    /// The config to build (default: the active config).
    pub name: Option<String>,
    #[clap(flatten)]
    pub opts: BuildOpts,
    #[clap(flatten)]
    pub topology: TopologyFlags,
    // §8.3.1
    /// Config directory, for compute-node runs (requires --attempt-id).
    #[clap(long, value_name = "PATH", requires = "attempt_id")]
    pub config_dir: Option<PathBuf>,
    /// Build attempt to run, for compute-node runs (requires --config-dir).
    #[clap(long, value_name = "N", requires = "config_dir")]
    pub attempt_id: Option<u32>,
    // §7.9. Lives here rather than on `BuildSubmitArgs` so the bare `cactup
    // build` — which only decides between running and submitting once it has
    // read the MDB — can accept it too. `build run` accepts it as far as clap
    // is concerned and then rejects it by hand, which is what lets the error
    // say *why* there was no queue wait to do.
    /// Wait for the queued build to finish (submit only).
    #[clap(long)]
    pub block: bool,
}

#[derive(clap::Args, Debug)]
pub(crate) struct BuildSubmitArgs {
    #[clap(flatten)]
    pub start: BuildStartArgs,
    /// Stream the build's output until it finishes, or Ctrl-C.
    // Conflicts with --block rather than composing with it: on a machine that
    // declares [scheduler].blocking-submit the submit command holds the
    // terminal for the whole build, so there is nothing to stream alongside
    // it. A flag pair whose combinability depends on the MDB entry is worse
    // than one that simply never combines — and `--follow` already waits.
    #[clap(long, conflicts_with = "block")]
    pub follow: bool,
}

impl Commands {
    /// The TOPOLOGY flags (§8.5) of a command that takes them.
    pub fn topology(&self) -> Option<&TopologyFlags> {
        match self {
            Commands::Build { start, command: None } => Some(&start.topology),
            Commands::Build { command: Some(BuildCommand::Run(start)), .. } => Some(&start.topology),
            Commands::Build { command: Some(BuildCommand::Submit(submit)), .. } => Some(&submit.start.topology),
            Commands::Sim(SimCommand::Submit(submit)) => Some(&submit.start.topology),
            Commands::Sim(SimCommand::Run(run)) => Some(&run.start.topology),
            Commands::Test(TestCommand::Run(start) | TestCommand::Submit(start)) => Some(&start.topology),
            // What import passes to the `sim submit`/`sim run` it starts.
            Commands::Import(args) => Some(&args.topology),
            _ => None,
        }
    }
}

#[derive(Subcommand, Debug)]
pub(crate) enum BuildCommand {
    /// Build a config in the active installation
    Run(BuildStartArgs),
    /// Submit a build to the queue
    Submit(BuildSubmitArgs),
    /// List build attempts in the active installation
    List {
        #[clap(long, help = "Show extended per-attempt details.")]
        long: bool,
        #[clap(long, help = "List build attempts across every installation.")]
        all: bool,
    },
    /// Show one config's most recent build attempt in detail
    Show {
        /// Config to inspect (default: the active config).
        name: Option<String>,
        #[clap(long, help = "Show extended per-attempt details.")]
        long: bool,
    },
    /// Tail the build's stdout/stderr
    Log {
        /// Config to inspect (default: the active config).
        name: Option<String>,
        #[clap(short, long, conflicts_with_all = ["follow_out", "follow_err"], help = "Side-by-side live TUI of stdout and stderr, until Ctrl-C.")]
        follow: bool,
        #[clap(short = 'o', long, conflicts_with = "follow_err", help = "Stream only stdout (tail -f style), until Ctrl-C.")]
        follow_out: bool,
        #[clap(short = 'e', long, help = "Stream only stderr (tail -f style), until Ctrl-C.")]
        follow_err: bool,
    },
    /// Stop a running/queued build
    Stop {
        /// Config to stop (default: the active config).
        name: Option<String>,
        #[clap(short, long, help = "Kill the build process directly instead of a graceful stop.")]
        force: bool,
    },
    /// Remove old build attempts, keeping only the most recent ones
    Prune {
        /// Config to prune (default: the active config).
        name: Option<String>,
        #[clap(long, value_name = "N", help = "Number of most-recent attempts to keep.")]
        keep: Option<u32>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum CacheCommand {
    /// Summarize a build's recorded compiles, or compare two builds
    ///
    /// A build with the build-cache knob set to record logs the cache key
    /// of every Cactus source compile. This prints how many compiles got a
    /// key, why the others did not, and the time spent computing keys.
    ///
    /// With --against, --against-installation or --against-attempt, it also
    /// prints how many compiles could have been served by the other build,
    /// and which part of the key differs for the rest.
    ///
    ///   cactup cache report sim --against sim-debug
    ///   cactup cache report sim --against-installation other-install
    ///   cactup cache report sim --against-attempt 3
    #[clap(verbatim_doc_comment)]
    Report {
        /// Config whose build to report on (default: the active config).
        name: Option<String>,
        /// Build attempt to report on (default: the newest that recorded its compiles).
        #[clap(long, value_name = "N")]
        attempt: Option<u32>,
        /// Compare with a build of this config (default: the same config).
        #[clap(long, value_name = "CONFIG")]
        against: Option<String>,
        /// Compare with a build in this installation (default: the same installation).
        #[clap(long, value_name = "ALIAS")]
        against_installation: Option<String>,
        /// Compare with this build attempt (default: the newest other recorded one).
        #[clap(long, value_name = "N")]
        against_attempt: Option<u32>,
        /// Also list each compile that the other build would not serve, and why.
        #[clap(long)]
        long: bool,
    },
    /// Diagnose why a config's compiles bypass or miss the build cache
    ///
    /// First checks the newest build's setup against the cache rules:
    ///
    ///   the environment
    ///   compilers and flags (compiles small probes, builds nothing)
    ///   what each thorn's make files add to its compiles (read with
    ///   make, so $(shell ...) commands run)
    ///
    /// Each finding names its source and what to change. Compilers and
    /// flags are checked only if the newest build last configured the
    /// config.
    ///
    /// Then reads the compile log of the newest build that used the cache
    /// (or --attempt) and reports:
    ///
    ///   how many objects the cache served
    ///   compiles kept out, by reason, with a fix where known
    ///   objects cached from other sources, by the file that differed
    ///   objects built otherwise, by what differed
    ///   compiles whose files were compared whole, by reason
    ///
    /// The cache itself is not read.
    ///
    ///   cactup cache doctor sim
    #[clap(verbatim_doc_comment)]
    Doctor {
        /// Config whose build to look at (default: the active config).
        name: Option<String>,
        /// Build attempt to look at (default: the newest that went through the cache).
        #[clap(long, value_name = "N")]
        attempt: Option<u32>,
    },
    /// Show what the build cache holds
    ///
    /// For each machine in the cache directory (the build-cache-home knob):
    /// object count and size, recent use, and leftovers from interrupted
    /// builds.
    #[clap(verbatim_doc_comment)]
    Stats,
    /// Remove unused objects from the build cache
    ///
    /// The cache is never pruned automatically. --unused-for removes objects
    /// not used or stored by any build for that long. --to-size then removes the
    /// least recently used until the cache fits. Either may be given alone.
    ///
    ///   cactup cache gc --unused-for 60d
    ///   cactup cache gc --to-size 200G
    ///   cactup cache gc --unused-for 30d --to-size 200G --dry-run
    #[clap(verbatim_doc_comment)]
    Gc {
        /// Remove objects unused for this long (e.g. 60d)
        ///
        /// A number with h, d, w, m (30 days) or y.
        #[clap(long, value_name = "AGE")]
        unused_for: Option<String>,
        /// Then shrink the cache to this size, least recently used first (e.g. 200G).
        #[clap(long, value_name = "SIZE")]
        to_size: Option<String>,
        /// Show what would be removed.
        #[clap(long)]
        dry_run: bool,
    },
    /// Read every object in the build cache and remove the damaged ones
    Verify,
    /// List or remove inputs downloaded by cactup import
    ///
    /// Inputs live in inputs-home (the knob, else the machine's value, else
    /// ~/.cactup/inputs). Each is listed with its size and last change.
    /// --clear removes them all, and the next import fetches what it needs.
    /// Existing simulations keep pointing at their inputs, so clear only
    /// what no simulation still reads.
    ///
    ///   cactup cache inputs
    ///   cactup cache inputs --clear
    #[clap(verbatim_doc_comment)]
    Inputs {
        /// Remove every downloaded and unpacked input.
        #[clap(long)]
        clear: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCommand {
    /// List configs in the active installation
    List,
    /// Show a config's metadata (default: the active config)
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
pub(crate) enum ClaimsCommand {
    /// Show the overrides and the any-of choices
    List,
    #[clap(verbatim_doc_comment)]
    /// Check the installed packages against each other's claims
    ///
    /// Reports, one line each:
    ///   clashing thorns
    ///   incompatibilities
    ///   forks installed beside their upstream
    ///   unmet dependency claims
    ///   CCL requirements not met by any installed thorn
    ///   `satisfies` overrides naming a missing repo
    /// Changes nothing. Fails when it finds a problem.
    Check,
    /// Change or remove a claim override
    #[command(args_conflicts_with_subcommands = true, arg_required_else_help = true)]
    Override {
        /// Remove an override, named by its key as `claims list` shows it
        #[clap(long, value_name = "KEY")]
        remove: Option<String>,
        #[clap(subcommand)]
        action: Option<OverrideAction>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum OverrideAction {
    /// Drop a claim: a dependency, incompatible-with or fork-of
    ///
    /// The claim is dropped from every version of the package.
    ///
    ///   cactup claims override drop max/foo:incompatible-with:et/bar
    #[clap(verbatim_doc_comment)]
    Drop {
        /// The claim: OWNER/PKG:KIND:TARGET
        #[clap(value_name = "OWNER/PKG:KIND:TARGET")]
        claim: String,
        /// Why, shown by `claims list`.
        #[clap(long)]
        note: Option<String>,
    },
    /// Add a dependency that the package's manifest lacks
    ///
    ///   cactup claims override add max/foo --dependency et/carpet --needs CarpetSlab
    #[clap(verbatim_doc_comment)]
    Add {
        /// The package that depends: OWNER/PKG
        #[clap(value_name = "OWNER/PKG")]
        package: String,
        /// The package that it depends on: OWNER/PKG, or OWNER/PKG@RANGE.
        #[clap(long, value_name = "TARGET")]
        dependency: String,
        /// Thorns of the target that it needs (comma separated).
        #[clap(long, required = true, value_delimiter = ',', value_name = "A,B")]
        needs: Vec<String>,
        /// Thorns of the package that need it (default: all).
        #[clap(long, value_delimiter = ',', value_name = "T,U")]
        needed_by: Vec<String>,
    },
    /// Add thorns to a dependency's needs
    ///
    ///   cactup claims override needs max/foo:dependency:et/bar Loop
    #[clap(verbatim_doc_comment)]
    Needs {
        /// The dependency: OWNER/PKG:dependency:TARGET
        #[clap(value_name = "OWNER/PKG:dependency:TARGET")]
        claim: String,
        /// Thorns of the target to add.
        #[clap(required = true, value_name = "THORN")]
        thorns: Vec<String>,
    },
    /// Let one package stand in for another
    ///
    /// The first takes every need for the second, with no version check.
    /// Name an unregistered repo of the installation as local:NAME.
    ///
    ///   cactup claims override satisfies max/carpet-mp et/carpet
    ///   cactup claims override satisfies local:carpetx et/carpetx
    #[clap(verbatim_doc_comment)]
    Satisfies {
        /// The package that stands in: OWNER/PKG or local:NAME
        #[clap(value_name = "X")]
        by: String,
        /// The package that it stands in for: OWNER/PKG
        #[clap(value_name = "Y")]
        target: String,
    },
}

// Plan §4.7
#[derive(Subcommand, Debug)]
pub(crate) enum PackageCommand {
    /// Write a cactarium.toml for a repo of the installation
    ///
    /// Reads the repo's thorns and the active config's build, and writes
    /// cactarium.toml at the repo's root for you to review and commit: the
    /// package's name and owners, and a dependency on each installed package
    /// whose thorns meet the requirements of the repo's thorns. What cactup
    /// cannot decide is written as a TODO comment.
    ///
    /// With --publish, writes nothing: registers the cactarium.toml held by
    /// the repo's newest release tag on its remote with Cactarium (cactup
    /// login), or checks the package for new releases if it is registered
    /// already.
    Init(PackageInitArgs),
}

/// `cactup package init`.
#[derive(clap::Args, Debug)]
pub(crate) struct PackageInitArgs {
    /// The repo, by its name under repos/ (default: the one holding the
    /// current directory).
    pub repo: Option<String>,
    /// The package's name (default: the repo's name, lowercased).
    #[clap(long, conflicts_with = "publish")]
    pub name: Option<String>,
    /// The owner (default: your Cactarium user)
    ///
    /// With --publish: the namespace to register under, one of your
    /// organizations (default: your own).
    #[clap(long, value_name = "OWNER")]
    pub owner: Option<String>,
    /// Depend on exactly the version used by the build (=X.Y.Z, not ~X.Y.Z).
    #[clap(long, conflicts_with = "publish")]
    pub exact: bool,
    /// Overwrite an existing cactarium.toml.
    #[clap(short, long, conflicts_with = "publish")]
    pub force: bool,
    /// Register the cactarium.toml of the newest pushed release tag; write nothing.
    #[clap(long)]
    pub publish: bool,
}

// §8.10
#[derive(Subcommand, Debug)]
pub(crate) enum StackCommand {
    /// Write a cactarium-stack.toml for the installation
    ///
    /// Writes a stack that extends the stack release in use by the
    /// installation, pins the base repos moved since, and adds the packages
    /// added with `cactup add`: an override where one replaces every checkout
    /// of a base repo, else a pinned section of cactarium-stack.th. The
    /// dependencies declared by the installed packages follow, for
    /// information. Refused while a repo has uncommitted or unpushed changes.
    ///
    /// With --publish --version, writes nothing: publishes the
    /// cactarium-stack.toml found there as that release.
    Init(StackInitArgs),
    /// Publish a stack release to Cactarium
    Publish {
        /// The stack definition, a cactarium-stack.toml.
        definition: PathBuf,
        /// The stack: owner/name.
        target: String,
        /// The release's version: X.Y.Z, or track:X.Y.Z.
        #[clap(long, value_name = "VERSION")]
        version: String,
        /// The stack's own thornlist, when its definition is a flat list.
        #[clap(long, value_name = "PATH")]
        thornlist: Option<PathBuf>,
    },
}

/// `cactup stack init`.
#[derive(clap::Args, Debug)]
pub(crate) struct StackInitArgs {
    /// Where to write it (default: the current directory).
    pub dir: Option<PathBuf>,
    /// The stack's name (default: the installation's alias, lowercased).
    #[clap(long, conflicts_with = "publish")]
    pub name: Option<String>,
    /// The owner (default: your Cactarium user)
    ///
    /// With --publish: the stack's owner (default: the first in its owners).
    #[clap(long, value_name = "OWNER")]
    pub owner: Option<String>,
    /// Overwrite existing files.
    #[clap(short, long, conflicts_with = "publish")]
    pub force: bool,
    /// Publish the cactarium-stack.toml found there; write nothing.
    #[clap(long, requires = "version")]
    pub publish: bool,
    /// The release's version, with --publish: X.Y.Z, or track:X.Y.Z.
    #[clap(long, value_name = "VERSION", requires = "publish")]
    pub version: Option<String>,
}

/// `cactup download`.
// §8.10, plan §6 S9
#[derive(Subcommand, Debug)]
pub(crate) enum DownloadCommand {
    /// Submit a download to Cactarium for review
    ///
    /// Downloads the file once, computes its SHA-256, and submits the URL
    /// and the hash with your token (cactup login). Cactarium verifies the
    /// file, then an admin reviews it. A shared .sim may pin a download only
    /// once it is approved.
    Submit {
        /// The download's URL.
        url: String,
        /// For the reviewer: what the file is, and why it can be trusted.
        #[clap(long, value_name = "TEXT")]
        note: Option<String>,
    },
}

/// `cactup import`.
// §8.9
#[derive(clap::Args, Debug)]
pub(crate) struct ImportArgs {
    /// The .sim file, or a Cactarium sim (owner/name)
    ///
    /// Use owner/name/sim for one sim of an entry that holds several.
    /// Choose the version with @range or @=X.Y.Z.
    pub simfile: PathBuf,
    /// Alias of the new installation (default: the .sim's name).
    #[clap(short, long)]
    pub alias: Option<String>,
    /// Install new sources even if an installation already matches the pins.
    #[clap(long)]
    pub new_installation: bool,
    /// Name of the config to build (default: the .sim's name).
    #[clap(long, value_name = "NAME")]
    pub config: Option<String>,
    /// Name of the simulation (default: the .sim's name).
    #[clap(long, value_name = "NAME")]
    pub sim: Option<String>,
    /// Optionlist flavor (required if the machine has several and no default).
    #[clap(long, value_name = "FLAVOR")]
    pub flavor: Option<String>,
    // §7.6: passed to the import's build, which runs with or without --submit/--run.
    /// Parallel make jobs (default: the machine's make-jobs, else 1)
    ///
    /// A number, or `max` for all threads available in the build context.
    #[clap(long, short = 'j', value_name = "N|max", value_parser = parse_make_jobs)]
    pub make_jobs: Option<MakeJobs>,
    /// Set a parfile knob input (repeatable)
    ///
    /// Stored with the simulation, never as a global knob.
    #[clap(long = "input", value_name = "KNOB=VALUE")]
    pub inputs: Vec<String>,
    /// Submit the simulation once created
    ///
    /// Takes the options below, as `sim submit` does. The .sim's suggestion
    /// fills in tasks, CPUs and GPUs per task, and walltime.
    #[clap(long, conflicts_with = "run")]
    pub submit: bool,
    /// With --submit: wait for the simulation's jobs to finish, as `sim
    /// submit --block` does
    #[clap(long, requires = "submit")]
    pub block: bool,
    /// Run the simulation in the foreground once created
    ///
    /// Takes the options below, as `sim run` does.
    #[clap(long)]
    pub run: bool,
    /// The prefix to install to.
    #[clap(long)]
    pub install_prefix: Option<String>,
    /// Skip creating the installation's symlink.
    #[clap(long)]
    pub no_symlink: bool,
    /// Take every default without asking.
    #[clap(short, long)]
    pub yes: bool,
    // §8.9: what `sim submit`/`sim run` take, passed through to the one
    // started; the .sim's run hints fill in tasks, CPUs and GPUs per task
    // and wall time where these give none. `-a` is `--alias` here, so the
    // allocation is long-only (the `mut_arg` on `Commands::Import`).
    #[clap(flatten, next_help_heading = "With --submit or --run")]
    pub start_flags: StartFlags,
    #[clap(flatten, next_help_heading = "With --submit or --run")]
    pub universe: UniverseFlags,
    #[clap(flatten, next_help_heading = "With --submit or --run")]
    pub topology: TopologyFlags,
}

/// `cactup sim export`. Every field the .sim carries has a default; on a
/// terminal, export asks for each one not given by a flag (`--yes` takes
/// them all as they are).
// §8.9
#[derive(clap::Args, Debug)]
pub(crate) struct SimExportArgs {
    /// The simulation to export.
    pub sim: String,
    /// Where to write it (default: <sim>.sim in the current directory).
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub describe: SimDescribeArgs,
    /// Trim the thornlist to the thorns needed by the parfile
    ///
    /// Keeps the thorns activated by the parfile and what they require. The
    /// rest become #DISABLED lines.
    #[clap(long)]
    pub trim: bool,
}

/// `cactup sim pack`: a .sim from a loose thornlist and parfile.
// §8.9
#[derive(clap::Args, Debug)]
pub(crate) struct SimPackArgs {
    /// The thornlist (a GetComponents CRL list).
    pub thornlist: PathBuf,
    /// The parfile (.par, or computed .py).
    pub parfile: PathBuf,
    /// Where to write it (default: <parfile name>.sim in the current directory).
    pub file: Option<PathBuf>,
    #[command(flatten)]
    pub describe: SimDescribeArgs,
    /// Trim the thornlist to the thorns needed by the parfile
    ///
    /// Keeps the thorns activated by the parfile and what they require. The
    /// rest become #DISABLED lines.
    #[clap(long)]
    pub trim: bool,
}

/// What a .sim says about itself, shared by `sim export` and `sim pack`.
#[derive(clap::Args, Debug, Default, Clone)]
pub(crate) struct SimDescribeArgs {
    /// Overwrite the file if it exists.
    #[clap(short, long)]
    pub force: bool,
    /// Take every default without asking.
    #[clap(short, long)]
    pub yes: bool,
    /// Name for the .sim (default: the simulation or parfile name).
    #[clap(long, value_name = "NAME")]
    pub name: Option<String>,
    /// One-line summary (default: the first line of the parfile's leading comment).
    #[clap(long, value_name = "TEXT")]
    pub summary: Option<String>,
    /// Description, in Markdown (default: the parfile's leading comment).
    #[clap(long, value_name = "TEXT")]
    pub description: Option<String>,
    /// Author, repeatable (default: your git user.name).
    #[clap(long = "author", value_name = "NAME")]
    pub authors: Vec<String>,
    /// SPDX license expression (default: the default-license knob, if set).
    #[clap(long, value_name = "SPDX")]
    pub license: Option<String>,
    /// Contact for help, an https:// or mailto: URL (default: the default-contact knob).
    #[clap(long, value_name = "URL")]
    pub contact: Option<String>,
    /// Where an input's file can be downloaded (KNOB=URL; repeatable)
    ///
    /// The file is downloaded once and its SHA-256 recorded. An archive
    /// (.tar, .tar.gz, .tar.xz, .zip) is unpacked on import. The knob then
    /// points at the archive member that its current value ends with, or the
    /// one named by --input-path. Lines in the archive's text files that differ
    /// from your copy only by the unpack location become edits applied
    /// after unpacking.
    #[clap(long = "input-url", value_name = "KNOB=URL")]
    pub input_urls: Vec<String>,
    /// Archive member that an input knob points at (KNOB=MEMBER; repeatable).
    #[clap(long = "input-path", value_name = "KNOB=MEMBER")]
    pub input_paths: Vec<String>,
    /// Skip the checks for sharing: a .sim for this machine only
    ///
    /// Without it, these are refused:
    ///   uncommitted changes; commits on no remote branch or tag
    ///   repos of no package registered with Cactarium
    ///   repos that nothing pins: svn, cvs, hg, darcs, git with no commit,
    ///     several downloads in one section
    ///   ignore components
    ///   downloads with no hash, on a private address, or not approved
    ///   URLs that nobody else can fetch, or with credentials
    /// Each git remote and Cactarium are asked once, without credentials.
    #[clap(long, verbatim_doc_comment)]
    pub local: bool,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SimCommand {
    // §8.2
    /// Create a simulation from a parfile
    Create {
        #[clap(short, long, help = "Replace an existing simulation of the same name (implies --ignore-machine).")]
        force: bool,
        // §7.4
        /// Use a config that was built for another machine, at your own risk.
        #[clap(long)]
        ignore_machine: bool,
        /// The simulation name.
        sim: String,
        // §6.2
        /// The parfile (.par, or computed .py).
        parfile: PathBuf,
        /// Config to attach (default: active config).
        #[clap(long, value_name = "CONFIG")]
        config: Option<String>,
        // §8.1
        /// Simulation directory (default: under sim-home).
        #[clap(long, value_name = "PATH")]
        sim_dir: Option<PathBuf>,
    },
    // §8.3
    /// Submit a simulation to the queue (creates it if given a parfile)
    Submit(SimSubmitArgs),
    // §8.4
    /// Run a simulation interactively, bypassing the queue
    Run(SimRunArgs),
    // §8.6
    /// Stop a running/queued simulation
    Stop {
        sim: String,
        #[clap(short, long, help = "Kill the job through the scheduler instead of stopping it gracefully.")]
        force: bool,
    },
    // §8.6
    /// Clean up a simulation's aborted restarts
    Clean { sim: String },
    // §8.9 (Cactarium .sim files)
    /// Export a simulation as a .sim file
    ///
    /// Records its parfile and the exact sources and build that it ran on.
    Export(SimExportArgs),
    // §8.9
    /// Make a .sim file from a thornlist and a parfile
    ///
    /// Pins each repo to what an installation fetched (-I), or to each
    /// branch's tip today.
    Pack(SimPackArgs),
    // §8.10
    /// Publish a .sim file to Cactarium as a version of owner/name
    Publish {
        /// The .sim file; it must be fully specified.
        file: PathBuf,
        /// Where to publish it: owner/name.
        target: String,
        /// The version to publish it as, X.Y.Z (default: the next major version).
        #[clap(long, value_name = "X.Y.Z")]
        version: Option<String>,
    },
    // §8.9
    /// Unpack a .sim file into a thornlist and a parfile
    ///
    /// Pins become !REPO_COMMIT lines in the thornlist.
    Unpack {
        /// The .sim file.
        file: PathBuf,
        /// Where to write them (default: the current directory).
        #[clap(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Overwrite files that exist.
        #[clap(short, long)]
        force: bool,
    },
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
    /// Parfile (creates the simulation if it does not exist).
    pub parfile: Option<PathBuf>,
    /// Config for the implicit create (default: active config).
    #[clap(long, value_name = "CONFIG")]
    pub config: Option<String>,
    /// Skip all confirmations and checks
    ///
    /// Implies --overwrite, --force-queue and --ignore-machine.
    #[clap(short, long)]
    pub force: bool,
    /// Replace an existing simulation on implicit create.
    #[clap(long)]
    pub overwrite: bool,
    // §4.4
    /// Do not warn that the sources changed since the config was built.
    #[clap(short, long)]
    pub silent: bool,
    #[clap(flatten)]
    pub start_flags: StartFlags,
    #[clap(flatten)]
    pub universe: UniverseFlags,
    #[clap(flatten)]
    pub topology: TopologyFlags,
}

// §4.4, §7.4, §8.8
/// The start flags shared by `sim submit`, `sim run` and `import`, beside the
/// TOPOLOGY and universe flags.
#[derive(clap::Args, Debug, Clone, Default)]
pub(crate) struct StartFlags {
    // §4.4
    /// Bypass the optionlist↔queue compatibility check.
    #[clap(long)]
    pub force_queue: bool,
    // §7.4
    /// Use a config or simulation that was built for another machine, at
    /// your own risk.
    #[clap(long)]
    pub ignore_machine: bool,
    // §8.8
    /// Time reserved for the final checkpoint (default: max(wall/24, 10 min))
    ///
    /// Sets @CHECKPOINT_WALLTIME@ to the walltime minus this buffer.
    #[clap(long, value_name = "(DD-)?HH:MM:SS", value_parser = parse_walltime)]
    pub checkpt_buffer: Option<Walltime>,
}

impl StartFlags {
    /// These flags as the command line that parses back to them.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.force_queue {
            args.push("--force-queue".to_owned());
        }
        if self.ignore_machine {
            args.push("--ignore-machine".to_owned());
        }
        if let Some(w) = self.checkpt_buffer {
            args.extend(["--checkpt-buffer".to_owned(), w.canonical()]);
        }
        args
    }
}

#[derive(clap::Args, Debug)]
pub(crate) struct SimSubmitArgs {
    #[clap(flatten)]
    pub start: SimStartArgs,
    // §8.3. Not on `SimStartArgs`: `sim run` already runs in the foreground.
    /// Wait for the simulation's jobs to finish before returning, every
    /// chained one included; fails if a run fails.
    #[clap(long)]
    pub block: bool,
}

#[derive(clap::Args, Debug)]
pub(crate) struct SimRunArgs {
    #[clap(flatten)]
    pub start: SimStartArgs,
    /// Launch under the debugger (@RUNDEBUG@/@DEBUGGER@).
    #[clap(long)]
    pub debug: bool,
    // §8.3.1, §8.8
    /// Restart to run, for compute-node runs (requires --sim-dir).
    #[clap(long, value_name = "N", requires = "sim_dir")]
    pub restart_id: Option<u32>,
    // §8.3.1
    /// Absolute simulation directory, for compute-node runs (requires --restart-id).
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
    /// Remove test output left in the Cactus tree
    ///
    /// Removes TEST/ and configs/<cfg>/TEST.
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
    /// Skip all confirmations and checks
    ///
    /// Implies --overwrite, --force-queue and --ignore-machine.
    #[clap(short, long)]
    pub force: bool,
    /// Replace an existing test run of the same name.
    #[clap(long)]
    pub overwrite: bool,
    // §4.4
    /// Bypass the optionlist↔queue compatibility check.
    #[clap(long)]
    pub force_queue: bool,
    // §7.4
    /// Use a config that was built for another machine, at your own risk.
    #[clap(long)]
    pub ignore_machine: bool,
    /// Do not warn that the sources changed since the config was built.
    #[clap(short, long)]
    pub silent: bool,
    #[clap(flatten)]
    pub universe: UniverseFlags,
    #[clap(flatten)]
    pub topology: TopologyFlags,
    // §11.3
    /// Tests to run (default: all)
    ///
    /// Test names, thorns (Thorn or arrangement/Thorn), Thorn/test pairs, or
    /// arrangements. A name that is both an arrangement and a thorn selects
    /// the arrangement.
    pub tests: Vec<String>,
    // §11.6
    /// Absolute test-run directory, for compute-node runs (requires --results-id).
    #[clap(long, value_name = "PATH", requires = "results_id")]
    pub test_dir: Option<PathBuf>,
    /// Results id to run, for compute-node runs (requires --test-dir).
    #[clap(long, value_name = "N", requires = "test_dir")]
    pub results_id: Option<u32>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum MachineCommand {
    /// List all machines in the MDB
    List,
    // §4.3
    /// Show the machine that this host resolves to, or a named one
    Show {
        name: Option<String>,
        /// List the machine's optionlist flavors instead of its summary.
        #[clap(long)]
        flavors: bool,
    },
    // §4.7
    /// Save a local machine definition to the user MDB
    Create {
        /// Machine name (default: this host's short hostname).
        name: Option<String>,
        /// Start from BASE instead of `generic` (no value: the detected machine).
        #[clap(long, value_name = "BASE", num_args = 0..=1)]
        from_existing: Option<Option<String>>,
        /// Take defaults instead of prompting.
        #[clap(long)]
        silent: bool,
        /// Skip host matching (select the machine with --machine only).
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
    use std::path::Path;

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
            vec!["cactup", "inst", "refetch", "--release", "master", "-f"],
            vec!["cactup", "inst", "refetch", "--release", "max/carpetx-mp"],
            vec!["cactup", "add", "max/carpetx/TestReal4", "max/carpetx/Loop", "--replace"],
            vec!["cactup", "remove", "max/carpetx", "max/spacetimex"],
            vec!["cactup", "refresh", "max/carpetx"],
            vec!["cactup", "import", "max/bhns@1", "--new-installation"],
            vec!["cactup", "installation", "refetch", "new.th", "--overwrite-modified", "--prune", "-n"],
            vec!["cactup", "inst", "refetch", "--overwrite", "SpacetimeX Cottonmouth", "-n"],
            vec!["cactup", "inst", "refetch", "--overwrite", "SpacetimeX", "--overwrite", "Cottonmouth", "-n"],
            vec!["cactup", "inst", "delta"],
            vec!["cactup", "installation", "delta", "et"],
            vec!["cactup", "config", "delta"],
            vec!["cactup", "config", "delta", "sim"],
            vec!["cactup", "install", "ET_2025_05", "--silent"],
            vec!["cactup", "install", "master", "--silent"],
            vec!["cactup", "install", "--thornlist", "my/list.th", "--silent"],
            vec!["cactup", "inst", "delete", "old"],
            vec!["cactup", "installation", "delete", "old", "--force"],
            // A bare positional beside an optional subcommand is the likeliest
            // thing to break here: "sim" must land as the config positional,
            // not be mistaken for (or reported as) an unknown subcommand. See
            // `build_bare_positional_is_not_mistaken_for_a_subcommand` below.
            vec!["cactup", "build", "sim", "--flavor", "cuda", "--unsafe", "-j", "8"],
            vec!["cactup", "build", "sim", "--optionlist", "/tmp/my.cfg"],
            vec!["cactup", "build", "sim", "--ignore-machine"],
            vec!["cactup", "build", "run", "sim", "--flavor", "cuda", "--universe", "et-sif"],
            vec![
                "cactup", "build", "run", "--config-dir", "/inst/configs/sim", "--attempt-id", "2",
            ],
            vec!["cactup", "build", "submit", "sim", "--follow"],
            vec!["cactup", "build", "submit", "sim", "--block"],
            // §7.9: --block rides on the shared start args, so the bare form
            // (which only learns whether it is submitting after reading the
            // MDB) has to accept it at parse time too.
            vec!["cactup", "build", "sim", "--block"],
            vec!["cactup", "build", "list", "--long", "--all"],
            vec!["cactup", "build", "show", "sim", "--long"],
            vec!["cactup", "build", "log", "sim", "--follow"],
            vec!["cactup", "build", "stop", "sim", "-f"],
            vec!["cactup", "build", "prune", "sim", "--keep", "3"],
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

    fn normalized(argv: &[&str]) -> Vec<String> {
        normalize_knob_args(argv.iter().map(OsString::from))
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn knob_override_spellings_normalize_to_name_equals_value() {
        // Two-token spellings fold into one.
        assert_eq!(
            normalized(&["cactup", "-K", "queue", "gpu", "sim", "list"]),
            ["cactup", "-K", "queue=gpu", "sim", "list"]
        );
        assert_eq!(
            normalized(&["cactup", "-Kqueue", "gpu", "sim", "list"]),
            ["cactup", "-Kqueue=gpu", "sim", "list"]
        );
        assert_eq!(
            normalized(&["cactup", "--knob", "queue", "gpu", "x"]),
            ["cactup", "--knob", "queue=gpu", "x"]
        );
        // Already-joined spellings are untouched — including a value that
        // itself contains '='.
        assert_eq!(
            normalized(&["cactup", "-K", "queue=gpu", "sim"]),
            ["cactup", "-K", "queue=gpu", "sim"]
        );
        assert_eq!(normalized(&["cactup", "-Kqueue=a=b", "sim"]), ["cactup", "-Kqueue=a=b", "sim"]);
        assert_eq!(
            normalized(&["cactup", "--knob=queue=gpu", "sim"]),
            ["cactup", "--knob=queue=gpu", "sim"]
        );
        // A flag-like next token is never swallowed as the value, and `--`
        // ends processing.
        assert_eq!(normalized(&["cactup", "-K", "queue", "-v"]), ["cactup", "-K", "queue", "-v"]);
        assert_eq!(
            normalized(&["cactup", "--", "-K", "queue", "gpu"]),
            ["cactup", "--", "-K", "queue", "gpu"]
        );
        // Repeatable, anywhere in the line.
        assert_eq!(
            normalized(&[
                "cactup", "sim", "submit", "bbh", "-K", "queue", "gpu", "-K", "kadath-initial-data=/x",
            ]),
            ["cactup", "sim", "submit", "bbh", "-K", "queue=gpu", "-K", "kadath-initial-data=/x"]
        );
    }

    #[test]
    fn knob_override_parses_and_validates() {
        let args = Args::try_parse_from([
            "cactup", "-K", "queue=gpu", "-K", "kadath-initial-data=/x y", "sim", "list",
        ])
        .unwrap();
        assert_eq!(
            args.globals.knob,
            [
                KnobOverride { name: "queue".into(), value: "gpu".into() },
                KnobOverride { name: "kadath-initial-data".into(), value: "/x y".into() },
            ]
        );
        // Global: accepted after the subcommand too.
        assert!(Args::try_parse_from(["cactup", "sim", "list", "-K", "queue=gpu"]).is_ok());
        // A standard knob's own validation applies; a bad name is rejected.
        let parse_err = |argv: &[&str]| Args::try_parse_from(argv).unwrap_err().to_string();
        let err = parse_err(&["cactup", "-K", "wisdom-frequency=loud", "wisdom"]);
        assert!(err.contains("Invalid wisdom-frequency value"), "{err}");
        assert!(Args::try_parse_from(["cactup", "-K", "wisdom-frequency=chatty", "wisdom"]).is_ok());
        let err = parse_err(&["cactup", "-K", "Bad_Name=1", "wisdom"]);
        assert!(err.contains("not a valid knob name"), "{err}");
        // What the updater installs is only ever fetched over https.
        let err = parse_err(&["cactup", "-K", "update-url=http://mirror.example.org", "update"]);
        assert!(err.contains("https is required"), "{err}");
        let err = parse_err(&["cactup", "-K", "queue", "wisdom"]);
        assert!(err.contains("expected NAME=VALUE"), "{err}");
        // `knob -c` creates a custom knob.
        let parsed = Args::try_parse_from(["cactup", "knob", "-c", "kadath-initial-data", "/x"]).unwrap();
        match parsed.command {
            Commands::Knob { name, value, custom, command: None } => {
                assert_eq!(
                    (name.as_deref(), value.as_deref(), custom),
                    (Some("kadath-initial-data"), Some("/x"), true)
                );
            }
            other => panic!("{other:?}"),
        }
        // `knob delete` is a subcommand beside the positionals; a knob name
        // that is not a subcommand still lands as the positional.
        let parsed = Args::try_parse_from(["cactup", "knob", "delete", "kadath-initial-data"]).unwrap();
        match parsed.command {
            Commands::Knob { command: Some(KnobCommand::Delete { name }), .. } => {
                assert_eq!(name, "kadath-initial-data");
            }
            other => panic!("{other:?}"),
        }
        let parsed = Args::try_parse_from(["cactup", "knob", "allocation", "hpc_xxx"]).unwrap();
        assert!(matches!(parsed.command, Commands::Knob { command: None, .. }));
        assert!(Args::try_parse_from(["cactup", "knob", "delete", "x", "-c"]).is_err());
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

    fn notice(line: &[&str]) -> Option<String> {
        let argv: Vec<OsString> = std::iter::once("cactup").chain(line.iter().copied()).map(OsString::from).collect();
        retired_flag_notice(&argv)
    }

    /// Every place `--variant` (or `--variants`) used to be accepted stops
    /// with the rename notice and the corrected command line — in either
    /// spelling, bare or with a value, after global flags, and whatever else
    /// on the line clap would have rejected.
    #[test]
    fn retired_flags_short_circuit_with_the_rewritten_command() {
        let cases: &[(&[&str], &str)] = &[
            (&["build", "sim", "--variant", "cuda", "-j", "8"], "cactup build sim --flavor cuda -j 8"),
            (&["build", "run", "--variant=cpu"], "cactup build run --flavor=cpu"),
            (&["build", "submit", "sim", "--variant", "cpu", "-q", "single"], "cactup build submit sim --flavor cpu -q single"),
            (&["--machine", "qbd", "build", "--variant"], "cactup --machine qbd build --flavor"),
            (&["build", "--variant", "x", "--optionlist", "y.cfg", "--bogus"], "cactup build --flavor x --optionlist y.cfg --bogus"),
            (&["test", "run", "--variant", "test", "foo"], "cactup test run foo"),
            (&["test", "submit", "--variant=t", "-q", "gpu"], "cactup test submit -q gpu"),
            (&["machine", "show", "qbd", "--variants"], "cactup machine show qbd --flavors"),
            (&["test", "--machine", "qbd", "run", "--variant", "t"], "cactup test --machine qbd run"),
            (&["build", "--thornlist", "my list.th", "--variant", "a"], "cactup build --thornlist 'my list.th' --flavor a"),
        ];
        for (line, rewritten) in cases {
            let notice = notice(line).unwrap_or_else(|| panic!("no notice for {line:?}"));
            assert!(notice.ends_with(&format!("\n  {rewritten}")), "{line:?}: {notice}");
        }
        assert!(notice(&["build", "--variant", "x"]).unwrap().starts_with("--variant has been renamed --flavor"));
        assert!(notice(&["test", "run", "--variant", "x"]).unwrap().starts_with("--variant has been removed: the queue now picks"));
    }

    /// Only where the flag was really accepted: elsewhere it stays an
    /// ordinary clap error, and nothing after `--` is ever inspected.
    #[test]
    fn retired_flags_are_left_alone_elsewhere() {
        for line in [
            &["sim", "submit", "s", "--variant", "x"][..],
            &["test", "list", "--variant", "x"],
            &["machine", "show", "--variant"],
            &["build", "--flavor", "x"],
            &["sim", "run", "s", "--", "--variant"],
            &["--machine", "build", "sim", "submit", "--variant", "x"],
            &["build", "log", "sim", "--variant", "x"],
            &["build", "list", "--variant", "x"],
        ] {
            assert_eq!(notice(line), None, "{line:?}");
        }
    }

    /// The retired spellings appear in no help text anywhere: they are never
    /// declared to clap, and nothing else in the help may say "variant".
    #[test]
    fn no_help_text_mentions_variant() {
        use clap::CommandFactory;
        fn walk(cmd: &mut clap::Command, path: &str) {
            let help = cmd.render_long_help().to_string();
            assert!(!help.to_lowercase().contains("variant"), "`{path}` help mentions variant:\n{help}");
            for sub in cmd.get_subcommands_mut() {
                let name = format!("{path} {}", sub.get_name());
                walk(sub, &name);
            }
        }
        let mut root = Args::command();
        root.build();
        walk(&mut root, "cactup");
    }

    /// `Commands::Build`'s optional subcommand sits beside a positional
    /// (`<config>`) — the likeliest spot for clap to mistake one for the
    /// other. "sim" here must land as the config, not be treated as an
    /// attempted (and unknown) subcommand name.
    #[test]
    fn build_bare_positional_is_not_mistaken_for_a_subcommand() {
        let args =
            Args::try_parse_from(["cactup", "build", "sim", "--flavor", "cuda", "--unsafe", "-j", "8"])
                .unwrap_or_else(|e| panic!("failed to parse: {e}"));
        match args.command {
            Commands::Build { start, command: None } => {
                assert_eq!(start.name.as_deref(), Some("sim"));
                assert_eq!(start.opts.flavor.as_deref(), Some("cuda"));
                assert!(start.opts.unsafe_build);
                assert_eq!(start.opts.make_jobs, Some(MakeJobs::Count(8)));
            }
            other => panic!("expected a bare build command, got {other:?}"),
        }
    }

    /// `-a ACCOUNT` is shorthand for `-K allocation=ACCOUNT` (§5.1): it joins
    /// the knob overlay on every command with TOPOLOGY flags; given both
    /// ways it must agree.
    #[test]
    fn a_is_the_allocation_knob_for_this_command() {
        let overlay = |argv: &[&str]| Args::try_parse_from(argv).unwrap().knob_overlay();
        let alloc = |argv: &[&str]| overlay(argv).unwrap().get("allocation").cloned();
        for argv in [
            &["cactup", "sim", "submit", "s", "-a", "acct"][..],
            &["cactup", "sim", "run", "s", "-a", "acct"],
            &["cactup", "test", "submit", "-a", "acct"],
            &["cactup", "test", "run", "-a", "acct"],
            &["cactup", "build", "-a", "acct"],
            &["cactup", "build", "run", "-a", "acct"],
            &["cactup", "build", "submit", "-a", "acct"],
        ] {
            assert_eq!(alloc(argv).as_deref(), Some("acct"), "{argv:?}");
        }
        assert_eq!(alloc(&["cactup", "-K", "allocation=acct", "sim", "submit", "s"]).as_deref(), Some("acct"));
        assert_eq!(alloc(&["cactup", "sim", "submit", "s"]), None);
        // The same value both ways is fine; other knobs ride along.
        let both = overlay(&["cactup", "-K", "allocation=acct", "-K", "queue=gpu", "sim", "submit", "s", "-a", "acct"])
            .unwrap();
        assert_eq!(both.get("allocation").map(String::as_str), Some("acct"));
        assert_eq!(both.get("queue").map(String::as_str), Some("gpu"));
        // Different values are refused, naming both.
        let err = overlay(&["cactup", "-K", "allocation=other", "sim", "submit", "s", "-a", "acct"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--allocation acct") && err.contains("-K allocation=other"), "{err}");
        // `cactup import` passes its --allocation to the sim it starts, so
        // the same holds there (where `-a` is the alias, not the account).
        assert_eq!(alloc(&["cactup", "import", "s.sim", "--submit", "--allocation", "acct"]).as_deref(), Some("acct"));
        assert_eq!(alloc(&["cactup", "import", "s.sim", "--submit", "-a", "mine"]), None);
        assert_eq!(
            alloc(&["cactup", "-K", "allocation=acct", "import", "s.sim", "--submit", "--allocation", "acct"]).as_deref(),
            Some("acct")
        );
        let err = overlay(&["cactup", "-K", "allocation=other", "import", "s.sim", "--submit", "--allocation", "acct"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--allocation acct") && err.contains("-K allocation=other"), "{err}");
    }

    #[test]
    fn rejects_contradictory_flags() {
        // --universe and --no-universe are mutually exclusive (§4.8).
        assert!(Args::try_parse_from(["cactup", "build", "c", "--universe", "u", "--no-universe"]).is_err());
        // --block and --follow both wait for the queued build; only --follow
        // streams it, and on a machine with [scheduler].blocking-submit there
        // is nothing to stream alongside the blocking command (§7.9).
        assert!(
            Args::try_parse_from(["cactup", "build", "submit", "c", "--block", "--follow"]).is_err()
        );
        // The build compute-node pair is all-or-nothing (§8.3.1), same shape
        // as sim run's below.
        assert!(Args::try_parse_from(["cactup", "build", "run", "--config-dir", "/x"]).is_err());
        assert!(Args::try_parse_from(["cactup", "build", "run", "--attempt-id", "3"]).is_err());
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
        // --optionlist displaces the machine's flavor selection entirely, so
        // pairing it with --flavor is a contradiction, not a refinement.
        assert!(
            Args::try_parse_from([
                "cactup", "build", "sim", "--optionlist", "x.cfg", "--flavor", "cuda"
            ])
            .is_err()
        );
        // `sim submit --block` waits for the queued jobs (§8.3); `sim run`
        // runs in the foreground, so it has no such flag.
        assert!(Args::try_parse_from(["cactup", "sim", "submit", "s", "x.par", "--block"]).is_ok());
        assert!(Args::try_parse_from(["cactup", "sim", "run", "s", "--block"]).is_err());
    }

    /// `--optionlist` names a user-supplied file in place of the machine's
    /// own flavors; it must land on `BuildOpts` untouched so `build/mod.rs`
    /// can load it (§4.4, §7.8).
    #[test]
    fn optionlist_flag_parses_into_build_opts() {
        let args = Args::try_parse_from(["cactup", "build", "sim", "--optionlist", "/tmp/my.cfg"])
            .unwrap_or_else(|e| panic!("failed to parse: {e}"));
        match args.command {
            Commands::Build { start, command: None } => {
                assert_eq!(start.opts.optionlist.as_deref(), Some(Path::new("/tmp/my.cfg")));
            }
            other => panic!("expected a bare build command, got {other:?}"),
        }
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
