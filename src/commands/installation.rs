//! `cactup installation` — the installation subsystem (spec §3): list, show,
//! use, and refetch Einstein Toolkit installations. `cactup list`/`show`/`use`
//! at the top level are shorthand for `list`/`show`/`use` here.

use super::Ctx;
use crate::args::InstallationCommand;
use crate::database::{CactusInstallation, UnfetchedRepo, UnfetchedReason};
use crate::installation::Installation;
use crate::Res;
use anyhow::{anyhow, bail};
use colored::Colorize;

/// How loudly a `conformance_lines` line should be reported. A skip is a
/// supported workflow (the user may simply have local work in that repo); a
/// failure means the user asked for a repo and did not get it, which is an
/// error state and reads angrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Alarm,
    Warn,
    Plain,
}

/// A detail string can be an arbitrarily long error message; cap it so one
/// failure can't blow out the block.
const DETAIL_SHOWN: usize = 100;

fn truncate_detail(detail: &str) -> String {
    if detail.chars().count() > DETAIL_SHOWN {
        let head: String = detail.chars().take(DETAIL_SHOWN - 3).collect();
        format!("{head}…")
    } else {
        detail.to_owned()
    }
}

/// Append one group's repo lines (capped at `repos_shown`) and their thorn
/// lines (capped at `thorns_shown`) to `lines`, all at `tone`.
fn push_group(
    lines: &mut Vec<(Tone, String)>,
    tone: Tone,
    repos: &[(&String, &UnfetchedRepo)],
    repos_shown: usize,
    thorns_shown: usize,
) {
    for (repo, info) in repos.iter().take(repos_shown) {
        let repo_line = match &info.detail {
            Some(detail) => format!("                  {repo} — {}", truncate_detail(detail)),
            None => format!("                  {repo}"),
        };
        lines.push((tone, repo_line));
        if !info.thorns.is_empty() {
            let head = info.thorns.iter().take(thorns_shown).cloned().collect::<Vec<_>>().join(", ");
            let named = match info.thorns.len().checked_sub(thorns_shown) {
                Some(rest) if rest > 0 => format!("{head}, +{rest} more"),
                _ => head,
            };
            lines.push((tone, format!("                    {named}")));
        }
    }
    if let Some(rest) = repos.len().checked_sub(repos_shown)
        && rest > 0
    {
        lines.push((tone, format!("                  +{rest} more repo(s)")));
    }
}

/// The `show` conformance block: the lines describing thorns whose on-disk
/// contents do not match the installation's recorded thornlist. Empty when
/// the tree fully conforms. Colourless strings on purpose (so they stay
/// testable) paired with a per-line [`Tone`]; `show_installation` applies the
/// colour.
///
/// A failed repo is an error the user did not ask for and did not get — it is
/// reported first, in [`Tone::Alarm`], ahead of a merely-skipped one (a
/// supported workflow, in [`Tone::Warn`]) — so the two are never visually
/// conflated.
///
/// Caps match `refetch`'s "partial adoption" report (§3.2): 6 thorns named
/// per repo, 8 repo lines, so one huge thornlist can't flood the terminal.
pub(crate) fn conformance_lines(entry: &CactusInstallation) -> Vec<(Tone, String)> {
    const REPOS_SHOWN: usize = 8;
    const THORNS_SHOWN: usize = 6;

    if entry.unfetched_repos.is_empty() {
        return Vec::new();
    }

    let failed_count = entry.failed_repo_count();
    let skipped_count = entry.skipped_repo_count();
    let thorn_count = entry.unfetched_thorn_count();

    let (word, tone) =
        if failed_count > 0 { ("INCOMPLETE", Tone::Alarm) } else { ("PARTIAL", Tone::Warn) };
    let header = match (failed_count > 0, skipped_count > 0) {
        (true, true) => format!(
            "  conformance:  {word} — {failed_count} repo(s) FAILED to fetch and \
             {skipped_count} repo(s) were skipped; {thorn_count} thorn(s) on disk do not match \
             the thornlist above."
        ),
        (true, false) => format!(
            "  conformance:  {word} — {failed_count} repo(s) FAILED to fetch; {thorn_count} \
             thorn(s) on disk do not match the thornlist above."
        ),
        (false, true) => format!(
            "  conformance:  {word} — {skipped_count} repo(s) were skipped, so {thorn_count} \
             thorn(s) on disk do not match the thornlist above."
        ),
        (false, false) => unreachable!(
            "a non-empty unfetched_repos always has at least one Failed or Skipped entry"
        ),
    };
    let mut lines = vec![(tone, header)];

    let failed: Vec<(&String, &UnfetchedRepo)> =
        entry.unfetched_repos.iter().filter(|(_, r)| r.reason == UnfetchedReason::Failed).collect();
    let skipped: Vec<(&String, &UnfetchedRepo)> =
        entry.unfetched_repos.iter().filter(|(_, r)| r.reason == UnfetchedReason::Skipped).collect();

    // Failed first: the error, not the choice, is the more important fact.
    if !failed.is_empty() {
        lines.push((
            Tone::Alarm,
            "                Failed:"
                .to_owned(),
        ));
        push_group(&mut lines, Tone::Alarm, &failed, REPOS_SHOWN, THORNS_SHOWN);
    }
    if !skipped.is_empty() {
        lines.push((
            Tone::Warn,
            "                Skipped:"
                .to_owned(),
        ));
        push_group(&mut lines, Tone::Warn, &skipped, REPOS_SHOWN, THORNS_SHOWN);
    }
    if !failed.is_empty() {
        lines.push((
            Tone::Plain,
            "                Retry the failed repo(s) with `cactup inst refetch`.".to_owned(),
        ));
    }
    if !skipped.is_empty() {
        lines.push((
            Tone::Plain,
            "                Fetch over the skipped ones with `cactup inst refetch -f` \
             (modified files are backed up first); `cactup inst delta` shows what differs."
                .to_owned(),
        ));
    }
    lines
}

