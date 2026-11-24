//! What Cactarium knows of each pin of a `.sim` (spec §8.9, §8.10): one
//! lookup of every git pin and every download before a `.sim` for others is
//! written or published. A git repo must be a registered package's, and a
//! download (a tarball component, an input's file) one Cactarium reviewed:
//! matched by its URL and sha256, and approved. The answer also labels each
//! git pin with its package (`RepoPin.cactarium`): `owner/pkg@<version>`
//! for a commit a version froze, else a bare `owner/pkg`.
// Plan §4.6; §6 S1 (lookup), S2 (pin labels), S9 (reviewed downloads).

use std::collections::BTreeMap;

use cactup_ecosystem_common::api::{DownloadInfo, DownloadQuery, LookedUp, PinInfo, PinQuery};
use cactup_ecosystem_common::sim::{RepoPin, SimFile};
use cactup_ecosystem_common::slug::{is_commit_id, Slug};
use cactup_ecosystem_common::url::Redacted;

use crate::cactarium::Client;
use crate::Res;

/// What one lookup asks: the git pins, each with the repo a line names,
/// and the downloads, each with what a line names (a repo, `input <knob>`).
#[derive(Debug, Default)]
pub struct Query {
    pub pins: Vec<(String, PinQuery)>,
    pub downloads: Vec<(String, DownloadQuery)>,
}

impl Query {
    /// The pins of `repos` worth asking about: each git pin at a full
    /// commit, as `url_of` gives its URL (`None` leaves it out: a URL
    /// refused, which must not leave the machine), and each download pinned
    /// by its sha256 (`url_of` again).
    pub fn of(repos: &[RepoPin], url_of: impl Fn(&RepoPin) -> Option<String>) -> Query {
        let mut query = Query::default();
        for pin in repos {
            let shown = super::shareable::shown_repo(&pin.name).to_owned();
            if let Some(commit) = pin.commit.as_deref().filter(|c| is_commit_id(c)) {
                if let Some(url) = url_of(pin) {
                    query.pins.push((shown, PinQuery { url, commit: commit.to_owned() }));
                }
            } else if let Some(sha256) = &pin.sha256
                && let Some(url) = url_of(pin)
            {
                query.downloads.push((shown, DownloadQuery { url, sha256: sha256.to_ascii_lowercase() }));
            }
        }
        query
    }

