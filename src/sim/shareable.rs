//! What may go into a `.sim` that leaves this machine (spec §8.9): every
//! repo URL one anybody can fetch, and no credentials anywhere in it.
//! `sim export` and `sim pack` check the thornlist and every input URL
//! before they write one, unless `--local` says the file is for use here
//! only. The check comes in two halves: [`classify`], offline, and
//! [`settle`], which probes every git remote anonymously and rewrites the
//! ssh ones that answer as https. Both run before anything else asks the
//! network (a pack's remote tips, which are then asked of the rewritten
//! URLs); a URL refused offline still waits for the probes, so one error
//! names every URL refused.
//!
//! The rules are the shared crate's (`cactup_ecosystem_common::url` and
//! `::probe`), so the Cactarium refuses exactly what export does. Every
//! component's URL is judged, not one per repo name. A git remote goes
//! through `probe::start`: refused by its form (credentials or a query, a
//! local path, git://, an ssh URL with no https equivalent), or probed
//! ([`crate::fetch::anonymous`]), an https or http remote itself and an ssh
//! remote as its https equivalent, which is written in its place, in the
//! thornlist (an import fetches what the thornlist names) and so in the
//! pins built from it. Every remote costs one request (two at a redirect to
//! the same repo). A download's URL, or another kind of repo's, is not
//! probed (it is not git): `url::download_shareability` judges it, so a
//! query is fine (`?download=1` is how download links work) unless a key
//! looks like a credential, and ssh, local paths and other schemes are
//! refused. A cvs root is no URL, and is refused only for a password (a
//! user name alone, `:pserver:anonymous@…`, is anonymous access). A
//! `!AUTH_URL` is held to its type's rule where it is fetched (svn, cvs),
//! and for a git repo, which never fetches it, to carrying no credentials.
//! A quoted URL is refused: the parser keeps the quotes, and gix will not
//! fetch such a URL. Then the whole text that goes into the `.sim`,
//! comments, unused `!DEFINE`s and overridden lines included, is searched
//! for any `scheme://` URL that carries credentials as a download's would.
//! `!ANON_USER`/`!ANON_PASS` hold the public logins of anonymous svn and
//! cvs access, never a URL, and are never refused. An input's URL is judged
//! as a download's.

use crate::fetch::anonymous::{self, Verdict};
use crate::fetch::git::{AskWindow, REMOTE_ASK_TIMEOUT};
use crate::thornlist::{self, ComponentType};
use crate::Res;
use anyhow::{bail, Context};
use cactup_ecosystem_common::probe::{self, Start};
use cactup_ecosystem_common::url::{download_shareability, shareability, unquote, Redacted, Shareability};
use std::collections::{BTreeMap, BTreeSet};

/// What every refusal ends with.
const LOCAL_HINT: &str = "or pass --local to export for use here only";

/// The offline half of the check: what [`classify`] found.
#[derive(Debug)]
pub struct Checked {
    /// A line per URL refused: "<repo>: <why>", "input <knob>: <why>",
    /// "thornlist line <n>: <why>".
    refused: Vec<String>,
    /// The git remotes to probe.
    asks: Vec<Ask>,
}

/// One git remote of the thornlist, however many components name it.
#[derive(Debug)]
struct Ask {
    /// The repos whose components name it, in order.
    repos: Vec<String>,
    /// The remote as written.
    url: String,
    /// What `probe::start` named to probe and publish.
    candidate: String,
}

