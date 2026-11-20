//! What may go into a `.sim` that leaves this machine (spec §8.9): every
//! repo URL one anybody can fetch, and no credentials anywhere in it.
//! `sim export` and `sim pack` check the thornlist and every input URL
//! before they write one, unless `--local` says the file is for use here
//! only. The check comes in two halves: [`classify`], offline, and
//! [`settle`], which asks the ssh remotes' https URLs and rewrites those
//! that answer. Both run before anything else asks the network (a pack's
//! remote tips, which are then asked of the rewritten URLs); a URL refused
//! offline still waits for the https asks, so one error names every URL
//! refused.
//!
//! The rules are the shared crate's (`cactup_ecosystem_common::url`), so the
//! Cactarium refuses exactly what export does. Every component's URL is
//! judged, not one per repo name: a git repo's must be
//! [`Shareability::Shareable`] (no credentials or query string, no local
//! path, no ssh). An ssh remote is written as its https equivalent instead
//! when that answers with no credentials at all
//! ([`crate::fetch::anonymous`]), in the thornlist (an import fetches what
//! the thornlist names) and so in the pins built from it. A download's URL,
//! or another kind of repo's, is not asked about (it is not git) and may
//! carry a query (`?download=1` is how download links work); it is refused
//! for a user name or password and for a local path. A `!AUTH_URL` (what
//! svn and cvs fetch from) is held to the same. A quoted URL is refused:
//! the parser keeps the quotes, so cactup would fetch them as part of it.
//! Then the whole text that goes into the `.sim`, comments, unused
//! `!DEFINE`s and overridden lines included, is searched for any
//! `scheme://` URL with a user name or password, or a token in its query.
//! `!ANON_USER`/`!ANON_PASS` are left alone: they hold the public logins
//! of anonymous svn and cvs access, never a URL; so is the anonymous user
//! of a cvs root (`:pserver:anonymous@…`), which is refused only for a
//! password. An input's URL is refused for a user name or password.

use crate::fetch::anonymous::{self, Answer};
use crate::fetch::git::{AskWindow, REMOTE_ASK_TIMEOUT};
use crate::thornlist::{self, ComponentType};
use crate::Res;
use anyhow::{bail, Context};
use cactup_ecosystem_common::url::{moved_refusal, shareability, Redacted, Shareability};
use std::collections::{BTreeMap, BTreeSet};

/// What every refusal ends with.
const LOCAL_HINT: &str = "or pass --local to export for use here only";

/// The offline half of the check: what [`classify`] found.
#[derive(Debug)]
pub struct Checked {
    /// A line per URL refused: "<repo>: <why>", "input <knob>: <why>",
    /// "thornlist line <n>: <why>".
    refused: Vec<String>,
    /// The ssh remotes whose https equivalents to ask.
    asks: Vec<Ask>,
}

/// One ssh URL of the thornlist, however many components name it.
#[derive(Debug)]
struct Ask {
    /// The repos whose components name it, in order.
    repos: Vec<String>,
    ssh: String,
    https: String,
}

/// The thornlist `text` (parsed as `list`) and each input's URL in
/// `input_urls` (KNOB, URL) judged without the network: the refusals, and
/// the ssh remotes whose https equivalents [`settle`] must ask. Every
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
            if !auth && let Some(ask) = asks.iter_mut().find(|a| a.ssh == url) {
                if !ask.repos.contains(&c.repo) {
                    ask.repos.push(c.repo.clone());
                }
                continue;
            }
            if !judged.insert((auth, url.to_owned())) {
                continue;
            }
            let why = if let Some(inner) = unquoted(url) {
                refused_urls.insert(inner.to_owned());
                Some(format!(
                    "{} is written in quotes, which the thornlist parser keeps as part of the URL, so it could not \
                     be fetched; remove the quotes",
                    Redacted(inner)
                ))
            } else if c.ty == ComponentType::Cvs {
                cvs_refusal(url)
            } else if auth || c.ty != ComponentType::Git {
                // Not asked about: credentials, and for what is fetched
                // (not a git repo's !AUTH_URL) a local path.
                download_refusal(url, !auth || c.ty != ComponentType::Git)
            } else {
                match shareability(url) {
                    Shareability::Shareable => None,
                    Shareability::Ssh { https: Some(https) } => {
                        asks.push(Ask { repos: vec![c.repo.clone()], ssh: url.to_owned(), https });
                        None
                    }
                    other => other.refusal(url),
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
        if let Some(why) = credentials_refusal(url) {
            refused.push(format!("input {knob}: {why}"));
        }
    }
    Checked { refused, asks }
}

