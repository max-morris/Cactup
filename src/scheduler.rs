//! Job submission & scheduler abstraction (spec §10, §8.6).
//!
//! Everything is driven by the machine `meta.toml` scheduler keys: `submit`
//! (job id parsed via `submit-pattern` group 1), its optional wait-for-the-job
//! flavor `blocking-submit`, `get-status` classified by the `*-pattern`
//! regexes into R/Q/H/U/E, and `stop`. Status is queried live, never stored
//! (D4).
//!
//! A job is named by a [`JobRef`]: its id and, where job ids are pids
//! (`job-ids = "pids"`), the host and process start time recorded with it
//! at submit. Every query and stop goes through the checks those allow, so
//! no caller hands a pid that means another process to `ps` or `pkill`.

use crate::mdb::meta::{JobIds, Meta, Phase, Universe, WrappedCommand};
use crate::template::{Syntax, VarSet};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use colored::Colorize;
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
    /// The status query failed (where the machine can tell, §10) for a
    /// restart whose run has not recorded its end: no state is known.
    Unknown,
    /// A restart whose run recorded its end, whose job the scheduler lists
    /// queued (or held) again (§8.6): `sim stop` stops it.
    Requeued,
    /// A restart whose run recorded its end, whose job the scheduler still
    /// lists running: leaving the queue, or starting again (§8.6). `sim
    /// stop` stops it.
    StillListed,
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

/// A recorded job, as the scheduler is asked about it: its id and, where
/// the machine's job ids are pids (`job-ids = "pids"`, §10), the host the
/// process was started on and its start time (`/proc/<pid>/stat`), read
/// the moment the id was known ([`JobRecord`]). A record without them is
/// one an older cactup made, or a batch scheduler's job. `since` is when
/// the record was made (its submission, or its creation just before),
/// which tells an older record's pid from a process that started later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobRef<'a> {
    pub id: &'a str,
    /// The host the pid's process was started on.
    pub host: Option<&'a str>,
    /// Its start time; `None` with a host recorded: the process was gone
    /// already when cactup read it.
    pub start: Option<u64>,
    /// When the record was made, where it says.
    pub since: Option<DateTime<Utc>>,
}

impl<'a> JobRef<'a> {
    pub fn new(id: &'a str, host: Option<&'a str>, start: Option<u64>, since: Option<DateTime<Utc>>) -> JobRef<'a> {
        JobRef { id, host, start, since }
    }

    /// A job id with nothing recorded beside it.
    #[cfg(test)]
    pub fn bare(id: &'a str) -> JobRef<'a> {
        JobRef { id, host: None, start: None, since: None }
    }

    /// The same, owned.
    pub fn to_record(self) -> JobRecord {
        JobRecord { id: self.id.to_owned(), host: self.host.map(str::to_owned), start: self.start, since: self.since }
    }

    fn key(&self) -> JobKey {
        (self.id.to_owned(), self.host.map(str::to_owned), self.start, self.since)
    }
}

/// What tells two recorded jobs apart: all that is recorded of them.
type JobKey = (String, Option<String>, Option<u64>, Option<DateTime<Utc>>);

/// A job id with what is recorded beside it, owned: a job just submitted
/// (or a foreground run's own pid, [`Scheduler::own_job`]), with, on a
/// `job-ids = "pids"` machine, where its process started (recorded as
/// `job-host` and `job-start`); or one read back from a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: String,
    pub host: Option<String>,
    pub start: Option<u64>,
    pub since: Option<DateTime<Utc>>,
}

impl JobRecord {
    pub fn job(&self) -> JobRef<'_> {
        JobRef::new(&self.id, self.host.as_deref(), self.start, self.since)
    }
}

/// What [`Scheduler::stop`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// The machine's stop command ran and succeeded.
    Sent,
    /// The job's pid names no process of the job's any more (a process that
    /// started at another time, or none): nothing was run.
    Gone,
    /// The job's pid is another host's, which this one cannot see or stop:
    /// nothing was run. Whether it still runs there is for the run's
    /// `running.lock` to say, never taken for gone here.
    Elsewhere(String),
}

/// [`Scheduler::stop`]'s refusal of a pid recorded without its host and
/// start time (by an older cactup): a live process with that pid here may
/// be another one, so it is never stopped.
#[derive(Debug)]
pub struct UnverifiedPid {
    pub id: String,
    /// The pid's process group, read when the refusal was made.
    group: Option<crate::lock::ProcGroup>,
}

impl UnverifiedPid {
    pub fn of(id: &str) -> UnverifiedPid {
        let group = id.parse::<u32>().ok().and_then(crate::lock::proc_group);
        UnverifiedPid { id: id.to_owned(), group }
    }
}

/// How to stop by hand the run whose top process is `pid`: the whole
/// process group where it holds nothing else (`kill -- -<pgid>`, which
/// stops the pid and the processes it started in that group, and nothing
/// else; any it started in another group named too); else no single
/// command does, and the processes are named.
fn stop_by_hand(id: &str, group: Option<&crate::lock::ProcGroup>) -> String {
    let pids = |list: &[u32]| list.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
    match group {
        Some(group) if group.own && group.outside.is_empty() => {
            format!("`kill -- -{}` stops it and every process it started", group.pgid)
        }
        Some(group) if group.own => format!(
            "`kill -- -{}` stops it and the processes it started in its group, but not pids {}, which it \
             started in groups of their own: stop those too",
            group.pgid,
            pids(&group.outside)
        ),
        Some(group) if group.tree.len() > 1 => format!(
            "no single command stops it alone, as its process group holds processes cactup cannot tell \
             are the job's: stop pid {id} and the processes it started (now pids {}; `pstree -p {id}` shows \
             them), or interrupt the run where it runs (Ctrl-C)",
            pids(&group.tree)
        ),
        _ => format!(
            "no single command stops it alone, as its process group holds processes cactup cannot tell are \
             the job's: stop pid {id} and any process it started (`pstree -p {id}` shows them), or interrupt \
             the run where it runs (Ctrl-C)"
        ),
    }
}

impl std::fmt::Display for UnverifiedPid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let how = stop_by_hand(&self.id, self.group.as_ref());
        write!(
            f,
            "job {0} was recorded by an older cactup, without the host and start time of its \
             process, and a process with pid {0} runs here: it may be the job or another process, \
             so cactup does not stop it (if it is the job, {how}; or try again once it has ended)",
            self.id
        )
    }
}

impl std::error::Error for UnverifiedPid {}

/// [`Scheduler::stop`]'s refusal of a pid whose process group is not its
/// own (it holds processes cactup cannot tell are the job's): the
/// machine's stop command (`pkill -g`, or `kill` of a process whose
/// siblings carry on) would reach, or miss, other processes.
#[derive(Debug)]
pub struct SharedGroup(pub String);

impl std::fmt::Display for SharedGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let group = self.0.parse::<u32>().ok().and_then(crate::lock::proc_group);
        let pgid = group.as_ref().map(|g| format!(" ({})", g.pgid)).unwrap_or_default();
        write!(
            f,
            "job {0} shares its process group{pgid} with processes cactup cannot tell are the job's, so cactup does \
             not stop it: the machine's stop command would stop them too, and no single command stops \
             the job alone. Stop pid {0} and the processes it started by hand (`pstree -p {0}` shows \
             them), or interrupt the run where it runs (Ctrl-C; a foreground `cactup sim run` started \
             from a script or later in a pipeline is one); a submit command that starts each job with \
             `setsid` gives it a group of its own",
            self.0
        )
    }
}

impl std::error::Error for SharedGroup {}

/// [`Scheduler::get_status`]'s error for a status query whose output says
/// neither how the job is nor that the scheduler does not know it (where
/// the machine sets `unknown-job-pattern`).
#[derive(Debug)]
pub struct FailedQuery(pub String);

impl std::fmt::Display for FailedQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FailedQuery {}

/// What the pid checks (§10, `job-ids = "pids"`) make of a recorded job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidCheck {
    /// Its process runs here, started when recorded: ask `ps`, stop it.
    Here,
    /// Not its process (a start time that differs, none at all, a process
    /// that started after an older record was made): gone, nothing is run.
    Gone,
    /// Another host's pid: nothing is run here, and the run's lock says
    /// whether it runs there.
    Elsewhere,
    /// An older record (no host, no start time) whose pid has a process
    /// here that may be the job: asked about, never stopped.
    Unverified,
}

/// The end of a [`FailedQuery`]'s text where the query printed something:
/// what to do if the scheduler's word for a job it does not know is there.
const QUERY_HINT: &str = "If the scheduler's message for a job it does not know is among these lines, \
                          unknown-job-pattern does not match it: fix the pattern in the machine's \
                          [scheduler] table.";

/// How long after a record's end its job is still asked about where a batch
/// scheduler may requeue it ([`Scheduler::asks_after_end`]).
pub const REQUEUE_WINDOW: chrono::TimeDelta = chrono::TimeDelta::days(14);

/// The note for a status query that failed after its record's end, taken
/// as ended by what only reads or removes records (displays, deletes).
pub fn note_taken_as_ended(job_id: &str) {
    eprintln!(
        "{} the status query for job {job_id} failed, and its run had recorded its end: taken as \
         ended (if this repeats, check that unknown-job-pattern in the machine's [scheduler] table \
         matches what get-status prints for a job the scheduler does not know)",
        "note:".yellow()
    );
}