/// The thornlist `text` (parsed as `list`) and each input's URL in
/// `input_urls` (KNOB, URL) judged without the network: the refusals, and
/// the git remotes [`settle`] must probe. Every
/// component's URL and `!AUTH_URL` is judged once per URL string; then the
/// whole text is searched for credentials (see the module docs).
pub fn classify(list: &thornlist::Thornlist, text: &str, input_urls: &[(String, String)]) -> Checked {
    let mut refused = Vec::new();
    let mut asks: Vec<Ask> = Vec::new();
    // The URLs judged, and of those the ones refused (the text search
    // leaves those to the line already said).
    let mut judged = BTreeSet::new();
    let mut refused_urls = BTreeSet::new();
    for c in list.components() {
        if c.ty == ComponentType::Ignore {
            continue;
        }
        let fields = [(c.url.as_deref(), c.repo.clone()), (c.auth_url.as_deref(), format!("{} (its !AUTH_URL)", c.repo))];
        for (i, (url, label)) in fields.into_iter().enumerate() {
            let Some(url) = url.filter(|u| !u.trim().is_empty()) else { continue };
            let auth = i == 1;
            if !auth && let Some(ask) = asks.iter_mut().find(|a| a.url == url) {
                if !ask.repos.contains(&c.repo) {
                    ask.repos.push(c.repo.clone());
                }
                continue;
            }
            if !judged.insert((auth, url.to_owned())) {
                continue;
            }
            let why = if unquote(url) != url.trim() {
                let inner = unquote(url);
                refused_urls.insert(inner.to_owned());
                Some(format!(
                    "{} is written in quotes, which the thornlist parser keeps as part of the URL, so it could not \
                     be fetched; remove the quotes",
                    Redacted(inner)
                ))
            } else if c.ty == ComponentType::Cvs {
                cvs_refusal(url)
            } else if auth && c.ty == ComponentType::Git {
                // Never fetched: only what it carries counts.
                match download_shareability(url) {
                    s @ Shareability::Credentials => s.download_refusal(url),
                    _ => None,
                }
            } else if auth || c.ty != ComponentType::Git {
                download_refusal(url)
            } else {
                match probe::start(url) {
                    Start::Probe { candidate } => {
                        asks.push(Ask { repos: vec![c.repo.clone()], url: url.to_owned(), candidate });
                        None
                    }
                    Start::Refuse(why) => Some(why),
                }
            };
            if let Some(why) = why {
                refused_urls.insert(url.to_owned());
                refused.push(format!("{label}: {why}"));
            }
        }
    }
    refused.extend(search_text(text, &refused_urls));
    for (knob, url) in input_urls {
        if let Some(why) = download_refusal(url) {
            refused.push(format!("input {knob}: {why}"));
        }
    }
    Checked { refused, asks }
}

/// Every `scheme://` URL in `text` that carries credentials as a download's
/// would (a user name but an ssh one, a password, a credential-like query
/// key: `url::download_shareability`), as "thornlist line <n>: <why>", but
/// for those in `already` (refused by name). `!ANON_USER`/`!ANON_PASS` hold
/// a bare login, no URL, so nothing there is refused.
fn search_text(text: &str, already: &BTreeSet<String>) -> Vec<String> {
    static URL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"[A-Za-z][A-Za-z0-9+.\-]*://[^\s"'<>`]+"#).expect("valid regex"));
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        for m in URL.find_iter(line) {
            let url = m.as_str().trim_end_matches([')', ']', '}', ',', '.', ';', ':']);
            if already.contains(url) {
                continue;
            }
            if let s @ Shareability::Credentials = download_shareability(url)
                && let Some(why) = s.download_refusal(url)
            {
                out.push(format!("thornlist line {}: {why}", n + 1));
            }
        }
    }
    out
}

/// Probe every git remote of `checked`, and fail with every refusal
/// (offline, or by its probe) in one error; else `text` (what `checked` was
/// classified from) with every remote published as another URL (an ssh
/// remote as its https equivalent) rewritten to it, one line said for
/// each, or `None` when there were none. The rewritten text is parsed back
/// and must name exactly the URLs expected. The probes run on the network
/// fan-out within one [`REMOTE_ASK_TIMEOUT`], a line each; Ctrl-C fails the
/// call ("Interrupted").
pub fn settle(checked: Checked, text: &str) -> Res<Option<String>> {
    let Checked { mut refused, asks } = checked;
    let verdicts = probe_all(&asks)?;
    let mut rewrites = BTreeMap::new();
    for (ask, verdict) in asks.iter().zip(verdicts) {
        match verdict {
            Verdict::Publish(published) if published != ask.url => {
                rewrites.insert(ask.url.clone(), published);
            }
            Verdict::Publish(_) => {}
            Verdict::Refuse(why) => refused.push(format!("{}: {why}", ask.repos.join(", "))),
        }
    }
    if !refused.is_empty() {
        bail!(
            "These URLs cannot go into a .sim for others: nobody else could fetch them, or they would publish \
             credentials:\n  {}\nFix them, {LOCAL_HINT}",
            refused.join("\n  ")
        );
    }
    if rewrites.is_empty() {
        return Ok(None);
    }
    let rewritten = rewrite(text, &rewrites)?;
    for ask in &asks {
        if let Some(published) = rewrites.get(&ask.url) {
            println!(
                "{}: written as {published}, which anyone can fetch, in place of the ssh URL {}",
                ask.repos.join(", "),
                Redacted(&ask.url)
            );
        }
    }
    Ok(Some(rewritten))
}