    /// The downloads of `sim`'s inputs that carry a URL and a sha256.
    pub fn add_inputs(&mut self, inputs: &[cactup_ecosystem_common::sim::Input]) {
        for input in inputs {
            if let (Some(url), Some(sha256)) = (&input.url, &input.sha256) {
                let query = DownloadQuery { url: url.clone(), sha256: sha256.to_ascii_lowercase() };
                self.downloads.push((format!("input {}", input.knob), query));
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty() && self.downloads.is_empty()
    }

    /// Ask `client`, with `item` counting what is answered.
    pub fn ask(&self, client: &Client, item: prodash::tree::Item) -> Res<LookedUp> {
        item.init(Some(self.pins.len() + self.downloads.len()), Some(prodash::unit::label("pins")));
        let pins: Vec<PinQuery> = self.pins.iter().map(|(_, q)| q.clone()).collect();
        let downloads: Vec<DownloadQuery> = self.downloads.iter().map(|(_, q)| q.clone()).collect();
        client.lookup_all(&pins, &downloads, |n| item.inc_by(n))
    }

    /// One line for each pin Cactarium does not vouch for, ending in the way
    /// past: a repo no registered package has, a download not approved.
    /// Repos `skip` names (shown as a line names them) are left out: a
    /// refusal of their URL by its probe already names them.
    pub fn problems(&self, answer: &LookedUp, skip: impl Fn(&str) -> bool) -> Vec<String> {
        let mut out = Vec::new();
        for ((repo, _), info) in self.pins.iter().zip(&answer.pins) {
            if info.package.is_none() && !skip(repo) {
                out.push(format!(
                    "{repo}: no registered package has this repo; register it with `cactup package init {repo} --publish`"
                ));
            }
        }
        for ((what, query), info) in self.downloads.iter().zip(&answer.downloads) {
            if let Some(line) = download_problem(what, &query.url, info) {
                out.push(line);
            }
        }
        out
    }

    /// One line for each git pin of a registered package at a commit that
    /// Cactarium says the package's repo does not have (`commit-known` is
    /// false; unknown is no refusal).
    pub fn unknown_commits(&self, answer: &LookedUp) -> Vec<String> {
        let pairs = self.pins.iter().zip(&answer.pins);
        pairs
            .filter(|(_, info)| info.package.is_some() && info.commit_known == Some(false))
            .map(|((repo, q), info)| {
                let package = info.package.as_deref().unwrap_or_default();
                let short = &q.commit[..12.min(q.commit.len())];
                format!("{repo} ({short}): commit not in the repo of {package}; push it, or pin a pushed commit")
            })
            .collect()
    }

    /// The label of each git pin of a registered package, by repo as a line
    /// names it.
    pub fn labels(&self, answer: &LookedUp) -> BTreeMap<String, Slug> {
        self.pins.iter().zip(&answer.pins).filter_map(|((repo, _), info)| Some((repo.clone(), label(info)?))).collect()
    }
}

/// The line for a download not approved by Cactarium, if it is not.
fn download_problem(what: &str, url: &str, info: &DownloadInfo) -> Option<String> {
    let url = Redacted(url);
    // A download whose check failed stays pending: the verification says so.
    if info.review.as_deref() != Some("approved") && info.verification.as_deref() == Some("failed") {
        let why = info.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
        return Some(format!(
            "{what}: Cactarium could not verify {url}{why}; resubmit it with `cactup download submit {url}`"
        ));
    }
    match (info.review.as_deref(), info.verification.as_deref()) {
        (Some("approved"), _) => None,
        (None, _) => Some(format!(
            "{what}: download not reviewed by Cactarium; submit it with `cactup download submit {url}`"
        )),
        (Some("pending"), Some("fetching")) => Some(format!("{what}: download being verified by Cactarium")),
        (Some("pending"), Some("ok" | "vouched")) => Some(format!("{what}: download awaiting review by Cactarium")),
        (Some("pending"), _) => Some(format!("{what}: download awaiting verification and review by Cactarium")),
        (Some("rejected"), _) => Some(format!("{what}: download rejected by Cactarium")),
        (Some(other), _) => Some(format!("{what}: download not approved by Cactarium ({other})")),
    }
}

/// Where a submitted download stands, one line each, as `download submit`
/// says it: its review, and how far Cactarium's check of it got. `again`:
/// it was just submitted again, which retries a check that failed.
pub fn download_status(info: &DownloadInfo, again: bool) -> Vec<String> {
    match (info.review.as_deref(), info.verification.as_deref()) {
        (Some("approved"), _) => vec!["Approved by Cactarium.".to_owned()],
        (Some("rejected"), _) => vec!["Rejected by Cactarium.".to_owned()],
        (_, Some("failed")) => {
            let why = info.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
            let next = match again {
                true => "Cactarium is checking it again.",
                false => "Submitting it again retries the check.",
            };
            vec![format!("Cactarium could not verify {}{why}.", Redacted(&info.url)), next.to_owned()]
        }
        (_, Some("fetching")) => vec!["Being verified by Cactarium.".to_owned()],
        (_, Some("ok" | "vouched")) => vec!["Verified by Cactarium; awaiting review by an admin.".to_owned()],
        (Some(other), None) if other != "pending" => vec![format!("Review: {other}.")],
        _ => vec!["Awaiting verification and review by Cactarium.".to_owned()],
    }
}

/// A git pin's label from what the lookup says of it: `owner/pkg@<version>`
/// when a version froze its commit, `owner/pkg` when none did, nothing for
/// no package. Never a `#` stamp: the pin's commit says which commit. A
/// version that is no exact version reads as none.
pub fn label(info: &PinInfo) -> Option<Slug> {
    let package: Slug = info.package.as_deref()?.parse().ok()?;
    if package.at.is_some() || package.sha.is_some() || package.thorn.is_some() {
        return None;
    }
    let versioned = info
        .version
        .as_deref()
        .and_then(|v| format!("{}@{v}", package.entry()).parse::<Slug>().ok())
        .filter(|s| s.exact().is_ok() && s.thorn.is_none() && s.sha.is_none());
    Some(versioned.unwrap_or(package))
}

/// `query` asked of `client` on its own, with a line of progress.
pub fn ask_alone(client: &Client, query: &Query) -> Res<LookedUp> {
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let answer = query.ask(client, progress.add_child("ask Cactarium what each pin is"));
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    answer
}

/// The line for a lookup that failed, among the other refusals; Ctrl-C is
/// "Interrupted" alone.
pub fn lookup_failed(e: &anyhow::Error) -> Res<String> {
    if gix::interrupt::is_triggered() || e.downcast_ref::<crate::cactarium::InFlight>().is_some() {
        anyhow::bail!("Interrupted");
    }
    let said = format!("{e:#}");
    let first = said.lines().next().unwrap_or_default();
    Ok(match first.starts_with("Cannot ask Cactarium") {
        true => first.to_owned(),
        false => format!("Cannot ask Cactarium what each pin is: {first}"),
    })
}

/// How `sim publish` words the fixes of what its file holds.
fn publish_fixes() -> super::export::Fixes {
    super::export::Fixes {
        when: "at export",
        commit: "commit them, rebuild, and export it again",
        unhashed: "download with no hash; pin it with !CHECKSUM_SHA256 and export it again".to_owned(),
        unpinned: "no commit pins it; export it again from a build that pinned it".to_owned(),
        unlisted: None,
        unknown_commits: false,
    }
}

/// `sim publish`'s check (spec §8.10), before anything is uploaded. Offline
/// first, as export checks what it writes: URLs nobody else can fetch or
/// that carry credentials (the thornlist's, each pin's, each input's),
/// `ignore` components, repos and downloads nothing pins, downloads on a
/// private address, sources dirty at export. Then the token, so that a user
/// without one is not kept waiting. Then one lookup, with a line of
/// progress: every git pin a registered package's, every download (pins and
/// inputs) approved by Cactarium. Fails naming each problem, a line each;
/// nothing is sent while anything offline is refused.
pub fn check_publish(client: &Client, file: &std::path::Path, sim: &SimFile) -> Res<()> {
    use cactup_ecosystem_common::probe::{self, Start};
    use cactup_ecosystem_common::url::download_shareability;
    let refuse = |lines: Vec<String>| anyhow::anyhow!("'{}' cannot be published:\n  {}", file.display(), lines.join("\n  "));
    let list = crate::thornlist::parse(&sim.thornlist.text)
        .map_err(|e| anyhow::anyhow!("'{}': its thornlist does not parse: {e:#}", file.display()))?;
    let inputs: Vec<(String, String)> =
        sim.inputs.iter().filter_map(|i| Some((i.knob.clone(), i.url.clone()?))).collect();
    let checked = super::shareable::classify(&list, &sim.thornlist.text, &inputs);
    let mut lines = checked.refused().to_vec();
    let mut refused_pins = std::collections::BTreeSet::new();
    for pin in sim.repos.iter().filter(|p| !checked.refuses(&p.name)) {
        let why = match (&pin.commit, &pin.sha256) {
            (Some(_), _) => match probe::start(&pin.url) {
                Start::Refuse(why) => Some(why),
                Start::Probe { .. } => None,
            },
            (None, Some(_)) => download_shareability(&pin.url).download_refusal(&pin.url),
            (None, None) => None,
        };
        if let Some(why) = why {
            lines.push(format!("{}: {why}", super::shareable::shown_repo(&pin.name)));
            refused_pins.insert(pin.name.clone());
        }
    }
    // One line for a repo with a URL refused: that refusal.
    let refused = |repo: &str| checked.refuses(repo) || refused_pins.contains(repo);
    lines.extend(super::export::content_problems(&list, &sim.repos, &publish_fixes(), &refused).lines);
    lines.extend(super::export::unpinned_git(&list, &sim.repos, &publish_fixes(), &refused));
    for (knob, url) in &inputs {
        if download_shareability(url).download_refusal(url).is_none()
            && let Some(why) = crate::fetch::anonymous::private_download(url)
        {
            lines.push(format!("input {knob}: {why}"));
        }
    }
    for pin in sim.repos.iter().filter(|p| p.dirty) {
        lines.push(format!("{}: uncommitted changes at export; commit them, rebuild, and export it again", pin.name));
    }
    for thorn in &sim.thornlist.untracked {
        lines.push(format!("{thorn}: not backed by any repo; put it in a repo and register that with Cactarium"));
    }
    // Pins the thornlist does not name (a file edited by hand).
    for pin in sim.repos.iter().filter(|p| p.commit.is_none() && p.sha256.is_none()) {
        if !list.components().iter().any(|c| c.repo == pin.name) {
            lines.push(format!("{}: no commit or sha256 pins it", pin.name));
        }
    }
    if !lines.is_empty() {
        return Err(refuse(lines));
    }
    crate::cactarium::token_for(client.base())?;
    let mut query = Query::of(&sim.repos, |pin| Some(pin.url.clone()));
    query.add_inputs(&sim.inputs);
    if query.is_empty() {
        return Ok(());
    }
    let answer = ask_alone(client, &query).map_err(|e| match lookup_failed(&e) {
        Ok(line) => refuse(vec![line]),
        Err(interrupted) => interrupted,
    })?;
    let lines = query.problems(&answer, |_| false);
    if !lines.is_empty() {
        return Err(refuse(lines));
    }
    Ok(())
}

/// A fake Cactarium for the tests of `sim export`, `pack` and `publish`:
/// `/lookup` in the crate's shapes, a publish, and files to download.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};

    /// Every request: its path and body.
    pub(crate) type Log = Arc<Mutex<Vec<(String, String)>>>;

    #[derive(Default)]
    pub(crate) struct Registry {
        /// URL → the package with that repo, and the version that froze
        /// every commit asked (none: no version did).
        pub packages: Vec<(String, String, Option<String>)>,
        /// Every other repo is a package's too: `et/<repo>`, at no version.
        pub all_registered: bool,
        /// URL → the review of that download (any sha256).
        pub downloads: Vec<(String, String)>,
        /// The end of a URL → the state of the server's check of that
        /// download, and why it failed.
        pub checks: Vec<(String, String, Option<String>)>,
        /// URLs submitted while it serves: pending from then on.
        pub fresh: Arc<Mutex<Vec<String>>>,
        /// A lookup answers 500, as a failing server does.
        pub fail_lookup: bool,
        /// Repos (by URL) whose commits the package's repo lacks.
        pub commit_unknown: Vec<String>,
        /// Path → the bytes a GET of it answers.
        pub files: Vec<(String, Vec<u8>)>,
        /// How long a lookup takes to answer.
        pub delay: std::time::Duration,
        /// Downloads submitted before: the end of the URL, review and the
        /// 409's message.
        pub submitted: Vec<(String, String, String)>,
    }

    impl Registry {
        /// Serve it; the base URL and the log.
        pub(crate) fn serve(self) -> (String, Log) {
            let log: Log = Arc::default();
            let seen = log.clone();
            let (base, _) = crate::fetch::download::test_server::serve_with_body(1000, move |path, body| {
                seen.lock().unwrap().push((path.to_owned(), String::from_utf8_lossy(body).into_owned()));
                self.answer(path, body)
            });
            // Its files stand in for downloads on a public host.
            let at = base.trim_start_matches("http://").to_owned();
            crate::fetch::anonymous::PUBLIC_IN_TESTS.lock().unwrap().insert(at);
            (base, log)
        }

        fn answer(&self, path: &str, body: &[u8]) -> (&'static str, String, Vec<u8>) {
            let ok = |body: String| ("200 OK", "api-generation: 1\r\n".to_owned(), body.into_bytes());
            if let Some((_, bytes)) = self.files.iter().find(|(p, _)| p == path) {
                return ("200 OK", String::new(), bytes.clone());
            }
            if path == "/api/v1/lookup" {
                std::thread::sleep(self.delay);
                if self.fail_lookup {
                    let body = br#"{"error":{"code":"internal","message":"database is locked"}}"#.to_vec();
                    return ("500 Internal Server Error", "api-generation: 1\r\n".to_owned(), body);
                }
                let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_slice(body).unwrap();
                let pins: Vec<serde_json::Value> = (asked.pins.iter())
                    .map(|p| {
                        let repo = p.url.rsplit('/').next().unwrap().trim_end_matches(".git").to_ascii_lowercase();
                        let (package, version) = match self.packages.iter().find(|(url, _, _)| *url == p.url) {
                            Some((_, package, version)) => (Some(package.clone()), version.clone()),
                            None if self.all_registered => (Some(format!("et/{repo}")), None),
                            None => (None, None),
                        };
                        let known = self.commit_unknown.contains(&p.url).then_some(false);
                        serde_json::json!({"url": p.url, "commit": p.commit, "package": package, "version": version,
                            "flesh": false, "fork-of": [], "commit-known": known})
                    })
                    .collect();
                let downloads: Vec<serde_json::Value> = (asked.downloads.iter())
                    .map(|d| {
                        let review = (self.downloads.iter().find(|(url, _)| *url == d.url).map(|(_, r)| r.clone()))
                            .or_else(|| {
                                let known = self.submitted.iter().find(|(end, _, _)| d.url.ends_with(end.as_str()));
                                known.map(|(_, r, _)| r.clone())
                            })
                            .or_else(|| self.fresh.lock().unwrap().contains(&d.url).then(|| "pending".to_owned()));
                        let check = self.checks.iter().find(|(end, _, _)| d.url.ends_with(end.as_str()));
                        serde_json::json!({"url": d.url, "sha256": d.sha256, "review": review,
                            "verification": check.map(|c| &c.1), "reason": check.and_then(|c| c.2.as_ref())})
                    })
                    .collect();
                return ok(serde_json::json!({"pins": pins, "downloads": downloads}).to_string());
            }
            if path == "/api/v1/downloads" {
                let asked: cactup_ecosystem_common::api::DownloadSubmission = serde_json::from_slice(body).unwrap();
                return match self.submitted.iter().find(|(end, _, _)| asked.url.ends_with(end.as_str())) {
                    Some((_, review, message)) => {
                        let body = serde_json::json!({"error": {"code": "conflict", "message": message}, "id": 7, "review": review});
                        ("409 Conflict", "api-generation: 1\r\n".to_owned(), body.to_string().into_bytes())
                    }
                    None => {
                        self.fresh.lock().unwrap().push(asked.url);
                        ok(r#"{"id":8,"review":"pending"}"#.to_owned())
                    }
                };
            }
            if path.starts_with("/api/v1/sims/") {
                return ok(r#"{"slug":"max/run@1.0.0","url":"u","visibility":"public","warnings":[]}"#.to_owned());
            }
            ("404 Not Found", "api-generation: 1\r\n".to_owned(), br#"{"error":{"code":"not-found","message":"Not found"}}"#.to_vec())
        }
    }

    /// The paths asked, in order.
    pub(crate) fn paths(log: &Log) -> Vec<String> {
        log.lock().unwrap().iter().map(|(p, _)| p.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(package: Option<&str>, version: Option<&str>) -> PinInfo {
        PinInfo {
            url: "u".into(),
            commit: "a".repeat(40),
            package: package.map(Into::into),
            version: version.map(Into::into),
            flesh: false,
            fork_of: Vec::new(),
            commit_known: None,
            review: None,
        }
    }

    /// `sim publish` asks Cactarium about every git pin, download pin and
    /// input download in one lookup, and refuses (uploading nothing) a repo
    /// no registered package has and a download not approved, a line each;
    /// once Cactarium vouches for all, the upload goes ahead.
    #[test]
    fn publish_refuses_before_uploading() {
        use cactup_ecosystem_common::sim::{Input, Parfile, ParfileKind, SimMeta, ThornlistText};
        use fake::{paths, Registry};
        let tmp = tempfile::tempdir().unwrap();
        let pin = |name: &str, commit: Option<String>, sha256: Option<String>| RepoPin {
            name: name.into(),
            url: format!("https://example.org/o/{name}"),
            git_ref: None,
            commit,
            sha256,
            cactarium: None,
            dirty: false,
        };
        let sim = SimFile {
            format: 1,
            sim: SimMeta {
                name: "run".into(),
                summary: None,
                description: None,
                authors: vec![],
                license: None,
                contact: None,
                cactarium: None,
                root: "Cactus".into(),
            },
            thornlist: ThornlistText { filename: "t.th".into(), text: "!CRL_VERSION = 1.0\n".into(), untracked: vec![] },
            parfile: Parfile { filename: "run.par".into(), kind: ParfileKind::Par, vars: vec![], text: "\n".into() },
            inputs: vec![Input {
                knob: "data".into(),
                description: None,
                url: Some("https://example.org/id.h5".into()),
                sha256: Some("d".repeat(64)),
                unpack: false,
                path: None,
                fixups: vec![],
                repo_file: None,
            }],
            repos: vec![
                pin("a", Some("a".repeat(40)), None),
                pin("c", Some("c".repeat(40)), None),
                pin("z.tgz", None, Some("e".repeat(64))),
            ],
            run: None,
            provenance: None,
        };
        let file = tmp.path().join("run.sim");
        std::fs::write(&file, sim.to_toml().unwrap()).unwrap();
        let publish = |registry: Registry, token: bool| {
            let (base, log) = registry.serve();
            if token {
                crate::cactarium::TEST_TOKENS.lock().unwrap().insert(base.clone(), "tok".into());
            }
            let db = crate::database::Db::in_dir(&tmp.path().join("db"));
            db.update(move |d| {
                d.set_knob(crate::cactarium::URL_KNOB, base);
                Ok(())
            })
            .unwrap();
            let ctx = crate::commands::Ctx {
                globals: crate::args::GlobalOpts {
                    verbose: false,
                    trace: false,
                    manifest_url: String::new(),
                    mdb_path: None,
                    machine: None,
                    installation: None,
                    hostname: None,
                    knob: Vec::new(),
                    risky_allow_untrusted_artifacts: false,
                },
                db,
            };
            (crate::commands::import::publish(&ctx, &file, "max/run", None), paths(&log))
        };
        let registry = Registry {
            packages: vec![("https://example.org/o/a".into(), "et/a".into(), Some("1.0.0".into()))],
            downloads: vec![("https://example.org/o/z.tgz".into(), "pending".into())],
            ..Default::default()
        };
        let (published, asked) = publish(registry, false);
        assert!(format!("{:#}", published.unwrap_err()).starts_with("No token for Cactarium"));
        assert!(asked.is_empty(), "no token, nothing asked: {asked:?}");
        let registry = Registry {
            packages: vec![("https://example.org/o/a".into(), "et/a".into(), Some("1.0.0".into()))],
            downloads: vec![("https://example.org/o/z.tgz".into(), "pending".into())],
            ..Default::default()
        };
        let (published, asked) = publish(registry, true);
        let err = format!("{:#}", published.unwrap_err());
        assert_eq!(
            err,
            format!(
                "'{}' cannot be published:\n  \
                 c: no registered package has this repo; register it with `cactup package init c --publish`\n  \
                 z.tgz: download awaiting verification and review by Cactarium\n  \
                 input data: download not reviewed by Cactarium; submit it with `cactup download submit https://example.org/id.h5`",
                file.display()
            )
        );
        assert_eq!(asked, ["/api/v1/lookup"], "nothing uploaded");

        let registry = Registry {
            all_registered: true,
            downloads: vec![
                ("https://example.org/o/z.tgz".into(), "approved".into()),
                ("https://example.org/id.h5".into(), "approved".into()),
            ],
            ..Default::default()
        };
        let (published, asked) = publish(registry, true);
        published.unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(asked, ["/api/v1/lookup", "/api/v1/sims/max/run"]);

        // What export refuses offline is refused before anything is asked: an
        // ignore component, a URL with a token (in the thornlist and in a
        // pin), an svn repo nothing pins, a git pin with none, a dirty one.
        let text = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = ignore\n!CHECKOUT = Private/Thorn\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://TOKEN1@github.com/o/t.git\n!CHECKOUT = T/One\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = svn\n!URL = https://svn.example.org/s/trunk\n!CHECKOUT = S/One\n";
        let mut bad = sim.clone();
        bad.thornlist.text = text.into();
        bad.inputs.clear();
        bad.repos = vec![
            RepoPin { url: "https://TOKEN1@github.com/o/t.git".into(), ..pin("t", Some("a".repeat(40)), None) },
            RepoPin { url: "https://TOKEN2@github.com/o/u.git".into(), ..pin("u", Some("b".repeat(40)), None) },
            pin("trunk", None, None),
            RepoPin { dirty: true, ..pin("d", Some("d".repeat(40)), None) },
        ];
        let toml = toml::to_string(&bad).unwrap();
        std::fs::write(&file, toml).unwrap();
        let (published, asked) = publish(Registry { all_registered: true, ..Default::default() }, true);
        let err = format!("{:#}", published.unwrap_err());
        for line in [
            "\n  t: https://***@github.com/o/t.git carries a user name or token",
            "\n  u: https://***@github.com/o/u.git carries a user name or token",
            "\n  Private/Thorn: an ignore component",
            "\n  trunk: svn repos cannot be pinned",
            "\n  d: uncommitted changes at export",
        ] {
            assert!(err.contains(line), "{line:?} not in: {err}");
        }
        assert!(!err.contains("TOKEN"), "{err}");
        assert!(asked.is_empty(), "nothing asked, no token sent: {asked:?}");
    }

    /// A failed lookup is one line, said once: the client's own "Cannot ask
    /// Cactarium …" is not wrapped again; Ctrl-C in flight is "Interrupted"
    /// alone, never a line among the refusals.
    #[test]
    fn a_failed_lookup_is_one_line() {
        let e = anyhow::anyhow!("Cannot ask Cactarium about these repos: a pin's url is 3000 bytes long");
        assert_eq!(lookup_failed(&e).unwrap(), "Cannot ask Cactarium about these repos: a pin's url is 3000 bytes long");
        let e = anyhow::anyhow!("Cactarium at http://x did not answer in time.\nTo use another server, set it.");
        assert_eq!(lookup_failed(&e).unwrap(), "Cannot ask Cactarium what each pin is: Cactarium at http://x did not answer in time.");
        let e = anyhow::Error::new(crate::cactarium::InFlight);
        assert_eq!(format!("{:#}", lookup_failed(&e).unwrap_err()), "Interrupted");
    }

    /// A pin is labeled with its version when one froze its commit (a track's
    /// too), else bare; never with a `#` stamp, and nothing without a package.
    #[test]
    fn a_pin_is_labeled_by_its_version_or_bare() {
        let shown = |i: &PinInfo| label(i).map(|s| s.to_string());
        assert_eq!(shown(&info(Some("et/carpetx"), Some("2026.5.1"))).as_deref(), Some("et/carpetx@2026.5.1"));
        assert_eq!(shown(&info(Some("et/carpetx"), Some("mp:1.2.0"))).as_deref(), Some("et/carpetx@mp:1.2.0"));
        assert_eq!(shown(&info(Some("et/carpetx"), None)).as_deref(), Some("et/carpetx"));
        assert_eq!(shown(&info(Some("et/carpetx"), Some("^1.0"))).as_deref(), Some("et/carpetx"), "no range");
        assert_eq!(shown(&info(None, None)), None);
        assert_eq!(shown(&info(Some("not a slug"), None)), None);
    }
}
