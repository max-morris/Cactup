//! What a resolution decided: the steps, the any-of choices, and what to record.
// Plan §2.3 (showing), §2.4 (records). Rendering only builds text; the caller prints it.

use std::fmt;

use super::remove::{is_requested, AddedFor};
use super::{PkgId, Release, Req, Why};

/// The plan's line when the flesh changes.
pub const FLESH_REBUILD: &str = "  The flesh changes; every config needs a rebuild";

/// What a [`Step::Replace`] takes the place of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replaced {
    /// A repo of the base thornlist.
    Base { repo: String },
    /// An added package (a flesh package another flesh displaces).
    Added { id: PkgId },
}

/// One thing the plan does.
#[derive(Clone, Debug)]
pub enum Step {
    /// A package the installation does not have.
    Add { release: Release, thorns: Vec<String>, why: Why },
    /// A named added package at the release it is at: recorded and fetched again, as re-adding
    /// does. `thorns`: every thorn enabled after.
    Readd { release: Release, thorns: Vec<String> },
    /// More thorns of a present package, at the release it is at (`release`, as read).
    AddThorns { id: PkgId, label: String, release: Release, thorns: Vec<String>, why: Why },
    /// A present package moved to another release. `thorns`: every thorn enabled after the move.
    Upgrade { from: Release, to: Release, thorns: Vec<String>, why: Why, flag: Option<&'static str> },
    /// Something dropped in favor of a package (shown after the step bringing it in). `thorns`:
    /// the base repo's checkouts it takes over; empty when it takes the whole repo. For the
    /// flesh, every config of the installation needs a rebuild. `explicit`: the package taking
    /// over was named (with `--replace`, or as the package the base repo is).
    Replace { replaced: Replaced, by: PkgId, thorns: Vec<String>, flesh: bool, explicit: bool },
    /// A present package left as it is, with something worth saying (a fork standing in, a
    /// named base package already there).
    Keep { id: PkgId, note: String },
}

/// How an any-of dependency chose its alternative (the rules, in order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceRule {
    /// An alternative the installation has, at a release that fits.
    Present,
    /// One the rest of the plan brings in, at a release that fits.
    Planned,
    /// The first listed (nothing else applied, or every alternative conflicts).
    FirstListed,
    /// The first listed that conflicts with nothing present or planned.
    FirstWithoutConflict,
}

impl fmt::Display for ChoiceRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ChoiceRule::Present => "already installed",
            ChoiceRule::Planned => "already in the plan",
            ChoiceRule::FirstListed => "the first listed",
            ChoiceRule::FirstWithoutConflict => "the first listed that conflicts with nothing",
        })
    }
}

/// Which any-of dependency a choice answers: the dependent and the alternatives' targets (stable
/// across the dependent's versions, unlike the claim's index).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChoiceKey {
    pub dependent: PkgId,
    pub alternatives: Vec<PkgId>,
}

/// One any-of decision: one per dependency, whichever alternative it chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub dependent: PkgId,
    /// `owner/name@version` of the dependent.
    pub dependent_label: String,
    /// The index of the dependency among the dependent release's claims.
    pub claim: usize,
    /// `any of et/carpet, et/cactuspugh`.
    pub label: String,
    pub alternatives: Vec<PkgId>,
    pub chosen: PkgId,
    pub rule: ChoiceRule,
}

impl Choice {
    pub fn key(&self) -> ChoiceKey {
        ChoiceKey { dependent: self.dependent.clone(), alternatives: self.alternatives.clone() }
    }
}

impl fmt::Display for Choice {
    /// `any of et/carpet, et/cactuspugh: the first listed`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.label, self.rule)
    }
}

/// Where a package (or a library inside it) places a library.
#[derive(Clone, Debug)]
pub struct Placement {
    /// The added package whose clone (or whose library's clone) it goes in.
    pub by: PkgId,
    /// `owner/name@version` of the release whose dependency places it.
    pub by_label: String,
    /// The library whose clone it goes inside; `None`: `by`'s own.
    pub inside: Option<PkgId>,
    pub into: String,
    /// What the dependency asks; `None` for a placement already installed.
    pub req: Option<Req>,
    /// Placed by this plan, rather than already installed.
    pub new: bool,
}