/// `text` with each `!URL` of `map` (URL as written → URL to publish)
/// rewritten, checked by parsing the result back: every component must
/// name exactly the URL it did, or the one it is published as.
fn rewrite(text: &str, map: &BTreeMap<String, String>) -> Res<String> {
    let list = thornlist::parse(text).context("The thornlist does not parse")?;
    let rewritten = rewrite_urls(text, map);
    let check = thornlist::parse(&rewritten).context("The thornlist with its ssh URLs rewritten does not parse")?;
    let mut stuck = Vec::new();
    for (i, old) in list.components().iter().enumerate() {
        let want = old.url.as_ref().map(|u| map.get(u).unwrap_or(u));
        let got = check.components().get(i).filter(|c| c.checkout == old.checkout && c.repo == old.repo);
        if got.map(|c| c.url.as_ref()) != Some(want) {
            let url = old.url.as_deref().unwrap_or("");
            let https = map.get(url).map_or(String::new(), |h| format!(" (to {h})"));
            stuck.push(format!("{}: {}{https}", old.checkout, Redacted(url)));
        }
    }
    if !stuck.is_empty() || check.components().len() != list.components().len() {
        bail!(
            "The thornlist's ssh URLs answer as https, but rewriting its !URL lines did not rewrite these, as no \
             !URL line spells them out:\n  {}\nWrite the https URLs in the thornlist, {LOCAL_HINT}",
            if stuck.is_empty() { "(the list changed shape)".to_owned() } else { stuck.join("\n  ") }
        );
    }
    Ok(rewritten)
}

/// Why `url`, a download's, an input's or another kind of repo's than git,
/// cannot be published, if it cannot: the shared crate's rule for
/// downloads (`url::download_shareability`).
fn download_refusal(url: &str) -> Option<String> {
    download_shareability(url).download_refusal(url)
}

/// Why the cvs root `root` (`:method:[user[:password]@]host:path`, no URL)
/// cannot be published, if it cannot: a password in it. A user name alone
/// is how anonymous access is written (`:pserver:anonymous@…`).
fn cvs_refusal(root: &str) -> Option<String> {
    let rest = root.trim().strip_prefix(':').and_then(|r| r.split_once(':')).map_or(root.trim(), |(_, r)| r);
    let (userinfo, host) = rest.rsplit_once('@')?;
    let (user, password) = userinfo.split_once(':')?;
    let method = &root.trim()[..root.trim().len() - rest.len()];
    (!password.is_empty()).then(|| {
        format!("{method}{user}:***@{host} carries a password, which must never be published; remove it")
    })
}

/// [`download_refusal`] for an input's URL given at a prompt: the error,
/// or nothing.
pub fn check_input_url(knob: &str, url: &str) -> Res<()> {
    match download_refusal(url) {
        Some(why) => bail!("Input {knob}: {why}, {LOCAL_HINT}"),
        None => Ok(()),
    }
}

