//! Job submission & scheduler abstraction (spec §10, §8.6).
//!
//! Everything is driven by the machine `meta.toml` scheduler keys: `submit`
//! (job id parsed via `submit-pattern` group 1), its optional wait-for-the-job
//! flavour `blocking-submit`, `get-status` classified by the `*-pattern`
//! regexes into R/Q/H/U/E, and `stop`. Status is queried live, never stored
//! (D4).

// Consumed by the Phase-3 SIM/TEST streams; unused until then.

use crate::mdb::meta::{Meta, Phase, Universe, WrappedCommand};
use crate::template::VarSet;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use regex::Regex;
use std::collections::HashMap;
use std::process::Command;

/// The number of parallel `get_statuses` workers; bounded because the
/// scheduler is a shared daemon (e.g. `slurmctld`), and a long simulation
/// history shouldn't hammer it with unbounded concurrent status queries.
const STATUS_WORKERS: usize = 8;

/// Live job status, per simfactory's letters (`simfactory-docs.txt` §14.7):
/// Running / Queued / Holding / Unknown (not in the queue) / Error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Queued,
    Holding,
    Unknown,
    Error,
}

impl JobStatus {
    pub fn letter(&self) -> char {
        match self {
            JobStatus::Running => 'R',
            JobStatus::Queued => 'Q',
            JobStatus::Holding => 'H',
            JobStatus::Unknown => 'U',
            JobStatus::Error => 'E',
        }
    }
}

/// What `sim list`/`show` displays for a simulation (§8.6, §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayState {
    /// Non-active, dependency-gated member of a pre-submitted chain (Q/H).
    Presubmitted,
    Running,
    Queued,
    Holding,
    /// Active restart with a live-but-idle job state (e.g. between chained
    /// segments): has an active restart, no queue presence, not terminated.
    Active,
    /// Active restart, job gone (U), termination reached.
    Finished,
    Error,
    /// No active restart.
    Inactive,
}

/// The §8.6 display-state derivation. `status` is `None` when the restart has
/// no job id at all; `chained` = it carries a `chained-job-id` (§9.3);
/// `terminated` = the run reached its termination condition.
pub fn display_state(
    has_active_restart: bool,
    status: Option<JobStatus>,
    chained: bool,
    terminated: bool,
) -> DisplayState {
    if !has_active_restart {
        return match status {
            Some(JobStatus::Queued | JobStatus::Holding) if chained => DisplayState::Presubmitted,
            _ => DisplayState::Inactive,
        };
    }
    match status {
        Some(JobStatus::Running) => DisplayState::Running,
        Some(JobStatus::Queued) => DisplayState::Queued,
        Some(JobStatus::Holding) => DisplayState::Holding,
        Some(JobStatus::Error) => DisplayState::Error,
        Some(JobStatus::Unknown) | None => {
            if terminated {
                DisplayState::Finished
            } else {
                DisplayState::Active
            }
        }
    }
}

/// Scheduler driver for one machine.
pub struct Scheduler<'m> {
    meta: &'m Meta,
}