pub fn dispatch(ctx: &Ctx, cmd: InstallationCommand) -> Res<()> {
    match cmd {
        InstallationCommand::List => super::list::dispatch(ctx),
        InstallationCommand::Show { alias } => show_installation(ctx, alias),
        InstallationCommand::Use { alias } => super::use_cmd::dispatch(ctx, alias),
        InstallationCommand::Refetch(args) => super::refetch::dispatch(ctx, args),
        InstallationCommand::Delta { alias } => super::delta::installation_delta(ctx, alias),
    }
}

/// `cactup installation show [alias]`: the active installation, or a named
/// one, in detail. Listing every installation is `cactup installation list`.
pub(crate) fn show_installation(ctx: &Ctx, alias: Option<String>) -> Res<()> {
    let database = ctx.db.read()?;

    // No alias → the contextually-relevant installation: the active one.
    let alias = match alias {
        Some(alias) => alias,
        None => database.active_installation.clone().ok_or_else(|| {
            anyhow!(
                "no active installation; run `cactup install`, or `cactup use <alias>` \
                 to activate an existing one (see `cactup list`)"
            )
        })?,
    };

    let Some(entry) = database.installations.get(&alias) else {
        bail!("no installation named \"{alias}\" (see `cactup list`)");
    };
    let active = database.active_installation.as_deref() == Some(&alias);

    print!("{}", entry.alias.bold());
    if active {
        print!("{}", " (active)".bold().bright_green());
    }
    println!();
    match (&entry.release, &entry.thornlist) {
        (Some(release), _) => println!("  release:      {release}"),
        // A custom installation is identified by the thornlist it was built
        // from — that is the only thing distinguishing it from any other.
        (None, Some(thornlist)) => {
            println!("  release:      (custom installation from {thornlist})")
        }
        (None, None) => println!("  release:      (custom installation)"),
    }
    // Install-time provenance above; where a refetch has since taken the
    // tree below (§2.1 current-release / current-thornlist). Shown only
    // when it actually differs.
    match (&entry.current_release, &entry.current_thornlist) {
        (Some(current), _) if Some(current) != entry.release.as_ref() => {
            println!("  now on:       {current} (refetched)")
        }
        (None, Some(current)) if Some(current) != entry.thornlist.as_ref() => {
            println!("  now on:       thornlist {current} (refetched)")
        }
        _ => {}
    }
    for (tone, line) in conformance_lines(entry) {
        match tone {
            Tone::Alarm => println!("{}", line.bright_red()),
            Tone::Warn => println!("{}", line.yellow()),
            Tone::Plain => println!("{line}"),
        }
    }
    println!("  path:         {}", entry.path);

    let inst = Installation::new(entry.alias.clone(), &entry.path);
    if let Ok(meta) = inst.meta() {
        match &meta.active_config {
            Some(config) => println!("  active-config: {config}"),
            None => println!("  active-config: {}", "(null-config)".yellow()),
        }
        if let Some(sim_home) = &meta.sim_home {
            println!("  sim-home:     {}", sim_home.display());
        }
        if let Some(test_home) = &meta.test_home {
            println!("  test-home:    {}", test_home.display());
        }
    }
    if let Ok(sims) = inst.simulations() {
        println!("  simulations:  {} (see `cactup sim list`)", sims.simulations.len());
    }
    if let Ok(tests) = inst.tests() {
        println!("  test runs:    {} (see `cactup test list`)", tests.tests.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{CactusInstallation, UnfetchedReason, UnfetchedRepo};
    use indexmap::IndexMap;

    fn entry(unfetched_repos: IndexMap<String, UnfetchedRepo>) -> CactusInstallation {
        CactusInstallation {
            alias: "et".into(),
            release: Some("ET_2026_05".into()),
            path: "/inst".into(),
            thornlist: None,
            current_release: None,
            current_thornlist: None,
            unfetched_repos,
        }
    }

    fn skipped(thorns: Vec<String>, detail: &str) -> UnfetchedRepo {
        UnfetchedRepo { reason: UnfetchedReason::Skipped, thorns, detail: Some(detail.to_owned()) }
    }

    fn failed(thorns: Vec<String>, detail: &str) -> UnfetchedRepo {
        UnfetchedRepo { reason: UnfetchedReason::Failed, thorns, detail: Some(detail.to_owned()) }
    }

    #[test]
    fn empty_map_yields_no_lines() {
        assert!(conformance_lines(&entry(IndexMap::new())).is_empty());
    }

    #[test]
    fn one_repo_names_its_thorns_and_counts_right() {
        let mut unfetched = IndexMap::new();
        unfetched.insert(
            "cactusbase".to_owned(),
            skipped(
                vec!["CactusBase/Boundary".to_owned(), "CactusBase/IOUtil".to_owned()],
                "worktree modified",
            ),
        );
        let lines = conformance_lines(&entry(unfetched));
        assert_eq!(lines[0].0, Tone::Warn);
        assert!(lines[0].1.contains("PARTIAL"), "{}", lines[0].1);
        assert!(lines[0].1.contains("1 repo(s)"), "{}", lines[0].1);
        assert!(lines[0].1.contains("2 thorn(s)"), "{}", lines[0].1);
        // [1] is the "skipped (...)" group header, [2] the repo line, [3] its
        // thorns.
        assert!(lines[2].1.contains("cactusbase"), "{}", lines[2].1);
        assert!(lines[2].1.contains("worktree modified"), "{}", lines[2].1);
        assert!(lines[3].1.contains("CactusBase/Boundary"), "{}", lines[3].1);
        assert!(lines[3].1.contains("CactusBase/IOUtil"), "{}", lines[3].1);
    }

    #[test]
    fn nine_thorns_are_capped_with_a_more_suffix() {
        let thorns: Vec<String> = (0..9).map(|i| format!("Arr/Thorn{i}")).collect();
        let mut unfetched = IndexMap::new();
        unfetched.insert("carpetx".to_owned(), skipped(thorns, "local commits"));
        let lines = conformance_lines(&entry(unfetched));
        assert!(lines[3].1.ends_with("+3 more"), "{}", lines[3].1);
    }

    #[test]
    fn ten_repos_are_capped_at_eight_plus_a_summary_line() {
        let mut unfetched = IndexMap::new();
        for i in 0..10 {
            unfetched.insert(
                format!("repo{i}"),
                UnfetchedRepo { reason: UnfetchedReason::Skipped, thorns: vec![], detail: None },
            );
        }
        let lines = conformance_lines(&entry(unfetched));
        // Header line, the "skipped (...)" group header, 8 repo lines,
        // "+2 more repo(s)", and the trailing remedy line.
        assert_eq!(lines.len(), 1 + 1 + 8 + 1 + 1);
        assert!(lines[10].1.contains("+2 more repo(s)"), "{}", lines[10].1);
    }

    /// A failure is an error state, not a choice, so its header says
    /// `INCOMPLETE` and is reported louder (`Tone::Alarm`) than a mere skip.
    #[test]
    fn failures_only_header_says_incomplete_and_is_alarm_toned() {
        let mut unfetched = IndexMap::new();
        unfetched.insert(
            "openpmd-api".to_owned(),
            failed(vec!["ExternalLibraries/openPMD".to_owned()], "connection reset by peer"),
        );
        let lines = conformance_lines(&entry(unfetched));
        assert_eq!(lines[0].0, Tone::Alarm);
        assert!(lines[0].1.contains("INCOMPLETE"), "{}", lines[0].1);
        assert!(lines[0].1.contains("1 repo(s) FAILED to fetch"), "{}", lines[0].1);
        assert!(!lines[0].1.contains("skipped"), "{}", lines[0].1);
    }

    #[test]
    fn skips_only_header_says_partial_and_is_warn_toned() {
        let mut unfetched = IndexMap::new();
        unfetched
            .insert("cactusbase".to_owned(), skipped(vec!["CactusBase/Boundary".to_owned()], "worktree modified"));
        let lines = conformance_lines(&entry(unfetched));
        assert_eq!(lines[0].0, Tone::Warn);
        assert!(lines[0].1.contains("PARTIAL"), "{}", lines[0].1);
        assert!(!lines[0].1.contains("FAILED"), "{}", lines[0].1);
    }

    /// Mixed entries: the failed group (an error) is reported ahead of the
    /// skipped group (a choice), and both remedy lines survive.
    #[test]
    fn mixed_entry_emits_failed_group_before_skipped_and_both_remedies() {
        let mut unfetched = IndexMap::new();
        unfetched.insert(
            "openpmd-api".to_owned(),
            failed(vec!["ExternalLibraries/openPMD".to_owned()], "connection reset by peer"),
        );
        unfetched.insert(
            "carpetx".to_owned(),
            skipped(
                vec![
                    "CarpetX/Algo".to_owned(),
                    "CarpetX/BoxUtils".to_owned(),
                    "CarpetX/CarpetXRegrid".to_owned(),
                    "CarpetX/Coordinates".to_owned(),
                    "CarpetX/Driver".to_owned(),
                    "CarpetX/ErrorEstimator".to_owned(),
                    "CarpetX/Interpolate".to_owned(),
                    "CarpetX/IO".to_owned(),
                ],
                "local commits",
            ),
        );
        unfetched.insert("cactusbase".to_owned(), skipped(vec!["CactusBase/Boundary".to_owned()], "worktree modified"));
        let lines = conformance_lines(&entry(unfetched));
        assert_eq!(lines[0].0, Tone::Alarm);
        assert!(lines[0].1.contains("INCOMPLETE"), "{}", lines[0].1);
        assert!(lines[0].1.contains("FAILED"), "{}", lines[0].1);
        assert!(lines[0].1.contains("were skipped"), "{}", lines[0].1);

        let failed_pos = lines.iter().position(|(_, l)| l.contains("Failed:")).unwrap();
        let skipped_pos = lines.iter().position(|(_, l)| l.contains("Skipped:")).unwrap();
        assert!(failed_pos < skipped_pos, "failed group must come first");

        assert!(lines
            .iter()
            .any(|(t, l)| *t == Tone::Plain && l.contains("Retry the failed repo(s)")));
        assert!(lines
            .iter()
            .any(|(t, l)| *t == Tone::Plain && l.contains("Fetch over the skipped ones")));
    }

    /// Error text can be arbitrarily long; it must not be allowed to blow out
    /// the block.
    #[test]
    fn long_detail_is_truncated_with_ellipsis() {
        let long = "e".repeat(150);
        let mut unfetched = IndexMap::new();
        unfetched.insert("openpmd-api".to_owned(), failed(vec![], &long));
        let lines = conformance_lines(&entry(unfetched));
        // [0] header, [1] group header, [2] repo line (no thorns => no thorn
        // line).
        let repo_line = &lines[2].1;
        assert!(repo_line.contains('…'), "{repo_line}");
        assert!(!repo_line.contains(&long), "{repo_line}");
        let detail_part = repo_line.split(" — ").nth(1).unwrap();
        // 97 chars of the original detail plus the ellipsis.
        assert_eq!(detail_part.chars().count(), 98, "{detail_part}");
    }
}