/// Probe each of `asks` anonymously ([`anonymous::check`]), on the network
/// fan-out within one window, with a progress line for each.
fn probe_all(asks: &[Ask]) -> Res<Vec<Verdict>> {
    if asks.is_empty() {
        return Ok(Vec::new());
    }
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child("ask git remotes without credentials");
    item.init(Some(asks.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let window = AskWindow::from(std::time::Instant::now(), REMOTE_ASK_TIMEOUT);
    let verdicts = crate::fetch::git::ask_all(asks, |ask| {
        let current = item.lock().expect("probe progress poisoned").add_child(ask.repos.join(", "));
        let verdict = anonymous::check(&ask.url, &ask.candidate, window);
        drop(current);
        item.lock().expect("probe progress poisoned").inc();
        verdict
    });
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    verdicts?.into_iter().collect()
}

/// `text` with each `!URL` that is one of `rewrites`' URLs (as written) set
/// to the URL it is published as; and with an ssh `!URL` built from
/// `$1`/`$2`, which several repos share, set to its own https equivalent
/// (the caller made sure every repo it names answered, and checks the
/// result). Every other
/// byte stays as it was.
fn rewrite_urls(text: &str, rewrites: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let new = body
            .trim_start()
            .strip_prefix("!URL")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(str::trim)
            .and_then(|url| {
                rewrites.get(url).cloned().or_else(|| match shareability(url) {
                    Shareability::Ssh { https } if url.contains('$') => https,
                    _ => None,
                })
            });
        match new {
            Some(https) => {
                let at = body.find('=').expect("a !URL line has its =") + 1;
                let spaces = body[at..].len() - body[at..].trim_start().len();
                out.push_str(&body[..at + spaces]);
                out.push_str(&https);
                out.push_str(&line[body.len()..]);
            }
            None => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A download's URL is judged by the crate's download rule: a query is
    /// fine unless a key looks like a credential, and a user name or
    /// password, ssh and a local path are refused; a cvs root only for a
    /// password.
    #[test]
    fn a_download_is_refused_for_credentials_not_for_a_query() {
        assert!(download_refusal("https://zenodo.org/records/1/files/id.tar.gz?download=1").is_none());
        assert!(download_refusal("https://example.org/f.h5").is_none());
        for url in [
            "https://max:s3cret@example.org/f.h5?download=1",
            "https://TOKEN@example.org/f.h5",
            "https://example.org/f.h5?private_token=TOKEN",
        ] {
            let why = download_refusal(url).unwrap();
            assert!(why.contains("carries credentials") && !why.contains("s3cret") && !why.contains("TOKEN"), "{why}");
        }
        assert!(download_refusal("svn+ssh://svn.example.org/r").unwrap().contains("ssh"));
        assert!(download_refusal("file:///srv/svn/r").unwrap().contains("local path"));
        assert!(cvs_refusal(":pserver:anonymous@cvs.example.org:/cvsroot").is_none());
        let why = cvs_refusal(":pserver:me:s3cret@cvs.example.org:/cvsroot").unwrap();
        assert!(why.starts_with(":pserver:me:***@cvs.example.org:/cvsroot carries a password"), "{why}");
    }

    /// The whole text is searched, comments and unused lines included, for
    /// any URL that carries credentials, each named by its line; ssh user
    /// names, anonymous logins, download queries and URLs already refused
    /// by name are not.
    #[test]
    fn credentials_anywhere_in_the_text_are_found() {
        let text = "!CRL_VERSION = 1.0\n# old: !URL = https://TOKEN1@github.com/o/a.git\n\
                    !DEFINE UNUSED = https://me:TOKEN2@example.org/x\n!URL = https://gitlab.com/o/b.git?private_token=TOKEN3\n\
                    !URL = ssh://git@github.com/o/c.git\n!ANON_USER = anonymous\n!ANON_PASS = anon\n\
                    !URL = https://zenodo.org/f.tar.gz?download=1\n!URL = https://TOKEN4@github.com/o/d.git\n";
        let found = search_text(text, &BTreeSet::from(["https://TOKEN4@github.com/o/d.git".to_owned()]));
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(found[0].starts_with("thornlist line 2: https://***@github.com/o/a.git carries credentials"), "{found:?}");
        assert!(found[1].starts_with("thornlist line 3: https://***@example.org/x"), "{found:?}");
        assert!(found[2].starts_with("thornlist line 4: https://gitlab.com/o/b.git?*** carries"), "{found:?}");
        assert!(!found.concat().contains("TOKEN"), "{found:?}");
    }

    /// Offline, a git:// remote is refused by its form (nothing to probe),
    /// every https, http and ssh remote is to be probed, and an input URL
    /// with a credential-like query key is refused while `?download=1`
    /// passes.
    #[test]
    fn every_git_remote_is_probed_and_git_protocol_refused() {
        let text = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = git://github.com/o/a.git\n!CHECKOUT = A/One\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://github.com/o/b.git\n!CHECKOUT = B/One\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = http://example.org/c\n!CHECKOUT = C/One\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = git@github.com:o/d.git\n!CHECKOUT = D/One\n";
        let list = thornlist::parse(text).unwrap();
        let inputs = [
            ("ok".to_owned(), "https://zenodo.org/records/1/files/f.h5?download=1".to_owned()),
            ("bad".to_owned(), "https://gitlab.com/api/v4/f.h5?private_token=TOKEN".to_owned()),
        ];
        let checked = classify(&list, text, &inputs);
        assert_eq!(checked.refused.len(), 2, "{:#?}", checked.refused);
        assert!(checked.refused[0].starts_with("a: git://github.com/o/a.git uses git://"), "{:?}", checked.refused);
        assert!(checked.refused[1].starts_with("input bad: https://gitlab.com/api/v4/f.h5?*** carries credentials"), "{:?}", checked.refused);
        let asked: Vec<(&str, &str)> = checked.asks.iter().map(|a| (a.url.as_str(), a.candidate.as_str())).collect();
        assert_eq!(
            asked,
            [
                ("https://github.com/o/b.git", "https://github.com/o/b.git"),
                ("http://example.org/c", "http://example.org/c"),
                ("git@github.com:o/d.git", "https://github.com/o/d.git"),
            ]
        );
    }

    /// An ssh `!URL` is rewritten as written or as a template; nothing else
    /// moves, spacing and line endings included.
    #[test]
    fn urls_are_rewritten_in_place() {
        let text = "!CRL_VERSION = 1.0\n# git@h.test:o/a.git\n!URL     = git@h.test:o/a.git\r\n!URL = git@h.test:o/$1.git\n\
                    !URL = git@other.test:o/b.git\n!URL = https://h.test/o/c\n";
        let rewrites = BTreeMap::from([("git@h.test:o/a.git".to_owned(), "https://h.test/o/a.git".to_owned())]);
        assert_eq!(
            rewrite_urls(text, &rewrites),
            "!CRL_VERSION = 1.0\n# git@h.test:o/a.git\n!URL     = https://h.test/o/a.git\r\n!URL = https://h.test/o/$1.git\n\
             !URL = git@other.test:o/b.git\n!URL = https://h.test/o/c\n"
        );
    }
}