/// Every `scheme://` URL in `text` with a user name or password, or a token
/// in its query, as "thornlist line <n>: <why>", but for those in
/// `already` (refused by name). `!ANON_USER`/`!ANON_PASS` hold a bare login,
/// no URL, so nothing there is refused.
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
            let why = credentials_refusal(url).or_else(|| {
                token_query_key(url).map(|key| {
                    format!(
                        "{} carries a token in its query ({key}=…), which must never be published; remove it",
                        Redacted(url)
                    )
                })
            });
            if let Some(why) = why {
                out.push(format!("thornlist line {}: {why}", n + 1));
            }
        }
    }
    out
}

/// The query key of `url` that names a token, password or signature, if it
/// has one. (The shared crate is to decide this; until then, this list.)
fn token_query_key(url: &str) -> Option<String> {
    let query = url.get(without_query(url).len()..)?.strip_prefix('?')?;
    let query = query.split('#').next().unwrap_or(query);
    query.split(['&', ';']).map(|pair| pair.split('=').next().unwrap_or(pair)).find_map(|key| {
        let k = key.to_ascii_lowercase().replace('-', "_");
        let named = ["token", "secret", "password", "passwd", "signature", "credential", "api_key", "apikey"]
            .iter()
            .any(|word| k.contains(word))
            || ["key", "sig", "auth", "pass", "pwd", "jwt", "session"].contains(&k.as_str());
        named.then(|| key.to_owned())
    })
}

/// `url` without one layer of surrounding quotes, when it has them.
fn unquoted(url: &str) -> Option<&str> {
    let url = url.trim();
    ['"', '\''].into_iter().find_map(|q| url.strip_prefix(q)?.strip_suffix(q))
}

/// Ask the https URL of each ssh remote of `checked`, and fail with every
/// refusal (offline, or by its answer) in one error; else `text` (what
/// `checked` was classified from) with every ssh `!URL` rewritten to its
/// https equivalent (one line said for each), or `None` when there were
/// none. The rewritten text is parsed back and must name exactly the URLs
/// expected. The asks run on the network fan-out within one
/// [`REMOTE_ASK_TIMEOUT`], a line each; Ctrl-C fails the call
/// ("Interrupted").
pub fn settle(checked: Checked, text: &str) -> Res<Option<String>> {
    let Checked { mut refused, asks } = checked;
    let answers = probe_all(&asks)?;
    let mut answered = Vec::new();
    for (ask, answer) in asks.into_iter().zip(answers) {
        let name = ask.repos.join(", ");
        match answer {
            Answer::Answered => answered.push(ask),
            Answer::Moved(to) => refused.push(format!("{name}: {}", moved_refusal(&ask.ssh, &ask.https, to.as_deref()))),
            Answer::Refused(how) => {
                let why = Shareability::Ssh { https: Some(ask.https.clone()) }.refusal(&ask.ssh).expect("not shareable");
                refused.push(format!("{name}: {why} (asked without credentials, it {how})"));
            }
        }
    }
    if !refused.is_empty() {
        bail!(
            "These URLs cannot go into a .sim for others: nobody else could fetch them, or they would publish \
             credentials:\n  {}\nFix them, {LOCAL_HINT}",
            refused.join("\n  ")
        );
    }
    if answered.is_empty() {
        return Ok(None);
    }
    let rewritten = rewrite(text, &answered)?;
    for ask in &answered {
        println!(
            "{}: written as {}, which anyone can fetch, in place of the ssh URL {}",
            ask.repos.join(", "),
            ask.https,
            Redacted(&ask.ssh)
        );
    }
    Ok(Some(rewritten))
}

