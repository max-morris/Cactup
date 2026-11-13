//! What a resolution decided: the steps, the any-of choices, and what to record.
// Plan §2.3 (showing), §2.4 (records). Rendering only builds text; the caller prints it.

use std::fmt;

use super::{PkgId, Release, Req, Why};

/// One thing the plan does.
#[derive(Clone, Debug)]
pub enum Step {
    /// A package the installation does not have.
    Add { release: Release, thorns: Vec<String>, why: Why },
    /// More thorns of a present package, at the release it is at.
    AddThorns { id: PkgId, label: String, thorns: Vec<String>, why: Why },
    /// A present package moved to another release. `thorns`: every thorn enabled after the move
    /// (the present ones plus the new needs).
    Upgrade { from: Release, to: Release, thorns: Vec<String>, why: Why, flag: Option<&'static str> },
    /// A base repo dropped in favor of a package (shown after the step bringing it in). For the
    /// flesh, every config of the installation needs a rebuild.
    Replace { base_repo: String, by: PkgId, flesh: bool },
    /// A present package left as it is, with something worth saying (a fork standing in, a
    /// release at no version taken to meet a range).
    Keep { id: PkgId, note: String },
}

/// How an any-of dependency chose its alternative (the rules, in order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceRule {
    /// An alternative the installation has.
    Present,
    /// One the rest of the plan brings in.
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

/// One any-of decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The dependent, `owner/name@version`.
    pub dependent: String,
    /// `any of et/carpet, et/cactuspugh`.
    pub label: String,
    pub chosen: PkgId,
    pub rule: ChoiceRule,
}

impl fmt::Display for Choice {
    /// As recorded in `added-for.*.choice`: `any of et/carpet, et/cactuspugh: the first listed`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.label, self.rule)
    }
}

/// Where one package places a library.
#[derive(Clone, Debug)]
pub struct Placement {
    /// The package (or library) whose dependency places it, `owner/name@version`.
    pub by: String,
    pub into: String,
    pub req: Req,
}

/// A library the final set builds against, at the one release every placement accepts.
#[derive(Clone, Debug)]
pub struct Library {
    pub release: Release,
    pub placements: Vec<Placement>,
}

/// A snapshot nobody vouches for, which the caller confirms before anything is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Untrusted {
    pub id: PkgId,
    pub commit: String,
}

/// What to keep in `installation.toml`'s `added-for` for a package the plan adds or moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: PkgId,
    /// Named on this command line. A caller merging into an existing entry keeps `true`.
    pub requested: bool,
    /// The dependents whose dependencies brought it in, `owner/name@version`.
    pub by: Vec<String>,
    /// The any-of choice, when one chose it.
    pub choice: Option<String>,
}

/// A resolution's result.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub choices: Vec<Choice>,
    pub notes: Vec<String>,
    /// Releases awaiting review that the plan chose, `owner/name@version`.
    pub pending: Vec<String>,
    pub untrusted: Vec<Untrusted>,
    pub libraries: Vec<Library>,
    pub records: Vec<Record>,
    /// The flesh is replaced: every config of the installation needs a rebuild.
    pub rebuild: bool,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.steps.iter().all(|s| matches!(s, Step::Keep { .. }))
    }

    /// Whether the plan goes beyond what was named, so it is shown and confirmed.
    pub fn beyond_named(&self) -> bool {
        self.steps.iter().any(|s| match s {
            Step::Add { why, .. } | Step::AddThorns { why, .. } | Step::Upgrade { why, .. } => {
                why != &Why::Named
            }
            Step::Replace { .. } => true,
            Step::Keep { .. } => false,
        })
    }

    /// The plan as people read it, one line per step, then the notes:
    /// ```text
    /// Plan for cactup add max/z4c:
    ///   add      max/z4c@1.2.0       Z4c, Z4cTest   named
    /// ```
    pub fn render(&self, command: &str) -> String {
        let rows: Vec<[String; 4]> = self.steps.iter().map(row).collect();
        // A replace line has no thorns or why, and does not widen those columns.
        let four = |r: &&[String; 4]| !(r[2].is_empty() && r[3].is_empty());
        let w0 = rows.iter().map(|r| r[0].len()).max().unwrap_or(0);
        let w1 = rows.iter().filter(four).map(|r| r[1].len()).max().unwrap_or(0);
        let w2 = rows.iter().filter(four).map(|r| r[2].len()).max().unwrap_or(0);
        let mut out = format!("Plan for {command}:\n");
        for r in &rows {
            let line = if r[2].is_empty() && r[3].is_empty() {
                format!("  {:w0$}  {}", r[0], r[1])
            } else {
                format!("  {:w0$}  {:w1$}  {:w2$}  {}", r[0], r[1], r[2], r[3])
            };
            out.push_str(line.trim_end());
            out.push('\n');
        }
        for p in &self.pending {
            out.push_str(&format!("  {p} is awaiting review: nobody has approved it\n"));
        }
        if self.rebuild {
            out.push_str("  The flesh changes: every config of this installation needs a rebuild\n");
        }
        for n in &self.notes {
            out.push_str(&format!("  note: {n}\n"));
        }
        out
    }
}

fn row(s: &Step) -> [String; 4] {
    match s {
        Step::Add { release, thorns, why } => {
            ["add".into(), release.label(), thorns.join(", "), why.to_string()]
        }
        Step::AddThorns { label, thorns, why, .. } => {
            ["add".into(), label.clone(), thorns.join(", "), why.to_string()]
        }
        Step::Upgrade { from, to, thorns, why, flag } => {
            let at = |r: &Release| {
                r.label().trim_start_matches(r.id.as_str()).trim_start_matches('@').to_owned()
            };
            let mut why = why.to_string();
            if let Some(flag) = flag {
                why.push_str(&format!(" ({flag})"));
            }
            ["upgrade".into(), format!("{} {} -> {}", to.id, at(from), at(to)), thorns.join(", "), why]
        }
        Step::Replace { base_repo, by, .. } => {
            ["replace".into(), format!("base repo {base_repo} with {by}"), String::new(), String::new()]
        }
        Step::Keep { id, note } => ["keep".into(), id.to_string(), String::new(), note.clone()],
    }
}