impl<'m> Scheduler<'m> {
    pub fn new(meta: &'m Meta) -> Scheduler<'m> {
        Scheduler { meta }
    }

    /// Submit a job: run the machine `submit` command (already carrying
    /// `@SCRIPTFILE@` etc. — substituted here with `vars`), optionally inside
    /// a submit universe (§4.8) — `(name, universe)`, the name selecting the
    /// universe's env-setup overrides (§6.1) — and parse the job id via
    /// `submit-pattern` group 1.
    pub fn submit(&self, vars: &VarSet, universe: Option<(&str, &Universe)>) -> Res<String> {
        let submit = self.command_for(
            "submit",
            self.meta.scheduler.submit.as_deref(),
            vars,
            universe.map(|(name, _)| name),
        )?;
        let output = self.run(&submit, universe.map(|(_, u)| u), vars)?;

        let pattern = self.meta.scheduler.submit_pattern.as_deref().unwrap_or("(.*)");
        let regex = Regex::new(pattern).with_context(|| format!("invalid submit-pattern {pattern:?}"))?;
        let job_id = regex
            .captures(output.trim())
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_owned())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                anyhow!("could not parse a job id from the submit output via {pattern:?}:\n{output}")
            })?;
        Ok(job_id)
    }

    /// Submit and wait (§10): run the machine's `blocking-submit` command —
    /// the analogue of `submit` that returns only once the job has *finished*
    /// (`sbatch --wait`, `qsub -W block=true`) — handing the job id to
    /// `on_job_id` the moment a line of output matches `submit-pattern`
    /// rather than at the end. Reporting the id from inside the wait is what
    /// keeps a blocking submission recoverable: the caller records it while
    /// the job is still running, so a Ctrl-C or a crash partway through a
    /// multi-hour build cannot leave a real queued job with nothing on disk
    /// naming it.
    ///
    /// How early that happens is the submit command's own business. One that
    /// prints the id and only then blocks keeps it in its stdio buffer until
    /// it exits unless it flushes (piped stdout is block-buffered, and
    /// `sbatch --wait` does not flush), in which case the id simply arrives at
    /// the end. Both orders work; only the recoverability window differs.
    ///
    /// The command's exit status is deliberately NOT the job's verdict —
    /// `sbatch --wait` relays the job's exit code, but what the job really did
    /// is whatever the cactup running on the compute node recorded, which the
    /// caller reads back from the attempt. A non-zero exit matters only when
    /// no job id ever appeared: then the submission itself is what failed.
    pub fn submit_blocking(
        &self,
        vars: &VarSet,
        universe: Option<(&str, &Universe)>,
        on_job_id: &mut dyn FnMut(&str) -> Res<()>,
    ) -> Res<String> {
        let submit = self.command_for(
            "blocking-submit",
            self.meta.scheduler.blocking_submit.as_deref(),
            vars,
            universe.map(|(name, _)| name),
        )?;
        let command = self.command_in(&submit, universe.map(|(_, u)| u), vars)?;

        let pattern = self.meta.scheduler.submit_pattern.as_deref().unwrap_or("(.*)");
        let regex = Regex::new(pattern).with_context(|| format!("invalid submit-pattern {pattern:?}"))?;

        let mut job_id: Option<String> = None;
        let waited = stream_until_exit(command, &submit, &mut |line| {
            if job_id.is_some() {
                return Ok(());
            }
            let Some(id) = regex
                .captures(line.trim())
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().trim().to_owned())
                .filter(|id| !id.is_empty())
            else {
                return Ok(());
            };
            on_job_id(&id)?;
            job_id = Some(id);
            Ok(())
        })?;

        if let Some(id) = job_id {
            return Ok(id);
        }
        // No job id at all. Name which of the three ways it went wrong: the
        // caller has nothing else left to go on.
        match waited {
            Waited::Interrupted => bail!("interrupted before the scheduler reported a job id"),
            Waited::Exited { status, output } if !status.success() => {
                bail!("the blocking submit exited unsuccessfully ({status}):\n{output}")
            }
            Waited::Exited { output, .. } => bail!(
                "could not parse a job id from the blocking-submit output via {pattern:?}:\n{output}"
            ),
        }
    }

    /// The variables a scheduler query may reference: the job id, plus the
    /// invoking user — machines that scope their queue query with `-u @USER@`
    /// (§10) need it substituted here, since no other var set reaches this far.
    fn query_vars(job_id: &str) -> VarSet {
        let mut vars = VarSet::new();
        vars.set("JOB_ID", job_id);
        vars.set("USER", crate::mdb::whoami());
        vars
    }

    /// Query live status for `job_id` (§10): run `get-status` and classify
    /// its output. A non-zero exit is NOT an error — e.g. `ps <pid>` exits 1
    /// when the process is gone, which is simply status U.
    pub fn get_status(&self, job_id: &str) -> Res<JobStatus> {
        let vars = Self::query_vars(job_id);
        let cmd = self.command_for("get-status", self.meta.scheduler.get_status.as_deref(), &vars, None)?;

        let output = self.spawn_sh(&cmd, true)?;
        classify(&output, job_id, self.meta)
    }

    /// Live status for many jobs at once (§10): dedupes the ids and runs the
    /// per-job `get-status` query on a bounded worker pool, because each query is
    /// its own scheduler round-trip and a long simulation history makes those the
    /// dominant cost of `sim list`. Ids whose query failed are simply absent from
    /// the map — the same tolerance the display paths already get from
    /// `get_status(..).ok()`.
    pub fn get_statuses(&self, job_ids: &[&str]) -> HashMap<String, JobStatus> {
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<&str> = job_ids.iter().copied().filter(|id| seen.insert(*id)).collect();

        let mut results = HashMap::new();
        if ids.is_empty() {
            return results;
        }
        // One listing answers for every id when the machine offers one; a
        // machine that doesn't, or a listing that failed, falls back to the
        // per-job pool below.
        if let Some(many) = self.meta.scheduler.get_status_many.as_deref()
            && let Ok(statuses) = self.status_listing(many, &ids)
        {
            return statuses;
        }
        if ids.len() == 1 {
            if let Ok(status) = self.get_status(ids[0]) {
                results.insert(ids[0].to_owned(), status);
            }
            return results;
        }

        let workers = STATUS_WORKERS.min(ids.len());
        let queue: std::sync::Mutex<std::collections::VecDeque<&str>> =
            std::sync::Mutex::new(ids.into_iter().collect());
        let results = std::sync::Mutex::new(results);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let id = match queue.lock().expect("status queue poisoned").pop_front() {
                        Some(id) => id,
                        None => return,
                    };
                    if let Ok(status) = self.get_status(id) {
                        let mut results = results.lock().expect("status results poisoned");
                        results.insert(id.to_owned(), status);
                    }
                });
            }
        });

        results.into_inner().expect("status results poisoned")
    }

    /// Classify every id in `ids` from ONE run of the machine's
    /// `get-status-many` listing (§10). A job is looked up by exact match on
    /// the line's first field — never by regex against the whole listing, which
    /// would let one job's line answer for another job's id — and the line is
    /// then classified exactly as a single-job query would be. An id the
    /// listing does not mention is U: not in the queue.
    fn status_listing(&self, template: &str, ids: &[&str]) -> Res<HashMap<String, JobStatus>> {
        let vars = Self::query_vars("");
        let cmd = self.command_for("get-status-many", Some(template), &vars, None)?;
        let output = self.spawn_sh(&cmd, true)?;

        let mut lines: HashMap<&str, &str> = HashMap::new();
        for line in output.lines() {
            if let Some(id) = line.split_whitespace().next() {
                lines.insert(id, line);
            }
        }

        let mut statuses = HashMap::with_capacity(ids.len());
        for id in ids {
            let status = match lines.get(id) {
                Some(line) => classify(line, id, self.meta)?,
                None => JobStatus::Unknown,
            };
            statuses.insert((*id).to_owned(), status);
        }
        Ok(statuses)
    }

    /// Stop `job_id` via the machine `stop` command.
    pub fn stop(&self, job_id: &str) -> Res<()> {
        let vars = Self::query_vars(job_id);
        let cmd = self.command_for("stop", self.meta.scheduler.stop.as_deref(), &vars, None)?;
        self.spawn_sh(&cmd, false)?;
        Ok(())
    }

    /// The execution host of `job_id`, via `exec-host` + `exec-host-pattern`
    /// group 1 (None when the machine defines no exec-host).
    pub fn exec_host(&self, job_id: &str) -> Res<Option<String>> {
        let Some(exec_host) = self.meta.scheduler.exec_host.as_deref() else {
            return Ok(None);
        };
        let vars = Self::query_vars(job_id);
        let cmd = self.command_for("exec-host", Some(exec_host), &vars, None)?;
        let output = self.spawn_sh(&cmd, true)?;

        let pattern = self.meta.scheduler.exec_host_pattern.as_deref().unwrap_or("(.*)");
        let regex = Regex::new(pattern).with_context(|| format!("invalid exec-host-pattern {pattern:?}"))?;
        Ok(regex
            .captures(output.trim())
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_owned()))
    }

    /// Prepend the submit-phase env-setup (§4.2) — under `env_universe`'s
    /// overrides when given (§6.1; only `submit` passes one; get-status /
    /// stop / exec-host keep the machine env) — and substitute `vars`.
    fn command_for(
        &self,
        what: &str,
        template: Option<&str>,
        vars: &VarSet,
        env_universe: Option<&str>,
    ) -> Res<String> {
        let template = template
            .ok_or_else(|| anyhow!("this machine's meta.toml defines no [scheduler].{what} command"))?;
        let cmd = vars
            .substitute(template)
            .with_context(|| format!("substituting the [scheduler].{what} command"))?;
        let env = self.meta.effective_env(env_universe, Phase::Submit);
        Ok(if env.is_empty() { cmd } else { format!("{env}\n{cmd}") })
    }

    /// The `Command` that runs `inner`, optionally wrapped in a universe
    /// (§4.8) — shared by the capturing [`Self::run`] and the streaming
    /// [`Self::submit_blocking`], so both wrap a snippet identically.
    fn command_in(&self, inner: &str, universe: Option<&Universe>, vars: &VarSet) -> Res<Command> {
        Ok(match universe {
            None => sh_command(inner),
            Some(u) => match u.wrap(vars, inner)? {
                WrappedCommand::Shell(cmd) => sh_command(&cmd),
                WrappedCommand::Argv(argv) => {
                    let mut command = Command::new(&argv[0]);
                    command.args(&argv[1..]);
                    command
                }
            },
        })
    }

    /// Run a snippet, optionally wrapped in a universe. stdout+stderr are
    /// captured together (submit templates conventionally end in `2>&1`, but
    /// a scheduler that ignores that convention still gets its message seen).
    fn run(&self, inner: &str, universe: Option<&Universe>, vars: &VarSet) -> Res<String> {
        let mut command = self.command_in(inner, universe, vars)?;
        crate::shell::trace_command(&command);
        let output = command
            .output()
            .with_context(|| format!("Failed to run: {inner}"))?;
        collect_output(output, false)
    }

    fn spawn_sh(&self, cmd: &str, tolerate_failure: bool) -> Res<String> {
        let mut command = sh_command(cmd);
        crate::shell::trace_command(&command);
        let output = command
            .output()
            .with_context(|| format!("Failed to run: {cmd}"))?;
        collect_output(output, tolerate_failure)
    }
}