/// What the scheduler said of a job asked about after its record's end
/// ([`Scheduler::ask_after_end`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterEnd {
    /// Listed in a state the patterns read: queued again, or leaving.
    Listed(JobStatus),
    /// Not known (`Unknown`), or listed in a state no pattern reads
    /// (`Error`): the ended job.
    Over(JobStatus),
    /// The status query failed: no answer. The [`FailedQuery`] text (the
    /// query, both patterns, its output, the `unknown-job-pattern` hint).
    Unanswered(String),
}

/// What a job the scheduler still lists after its record's end may be, by
/// its state: queued or held, queued again; running (the patterns read
/// `CF`, `R`, `CG`, `TO` alike), leaving the queue or starting again.
pub fn listed_state_words(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued again",
        JobStatus::Holding => "queued again, and held",
        _ => "listed running: leaving the queue, or starting again",
    }
}

/// How a job the scheduler still lists after its record's end reads, by
/// its state, with what to do about it (`stop`: the cactup command that
/// stops it): queued or held, queued again; running, leaving the queue or
/// starting again (the patterns cannot tell which).
pub fn listed_after_end(status: JobStatus, stop: &str) -> String {
    match status {
        JobStatus::Queued => format!("as queued again: `{stop}` to stop it, or wait until it has run"),
        JobStatus::Holding => format!("as queued again, and held: `{stop}` to stop it"),
        _ => format!(
            "as running (leaving the queue, or starting again): wait until it leaves the queue, or `{stop}` \
             to stop it"
        ),
    }
}

/// What a failed stop command said, on one line: its output, then its exit
/// status (`collect_output`'s "command exited unsuccessfully (<status>):"
/// heading, turned around).
pub fn stop_said(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    let (status, rest) = match text.strip_prefix("command exited unsuccessfully (").and_then(|t| t.split_once("):")) {
        Some((status, rest)) => (Some(status), rest),
        None => (None, text.as_str()),
    };
    let said: Vec<&str> = rest.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    match (said.is_empty(), status) {
        (true, Some(status)) => status.to_owned(),
        (true, None) => "nothing".to_owned(),
        (false, Some(status)) => format!("{} ({status})", said.join(" ")),
        (false, None) => said.join(" "),
    }
}

/// The end a record's job is asked about after (`finished`), unless a stop
/// sent after it settled the job (`job_stopped`, later than `finished`):
/// then `None`, taken as ended without a query.
pub fn end_to_ask(finished: Option<DateTime<Utc>>, job_stopped: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    finished.filter(|finished| job_stopped.is_none_or(|stopped| stopped < *finished))
}

/// What cactup knows of a job it stops ([`Scheduler::stop_judged`]).
#[derive(Debug, Clone, Copy)]
pub enum StopCase<'a> {
    /// The scheduler lists it in a state its patterns read (queued, held,
    /// running): any failure of the stop command is an error.
    Listed,
    /// Listed in a state no pattern reads (suspended, exiting, completed).
    Unread,
    /// Its status query failed: the [`FailedQuery`] text.
    Unanswered(&'a str),
}

/// What a stop judged by [`Scheduler::stop_judged`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndedStop {
    /// The scheduler took it (or, where job ids are pids, there was
    /// nothing to stop).
    Sent,
    /// The stop command failed saying, by the machine's
    /// `unknown-job-pattern`, that the scheduler does not know the job (one
    /// it has forgotten), what it said attached: taken as the job ended.
    NotKnown(String),
}

/// How much later than its record a process with an older record's pid may
/// have started and still be the job: its submission takes moments, but the
/// boot-time clock is coarse.
const OLD_RECORD_SLACK: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether a job with `status` is live, for a record whose run has
/// (`ended`) or has not recorded its end: listed in a state the machine's
/// patterns read (running, queued, held), or, before the end, listed in
/// any state. After the end (asked only within the requeue window) a job
/// in a read state is "still listed": either leaving the queue (Slurm's
/// `CG`, a timeout's `TO`, read as running) or starting again (queued,
/// held, or `CF` and `R` before its run clears the record's end), which
/// the patterns cannot tell apart, and each command takes the safe reading
/// for what it does with it (§10). A state no pattern reads (`Error`) is
/// the ended job after the end (Torque's completed `C`, Slurm's `CD`, `F`,
/// `CA`, `OOM`, `NF`: a requeued job always passes through read states,
/// Slurm `PD`, `CF`, `R`, Torque `Q`, `R`); before it, the job may be
/// suspended (Slurm's `S`), exiting, or over before its run could record
/// its end, which cactup cannot tell apart: the job is live, stopped by
/// [`Scheduler::stop_judged`] with [`StopCase::Unread`].
pub fn is_live(status: JobStatus, ended: bool) -> bool {
    match status {
        JobStatus::Queued | JobStatus::Holding | JobStatus::Running => true,
        JobStatus::Error => !ended,
        JobStatus::Unknown => false,
    }
}

/// `text` with its first letter capitalized, for a line of its own.
pub fn sentence(text: String) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// What is said of a job listed in a state the machine's patterns do not
/// read before its run recorded its end (`what`: "test job 42", "job 42").
pub fn unread_state(what: &str) -> String {
    format!("the scheduler lists {what} in a state cactup does not read (suspended, exiting, or ended)")
}

/// Live statuses from [`Scheduler::get_statuses`], looked up by the same
/// [`JobRef`] they were asked for: two records with one id (a pid used
/// again) are two jobs. A job asked about with no status here: its query
/// failed (or Ctrl-C cut it short).
#[derive(Debug, Default)]
pub struct Statuses(HashMap<JobKey, JobStatus>);

impl Statuses {
    pub fn get(&self, job: JobRef<'_>) -> Option<&JobStatus> {
        self.0.get(&job.key())
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Scheduler driver for one machine.
pub struct Scheduler<'m> {
    meta: &'m Meta,
    /// The machine whose scheduler this is, where known: a record made for
    /// another machine is not asked about after its end (two clusters
    /// sharing a home).
    machine: Option<&'m str>,
}

impl<'m> Scheduler<'m> {
    /// A scheduler that does not know its machine's name (tests).
    #[cfg(test)]
    pub fn new(meta: &'m Meta) -> Scheduler<'m> {
        Scheduler { meta, machine: None }
    }

    /// The scheduler of `machine`, knowing its name.
    pub fn for_machine(machine: &'m crate::mdb::Machine) -> Scheduler<'m> {
        Scheduler { meta: &machine.meta, machine: Some(&machine.name) }
    }

    /// The same scheduler, taking itself for machine `name` (tests).
    #[cfg(test)]
    pub fn named(meta: &'m Meta, name: &'m str) -> Scheduler<'m> {
        Scheduler { meta, machine: Some(name) }
    }

    /// Submit a job: run the machine `submit` command (already carrying
    /// `@SCRIPTFILE@` etc. — substituted here with `vars`), optionally inside
    /// a submit universe (§4.8) — `(name, universe)`, the name selecting the
    /// universe's env-setup overrides (§6.1) — and parse the job id via
    /// `submit-pattern` group 1. On a `job-ids = "pids"` machine the pid's
    /// host and start time come with it ([`JobRecord`]), read at once.
    pub fn submit(&self, vars: &VarSet, universe: Option<(&str, &Universe)>) -> Res<JobRecord> {
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
        Ok(self.submitted(job_id))
    }

    /// `job_id`, just handed out, with what a `job-ids = "pids"` machine
    /// records beside it: this host, and the start time of the process
    /// with that pid, read now (`None` if it has ended already, or the id
    /// is no pid). Nothing beside it elsewhere.
    fn submitted(&self, job_id: String) -> JobRecord {
        match self.job_ids() {
            JobIds::Scheduler => JobRecord { id: job_id, host: None, start: None, since: None },
            JobIds::Pids => {
                let start = job_id.parse().ok().and_then(crate::lock::proc_starttime);
                JobRecord { id: job_id, host: Some(crate::lock::our_hostname()), start, since: None }
            }
        }
    }

    /// The job id a foreground run records: this process's pid, and on a
    /// `job-ids = "pids"` machine, where the scheduler's `ps` and `stop`
    /// are asked about it, its host and start time as for a submitted job.
    pub fn own_job(&self) -> JobRecord {
        self.submitted(std::process::id().to_string())
    }

    /// Submit and wait (§10): run the machine's `blocking-submit` command —
    /// the analog of `submit` that returns only once the job has *finished*
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
    ///
    /// On a `job-ids = "pids"` machine the pid's host and start time are
    /// read the moment its line arrives, while the command waits on it.
    pub fn submit_blocking(
        &self,
        vars: &VarSet,
        universe: Option<(&str, &Universe)>,
        on_job_id: &mut dyn FnMut(&JobRecord) -> Res<()>,
    ) -> Res<JobRecord> {
        let submit = self.command_for(
            "blocking-submit",
            self.meta.scheduler.blocking_submit.as_deref(),
            vars,
            universe.map(|(name, _)| name),
        )?;
        let command = self.command_in(&submit, universe.map(|(_, u)| u), vars)?;

        let pattern = self.meta.scheduler.submit_pattern.as_deref().unwrap_or("(.*)");
        let regex = Regex::new(pattern).with_context(|| format!("invalid submit-pattern {pattern:?}"))?;

        let mut job_id: Option<JobRecord> = None;
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
            let submitted = self.submitted(id);
            on_job_id(&submitted)?;
            job_id = Some(submitted);
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

    /// Query live status for `job` (§10): run `get-status` and classify
    /// its output. A non-zero exit is NOT an error — e.g. `ps <pid>` exits 1
    /// when the process is gone, which is simply status U.
    ///
    /// On a `job-ids = "pids"` machine, a pid that is not the job's process
    /// ([`Self::pid_check`]) is U with nothing run. Where the machine sets
    /// `unknown-job-pattern`, output with no line for the job is U only if
    /// a line says the scheduler does not know it; otherwise the query
    /// failed, and this is an error naming the command and its output.
    pub fn get_status(&self, job: JobRef<'_>) -> Res<JobStatus> {
        // Gone, or another host's pid, which `ps` here cannot see (callers
        // ask `recorded_elsewhere` and the run's lock about that).
        if matches!(self.pid_check(job), Some(PidCheck::Gone | PidCheck::Elsewhere)) {
            return Ok(JobStatus::Unknown);
        }
        let job_id = job.id;
        let vars = Self::query_vars(job_id);
        let cmd = self.command_for("get-status", self.meta.scheduler.get_status.as_deref(), &vars, None)?;

        let output = self.spawn_sh(&cmd, true)?;
        // Ctrl-C reaches the query too, which then prints nothing: that is
        // no answer, not "not in the queue" (CLAUDE.md's interrupt
        // contract), and a caller must not act on it.
        if gix::interrupt::is_triggered() {
            bail!("interrupted");
        }
        if let Some(status) = classify_listed(&output, job_id, self.meta)? {
            return Ok(status);
        }
        if self.unknown_job(&output, job_id)? == Some(false) {
            let sched = &self.meta.scheduler;
            let query = vars.substitute(sched.get_status.as_deref().unwrap_or_default(), Syntax::Shell).unwrap_or_default();
            let status_pattern = sched.status_pattern.as_deref().unwrap_or("$^");
            let unknown_pattern = sched.unknown_job_pattern.as_deref().unwrap_or_default();
            let said = match output.trim() {
                "" => "It printed nothing.".to_owned(),
                text => format!("It printed:\n{text}\n{QUERY_HINT}"),
            };
            return Err(FailedQuery(format!(
                "the status query for job {job_id}, `{query}`, printed no line matching the machine's \
                 status-pattern ('{status_pattern}') for the job, and none matching its \
                 unknown-job-pattern ('{unknown_pattern}'), so cactup cannot tell whether the job \
                 runs. {said}"
            ))
            .into());
        }
        Ok(JobStatus::Unknown)
    }

    /// [`Self::get_status`] for a recorded job whose run has (`ended`) or has
    /// not recorded its end. After the end, a status query that failed
    /// ([`FailedQuery`]) is taken as the job ended, saying so in one line:
    /// asking is only the best-effort check for a requeued job, and must not
    /// hold up a record that ended for good. Before the end it is the error.
    pub fn get_status_of(&self, job: JobRef<'_>, ended: bool) -> Res<JobStatus> {
        match self.get_status(job) {
            Err(e) if ended && e.downcast_ref::<FailedQuery>().is_some() => {
                note_taken_as_ended(job.id);
                Ok(JobStatus::Unknown)
            }
            other => other,
        }
    }

    /// The host a `job-ids = "pids"` job was recorded on, where that is not
    /// this one: its pid means nothing here, so nothing here asks about or
    /// stops it, and only the run's `running.lock` says whether it runs.
    pub fn recorded_elsewhere<'j>(&self, job: JobRef<'j>) -> Option<&'j str> {
        match self.pid_check(job) {
            Some(PidCheck::Elsewhere) => job.host,
            _ => None,
        }
    }