/// `text` with the `!URL` of each of `answered` rewritten to its https URL,
/// checked by parsing the result back: every component must name exactly
/// the URL it did, or its https equivalent when that was rewritten.
fn rewrite(text: &str, answered: &[Ask]) -> Res<String> {
    let map: BTreeMap<String, String> = answered.iter().map(|a| (a.ssh.clone(), a.https.clone())).collect();
    let list = thornlist::parse(text).context("The thornlist does not parse")?;
    let rewritten = rewrite_urls(text, &map);
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

/// Why `url`, a download's or another kind of repo's than git, cannot be
/// published, if it cannot: a user name or password in it, or, when it is
/// `fetched` from as given, a local path. A query string is no reason:
/// download links carry them.
fn download_refusal(url: &str, fetched: bool) -> Option<String> {
    credentials_refusal(url).or_else(|| match shareability(without_query(url)) {
        s @ Shareability::Local if fetched => s.refusal(url),
        _ => None,
    })
}

/// Why `url` cannot be published for what its authority carries, if it
/// cannot: a user name (but an ssh one) or a password, as the shared crate
/// tells credentials from a URL with its query left out. Said for what was
/// found, not for the query it may also have.
fn credentials_refusal(url: &str) -> Option<String> {
    matches!(shareability(without_query(url)), Shareability::Credentials).then(|| {
        format!("{} carries a user name or password, which must never be published; remove it from the URL", Redacted(url))
    })
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

/// [`credentials_refusal`] for an input's URL given at a prompt: the error,
/// or nothing.
pub fn check_input_url(knob: &str, url: &str) -> Res<()> {
    match credentials_refusal(url) {
        Some(why) => bail!("Input {knob}: {why}, {LOCAL_HINT}"),
        None => Ok(()),
    }
}

/// `url` without its query or fragment, when it has a path to cut them
/// from (the authority, which may hold a password with a `?` in it, is
/// left alone).
fn without_query(url: &str) -> &str {
    let start = url.find("://").map_or(0, |i| i + 3);
    let Some(slash) = url[start..].find('/') else { return url };
    let path = start + slash;
    match url[path..].find(['?', '#']) {
        Some(q) => &url[..path + q],
        None => url,
    }
}

/// Probe the https URL of each of `asks` for an anonymous answer, on the network fan-out within one window, with a
/// progress line for each.
fn probe_all(asks: &[Ask]) -> Res<Vec<Answer>> {
    if asks.is_empty() {
        return Ok(Vec::new());
    }
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child("ask ssh remotes' https URLs");
    item.init(Some(asks.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let window = AskWindow::from(std::time::Instant::now(), REMOTE_ASK_TIMEOUT);
    let answers = crate::fetch::git::ask_all(asks, |ask| {
        let current = item.lock().expect("probe progress poisoned").add_child(ask.repos.join(", "));
        let answer = anonymous::probe(&ask.https, window);
        drop(current);
        item.lock().expect("probe progress poisoned").inc();
        answer
    });
    drop(item);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    answers?.into_iter().collect()
}

/// `text` with each `!URL` that is one of `rewrites`' ssh URLs (as written)
/// set to its https URL; and with an ssh `!URL` built from `$1`/`$2`, which
/// several repos share, set to its own https equivalent (the caller made
/// sure every repo it names answered, and checks the result). Every other
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

    /// A user name or password is refused in a download's URL, and a local
    /// path in what is fetched; a query string is not, nor is an ordinary
    /// URL, and the refusal does not name what it does not refuse.
    #[test]
    fn a_download_is_refused_for_credentials_not_for_a_query() {
        assert!(download_refusal("https://zenodo.org/records/1/files/id.tar.gz?download=1", true).is_none());
        assert!(download_refusal("https://example.org/f.h5", true).is_none());
        for url in ["https://max:s3cret@example.org/f.h5?download=1", "https://TOKEN@example.org/f.h5"] {
            let why = credentials_refusal(url).unwrap();
            assert!(why.contains("user name or password") && !why.contains("query"), "{why}");
            assert!(!why.contains("s3cret") && !why.contains("TOKEN"), "{why}");
        }
        assert!(credentials_refusal("/data/f.h5").is_none(), "an input's local path fails on download");
        assert!(download_refusal("file:///srv/svn/r", true).unwrap().contains("local path"));
        assert!(cvs_refusal(":pserver:anonymous@cvs.example.org:/cvsroot").is_none());
        let why = cvs_refusal(":pserver:me:s3cret@cvs.example.org:/cvsroot").unwrap();
        assert!(why.starts_with(":pserver:me:***@cvs.example.org:/cvsroot carries a password"), "{why}");
    }

    /// The whole text is searched, comments and unused lines included, for
    /// a user name, password or query token in any URL, each named by its
    /// line; ssh user names, anonymous logins, download queries and URLs
    /// already refused by name are not.
    #[test]
    fn credentials_anywhere_in_the_text_are_found() {
        let text = "!CRL_VERSION = 1.0\n# old: !URL = https://TOKEN1@github.com/o/a.git\n\
                    !DEFINE UNUSED = https://me:TOKEN2@example.org/x\n!URL = https://gitlab.com/o/b.git?private_token=TOKEN3\n\
                    !URL = ssh://git@github.com/o/c.git\n!ANON_USER = anonymous\n!ANON_PASS = anon\n\
                    !URL = https://zenodo.org/f.tar.gz?download=1\n!URL = https://TOKEN4@github.com/o/d.git\n";
        let found = search_text(text, &BTreeSet::from(["https://TOKEN4@github.com/o/d.git".to_owned()]));
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(found[0].starts_with("thornlist line 2: https://***@github.com/o/a.git carries a user name"), "{found:?}");
        assert!(found[1].starts_with("thornlist line 3: https://***@example.org/x"), "{found:?}");
        assert!(found[2].starts_with("thornlist line 4: ") && found[2].contains("(private_token=…)"), "{found:?}");
        assert!(!found.concat().contains("TOKEN"), "{found:?}");
        assert_eq!(token_query_key("https://h/f?download=1&X-Amz-Signature=s").as_deref(), Some("X-Amz-Signature"));
        assert_eq!(token_query_key("https://h/f?download=1"), None);
        assert_eq!(unquoted("\"https://h/x\""), Some("https://h/x"));
        assert_eq!(unquoted("https://h/x"), None);
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