/// `/bin/sh -c <cmd>` — the one shape every scheduler snippet runs as.
fn sh_command(cmd: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", cmd]);
    command
}

/// How a streamed child stopped being waited on.
enum Waited {
    Exited { status: std::process::ExitStatus, output: String },
    /// Ctrl-C cut the wait short. Nothing was done to the job itself — only
    /// the local process that was watching it went away.
    Interrupted,
}

/// How often the wait loop wakes to drain output and re-check the child. A
/// blocking submit sits here for as long as the job runs, so what a round
/// costs is the wake-up alone — no filesystem or scheduler traffic — and
/// 100 ms buys an instant interrupt response for nothing.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Run `command` to completion, feeding every line it writes (stdout and
/// stderr alike) to `line` as it arrives, and returning the exit status with
/// the full combined output.
///
/// The wait is interrupt-aware, as any long-running child's must be: the flag
/// is polled every [`WAIT_POLL`], and a child that has not taken the
/// terminal's own SIGINT a couple of seconds later is killed — the same shape
/// `sim::start::spawn_and_wait` uses. Interrupting stops only the *watching*:
/// a queued job carries on regardless, which is why this reports
/// `Interrupted` instead of failing, and leaves the verdict to the caller.
fn stream_until_exit(
    mut command: Command,
    what: &str,
    line: &mut dyn FnMut(&str) -> Res<()>,
) -> Res<Waited> {
    use std::io::BufRead;

    command.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    crate::shell::trace_command(&command);
    let mut child = command.spawn().with_context(|| format!("Failed to run: {what}"))?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    for src in [
        Box::new(child.stdout.take().expect("stdout piped")) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().expect("stderr piped")),
    ] {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for l in std::io::BufReader::new(src).lines() {
                // A send fails only once the receiver is gone, i.e. nobody is
                // listening any more — stop reading rather than spin.
                if l.map(|l| tx.send(l).is_err()).unwrap_or(true) {
                    break;
                }
            }
        }));
    }
    // The parent's own sender has to go, or the final drain never ends.
    drop(tx);

    let mut output = String::new();
    let mut interrupted_at: Option<std::time::Instant> = None;
    let status = loop {
        while let Ok(l) = rx.try_recv() {
            output.push_str(&l);
            output.push('\n');
            line(&l)?;
        }
        if let Some(status) = child.try_wait().with_context(|| format!("waiting for: {what}"))? {
            break status;
        }
        if gix::interrupt::is_triggered() {
            let since = interrupted_at.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() > std::time::Duration::from_secs(2) {
                let _ = child.kill();
            }
        }
        std::thread::sleep(WAIT_POLL);
    };

    // The child is gone, so its readers see EOF and drop their senders: this
    // blocking drain terminates on its own, and cannot lose the last lines.
    while let Ok(l) = rx.recv() {
        output.push_str(&l);
        output.push('\n');
        line(&l)?;
    }
    for reader in readers {
        let _ = reader.join();
    }

    Ok(if interrupted_at.is_some() {
        Waited::Interrupted
    } else {
        Waited::Exited { status, output }
    })
}