    /// `[scheduler].job-ids` (§10): whether this machine's job ids are a
    /// batch system's or pids on the submitting host.
    pub fn job_ids(&self) -> JobIds {
        self.meta.scheduler.job_ids
    }

    /// Whether a job whose run recorded its end is still asked about: a
    /// batch scheduler may run it again (preemption, `scontrol requeue`,
    /// Torque's `qrerun`), and its queue says so. On a `job-ids = "pids"`
    /// machine nothing does, and an older record's pid may be another
    /// process's by now (§10), so such a job is done.
    pub fn may_requeue(&self) -> bool {
        self.job_ids() == JobIds::Scheduler
    }

    /// Whether a job of a record made for machine `record_machine`, whose
    /// record ended at `ended_at`, is still asked about: where a batch
    /// scheduler may requeue it ([`Self::may_requeue`]), only for a record of
    /// this machine (another's, in a home two clusters share, names a job of
    /// that cluster's), and only within [`REQUEUE_WINDOW`] of the end. Later
    /// its id may by now name an unrelated job (ids wrap around, a cluster is
    /// reinstalled), which would refuse a delete or be stopped by its `-f`;
    /// and a record with no end time is taken as ended too.
    pub fn asks_after_end(&self, record_machine: &str, ended_at: Option<DateTime<Utc>>) -> bool {
        self.may_requeue()
            && self.machine.is_none_or(|machine| machine == record_machine)
            && ended_at.is_some_and(|ended| Utc::now() - ended < REQUEUE_WINDOW)
    }

    /// Ask about `job` after its record's end (within the requeue window):
    /// still listed in a state the patterns read, said to be gone or over
    /// (not known, absent from a listing, a state no pattern reads), or not
    /// answered (a failed query). What each command does with it is its own
    /// safe reading (§10): stops are sent unless the scheduler answered that
    /// the job is gone; new work is not started beside an unanswered job.
    pub fn ask_after_end(&self, job: JobRef<'_>) -> Res<AfterEnd> {
        match self.get_status(job) {
            Ok(status) if is_live(status, true) => Ok(AfterEnd::Listed(status)),
            Ok(status) => Ok(AfterEnd::Over(status)),
            Err(e) => match e.downcast::<FailedQuery>() {
                Ok(failed) => Ok(AfterEnd::Unanswered(failed.0)),
                Err(e) => Err(e),
            },
        }
    }

    /// Stop `job`, judging a stop command that fails by the one rule, for
    /// every path that may take a failed stop for the job being over
    /// (`case`: what cactup knows of the job). A job the scheduler lists in
    /// a state its patterns read ([`StopCase::Listed`]): any failure is an
    /// error. Otherwise (listed in a state no pattern reads, or its status
    /// query failed) the failure is a refusal that settles the job only
    /// where the stop command's output matches the machine's
    /// `unknown-job-pattern`: the scheduler saying it does not know the job
    /// ([`EndedStop::NotKnown`]; Torque's `qdel` of a forgotten job). Any
    /// other failure (the controller not reached, a timeout) is an error
    /// that shows the output, and the failed query's after a failed query,
    /// with the pattern hint: nothing is settled, recorded or deleted on
    /// it. Slurm's `scancel` takes the stop of a job it does not know
    /// without a word, so there the stop is simply taken. Where the machine
    /// sets no `unknown-job-pattern`, nothing fails as unanswered (no line
    /// for the job is a job gone) and no failure settles anything. Ctrl-C
    /// is "interrupted" still; the pid refusals ([`UnverifiedPid`],
    /// [`SharedGroup`]) are errors still.
    pub fn stop_judged(&self, job: JobRef<'_>, case: StopCase<'_>) -> Res<EndedStop> {
        let e = match self.stop(job) {
            Ok(_) => return Ok(EndedStop::Sent),
            Err(e) => e,
        };
        if gix::interrupt::is_triggered()
            || e.downcast_ref::<UnverifiedPid>().is_some()
            || e.downcast_ref::<SharedGroup>().is_some()
        {
            return Err(e);
        }
        let said = stop_said(&e);
        if matches!(case, StopCase::Listed) {
            bail!("the stop command for job {} failed: {said}", job.id);
        }
        if self.unknown_job(&format!("{e:#}"), job.id)? == Some(true) {
            return Ok(EndedStop::NotKnown(said));
        }
        // Where no pattern is set, there is none to fix.
        let hint = match self.meta.scheduler.unknown_job_pattern.as_deref() {
            Some(pattern) => format!(
                "\nIf the scheduler's message for a job it does not know is among these lines, \
                 unknown-job-pattern ('{pattern}') does not match it: fix the pattern in the machine's \
                 [scheduler] table."
            ),
            None => String::new(),
        };
        match case {
            // The query's hint and the stop's are one: said once, after both.
            StopCase::Unanswered(failed) => bail!(
                "{}\nThe stop command for job {} failed too: {said}{hint}",
                failed.strip_suffix(QUERY_HINT).unwrap_or(failed).trim_end(),
                job.id
            ),
            // Listed in a state no pattern reads: a scheduler may refuse to
            // stop a job it lists as over (Torque's `qdel` of a `C` job)
            // until it lets the job go.
            _ => bail!(
                "the stop command for job {} failed: {said}\nThe scheduler lists the job in a state the \
                 machine's patterns do not read: try again once the scheduler answers, or, if the job has \
                 ended (Torque lists a completed job as `C` for a while), once it no longer lists it.{hint}",
                job.id
            ),
        }
    }

    /// The machine's stop command for `job_id`, as the user may run it by
    /// hand (without the environment setup cactup prepends).
    pub fn stop_command(&self, job_id: &str) -> Option<String> {
        let template = self.meta.scheduler.stop.as_deref()?;
        Self::query_vars(job_id).substitute(template, Syntax::Shell).ok()
    }