/// A library the plan places, at the one release every placement accepts (an installed one stays
/// at its commit).
#[derive(Clone, Debug)]
pub struct Library {
    pub release: Release,
    pub placements: Vec<Placement>,
}

/// A snapshot nobody vouches for, chosen or placed by the plan, which the caller confirms before
/// anything is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Untrusted {
    pub id: PkgId,
    pub commit: String,
    /// The branch tip it was read at (`head`, `master`); `None` for a commit.
    pub tip: Option<String>,
}

/// What a plan says about a package's `added-for` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: PkgId,
    /// The commit the package ends up at: the entry's `for-commit`.
    pub commit: String,
    /// `Some(true)`: named on this command line. `Some(false)`: brought in by this plan.
    /// `None`: present and not named, so whatever the existing entry says (see [`Record::merge`]).
    pub requested: Option<bool>,
    /// The dependents whose dependencies brought it in, `owner/name@version`.
    pub by: Vec<String>,
    /// The any-of dependencies that chose it.
    pub choices: Vec<ChoiceKey>,
}

impl Record {
    /// The entry to write, given the existing one and the `[[added]]` commit it stood beside. A
    /// caller passes `old = None` for a package with no `[[added]]` entry (an entry an older
    /// binary's `remove` left behind is no entry). `requested` stays true once true, and a
    /// missing or stale entry counts as requested, so no orphan pass ever removes a package the
    /// user may have named. A package this plan brings in (`Some(false)`) starts afresh.
    pub fn merge(&self, old: Option<&AddedFor>, old_commit: Option<&str>) -> AddedFor {
        let old = old.filter(|_| self.requested != Some(false));
        let requested = match self.requested {
            Some(true) => true,
            Some(false) => false,
            None => is_requested(old, old_commit.unwrap_or_default()),
        };
        let mut by: Vec<String> = old.map(|o| o.by.clone()).unwrap_or_default();
        for b in &self.by {
            if !by.contains(b) {
                by.push(b.clone());
            }
        }
        let mut choices: Vec<ChoiceKey> = old.map(|o| o.choices.clone()).unwrap_or_default();
        for c in &self.choices {
            if !choices.contains(c) {
                choices.push(c.clone());
            }
        }
        AddedFor { requested, by, choices, for_commit: Some(self.commit.clone()) }
    }
}

/// What a rendered line is, for the caller to style (a release awaiting review in red).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Header,
    Step,
    Pending,
    Rebuild,
    Note,
}

/// A resolution's result.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub choices: Vec<Choice>,
    pub notes: Vec<String>,
    /// Releases the search could not consider because they need what Cactarium did not
    /// read, each with why: such a plan is always shown and confirmed.
    pub skipped: Vec<String>,
    /// Releases awaiting review that the plan chose, `owner/name@version`.
    pub pending: Vec<String>,
    pub untrusted: Vec<Untrusted>,
    pub libraries: Vec<Library>,
    pub records: Vec<Record>,
    /// The flesh is replaced: every config of the installation needs a rebuild.
    pub rebuild: bool,
}

impl Plan {
    /// Nothing to do but say something.
    pub fn is_empty(&self) -> bool {
        self.steps.iter().all(|s| matches!(s, Step::Keep { .. }))
    }

    /// Whether the plan goes beyond what was named (or passed over releases it could not
    /// check), so it is shown and confirmed.
    pub fn beyond_named(&self) -> bool {
        !self.skipped.is_empty()
            || self.steps.iter().any(|s| match s {
                Step::Add { why, .. } | Step::AddThorns { why, .. } | Step::Upgrade { why, .. } => {
                    why != &Why::Named
                }
                Step::Replace { explicit, .. } => !explicit,
                Step::Readd { .. } | Step::Keep { .. } => false,
            })
    }

