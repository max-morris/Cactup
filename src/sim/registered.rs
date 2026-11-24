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
                    "{repo}: no registered package has this repo; push it and register it with \
                     `cactup package init {repo} --publish`, or fork a registered package"
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

    /// The label of each git pin of a registered package, by repo as a line
    /// names it.
    pub fn labels(&self, answer: &LookedUp) -> BTreeMap<String, Slug> {
        self.pins.iter().zip(&answer.pins).filter_map(|((repo, _), info)| Some((repo.clone(), label(info)?))).collect()
    }
}

/// The line for a download not approved by Cactarium, if it is not.
fn download_problem(what: &str, url: &str, info: &DownloadInfo) -> Option<String> {
    let url = Redacted(url);
    match info.review.as_deref() {
        Some("approved") => None,
        None => Some(format!(
            "{what}: download not reviewed by Cactarium; submit it with `cactup download submit {url}`"
        )),
        Some("pending") => Some(format!("{what}: download awaiting verification and review by Cactarium")),
        Some("rejected") => Some(format!("{what}: download rejected by Cactarium")),
        Some(other) => Some(format!("{what}: download not approved by Cactarium ({other})")),
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

/// `sim publish`'s check (spec §8.10), before anything is uploaded: every
/// git pin of `sim` a registered package's, every download (pins and
/// inputs) approved by Cactarium, asked in one lookup with a line of
/// progress. Fails naming each that is not.
pub fn check_publish(client: &Client, file: &std::path::Path, sim: &SimFile) -> Res<()> {
    let mut query = Query::of(&sim.repos, |pin| Some(pin.url.clone()));
    query.add_inputs(&sim.inputs);
    if query.is_empty() {
        return Ok(());
    }
    let lines = query.problems(&ask_alone(client, &query)?, |_| false);
    if !lines.is_empty() {
        anyhow::bail!("'{}' cannot be published:\n  {}", file.display(), lines.join("\n  "));
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
        /// Path → the bytes a GET of it answers.
        pub files: Vec<(String, Vec<u8>)>,
        /// How long a lookup takes to answer.
        pub delay: std::time::Duration,
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
            (base, log)
        }

        fn answer(&self, path: &str, body: &[u8]) -> (&'static str, String, Vec<u8>) {
            let ok = |body: String| ("200 OK", "api-generation: 1\r\n".to_owned(), body.into_bytes());
            if let Some((_, bytes)) = self.files.iter().find(|(p, _)| p == path) {
                return ("200 OK", String::new(), bytes.clone());
            }
            if path == "/api/v1/lookup" {
                std::thread::sleep(self.delay);
                let asked: cactup_ecosystem_common::api::LookupRequest = serde_json::from_slice(body).unwrap();
                let pins: Vec<serde_json::Value> = (asked.pins.iter())
                    .map(|p| {
                        let repo = p.url.rsplit('/').next().unwrap().trim_end_matches(".git").to_ascii_lowercase();
                        let (package, version) = match self.packages.iter().find(|(url, _, _)| *url == p.url) {
                            Some((_, package, version)) => (Some(package.clone()), version.clone()),
                            None if self.all_registered => (Some(format!("et/{repo}")), None),
                            None => (None, None),
                        };
                        serde_json::json!({"url": p.url, "commit": p.commit, "package": package, "version": version,
                            "flesh": false, "fork-of": [], "commit-known": null})
                    })
                    .collect();
                let downloads: Vec<serde_json::Value> = (asked.downloads.iter())
                    .map(|d| {
                        let review = self.downloads.iter().find(|(url, _)| *url == d.url).map(|(_, r)| r.clone());
                        serde_json::json!({"url": d.url, "sha256": d.sha256, "review": review})
                    })
                    .collect();
                return ok(serde_json::json!({"pins": pins, "downloads": downloads}).to_string());
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
        let publish = |registry: Registry| {
            let (base, log) = registry.serve();
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
        let (published, asked) = publish(registry);
        let err = format!("{:#}", published.unwrap_err());
        assert_eq!(
            err,
            format!(
                "'{}' cannot be published:\n  \
                 c: no registered package has this repo; push it and register it with `cactup package init c --publish`, or fork a registered package\n  \
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
        let (published, asked) = publish(registry);
        // Uploaded, or stopped for want of a token: either way past the check.
        match published {
            Ok(()) => assert_eq!(asked, ["/api/v1/lookup", "/api/v1/sims/max/run"]),
            Err(e) => assert!(format!("{e:#}").starts_with("No token for Cactarium"), "{e:#}"),
        }
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