    /// Whether `job` is the job's own process here but its process group
    /// is not the job's own (it holds processes cactup cannot tell are the
    /// job's), so
    /// [`Self::stop`] refuses it ([`SharedGroup`]).
    pub fn shares_group(&self, job: JobRef<'_>) -> bool {
        self.pid_check(job) == Some(PidCheck::Here) && !Self::own_group(job.id)
    }

    /// Whether pid `id`'s process group is its own: every process in it is
    /// the pid or one it started (`lock::proc_group`). Whether the pid
    /// leads it does not matter: a job started without `setsid` from an
    /// interactive shell is in the group of the cactup that submitted it,
    /// which has exited, and holds nothing else.
    fn own_group(id: &str) -> bool {
        id.parse::<u32>().ok().and_then(crate::lock::proc_group).is_some_and(|group| group.own)
    }

    /// What can be said of `job` where the pid checks rule it out without
    /// running anything (job ids that are pids): its process is gone or is
    /// another process now, or it is another host's. `None` where the
    /// checks leave the asking to `get-status` (a batch scheduler, a pid
    /// whose process is the job's).
    pub fn ruled_out(&self, job: JobRef<'_>) -> Option<String> {
        match self.pid_check(job)? {
            PidCheck::Gone => Some(format!("process {} is gone, or is another process now", job.id)),
            PidCheck::Elsewhere => Some(format!(
                "it ran on host {}, and nothing of its run holds its lock",
                job.host.unwrap_or_default()
            )),
            PidCheck::Here | PidCheck::Unverified => None,
        }
    }

    /// Whether `job` is verified as the job's own process here, in a group
    /// of its own, so the machine's stop command stops exactly it: a pid
    /// with its host and start time ([`PidCheck::Here`]) whose group is its
    /// own. Never true on a batch scheduler, where the scheduler answers.
    pub fn stoppable_pid(&self, job: JobRef<'_>) -> bool {
        self.pid_check(job) == Some(PidCheck::Here) && Self::own_group(job.id)
    }

    /// Whether `job` is a pid an older cactup recorded (no host, no start
    /// time) whose process here may be the job: asked about, never stopped
    /// ([`UnverifiedPid`]).
    pub fn unverified(&self, job: JobRef<'_>) -> bool {
        self.pid_check(job) == Some(PidCheck::Unverified)
    }

    /// [`Self::get_statuses`] for a display path (`sim list`/`show`, `test
    /// list`/`show`, `build list`/`show`) or `sim delete`: under a progress
    /// line (the per-job fallback after a failed listing can take a while),
    /// failing with "interrupted" on Ctrl-C rather than handing back the
    /// statuses it did not get.
    pub fn get_statuses_shown(&self, jobs: &[JobRef<'_>]) -> Res<Statuses> {
        let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
        let asking = progress.add_child("ask the scheduler");
        asking.init(Some(jobs.len()), Some(prodash::unit::label("jobs")));
        let asking = std::sync::Mutex::new(asking);
        let statuses = self.get_statuses_counted(jobs, &|n| asking.lock().expect("progress poisoned").inc_by(n));
        drop(asking);
        if let Some(renderer) = renderer {
            renderer.shutdown_and_wait();
        }
        if gix::interrupt::is_triggered() {
            bail!("interrupted");
        }
        Ok(statuses)
    }

    /// Whether this machine's status query tells a job it does not know
    /// from a query that failed: it sets `unknown-job-pattern` (§10).
    pub fn tells_failed_queries(&self) -> bool {
        self.meta.scheduler.unknown_job_pattern.is_some()
    }

    /// The `job-ids = "pids"` checks (§10), `None` on a batch scheduler's
    /// machine. A recorded host must be this one (another host's pid is
    /// [`PidCheck::Elsewhere`]) and the recorded start time that of the
    /// process with the pid now: a pid is used again once its process has
    /// gone. A job recorded without either (by an older cactup) is asked
    /// about only where its pid has a process here that started no later
    /// than the record was made, and never stopped.
    fn pid_check(&self, job: JobRef<'_>) -> Option<PidCheck> {
        if self.job_ids() != JobIds::Pids {
            return None;
        }
        let Ok(pid) = job.id.parse::<u32>() else { return Some(PidCheck::Gone) };
        Some(match job.host {
            None => match crate::lock::proc_starttime(pid) {
                None => PidCheck::Gone,
                // Started well after the record was made: not the job.
                Some(_) if job
                    .since
                    .zip(crate::lock::proc_start_wall(pid))
                    .is_some_and(|(since, started)| started > std::time::SystemTime::from(since) + OLD_RECORD_SLACK) =>
                {
                    PidCheck::Gone
                }
                Some(_) => PidCheck::Unverified,
            },
            Some(host) if host != crate::lock::our_hostname() => PidCheck::Elsewhere,
            Some(_) => match job.start {
                Some(start) if crate::lock::proc_starttime(pid) == Some(start) => PidCheck::Here,
                _ => PidCheck::Gone,
            },
        })
    }

    /// Whether `output`, what `get-status` printed for `job_id` (stdout and
    /// stderr, as [`Self::get_status`] reads it), has a line matching
    /// `[scheduler].unknown-job-pattern` (§10): the scheduler saying it does
    /// not know the job. `None` when the machine declares no such pattern.
    pub fn unknown_job(&self, output: &str, job_id: &str) -> Res<Option<bool>> {
        let Some(pattern) = self.meta.scheduler.unknown_job_pattern.as_deref() else { return Ok(None) };
        let pattern = pattern.replace("@JOB_ID@", &regex::escape(job_id));
        let regex = Regex::new(&pattern).with_context(|| format!("invalid unknown-job-pattern {pattern:?}"))?;
        Ok(Some(output.lines().any(|line| regex.is_match(line))))
    }

    /// Live status for many jobs at once (§10): dedupes the jobs and runs the
    /// per-job `get-status` query on a bounded worker pool, because each query is
    /// its own scheduler round-trip and a long simulation history makes those the
    /// dominant cost of `sim list`. Jobs whose query failed are simply absent from
    /// the result — the same tolerance the display paths already get from
    /// `get_status(..).ok()`. A pid the `job-ids = "pids"` checks take for
    /// gone is U with nothing run, as in [`Self::get_status`].
    // Every command asks through `get_statuses_shown`; tests ask directly.
    #[cfg(test)]
    pub fn get_statuses(&self, jobs: &[JobRef<'_>]) -> Statuses {
        self.get_statuses_counted(jobs, &|_| {})
    }

    /// [`Self::get_statuses`], calling `done(n)` as each `n` distinct jobs
    /// are answered (or failed), for a caller's progress line.
    pub fn get_statuses_counted(&self, jobs: &[JobRef<'_>], done: &(dyn Fn(usize) + Sync)) -> Statuses {
        let mut seen = std::collections::HashSet::new();
        let mut results = HashMap::new();
        let mut asked: Vec<JobRef<'_>> = Vec::new();
        for job in jobs {
            if !seen.insert(job.key()) {
                continue;
            }
            if matches!(self.pid_check(*job), Some(PidCheck::Gone | PidCheck::Elsewhere)) {
                results.insert(job.key(), JobStatus::Unknown);
                done(1);
            } else {
                asked.push(*job);
            }
        }
        if asked.is_empty() {
            return Statuses(results);
        }
        // One listing answers for every job when the machine offers one; a
        // machine that doesn't, or a listing that failed, falls back to the
        // per-job pool below.
        if let Some(many) = self.meta.scheduler.get_status_many.as_deref()
            && let Ok(statuses) = self.status_listing(many, &asked)
        {
            done(asked.len());
            results.extend(statuses);
            return Statuses(results);
        }
        if asked.len() == 1 {
            if let Ok(status) = self.get_status(asked[0]) {
                results.insert(asked[0].key(), status);
            }
            done(1);
            return Statuses(results);
        }

        let workers = STATUS_WORKERS.min(asked.len());
        let queue: std::sync::Mutex<std::collections::VecDeque<JobRef<'_>>> =
            std::sync::Mutex::new(asked.into_iter().collect());
        let results = std::sync::Mutex::new(results);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    // Ctrl-C: no query more (each would fail "interrupted"
                    // anyway, once it had run).
                    if gix::interrupt::is_triggered() {
                        return;
                    }
                    let job = match queue.lock().expect("status queue poisoned").pop_front() {
                        Some(job) => job,
                        None => return,
                    };
                    if let Ok(status) = self.get_status(job) {
                        let mut results = results.lock().expect("status results poisoned");
                        results.insert(job.key(), status);
                    }
                    done(1);
                });
            }
        });