fn collect_output(output: std::process::Output, tolerate_failure: bool) -> Res<String> {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() && !tolerate_failure {
        bail!("command exited unsuccessfully ({}):\n{combined}", output.status);
    }
    Ok(combined)
}

/// Classify `get-status` output (`simfactory-docs.txt` §14.7): find the line
/// matching `status-pattern` (no line ⇒ U — not in the queue); on that line
/// test holding → H, queued → Q, running → R; an in-queue line matching none
/// of them is E.
fn classify(output: &str, job_id: &str, meta: &Meta) -> Res<JobStatus> {
    let sched = &meta.scheduler;
    let compile = |what: &str, pattern: Option<&str>| -> Res<Regex> {
        // `$^` (match nothing) is the conventional never-pattern default.
        let pattern = pattern.unwrap_or("$^").replace("@JOB_ID@", &regex::escape(job_id));
        Regex::new(&pattern).with_context(|| format!("invalid {what} {pattern:?}"))
    };
    let status = compile("status-pattern", sched.status_pattern.as_deref())?;
    let holding = compile("holding-pattern", sched.holding_pattern.as_deref())?;
    let queued = compile("queued-pattern", sched.queued_pattern.as_deref())?;
    let running = compile("running-pattern", sched.running_pattern.as_deref())?;

    let Some(line) = output.lines().find(|line| status.is_match(line)) else {
        return Ok(JobStatus::Unknown);
    };
    Ok(if holding.is_match(line) {
        JobStatus::Holding
    } else if queued.is_match(line) {
        JobStatus::Queued
    } else if running.is_match(line) {
        JobStatus::Running
    } else {
        JobStatus::Error
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(scheduler_toml: &str) -> Meta {
        toml::from_str(&format!(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            {scheduler_toml}
            [queues.local]
            default = true
            [variants.submitscript]
            "default" = ["local"]
            [variants.runscript]
            "default" = ["local"]
            [variants.optionlist]
            variants = ["default"]
            "#
        ))
        .unwrap()
    }

    #[test]
    fn submit_parses_job_id_and_prepends_env() {
        let mut m = meta(
            r#"
            submit = "echo submitted as $CACTUP_TEST_MARKER-@SCRIPTFILE@"
            submit-pattern = "as (.*)"
            "#,
        );
        m.environment.env_setup = Some("CACTUP_TEST_MARKER=77".to_owned());
        let mut vars = VarSet::new();
        vars.set("SCRIPTFILE", "run.sh");
        assert_eq!(Scheduler::new(&m).submit(&vars, None).unwrap(), "77-run.sh");
    }

    #[test]
    fn submit_inside_a_universe() {
        let m = meta(r#"submit = "echo id=@SCRIPTFILE@""#);
        let universe: Universe = toml::from_str(r#"wrapper-argv = ["env", "IGNORED=1"]"#).unwrap();
        let mut vars = VarSet::new();
        vars.set("SCRIPTFILE", "s.sh");
        // submit-pattern defaults to (.*) over the trimmed output.
        assert_eq!(Scheduler::new(&m).submit(&vars, Some(("sif", &universe))).unwrap(), "id=s.sh");
    }

    #[test]
    fn submit_env_follows_the_submit_universe() {
        // The submit universe's env-submit-setup override (§6.1) replaces the
        // machine key for `submit` only; get-status/stop keep the machine env.
        let mut m = meta(r#"submit = "echo id=@SCRIPTFILE@-$CACTUP_SUB_ENV""#);
        m.environment.env_submit_setup = Some("CACTUP_SUB_ENV=base".to_owned());
        let uni: Universe = toml::from_str(r#"env-submit-setup = "CACTUP_SUB_ENV=uni""#).unwrap();
        m.universes.insert("u".to_owned(), uni);
        let mut vars = VarSet::new();
        vars.set("SCRIPTFILE", "s.sh");
        let sched = Scheduler::new(&m);
        assert_eq!(sched.submit(&vars, None).unwrap(), "id=s.sh-base");
        assert_eq!(
            sched.submit(&vars, Some(("u", &m.universes["u"]))).unwrap(),
            "id=s.sh-uni"
        );
    }

    /// The point of the streaming parse (§10): the job id has to reach the
    /// caller while the job is still running, not once the blocking command
    /// finally returns — that window is the whole reason a Ctrl-C during a
    /// multi-hour blocking build does not orphan the job.
    #[test]
    fn blocking_submit_reports_the_job_id_before_the_command_returns() {
        let dir = tempfile::tempdir().unwrap();
        let done = dir.path().join("done");
        let m = meta(&format!(
            r#"
            blocking-submit = "echo 'Submitted batch job 4242'; sleep 0.4; touch {done}"
            submit-pattern = 'Submitted batch job ([0-9]+)'
            "#,
            done = done.display()
        ));

        let mut seen_while_running = None;
        let job_id = Scheduler::new(&m)
            .submit_blocking(&VarSet::new(), None, &mut |id| {
                seen_while_running = Some((id.to_owned(), done.exists()));
                Ok(())
            })
            .unwrap();

        assert_eq!(job_id, "4242");
        let (reported, finished_already) = seen_while_running.expect("the job id was never reported");
        assert_eq!(reported, "4242");
        assert!(!finished_already, "the id was only reported after the command had finished");
        assert!(done.exists(), "submit_blocking returned before the command was done");
    }

    /// `sbatch --wait` relays the *job's* exit code, which says nothing about
    /// whether cactup's own build on the compute node succeeded — the attempt's
    /// recorded outcome is the only authority on that (§7.9). So a non-zero
    /// exit with a job id in hand is not an error here.
    #[test]
    fn blocking_submit_ignores_a_nonzero_exit_once_it_has_a_job_id() {
        let m = meta(
            r#"
            blocking-submit = "echo '[JOB-7]'; exit 3"
            submit-pattern = '\[(JOB-[^\]]*)\]'
            "#,
        );
        assert_eq!(
            Scheduler::new(&m).submit_blocking(&VarSet::new(), None, &mut |_| Ok(())).unwrap(),
            "JOB-7"
        );
    }

    /// No job id AND a failed command is a failed *submission* — the one case
    /// where the exit status does mean something. stderr is captured with
    /// stdout so the scheduler's complaint actually reaches the user.
    #[test]
    fn blocking_submit_without_a_job_id_reports_the_submission_failure() {
        let m = meta(
            r#"
            blocking-submit = "echo 'sbatch: error: invalid partition' >&2; exit 1"
            submit-pattern = 'Submitted batch job ([0-9]+)'
            "#,
        );
        let err = format!(
            "{:#}",
            Scheduler::new(&m).submit_blocking(&VarSet::new(), None, &mut |_| Ok(())).unwrap_err()
        );
        assert!(err.contains("exited unsuccessfully"), "{err}");
        assert!(err.contains("invalid partition"), "{err}");
    }

    /// A command that succeeded but printed nothing the pattern matches is a
    /// different failure, and says so.
    #[test]
    fn blocking_submit_that_prints_no_job_id_says_which_pattern_missed() {
        let m = meta(
            r#"
            blocking-submit = "echo nothing useful here"
            submit-pattern = 'Submitted batch job ([0-9]+)'
            "#,
        );
        let err = format!(
            "{:#}",
            Scheduler::new(&m).submit_blocking(&VarSet::new(), None, &mut |_| Ok(())).unwrap_err()
        );
        assert!(err.contains("could not parse a job id"), "{err}");
        assert!(err.contains("Submitted batch job"), "{err}");
    }

    /// The key is optional; a machine without it is asked to run a command it
    /// never declared, and the error names the key rather than the scheduler.
    #[test]
    fn blocking_submit_names_the_missing_key() {
        let m = meta(r#"submit = "echo 1""#);
        let err = format!(
            "{:#}",
            Scheduler::new(&m).submit_blocking(&VarSet::new(), None, &mut |_| Ok(())).unwrap_err()
        );
        assert!(err.contains("[scheduler].blocking-submit"), "{err}");
    }

    /// Substitution and the submit-phase env-setup apply exactly as they do to
    /// `submit` — the two keys are the same command in two flavours.
    #[test]
    fn blocking_submit_substitutes_and_prepends_env_like_submit() {
        let mut m = meta(
            r#"
            blocking-submit = "echo submitted as $CACTUP_TEST_MARKER-@SCRIPTFILE@"
            submit-pattern = "as (.*)"
            "#,
        );
        m.environment.env_setup = Some("CACTUP_TEST_MARKER=77".to_owned());
        let mut vars = VarSet::new();
        vars.set("SCRIPTFILE", "build.sh");
        assert_eq!(
            Scheduler::new(&m).submit_blocking(&vars, None, &mut |_| Ok(())).unwrap(),
            "77-build.sh"
        );
    }

    #[test]
    fn status_classification_ps_style() {
        // generic-machine style patterns (§4.6): in ps ⇒ running.
        let m = meta(
            r#"
            get-status = "true"
            status-pattern = "^ *@JOB_ID@ "
            queued-pattern = "$^"
            running-pattern = "^"
            holding-pattern = "$^"
            "#,
        );
        let ps = "    PID TTY  TIME CMD\n  12345 ?    0:01 cactus_sim\n";
        assert_eq!(classify(ps, "12345", &m).unwrap(), JobStatus::Running);
        assert_eq!(classify(ps, "999", &m).unwrap(), JobStatus::Unknown);
        assert_eq!(classify("", "12345", &m).unwrap(), JobStatus::Unknown);
    }

    #[test]
    fn status_classification_batch_style() {
        let m = meta(
            r#"
            get-status = "true"
            status-pattern = "^@JOB_ID@ "
            queued-pattern = " PD "
            running-pattern = " R "
            holding-pattern = " H "
            "#,
        );
        assert_eq!(classify("42  R  0:10\n", "4", &m).unwrap(), JobStatus::Unknown); // id must match exactly at ^
        assert_eq!(classify("42 R 0:10\n", "42", &m).unwrap(), JobStatus::Running);
        assert_eq!(classify("42 PD 0:00\n", "42", &m).unwrap(), JobStatus::Queued);
        assert_eq!(classify("42 H 0:00\n", "42", &m).unwrap(), JobStatus::Holding);
        assert_eq!(classify("42 X 0:00\n", "42", &m).unwrap(), JobStatus::Error);
    }

    #[test]
    fn live_get_status_tolerates_nonzero_exit() {
        // `ps <dead pid>` exits 1; that's status U, not an error (§10).
        let m = meta(
            r#"
            get-status = "echo 'header'; exit 1"
            status-pattern = "^ *@JOB_ID@ "
            "#,
        );
        assert_eq!(Scheduler::new(&m).get_status("31337").unwrap(), JobStatus::Unknown);
    }

    #[test]
    fn get_statuses_dedupes_and_runs_concurrently() {
        // Status depends on job id mod 3, so distinct ids classify
        // differently; 12 distinct ids is enough that STATUS_WORKERS (8)
        // actually recycles workers rather than covering them all at once.
        let m = meta(
            r#"
            get-status = "id=@JOB_ID@; r=$((id % 3)); if [ \"$r\" -eq 0 ]; then s=R; elif [ \"$r\" -eq 1 ]; then s=Q; else s=H; fi; echo \"$id $s\""
            status-pattern = "^@JOB_ID@ "
            queued-pattern = " Q"
            running-pattern = " R"
            holding-pattern = " H"
            "#,
        );
        let sched = Scheduler::new(&m);
        // "5" appears twice: a duplicate id must still be queried once and
        // land as a single entry.
        let ids = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "5"];
        let statuses = sched.get_statuses(&ids);

        assert_eq!(statuses.len(), 12);
        let want = [
            ("1", JobStatus::Queued),
            ("2", JobStatus::Holding),
            ("3", JobStatus::Running),
            ("4", JobStatus::Queued),
            ("5", JobStatus::Holding),
            ("6", JobStatus::Running),
            ("7", JobStatus::Queued),
            ("8", JobStatus::Holding),
            ("9", JobStatus::Running),
            ("10", JobStatus::Queued),
            ("11", JobStatus::Holding),
            ("12", JobStatus::Running),
        ];
        for (id, status) in want {
            assert_eq!(statuses.get(id).copied(), Some(status), "job {id}");
        }
    }

    #[test]
    fn get_statuses_handles_empty_and_singleton_input() {
        let m = meta(
            r#"
            get-status = "echo '@JOB_ID@ R'"
            status-pattern = "^@JOB_ID@ "
            running-pattern = " R"
            "#,
        );
        let sched = Scheduler::new(&m);
        assert!(sched.get_statuses(&[]).is_empty());
        let statuses = sched.get_statuses(&["7"]);
        assert_eq!(statuses.get("7").copied(), Some(JobStatus::Running));
    }

    #[test]
    fn stop_propagates_failure() {
        let ok = meta(r#"stop = "true""#);
        Scheduler::new(&ok).stop("1").unwrap();
        let bad = meta(r#"stop = "echo no permission >&2; false""#);
        let err = format!("{:#}", Scheduler::new(&bad).stop("1").unwrap_err());
        assert!(err.contains("no permission"), "{err}");
        let missing = meta("");
        assert!(Scheduler::new(&missing).stop("1").is_err());
    }

    #[test]
    fn status_listing_classifies_every_id_from_one_call() {
        // The exact pattern set anvil/frontera/expanse use (§10).
        let m = meta(
            r#"
            get-status-many = "printf '3774765 R (None)\n3774766 PD (Priority)\n3774767 PD (JobHeldUser)\n'"
            status-pattern  = '@JOB_ID@ '
            queued-pattern  = ' PD '
            running-pattern = ' (CF|CG|R|TO) '
            holding-pattern = '\(JobHeldUser\)'
            "#,
        );
        let sched = Scheduler::new(&m);
        let ids = ["3774765", "3774766", "3774767", "9999999"];
        let statuses = sched.get_statuses(&ids);

        assert_eq!(statuses.get("3774765").copied(), Some(JobStatus::Running));
        assert_eq!(statuses.get("3774766").copied(), Some(JobStatus::Queued));
        assert_eq!(statuses.get("3774767").copied(), Some(JobStatus::Holding));
        // Not in the listing at all ⇒ U, not absent.
        assert_eq!(statuses.get("9999999").copied(), Some(JobStatus::Unknown));
    }

    #[test]
    fn status_listing_runs_the_command_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let m = meta(&format!(
            r#"
            get-status-many = "echo called >> {marker} && printf '1 R (None)\n'"
            status-pattern  = '@JOB_ID@ '
            queued-pattern  = ' PD '
            running-pattern = ' (CF|CG|R|TO) '
            holding-pattern = '\(JobHeldUser\)'
            "#,
            marker = marker.display()
        ));
        let sched = Scheduler::new(&m);
        // 10 distinct ids: the listing must still answer for all of them
        // from a single round-trip, regardless of how many are requested.
        let ids = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"];
        let _ = sched.get_statuses(&ids);

        let calls = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(calls.lines().count(), 1, "expected exactly one scheduler round-trip:\n{calls}");
    }

    #[test]
    fn status_listing_never_lets_one_job_answer_for_another() {
        // status-pattern is the unanchored '@JOB_ID@ ', so a naive regex
        // search over the whole listing would let job 3774765's line match
        // as a substring for job 774765 too. Ids must be matched by exact
        // first-field equality, not by regex over the listing text.
        let m = meta(
            r#"
            get-status-many = "printf '3774765 R (None)\n'"
            status-pattern  = '@JOB_ID@ '
            queued-pattern  = ' PD '
            running-pattern = ' (CF|CG|R|TO) '
            holding-pattern = '\(JobHeldUser\)'
            "#,
        );
        let sched = Scheduler::new(&m);
        let statuses = sched.get_statuses(&["3774765", "774765"]);
        assert_eq!(statuses.get("3774765").copied(), Some(JobStatus::Running));
        assert_eq!(statuses.get("774765").copied(), Some(JobStatus::Unknown));
    }

    #[test]
    fn status_listing_failure_falls_back_to_per_job_queries() {
        // @NOT_A_VAR@ is not a substitutable token, so command_for's
        // vars.substitute errors before the listing command ever runs; that's
        // a genuine Err from status_listing, not just a nonzero exit (which
        // get-status-many alone tolerates). get_statuses must then fall back
        // to the per-job get-status pool.
        let m = meta(
            r#"
            get-status-many = "echo @NOT_A_VAR@"
            get-status      = "id=@JOB_ID@; if [ \"$id\" = 1 ]; then echo '1 R'; else echo '2 PD'; fi"
            status-pattern  = "^@JOB_ID@ "
            queued-pattern  = " PD"
            running-pattern = " R"
            "#,
        );
        let sched = Scheduler::new(&m);
        let statuses = sched.get_statuses(&["1", "2"]);
        assert_eq!(statuses.get("1").copied(), Some(JobStatus::Running));
        assert_eq!(statuses.get("2").copied(), Some(JobStatus::Queued));
    }

    #[test]
    fn every_slurm_machine_classifies_its_own_listing_format() {
        // A guard on the mdb itself: any machine that defines get-status-many
        // must have status/queued/running/holding patterns that correctly
        // classify lines in ITS OWN listing format, so a future mdb edit that
        // breaks the format is caught here rather than at `sim list` time.
        let mdb = crate::mdb::Mdb::with_roots(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            std::path::PathBuf::from("/nonexistent-user-mdb"),
        );
        for (name, _layer) in mdb.machines().unwrap() {
            let machine = mdb.load(&name).unwrap_or_else(|e| panic!("machine {name} failed to load: {e:#}"));
            if machine.meta.scheduler.get_status_many.is_none() {
                continue;
            }
            let id = "3774765";
            let running_line = format!("{id} R (None)");
            let queued_line = format!("{id} PD (Priority)");
            let held_line = format!("{id} PD (JobHeldUser)");

            assert_eq!(
                classify(&running_line, id, &machine.meta).unwrap(),
                JobStatus::Running,
                "{name}: {running_line:?} should classify as Running"
            );
            assert_eq!(
                classify(&queued_line, id, &machine.meta).unwrap(),
                JobStatus::Queued,
                "{name}: {queued_line:?} should classify as Queued"
            );
            let held = classify(&held_line, id, &machine.meta).unwrap();
            assert!(
                matches!(held, JobStatus::Holding | JobStatus::Queued),
                "{name}: {held_line:?} classified as {held:?}, expected Holding or Queued"
            );
        }
    }

    #[test]
    fn display_state_matrix() {
        use DisplayState as D;
        use JobStatus as J;
        // §8.6: PRESUBMITTED = chained, not active, Q/H.
        assert_eq!(display_state(false, Some(J::Queued), true, false), D::Presubmitted);
        assert_eq!(display_state(false, Some(J::Holding), true, false), D::Presubmitted);
        assert_eq!(display_state(false, Some(J::Queued), false, false), D::Inactive);
        assert_eq!(display_state(false, None, false, false), D::Inactive);
        // Active restart follows R/Q/H/E live status.
        assert_eq!(display_state(true, Some(J::Running), false, false), D::Running);
        assert_eq!(display_state(true, Some(J::Queued), false, false), D::Queued);
        assert_eq!(display_state(true, Some(J::Holding), true, false), D::Holding);
        assert_eq!(display_state(true, Some(J::Error), false, false), D::Error);
        // U + terminated = FINISHED; U + not terminated = ACTIVE (§10).
        assert_eq!(display_state(true, Some(J::Unknown), false, true), D::Finished);
        assert_eq!(display_state(true, Some(J::Unknown), true, false), D::Active);
        assert_eq!(display_state(true, None, false, true), D::Finished);
    }
}