    /// The plan as people read it, each line with its kind: the header, one line per step, then
    /// the releases awaiting review, the rebuild notice and the notes.
    /// ```text
    /// Plan for cactup add max/z4c:
    ///   add      max/z4c@1.2.0       Z4c, Z4cTest   named
    /// ```
    pub fn lines(&self, command: &str) -> Vec<(LineKind, String)> {
        let rows: Vec<[String; 4]> = self.steps.iter().map(row).collect();
        let mut out = vec![(LineKind::Header, format!("Plan for {command}:"))];
        out.extend(table(&rows).into_iter().map(|l| (LineKind::Step, l)));
        for p in &self.pending {
            out.push((LineKind::Pending, format!("  {p} awaits review")));
        }
        if self.rebuild {
            out.push((LineKind::Rebuild, FLESH_REBUILD.to_owned()));
        }
        for n in &self.notes {
            out.push((LineKind::Note, format!("  note: {n}")));
        }
        for n in &self.skipped {
            out.push((LineKind::Note, format!("  note: {n}")));
        }
        out
    }

    /// [`Plan::lines`] as one text, unstyled.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn render(&self, command: &str) -> String {
        self.lines(command).into_iter().map(|(_, l)| l + "\n").collect()
    }
}

/// Rows in aligned columns; a row with only two cells does not widen the others.
pub(super) fn table(rows: &[[String; 4]]) -> Vec<String> {
    let four = |r: &&[String; 4]| !(r[2].is_empty() && r[3].is_empty());
    let w0 = rows.iter().map(|r| r[0].len()).max().unwrap_or(0);
    let w1 = rows.iter().filter(four).map(|r| r[1].len()).max().unwrap_or(0);
    let w2 = rows.iter().filter(four).map(|r| r[2].len()).max().unwrap_or(0);
    rows.iter()
        .map(|r| {
            let line = if r[2].is_empty() && r[3].is_empty() {
                format!("  {:w0$}  {}", r[0], r[1])
            } else {
                format!("  {:w0$}  {:w1$}  {:w2$}  {}", r[0], r[1], r[2], r[3])
            };
            line.trim_end().to_owned()
        })
        .collect()
}

fn row(s: &Step) -> [String; 4] {
    match s {
        Step::Add { release, thorns, why } => {
            ["add".into(), release.label(), thorns.join(", "), why.to_string()]
        }
        Step::Readd { release, thorns } => {
            ["re-add".into(), release.label(), thorns.join(", "), Why::Named.to_string()]
        }
        Step::AddThorns { label, thorns, why, .. } => {
            ["add".into(), label.clone(), thorns.join(", "), why.to_string()]
        }
        Step::Upgrade { from, to, thorns, why, flag } => {
            // A tip shows the commit it was read at: `head (0123456789ab)`.
            let at = |r: &Release| match &r.at {
                super::At::Tip { name, commit } => format!("{name} ({})", super::short(commit)),
                _ => r.label().trim_start_matches(r.id.as_str()).trim_start_matches('@').to_owned(),
            };
            let mut why = why.to_string();
            if let Some(flag) = flag {
                why.push_str(&format!(" ({flag})"));
            }
            ["upgrade".into(), format!("{} {} -> {}", to.id, at(from), at(to)), thorns.join(", "), why]
        }
        Step::Replace { replaced, by, thorns, .. } => {
            let what = match (replaced, thorns.is_empty()) {
                (Replaced::Base { repo }, true) => format!("base repo {repo}"),
                (Replaced::Base { repo }, false) => format!("{} of base repo {repo}", thorns.join(", ")),
                (Replaced::Added { id }, _) => id.to_string(),
            };
            ["replace".into(), format!("{what} with {by}"), String::new(), String::new()]
        }
        // The note names the package that stays: not said twice.
        Step::Keep { id, note } => {
            let note = note.strip_prefix(&format!("{id} ")).unwrap_or(note);
            ["keep".into(), id.to_string(), String::new(), note.to_owned()]
        }
    }
}