        Statuses(results.into_inner().expect("status results poisoned"))
    }

    /// Classify every job in `jobs` from ONE run of the machine's
    /// `get-status-many` listing (§10). A job is looked up by exact match on
    /// the line's first field — never by regex against the whole listing, which
    /// would let one job's line answer for another job's id — and the line is
    /// then classified exactly as a single-job query would be. A job the
    /// listing does not mention is U: not in the queue (a listing says nothing
    /// of the jobs it leaves out, so `unknown-job-pattern` has no part here).
    /// A listing that exits non-zero failed (Slurm's `squeue` exits 1 on a
    /// timeout, printing nothing on stdout): that is an error, and
    /// [`Self::get_statuses`] falls back to the per-job queries, whose own
    /// failures their callers handle. An empty listing exits 0: no job
    /// listed.
    fn status_listing(
        &self,
        template: &str,
        jobs: &[JobRef<'_>],
    ) -> Res<HashMap<JobKey, JobStatus>> {
        let vars = Self::query_vars("");
        let cmd = self.command_for("get-status-many", Some(template), &vars, None)?;
        let output = self.spawn_sh(&cmd, false)?;
        // As in `get_status`: a listing Ctrl-C cut short lists nothing,
        // which says nothing of the jobs.
        if gix::interrupt::is_triggered() {
            bail!("interrupted");
        }

        let mut lines: HashMap<&str, &str> = HashMap::new();
        for line in output.lines() {
            if let Some(id) = line.split_whitespace().next() {
                lines.insert(id, line);
            }
        }

        let mut statuses = HashMap::with_capacity(jobs.len());
        for job in jobs {
            let status = match lines.get(job.id) {
                Some(line) => classify(line, job.id, self.meta)?,
                None => JobStatus::Unknown,
            };
            statuses.insert(job.key(), status);
        }
        Ok(statuses)
    }

    /// Stop `job` via the machine `stop` command. On a `job-ids = "pids"`
    /// machine, nothing is run for a pid that is not the job's process
    /// ([`Stopped::Gone`]) or is another host's ([`Stopped::Elsewhere`]); an
    /// older record's pid with a process here is refused ([`UnverifiedPid`]:
    /// that process may be another one), and so is the job's own process
    /// where its process group is not its own ([`SharedGroup`]: the stop
    /// command would reach its group's other processes).
    pub fn stop(&self, job: JobRef<'_>) -> Res<Stopped> {
        match self.pid_check(job) {
            Some(PidCheck::Gone) => return Ok(Stopped::Gone),
            Some(PidCheck::Elsewhere) => return Ok(Stopped::Elsewhere(job.host.unwrap_or_default().to_owned())),
            Some(PidCheck::Unverified) => return Err(UnverifiedPid::of(job.id).into()),
            Some(PidCheck::Here) if !Self::own_group(job.id) => return Err(SharedGroup(job.id.to_owned()).into()),
            Some(PidCheck::Here) => {}
            None => {}
        }
        let vars = Self::query_vars(job.id);
        let cmd = self.command_for("stop", self.meta.scheduler.stop.as_deref(), &vars, None)?;
        // A stop command Ctrl-C ended is no failure of the scheduler's.
        self.spawn_sh(&cmd, false).map_err(|e| if gix::interrupt::is_triggered() { anyhow!("interrupted") } else { e })?;
        Ok(Stopped::Sent)
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
            .substitute(template, Syntax::Shell)
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
    Ok(classify_listed(output, job_id, meta)?.unwrap_or(JobStatus::Unknown))
}

/// [`classify`], `None` where no line matches `status-pattern`: the output
/// does not list the job ([`Scheduler::get_status`] then reads
/// `unknown-job-pattern`).
fn classify_listed(output: &str, job_id: &str, meta: &Meta) -> Res<Option<JobStatus>> {
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
        return Ok(None);
    };
    Ok(Some(if holding.is_match(line) {
        JobStatus::Holding
    } else if queued.is_match(line) {
        JobStatus::Queued
    } else if running.is_match(line) {
        JobStatus::Running
    } else {
        JobStatus::Error
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare<'a>(ids: &[&'a str]) -> Vec<JobRef<'a>> {
        ids.iter().map(|id| JobRef::bare(id)).collect()
    }

    fn meta(scheduler_toml: &str) -> Meta {
        toml::from_str(&format!(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            {scheduler_toml}
            [queues.local]
            default = true
            [submitscripts]
            "default" = ["local"]
            [runscripts]
            "default" = ["local"]
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
        assert_eq!(Scheduler::new(&m).submit(&vars, None).unwrap().id, "77-run.sh");
    }

    #[test]
    fn submit_inside_a_universe() {
        let m = meta(r#"submit = "echo id=@SCRIPTFILE@""#);
        let universe: Universe = toml::from_str(r#"wrapper-argv = ["env", "IGNORED=1"]"#).unwrap();
        let mut vars = VarSet::new();
        vars.set("SCRIPTFILE", "s.sh");
        // submit-pattern defaults to (.*) over the trimmed output.
        assert_eq!(Scheduler::new(&m).submit(&vars, Some(("sif", &universe))).unwrap().id, "id=s.sh");
    }

    #[test]
    fn submit_env_follows_the_submit_universe() {
        // The submit universe's submit-env-setup override (§6.1) replaces the
        // machine key for `submit` only; get-status/stop keep the machine env.
        let mut m = meta(r#"submit = "echo id=@SCRIPTFILE@-$CACTUP_SUB_ENV""#);
        m.environment.submit_env_setup = Some("CACTUP_SUB_ENV=base".to_owned());
        let uni: Universe = toml::from_str(r#"submit-env-setup = "CACTUP_SUB_ENV=uni""#).unwrap();
        m.universes.insert("u".to_owned(), uni);
        let mut vars = VarSet::new();
        vars.set("SCRIPTFILE", "s.sh");
        let sched = Scheduler::new(&m);
        assert_eq!(sched.submit(&vars, None).unwrap().id, "id=s.sh-base");
        assert_eq!(
            sched.submit(&vars, Some(("u", &m.universes["u"]))).unwrap().id,
            "id=s.sh-uni"
        );
    }

    /// The generation-3 job-id keys (§10): exposed as declared, and the
    /// pattern sees `get-status`'s stderr, where Slurm says it has no such
    /// job, because the query's output is read with both streams.
    #[test]
    fn the_job_id_keys_are_exposed_and_the_pattern_sees_stderr() {
        let m = meta(r#"get-status = "squeue -j @JOB_ID@""#);
        let sched = Scheduler::new(&m);
        assert_eq!(sched.job_ids(), JobIds::Scheduler);
        assert_eq!(sched.unknown_job("anything", "12").unwrap(), None);

        let m = meta(
            r#"
            job-ids = "pids"
            get-status = "echo JOBID; echo 'slurm_load_jobs error: Invalid job id specified' >&2; exit 1"
            unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified|^@JOB_ID@ gone$'
            "#,
        );
        m.validate("fake").unwrap();
        let sched = Scheduler::new(&m);
        assert_eq!(sched.job_ids(), JobIds::Pids);
        let vars = Scheduler::query_vars("12.x");
        let cmd = sched.command_for("get-status", m.scheduler.get_status.as_deref(), &vars, None).unwrap();
        let output = sched.spawn_sh(&cmd, true).unwrap();
        assert_eq!(sched.unknown_job(&output, "12.x").unwrap(), Some(true), "{output}");
        assert_eq!(sched.unknown_job("JOBID\n12.x gone\n", "12.x").unwrap(), Some(true));
        assert_eq!(sched.unknown_job("JOBID\n12y gone\n", "12.x").unwrap(), Some(false));
        // The scheduler says it does not know the job: gone. (On this
        // `pids` machine the id is no pid, so nothing is even asked.)
        assert_eq!(sched.get_status(JobRef::bare("12.x")).unwrap(), JobStatus::Unknown);
    }

    /// A live child process of the test's own, killed and reaped on drop.
    struct Sleeper(std::process::Child);

    impl Sleeper {
        /// Leading a process group of its own, as a job `setsid` starts.
        fn start() -> Sleeper {
            use std::os::unix::process::CommandExt;
            Sleeper(std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap())
        }

        /// In the test's own process group, as a job a submit command
        /// backgrounds without `setsid`.
        fn start_in_our_group() -> Sleeper {
            Sleeper(std::process::Command::new("sleep").arg("30").spawn().unwrap())
        }

        fn pid(&self) -> String {
            self.0.id().to_string()
        }

        fn start_time(&self) -> u64 {
            crate::lock::proc_starttime(self.0.id()).unwrap()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A `job-ids = "pids"` machine whose `get-status` and `stop` leave a
    /// mark when they run: what the generic machine's `ps` and `pkill` would
    /// be asked, without asking them.
    fn pids_machine(dir: &std::path::Path) -> Meta {
        meta(&format!(
            r#"
            job-ids = "pids"
            submit = "sleep 30 > /dev/null 2>&1 & echo $!"
            get-status = "echo asked >> {asked}; echo '  @JOB_ID@ ?  0:00 sleep'"
            stop = "echo @JOB_ID@ >> {stopped}"
            status-pattern = "^ *@JOB_ID@ "
            running-pattern = "^"
            "#,
            asked = dir.join("asked").display(),
            stopped = dir.join("stopped").display()
        ))
    }

    fn marks(dir: &std::path::Path, what: &str) -> usize {
        std::fs::read_to_string(dir.join(what)).map(|s| s.lines().count()).unwrap_or(0)
    }

    /// `job-ids = "pids"` (§10): `submit` records this host and the
    /// backgrounded process's start time, read at once; a process gone
    /// before cactup reads it is recorded as such (a host, no start). A
    /// batch scheduler's job id gets neither.
    #[test]
    fn a_pid_is_recorded_with_its_host_and_start_time() {
        let dir = tempfile::tempdir().unwrap();
        let m = pids_machine(dir.path());
        let job = Scheduler::new(&m).submit(&VarSet::new(), None).unwrap();
        let pid: u32 = job.id.parse().unwrap();
        // Kill what the submit started, whatever the assertions say.
        let _reap = kill_on_drop(pid);
        assert_eq!(job.host.as_deref(), Some(crate::lock::our_hostname().as_str()));
        assert_eq!(job.start, crate::lock::proc_starttime(pid));
        assert!(job.start.is_some());

        let gone = meta(r#"job-ids = "pids"
            submit = "sh -c 'exit 0' & p=$!; wait $p; echo $p""#);
        let job = Scheduler::new(&gone).submit(&VarSet::new(), None).unwrap();
        assert_eq!(job.host.as_deref(), Some(crate::lock::our_hostname().as_str()));
        assert_eq!(job.start, None, "a process gone at once is recorded gone");

        let batch = meta(r#"submit = "echo 'Submitted batch job 77'"
            submit-pattern = "job ([0-9]+)""#);
        let job = Scheduler::new(&batch).submit(&VarSet::new(), None).unwrap();
        assert_eq!(job, JobRecord { id: "77".to_owned(), host: None, start: None, since: None });
    }

    /// As `submit`, a `blocking-submit` on a `job-ids = "pids"` machine:
    /// the start time is read as the id's line arrives, while the command
    /// still waits on the process.
    #[test]
    fn a_blocking_submits_pid_is_recorded_while_it_runs() {
        let m = meta(r#"job-ids = "pids"
            blocking-submit = "sleep 1 & p=$!; echo $p; wait $p""#);
        let mut seen = None;
        let job = Scheduler::new(&m)
            .submit_blocking(&VarSet::new(), None, &mut |job| {
                seen = Some((job.clone(), crate::lock::proc_starttime(job.id.parse().unwrap())));
                Ok(())
            })
            .unwrap();
        let (reported, start_then) = seen.expect("the job id was never reported");
        assert_eq!(reported, job);
        assert!(job.start.is_some());
        assert_eq!(job.start, start_then);
        assert_eq!(job.host.as_deref(), Some(crate::lock::our_hostname().as_str()));
    }

    /// Kills `pid` (a process this test started through a shell, so not
    /// its child) when dropped.
    fn kill_on_drop(pid: u32) -> impl Drop {
        struct Kill(u32);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = std::process::Command::new("kill").arg(self.0.to_string()).status();
            }
        }
        Kill(pid)
    }

    /// The pid checks (§10): the job's own process, on the recorded host
    /// with the recorded start time, is asked about and stopped; a pid
    /// used again (a live process that started at another time) and a
    /// process recorded gone are taken as gone; another host's pid is
    /// neither asked about nor stopped here, and never taken for gone
    /// (`Stopped::Elsewhere`): neither `get-status` nor `stop` runs for any
    /// of them.
    #[test]
    fn a_pid_is_asked_about_only_while_it_is_the_jobs_process() {
        let dir = tempfile::tempdir().unwrap();
        let m = pids_machine(dir.path());
        let sched = Scheduler::new(&m);
        let sleeper = Sleeper::start();
        let pid = sleeper.pid();
        let here = crate::lock::our_hostname();

        let reused = JobRef::new(&pid, Some(&here), Some(sleeper.start_time() + 1), None);
        assert_eq!(sched.get_status(reused).unwrap(), JobStatus::Unknown);
        assert_eq!(sched.stop(reused).unwrap(), Stopped::Gone);
        let elsewhere = JobRef::new(&pid, Some("some-other-host"), Some(sleeper.start_time()), None);
        assert_eq!(sched.recorded_elsewhere(elsewhere), Some("some-other-host"));
        assert_eq!(sched.get_status(elsewhere).unwrap(), JobStatus::Unknown);
        assert_eq!(sched.stop(elsewhere).unwrap(), Stopped::Elsewhere("some-other-host".to_owned()));
        let gone_at_submit = JobRef::new(&pid, Some(&here), None, None);
        assert_eq!(sched.get_status(gone_at_submit).unwrap(), JobStatus::Unknown);
        assert_eq!(sched.stop(gone_at_submit).unwrap(), Stopped::Gone);
        assert_eq!(sched.recorded_elsewhere(gone_at_submit), None);
        let statuses = sched.get_statuses(&[reused, elsewhere, gone_at_submit]);
        assert_eq!(statuses.len(), 3, "the same id with other records is another job");
        assert!([reused, elsewhere, gone_at_submit].iter().all(|job| statuses.get(*job) == Some(&JobStatus::Unknown)));
        assert_eq!((marks(dir.path(), "asked"), marks(dir.path(), "stopped")), (0, 0), "a pid not the job's was asked about");

        let own = JobRef::new(&pid, Some(&here), Some(sleeper.start_time()), None);
        assert_eq!(sched.get_status(own).unwrap(), JobStatus::Running);
        assert_eq!(sched.get_statuses(&[own, reused]).get(own), Some(&JobStatus::Running));
        assert_eq!(sched.stop(own).unwrap(), Stopped::Sent);
        assert_eq!((marks(dir.path(), "asked"), marks(dir.path(), "stopped")), (2, 1));
    }

    /// The job's own process whose process group is not its own (a submit
    /// without `setsid`, or a foreground run started from a script):
    /// the stop command (`pkill -g`) would reach the group's other
    /// processes, so nothing is run and the refusal names the pid.
    #[test]
    fn a_pid_sharing_its_process_group_is_never_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let m = pids_machine(dir.path());
        let sched = Scheduler::new(&m);
        let sleeper = Sleeper::start_in_our_group();
        let pid = sleeper.pid();
        let here = crate::lock::our_hostname();
        let own = JobRef::new(&pid, Some(&here), Some(sleeper.start_time()), None);
        assert_eq!(sched.get_status(own).unwrap(), JobStatus::Running);
        let err = sched.stop(own).unwrap_err();
        assert!(err.downcast_ref::<SharedGroup>().is_some(), "{err:#}");
        let text = format!("{err:#}");
        assert!(text.contains("shares its process group") && text.contains(&format!("pstree -p {pid}")), "{text}");
        assert!(!text.contains("kill"), "no single command stops it alone: {text}");
        assert_eq!(marks(dir.path(), "stopped"), 0);
    }

    /// A run the way a job started without `setsid` from an interactive
    /// shell is: the group's leader (the cactup that submitted it) gone,
    /// the group holding only the job's processes, a run-script (`top`)
    /// and the process it started (`child`).
    struct OrphanRun {
        top: u32,
        child: u32,
        pgid: u32,
    }

    impl OrphanRun {
        fn start() -> OrphanRun {
            OrphanRun::start_with("sleep 300")
        }

        /// The run-script runs `child` (in the background) and waits.
        fn start_with(child: &str) -> OrphanRun {
            use std::os::unix::process::CommandExt;
            let out = std::process::Command::new("sh")
                .args(["-c", &format!("sh -c '{child} & wait' > /dev/null 2>&1 < /dev/null & echo $!")])
                .process_group(0)
                .output()
                .unwrap();
            let top: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
            let pgid = crate::lock::proc_pgrp(top).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let group = crate::lock::proc_group(top).unwrap();
                if let Some(&child) = group.tree.iter().find(|&&p| p != top) {
                    return OrphanRun { top, child, pgid };
                }
                assert!(std::time::Instant::now() < deadline, "the run's child never started");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        fn gone(&self) -> bool {
            // A killed process lingers as a zombie until its parent (init,
            // or the run-script) reaps it: gone either way.
            [self.top, self.child].iter().all(|p| {
                std::fs::read_to_string(format!("/proc/{p}/stat"))
                    .map(|stat| stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')))
                    .unwrap_or(true)
            })
        }

        fn await_gone(&self) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !self.gone() {
                if std::time::Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            true
        }
    }

    impl Drop for OrphanRun {
        fn drop(&mut self) {
            let _ = std::process::Command::new("kill").args(["-9", &self.top.to_string(), &self.child.to_string()]).status();
        }
    }

    /// A job whose leader is gone but whose process group holds only the
    /// job's processes (submitted without `setsid` from an interactive
    /// shell, as master's MDB did) is the job's own group: the machine's
    /// stop command is run (the generic machine's real one stops the whole
    /// run and nothing else), and the advice for an older record of it,
    /// `kill -- -<pgid>`, stops the whole run too.
    #[test]
    fn a_group_holding_only_the_jobs_processes_is_its_own() {
        let run = OrphanRun::start();
        assert_ne!(run.pgid, run.top, "the run's top process does not lead its group");
        let group = crate::lock::proc_group(run.top).unwrap();
        assert!(group.own, "{group:?}");
        assert_eq!((group.pgid, group.tree.clone()), (run.pgid, vec![run.top.min(run.child), run.top.max(run.child)]));
        // Our own group holds the test runner, which no sleeper started.
        let ours = Sleeper::start_in_our_group();
        assert!(!crate::lock::proc_group(ours.0.id()).unwrap().own);

        let dir = tempfile::tempdir().unwrap();
        let marking = pids_machine(dir.path());
        let sched = Scheduler::new(&marking);
        let (top, here) = (run.top.to_string(), crate::lock::our_hostname());
        let start = crate::lock::proc_starttime(run.top);
        let job = JobRef::new(&top, Some(&here), start, None);
        assert!(!sched.shares_group(job) && sched.stoppable_pid(job));
        assert_eq!(sched.stop(job).unwrap(), Stopped::Sent);
        assert_eq!(marks(dir.path(), "stopped"), 1);

        // The generic machine's own stop command, for real.
        let generic = meta(
            r#"
            job-ids = "pids"
            get-status = "ps @JOB_ID@"
            stop = "pkill -g $(ps -o pgid= -p @JOB_ID@)"
            status-pattern = "^ *@JOB_ID@ "
            running-pattern = "^"
            "#,
        );
        assert_eq!(Scheduler::new(&generic).stop(job).unwrap(), Stopped::Sent);
        assert!(run.await_gone(), "the stop command left part of the run");
        assert!(crate::lock::proc_starttime(ours.0.id()).is_some(), "the stop command reached another group");

        // An older record of such a run: never stopped, and the advice
        // stops the whole run when followed.
        let run = OrphanRun::start();
        let text = UnverifiedPid::of(&run.top.to_string()).to_string();
        let advice = format!("kill -- -{}", run.pgid);
        assert!(text.contains(&format!("`{advice}`")), "{text}");
        assert!(std::process::Command::new("sh").args(["-c", &advice]).status().unwrap().success());
        assert!(run.await_gone(), "{advice} left part of the run");
        assert!(crate::lock::proc_starttime(ours.0.id()).is_some(), "{advice} reached another group");
    }

    /// A run whose group is its own but one of whose processes made a group
    /// of its own (`setsid`): `kill -- -<pgid>` would leave that one, so the
    /// advice names it rather than claim the whole run.
    #[test]
    fn the_advice_names_a_process_of_the_run_outside_its_group() {
        let run = OrphanRun::start_with("setsid sleep 300");
        // The child is forked before it makes its own group: wait for that.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let group = loop {
            let group = crate::lock::proc_group(run.top).unwrap();
            if !group.outside.is_empty() || std::time::Instant::now() > deadline {
                break group;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(group.own, "{group:?}");
        assert_eq!(group.outside, vec![run.child], "{group:?}");
        let text = UnverifiedPid::of(&run.top.to_string()).to_string();
        assert!(text.contains(&format!("`kill -- -{}`", run.pgid)) && text.contains(&format!("but not pids {}", run.child)), "{text}");
        assert!(!text.contains("every process it started"), "{text}");
    }

    /// A run-script in a group that holds processes it did not start (the
    /// test runner's): no single command stops it alone. An older record
    /// of it names the processes to stop; a verified one is refused.
    #[test]
    fn a_run_sharing_its_group_with_others_names_its_processes() {
        let mut shell = std::process::Command::new("sh").args(["-c", "sleep 300 & wait"]).spawn().unwrap();
        let top = shell.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let group = loop {
            let group = crate::lock::proc_group(top).unwrap();
            if group.tree.len() == 2 {
                break group;
            }
            assert!(std::time::Instant::now() < deadline, "the run's child never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(!group.own);
        let text = UnverifiedPid::of(&top.to_string()).to_string();
        let tree: Vec<String> = group.tree.iter().map(u32::to_string).collect();
        assert!(text.contains(&format!("now pids {}", tree.join(", "))) && text.contains(&format!("pstree -p {top}")), "{text}");
        assert!(!text.contains("kill"), "{text}");
        let _ = std::process::Command::new("kill").args(["-9", &tree[0], &tree[1]]).status();
        let _ = shell.wait();
    }

    /// After a record's end, a stop command that fails settles nothing,
    /// but for one exception: after a failed status query, a refusal whose
    /// output matches `unknown-job-pattern` (the scheduler saying it does
    /// not know the job: Torque's `qdel` of a forgotten job, exit 153) is
    /// taken as the job ended, what it said kept. A transient failure (the
    /// controller not reached) is an error after a failed query, saying so
    /// with the pattern hint; and any failure is an error after the
    /// scheduler just listed the job.
    #[test]
    fn a_stop_that_fails_after_the_end_settles_only_a_job_the_scheduler_does_not_know() {
        let with_stop = |stop: &str| {
            meta(&format!(
                r#"
                stop = "{stop}"
                unknown-job-pattern = 'Unknown Job Id'
                status-pattern = "^@JOB_ID@ "
                "#
            ))
        };
        let unknown = with_stop("echo 'qdel: Unknown Job Id 4242.qb2' >&2; exit 153");
        let sched = Scheduler::new(&unknown);
        let failed = &format!("the status query for job 4242 failed. It printed:\nqstat: cannot connect to server qb2\n{QUERY_HINT}");
        for case in [StopCase::Unanswered(failed), StopCase::Unread] {
            match sched.stop_judged(JobRef::bare("4242"), case).unwrap() {
                EndedStop::NotKnown(said) => assert_eq!(said, "qdel: Unknown Job Id 4242.qb2 (exit status: 153)"),
                other => panic!("{case:?}: {other:?}"),
            }
        }
        // Just listed: the same refusal is an error, nothing settled.
        let err = sched.stop_judged(JobRef::bare("4242"), StopCase::Listed).unwrap_err().to_string();
        assert!(err.contains("the stop command for job 4242 failed: qdel: Unknown Job Id"), "{err}");
        assert!(sched.stop(JobRef::bare("4242")).is_err(), "before the end it is an error still");
        for transient in [
            "scancel: error: Kill job error on job id 4242: Socket timed out on send/recv operation",
            "scancel: error: Unable to contact slurm controller (connect failure)",
        ] {
            let m = with_stop(&format!("echo '{transient}' >&2; exit 1"));
            let sched = Scheduler::new(&m);
            // After a failed query: both outputs shown, with the hint.
            let err = sched.stop_judged(JobRef::bare("4242"), StopCase::Unanswered(failed)).unwrap_err().to_string();
            assert!(err.contains("failed too: ") && err.contains(transient), "{err}");
            assert!(err.contains("qstat: cannot connect to server qb2"), "the failed query's output: {err}");
            assert!(err.contains("unknown-job-pattern ('Unknown Job Id') does not match it"), "{err}");
            assert_eq!(err.matches("If the scheduler's message").count(), 1, "one hint: {err}");
            let err = sched.stop_judged(JobRef::bare("4242"), StopCase::Unread).unwrap_err().to_string();
            assert!(err.contains("the stop command for job 4242 failed") && err.contains(transient), "{err}");
            assert!(err.contains("does not match it") && err.contains("once it no longer lists it"), "{err}");
            let err = sched.stop_judged(JobRef::bare("4242"), StopCase::Listed).unwrap_err().to_string();
            assert!(err.contains("the stop command for job 4242 failed") && err.contains(transient), "{err}");
        }
        let taking = with_stop("true");
        assert_eq!(Scheduler::new(&taking).stop_judged(JobRef::bare("4242"), StopCase::Unread).unwrap(), EndedStop::Sent);
        // A machine with no `unknown-job-pattern`: no pattern to fix is named.
        let unpatterned = meta(
            r#"
            stop = "echo 'qdel: Invalid state for job - COMPLETE 4242.qb' >&2; exit 15"
            status-pattern = "^@JOB_ID@ "
            "#,
        );
        let err = Scheduler::new(&unpatterned).stop_judged(JobRef::bare("4242"), StopCase::Unread).unwrap_err().to_string();
        assert!(err.contains("Invalid state") && err.contains("once it no longer lists it"), "{err}");
        assert!(!err.contains("unknown-job-pattern"), "{err}");
    }

    /// A job an older cactup recorded (no host, no start time) on a
    /// `job-ids = "pids"` machine: asked about only where its pid has a
    /// process here that started no later than the record was made (give
    /// or take the slack), and never stopped, since that process may be
    /// another. One that started well after the record is not the job.
    #[test]
    fn an_older_records_pid_is_asked_about_where_it_runs_and_never_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let m = pids_machine(dir.path());
        let sched = Scheduler::new(&m);
        let sleeper = Sleeper::start();
        let pid = sleeper.pid();

        for since in [None, Some(Utc::now())] {
            let old = JobRef::new(&pid, None, None, since);
            assert_eq!(sched.get_status(old).unwrap(), JobStatus::Running);
            let err = sched.stop(old).unwrap_err();
            assert!(err.downcast_ref::<UnverifiedPid>().is_some(), "{err:#}");
            assert!(format!("{err:#}").contains(&format!("pid {pid}")), "{err:#}");
        }
        assert_eq!((marks(dir.path(), "asked"), marks(dir.path(), "stopped")), (2, 0));

        // The record is an hour older than the process with its pid.
        let long_ago = Utc::now() - chrono::Duration::hours(1);
        let reused = JobRef::new(&pid, None, None, Some(long_ago));
        assert_eq!(sched.get_status(reused).unwrap(), JobStatus::Unknown);
        assert_eq!(sched.stop(reused).unwrap(), Stopped::Gone);

        let dead = 999_999_999u32.to_string();
        let old_dead = JobRef::new(&dead, None, None, None);
        assert_eq!(sched.get_status(old_dead).unwrap(), JobStatus::Unknown);
        assert_eq!(sched.stop(old_dead).unwrap(), Stopped::Gone);
        assert_eq!((marks(dir.path(), "asked"), marks(dir.path(), "stopped")), (2, 0));
    }

    /// A job listed in a state the patterns read is live, before a record's
    /// end and after it (there "still listed": queued again, leaving the
    /// queue as `CG`, `TO`, or starting again as `CF`, `R`, which cactup
    /// cannot tell apart). One in a state no pattern reads is live before
    /// the end (suspended, as Slurm's `S`, or over before its run recorded
    /// it) and not after it (the ended job: Torque's `C`, Slurm's `CD`).
    #[test]
    fn a_job_listed_is_live_but_after_the_end_only_in_a_read_state() {
        use JobStatus::*;
        for (status, before, after) in
            [(Running, true, true), (Queued, true, true), (Holding, true, true), (Error, true, false), (Unknown, false, false)]
        {
            assert_eq!((is_live(status, false), is_live(status, true)), (before, after), "{status:?}");
        }
    }

    /// `unknown-job-pattern` (§10): no line for the job is a job gone only
    /// where the scheduler says it does not know it (Slurm 24.11's `squeue
    /// -j`: the message on stderr, nothing on stdout, exit 1); otherwise
    /// the query failed, an error naming the command and its output. A
    /// machine without the key reads both as gone, as before; the batch
    /// listing reads a job it leaves out as gone either way.
    #[test]
    fn a_failed_status_query_is_an_error_where_the_machine_can_tell() {
        let squeue = |stderr: &str, extra: &str| {
            meta(&format!(
                r#"
                get-status = "echo '{stderr}' >&2; exit 1"
                status-pattern = "^ *@JOB_ID@ "
                running-pattern = " R "
                {extra}
                "#
            ))
        };
        let pattern = "unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'";
        let unknown = squeue("slurm_load_jobs error: Invalid job id specified", pattern);
        assert_eq!(Scheduler::new(&unknown).get_status(JobRef::bare("4242")).unwrap(), JobStatus::Unknown);

        let failed = squeue("slurm_load_jobs error: Socket timed out on send/recv operation", pattern);
        let sched = Scheduler::new(&failed);
        let err = sched.get_status(JobRef::bare("4242")).unwrap_err();
        assert!(err.downcast_ref::<FailedQuery>().is_some());
        let err = format!("{err:#}");
        assert!(err.contains("the status query for job 4242, `echo 'slurm_load_jobs error: Socket timed out"), "names the command: {err}");
        assert!(err.contains("status-pattern ('^ *@JOB_ID@ ')"), "names the status pattern: {err}");
        assert!(err.contains("unknown-job-pattern ('slurm_load_jobs error: Invalid job id specified')"), "{err}");
        assert!(err.contains("It printed:\nslurm_load_jobs error: Socket timed out on send/recv operation\n"), "{err}");
        assert!(err.contains("unknown-job-pattern does not match it: fix the pattern"), "{err}");
        assert!(!err.contains("could not ask"), "{err}");
        // Display paths take it as no status at all.
        assert!(sched.get_statuses(&[JobRef::bare("4242")]).is_empty());
        // After the record's end it is taken as the job ended; before, the error.
        assert_eq!(sched.get_status_of(JobRef::bare("4242"), true).unwrap(), JobStatus::Unknown);
        assert!(sched.get_status_of(JobRef::bare("4242"), false).is_err());

        let silent = squeue("", pattern);
        let err = format!("{:#}", Scheduler::new(&silent).get_status(JobRef::bare("4242")).unwrap_err());
        assert!(err.ends_with("It printed nothing."), "{err}");

        let without = squeue("slurm_load_jobs error: Socket timed out on send/recv operation", "");
        assert_eq!(Scheduler::new(&without).get_status(JobRef::bare("4242")).unwrap(), JobStatus::Unknown);

        let listing = meta(&format!(
            r#"
            get-status-many = "printf '1 R\n'"
            status-pattern = "^ *@JOB_ID@ "
            running-pattern = " R"
            {pattern}
            "#
        ));
        let statuses = Scheduler::new(&listing).get_statuses(&bare(&["1", "2"]));
        assert_eq!(statuses.get(JobRef::bare("2")), Some(&JobStatus::Unknown));
    }

    /// After a record's end its job is asked about only on a batch
    /// scheduler and only within the requeue window (14 days): a job id
    /// that old may name an unrelated job; a record with no end time is
    /// taken as ended.
    #[test]
    fn a_job_is_asked_about_after_its_end_only_within_the_requeue_window() {
        let batch = meta("");
        let batch = Scheduler::new(&batch);
        let pids = meta(r#"job-ids = "pids""#);
        let pids = Scheduler::new(&pids);
        let days = |n: i64| Some(Utc::now() - chrono::Duration::days(n));
        assert!(batch.asks_after_end("m", days(0)));
        assert!(batch.asks_after_end("m", days(13)));
        assert!(!batch.asks_after_end("m", days(15)));
        assert!(!batch.asks_after_end("m", None));
        assert!(!pids.asks_after_end("m", days(0)));
        // A record made for another machine (a home two clusters share):
        // never asked after its end.
        let m = meta("");
        let here = Scheduler::named(&m, "qb");
        assert!(here.asks_after_end("qb", days(0)));
        assert!(!here.asks_after_end("mike", days(0)));
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
                seen_while_running = Some((id.id.clone(), done.exists()));
                Ok(())
            })
            .unwrap()
            .id;

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
            Scheduler::new(&m).submit_blocking(&VarSet::new(), None, &mut |_| Ok(())).unwrap().id,
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
    /// `submit` — the two keys are the same command in two flavors.
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
            Scheduler::new(&m).submit_blocking(&vars, None, &mut |_| Ok(())).unwrap().id,
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
        assert_eq!(Scheduler::new(&m).get_status(JobRef::bare("31337")).unwrap(), JobStatus::Unknown);
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
        let statuses = sched.get_statuses(&bare(&ids));

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
            assert_eq!(statuses.get(JobRef::bare(id)).copied(), Some(status), "job {id}");
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
        let statuses = sched.get_statuses(&bare(&["7"]));
        assert_eq!(statuses.get(JobRef::bare("7")).copied(), Some(JobStatus::Running));
    }

    #[test]
    fn stop_propagates_failure() {
        let ok = meta(r#"stop = "true""#);
        Scheduler::new(&ok).stop(JobRef::bare("1")).unwrap();
        let bad = meta(r#"stop = "echo no permission >&2; false""#);
        let err = format!("{:#}", Scheduler::new(&bad).stop(JobRef::bare("1")).unwrap_err());
        assert!(err.contains("no permission"), "{err}");
        let missing = meta("");
        assert!(Scheduler::new(&missing).stop(JobRef::bare("1")).is_err());
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
        let statuses = sched.get_statuses(&bare(&ids));

        assert_eq!(statuses.get(JobRef::bare("3774765")).copied(), Some(JobStatus::Running));
        assert_eq!(statuses.get(JobRef::bare("3774766")).copied(), Some(JobStatus::Queued));
        assert_eq!(statuses.get(JobRef::bare("3774767")).copied(), Some(JobStatus::Holding));
        // Not in the listing at all ⇒ U, not absent.
        assert_eq!(statuses.get(JobRef::bare("9999999")).copied(), Some(JobStatus::Unknown));
    }

    /// A `get-status-many` listing that exits non-zero failed (Slurm 24.11's
    /// `squeue -h -u` on a timeout: the message on stderr, nothing on
    /// stdout, exit 1): it says nothing of any job, so the per-job queries
    /// answer instead, and their own failure is what callers get (an error
    /// before a record's end, taken as ended after it). An empty listing
    /// exits 0: every job asked about is gone.
    #[test]
    fn a_failing_listing_falls_back_to_the_per_job_queries() {
        let timeout = "echo 'slurm_load_jobs error: Socket timed out on send/recv operation' >&2; exit 1";
        let machine = |many: &str, single: &str| {
            meta(&format!(
                r#"
                get-status-many = "{many}"
                get-status = "{single}"
                unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'
                status-pattern = "^@JOB_ID@ "
                running-pattern = " R"
                "#
            ))
        };
        let jobs = bare(&["1", "2"]);

        let per_job_answers = machine(timeout, "echo '@JOB_ID@ R'");
        let statuses = Scheduler::new(&per_job_answers).get_statuses(&jobs);
        assert_eq!(statuses.get(JobRef::bare("1")), Some(&JobStatus::Running));
        assert_eq!(statuses.get(JobRef::bare("2")), Some(&JobStatus::Running));

        let both_fail = machine(timeout, timeout);
        let sched = Scheduler::new(&both_fail);
        assert!(sched.get_statuses(&jobs).is_empty(), "a failed listing marked jobs gone");
        let err = sched.get_status_of(JobRef::bare("1"), false).unwrap_err();
        assert!(err.downcast_ref::<FailedQuery>().is_some(), "{err:#}");
        assert_eq!(sched.get_status_of(JobRef::bare("1"), true).unwrap(), JobStatus::Unknown);

        let empty = machine("true", "echo '@JOB_ID@ R'");
        let statuses = Scheduler::new(&empty).get_statuses(&jobs);
        assert_eq!(statuses.get(JobRef::bare("1")), Some(&JobStatus::Unknown));
        assert_eq!(statuses.get(JobRef::bare("2")), Some(&JobStatus::Unknown));
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
        let _ = sched.get_statuses(&bare(&ids));

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
        let statuses = sched.get_statuses(&bare(&["3774765", "774765"]));
        assert_eq!(statuses.get(JobRef::bare("3774765")).copied(), Some(JobStatus::Running));
        assert_eq!(statuses.get(JobRef::bare("774765")).copied(), Some(JobStatus::Unknown));
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
        let statuses = sched.get_statuses(&bare(&["1", "2"]));
        assert_eq!(statuses.get(JobRef::bare("1")).copied(), Some(JobStatus::Running));
        assert_eq!(statuses.get(JobRef::bare("2")).copied(), Some(JobStatus::Queued));
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
