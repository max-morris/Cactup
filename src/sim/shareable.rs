//! What may go into a `.sim` that leaves this machine (spec §8.9): every
//! repo URL one anybody can fetch, and no credentials anywhere in it.
//! `sim export` and `sim pack` check the thornlist and every input URL
//! before they write one, unless `--local` says the file is for use here
//! only. The check comes in two halves: [`classify`], offline, and
//! [`settle_with`], which probes every git remote anonymously, at the same
//! time as Cactarium is asked about the pins ([`super::registered`]), and
//! [`Settled::apply`], which rewrites the ssh remotes that answer as https.
//! They run before anything else asks the network (a pack's remote tips,
//! which are then asked of the rewritten URLs); a URL refused offline still
//! waits for the probes, so one error ([`refusal`]) names everything
//! refused.
//!
//! The rules are the shared crate's (`cactup_ecosystem_common::url` and
//! `::probe`), so Cactarium refuses exactly what export does. Every
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
//! fetch such a URL. An input's URL is judged as a download's. All of these
//! are fetched, so a credential in one is refused.
//!
//! Then the whole text that goes into the `.sim` is searched for
//! credentials ([`search_text`]): a `scheme://` URL that carries them as a
//! download's would, an scp-like `user:password@host:path`, a cvs root with
//! a password. One found where nothing fetches it (a comment, an unused
//! `!DEFINE`, an overridden `!URL`, a `#` comment after a URL) is hidden,
//! not refused: its secret becomes `***` in the copy that goes into the
//! `.sim`, a line said for each. Export reads the simulation's frozen
//! thornlist, which nobody edits, so a refusal there could not be fixed.
//! The text so hidden is parsed back and must give the same components.
//! `!ANON_USER`/`!ANON_PASS` hold the public logins of anonymous svn and
//! cvs access, never a URL, and are never refused.

use super::registered;
use crate::cactarium::Client;
use crate::fetch::anonymous::{self, Verdict};
use crate::fetch::git::{AskWindow, REMOTE_ASK_TIMEOUT};
use crate::thornlist::{self, ComponentType};
use crate::Res;
use anyhow::{bail, Context};
use cactup_ecosystem_common::api::LookedUp;
use cactup_ecosystem_common::probe::{self, Start};
use cactup_ecosystem_common::url::{download_shareability, shareability, unquote, Redacted, Shareability};
use colored::Colorize;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::LazyLock;

/// The line every refusal ends with.
pub(crate) const LOCAL_HINT: &str = "Pass --local to export for use here only.";

/// The offline half of the check: what [`classify`] found.
#[derive(Debug, Default)]
pub struct Checked {
    /// A line per URL refused: "<repo>: <why>", "input <knob>: <why>".
    refused: Vec<String>,
    /// The repos (as the thornlist names them) with a URL refused.
    refused_repos: BTreeSet<String>,
    /// The git remotes to probe.
    asks: Vec<Ask>,
    /// The credentials found where nothing fetches them, to hide.
    hidden: Vec<Spot>,
}

impl Checked {
    /// A line per URL refused.
    pub fn refused(&self) -> &[String] {
        &self.refused
    }

    /// Whether a URL of repo `repo` (as the thornlist names it) was refused.
    pub fn refuses(&self, repo: &str) -> bool {
        self.refused_repos.contains(repo)
    }

    /// The URL the git remote `url` is to be published as, if it is probed:
    /// an ssh remote's https equivalent, any other remote itself. `None` for
    /// one refused offline.
    pub fn published_as(&self, url: &str) -> Option<&str> {
        self.asks.iter().find(|a| a.url == url).map(|a| a.candidate.as_str())
    }
}

/// A credential [`search_text`] found in the thornlist's text.
#[derive(Debug, PartialEq, Eq)]
struct Spot {
    /// Its line's index in the text.
    line: usize,
    /// The byte ranges in that line (without its line ending) that become
    /// `***`.
    secrets: Vec<Range<usize>>,
}

/// One git remote of the thornlist, however many components name it.
#[derive(Debug)]
struct Ask {
    /// The repos whose components name it, in order, as a refusal names
    /// them ([`repo_label`]).
    repos: Vec<String>,
    /// The same repos as the thornlist names them.
    names: Vec<String>,
    /// The remote as written.
    url: String,
    /// What `probe::start` named to probe and publish.
    candidate: String,
}

/// The thornlist `text` (parsed as `list`) and each input's URL in
/// `input_urls` (KNOB, URL) judged without the network: the refusals, and
/// the git remotes [`settle`] must probe, and the credentials to hide. Every
/// component's URL and `!AUTH_URL` is judged once per URL string; then the
/// whole text is searched for credentials (see the module docs).
pub fn classify(list: &thornlist::Thornlist, text: &str, input_urls: &[(String, String)]) -> Checked {
    let mut refused = Vec::new();
    let mut refused_repos = BTreeSet::new();
    let mut asks: Vec<Ask> = Vec::new();
    // The URLs judged, and of those the ones refused (the text search
    // leaves those to the refusal already said).
    let mut judged = BTreeSet::new();
    let mut refused_urls = BTreeSet::new();
    for c in list.components() {
        if c.ty == ComponentType::Ignore {
            continue;
        }
        let name = repo_label(list, text, &c.repo, c.url.as_deref());
        let fields = [(c.url.as_deref(), name.clone()), (c.auth_url.as_deref(), format!("{name} (its !AUTH_URL)"))];
        for (i, (url, label)) in fields.into_iter().enumerate() {
            let Some(url) = url.filter(|u| !u.trim().is_empty()) else { continue };
            let auth = i == 1;
            if !auth && let Some(ask) = asks.iter_mut().find(|a| a.url == url) {
                if !ask.repos.contains(&name) {
                    ask.repos.push(name.clone());
                }
                if !ask.names.contains(&c.repo) {
                    ask.names.push(c.repo.clone());
                }
                continue;
            }
            if !judged.insert((auth, url.to_owned())) {
                if refused_urls.contains(url) || refused_urls.contains(unquote(url)) {
                    refused_repos.insert(c.repo.clone());
                }
                continue;
            }
            let why = if unquote(url) != url.trim() {
                let inner = unquote(url);
                refused_urls.insert(inner.to_owned());
                Some(format!("{} is written in quotes; remove them", Redacted(inner)))
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
                        let names = vec![c.repo.clone()];
                        asks.push(Ask { repos: vec![name.clone()], names, url: url.to_owned(), candidate });
                        None
                    }
                    Start::Refuse(why) => Some(why),
                }
            };
            if let Some(why) = why {
                refused_urls.insert(url.to_owned());
                refused_repos.insert(c.repo.clone());
                refused.push(format!("{label}: {why}"));
            }
        }
    }
    let hidden = search_text(text, &refused_urls);
    for (knob, url) in input_urls {
        if let Some(why) = download_refusal(url) {
            refused.push(format!("input {knob}: {why}"));
        }
    }
    Checked { refused, refused_repos, asks, hidden }
}

/// How a refusal names the repo `repo` (as the thornlist derives the name)
/// of a component whose URL is `url`: the name [`shown_repo`] shows,
/// without surrounding quotes
/// (a quoted URL's name keeps its closing one) or a trailing `.git`; and
/// when another section gives the same name to another URL, the thornlist
/// line of this one's `!URL`, so the section can be told apart.
fn repo_label(list: &thornlist::Thornlist, text: &str, repo: &str, url: Option<&str>) -> String {
    let bare = shown_repo(repo).trim_matches(['"', '\'']);
    let name = bare.strip_suffix(".git").unwrap_or(bare);
    let urls: BTreeSet<&str> = list.components().iter().filter(|c| c.repo == repo).filter_map(|c| c.url.as_deref()).collect();
    let line = url.filter(|_| urls.len() > 1).and_then(|url| {
        text.lines()
            .position(|l| {
                l.trim_start()
                    .strip_prefix("!URL")
                    .and_then(|rest| rest.trim_start().strip_prefix('='))
                    .is_some_and(|value| value.trim() == url.trim())
            })
            .map(|i| i + 1)
    });
    match line {
        Some(n) => format!("{name} (thornlist line {n})"),
        None => name.to_owned(),
    }
}

/// The repo name `repo` (as the thornlist derives it) as any message shows
/// it: cut at its first `?` or `#`. The parser names a repo after its URL's
/// last segment, query and all, and a query may hold a token
/// (`r.git?private_token=…`, `f.tar.gz?token=…`).
pub fn shown_repo(repo: &str) -> &str {
    repo.split(['?', '#']).next().unwrap_or(repo)
}

/// Every credential in `text` but those in `already` (refused by name, as
/// fetched): a `scheme://` URL that carries one as a download's would (a
/// user name but an ssh one, a password, a credential-like query key:
/// `url::download_shareability`), an scp-like `user:password@host:path` (or one with a
/// query: `url::shareability`), a cvs root with a password ([`cvs_secrets`]).
/// `!ANON_USER`/`!ANON_PASS` hold a bare login, no URL, so nothing there is
/// found.
fn search_text(text: &str, already: &BTreeSet<String>) -> Vec<Spot> {
    // A password may hold a quote or a backtick: the userinfo runs to the
    // last `@` before the path.
    static URL: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"[A-Za-z][A-Za-z0-9+.\-]*://(?:[^\s/?#]*@)?[^\s"'<>`]*"#).expect("valid regex")
    });
    static CVS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?:^|[\s"'`=(<])(:[A-Za-z][A-Za-z0-9_+\-]*[;,:][^\s"'`<>]*)"#).expect("valid regex")
    });
    static SCP: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?:^|[\s"'`=(<])([^\s/@]+@[^\s"'`<>/@:\[]+:[^\s"'`<>]*)"#).expect("valid regex")
    });
    let trim = |m: regex::Match| {
        let token = m.as_str().trim_end_matches([')', ']', '}', ',', '.', ';', ':']);
        m.start()..m.start() + token.len()
    };
    let mut out = Vec::new();
    for (n, line) in text.split_inclusive('\n').enumerate() {
        let line = line.trim_end_matches(['\n', '\r']);
        // Each token once: the URL form first, then a cvs root, then the
        // scp form, none inside another's span.
        let mut taken: Vec<Range<usize>> = Vec::new();
        let mut secrets = Vec::new();
        let candidates = URL
            .find_iter(line)
            .map(|m| (0, trim(m)))
            .chain(CVS.captures_iter(line).filter_map(|c| c.get(1)).map(|m| (1, trim(m))))
            .chain(SCP.captures_iter(line).filter_map(|c| c.get(1)).map(|m| (2, trim(m))))
            .collect::<Vec<_>>();
        for (kind, span) in candidates {
            if taken.iter().any(|t| t.start < span.end && span.start < t.end) {
                continue;
            }
            taken.push(span.clone());
            let token = &line[span.clone()];
            if already.contains(token) {
                continue;
            }
            let found = match kind {
                0 if download_shareability(token) == Shareability::Credentials => url_secrets(token),
                1 => cvs_secrets(token),
                2 if shareability(token) == Shareability::Credentials => url_secrets(token),
                _ => Vec::new(),
            };
            secrets.extend(found.into_iter().map(|r| span.start + r.start..span.start + r.end));
        }
        if !secrets.is_empty() {
            out.push(Spot { line: n, secrets });
        }
    }
    out
}

/// The byte ranges of the secrets in `url` (a `scheme://` URL or an
/// scp-like one, judged to carry credentials): the userinfo (but an ssh
/// URL's user name: only its password), and the query or fragment, whole.
fn url_secrets(url: &str) -> Vec<Range<usize>> {
    let (scheme, start) = match url.find("://") {
        Some(i) => (Some(&url[..i]), i + 3),
        None => (None, 0),
    };
    let ssh = scheme.is_none_or(|s| s.to_ascii_lowercase().contains("ssh"));
    let authority_end = start + url[start..].find('/').unwrap_or(url.len() - start);
    let mut secrets = Vec::new();
    let mut host = start;
    if let Some(at) = url[start..authority_end].rfind('@').map(|at| start + at) {
        let userinfo = start..at;
        match url[userinfo.clone()].find(':') {
            Some(colon) if ssh => secrets.push(start + colon + 1..at),
            _ if ssh => {}
            _ => secrets.push(userinfo),
        }
        host = at + 1;
    }
    if let Some(q) = url[host..].find(['?', '#']) {
        secrets.push(host + q + 1..url.len());
    }
    secrets.retain(|r| !r.is_empty());
    secrets
}

/// `line` with each of `secrets` (byte ranges, in any order, may overlap)
/// replaced by `***`.
fn hide(line: &str, secrets: &[Range<usize>]) -> String {
    let mut sorted = secrets.to_vec();
    sorted.sort_by_key(|r| r.start);
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    for r in sorted {
        if r.end <= at {
            continue;
        }
        out.push_str(&line[at..r.start.max(at)]);
        out.push_str("***");
        at = r.end;
    }
    out.push_str(&line[at..]);
    out
}

/// `text` with every spot of `hidden` hidden; every other byte as it was.
fn hide_spots(text: &str, hidden: &[Spot]) -> String {
    let mut out = String::with_capacity(text.len());
    for (n, line) in text.split_inclusive('\n').enumerate() {
        let body = line.trim_end_matches(['\n', '\r']);
        let secrets: Vec<Range<usize>> =
            hidden.iter().filter(|s| s.line == n).flat_map(|s| s.secrets.iter().cloned()).collect();
        if secrets.is_empty() {
            out.push_str(line);
        } else {
            out.push_str(&hide(body, &secrets));
            out.push_str(&line[body.len()..]);
        }
    }
    out
}

/// Probe every git remote of `checked`, and fail with every refusal
/// (offline, or by its probe) and every line of `more` in one error; else
/// `text` (what `checked` was classified from) as [`Settled::apply`] gives
/// it. The probes run all at once within one [`REMOTE_ASK_TIMEOUT`], a
/// line each; Ctrl-C fails the call ("Interrupted").
pub fn settle(checked: Checked, text: &str, more: Vec<String>) -> Res<Option<String>> {
    let settled = settle_with(checked, None)?;
    let mut refused = settled.refused.clone();
    refused.extend(more);
    if !refused.is_empty() {
        return Err(refusal(&refused));
    }
    settled.apply(text)
}

/// The one error for everything refused, a line each, ending with the way
/// past.
pub fn refusal(lines: &[String]) -> anyhow::Error {
    anyhow::anyhow!("These cannot go into a shared .sim:\n  {}\n{LOCAL_HINT}", lines.join("\n  "))
}

/// What [`settle_with`] found.
#[derive(Debug)]
pub struct Settled {
    /// A line per URL refused, offline or by its probe.
    pub refused: Vec<String>,
    /// The repos (as the thornlist names them) with a URL refused.
    pub refused_repos: BTreeSet<String>,
    /// The asks, and each one's URL to publish as, when another.
    asks: Vec<Ask>,
    rewrites: BTreeMap<String, String>,
    hidden: Vec<Spot>,
    /// Cactarium's answer to the lookup asked alongside, if one was.
    pub looked_up: Option<Res<LookedUp>>,
}

impl Settled {
    /// `text` (what the check was classified from), once nothing is
    /// refused, with every remote published as another URL (an ssh remote
    /// as its https equivalent) rewritten to it and every credential found
    /// where nothing fetches it hidden, a line said for each, or `None` when
    /// there were none. The new text is parsed back and must give the same
    /// components, but for the URLs rewritten.
    pub fn apply(&self, text: &str) -> Res<Option<String>> {
        if self.rewrites.is_empty() && self.hidden.is_empty() {
            return Ok(None);
        }
        let rewritten = rewrite(text, &self.hidden, &self.rewrites)?;
        for note in hidden_notes(&rewritten, &self.hidden) {
            println!("{}", note.yellow());
        }
        for ask in &self.asks {
            if let Some(published) = self.rewrites.get(&ask.url) {
                println!("{}: published as {published} (was {}).", ask.repos.join(", "), Redacted(&ask.url));
            }
        }
        Ok(Some(rewritten))
    }
}

/// Probe every git remote of `checked` and, at the same time, ask
/// Cactarium `lookup` (one renderer, a line each): what was refused, and
/// what the lookup answered. Refuses nothing itself. The probes run all at
/// once within one [`REMOTE_ASK_TIMEOUT`]; Ctrl-C fails the call
/// ("Interrupted") at once.
pub fn settle_with(checked: Checked, lookup: Option<(&Client, &registered::Query)>) -> Res<Settled> {
    let Checked { mut refused, mut refused_repos, asks, hidden } = checked;
    let window = AskWindow::from(std::time::Instant::now(), REMOTE_ASK_TIMEOUT);
    let lookup = lookup.filter(|(_, q)| !q.is_empty());
    let (progress, renderer) = if asks.is_empty() && lookup.is_none() {
        (prodash::tree::Root::new(), None)
    } else {
        crate::manifest::setup_prodash_if_tty()
    };
    let (verdicts, looked_up) = std::thread::scope(|scope| {
        let asking = lookup.map(|(client, query)| {
            let item = progress.add_child("ask Cactarium what each pin is");
            scope.spawn(move || query.ask(client, item))
        });
        let verdicts = probe_all(&progress, &asks, window);
        let looked_up = asking.map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("The lookup died"))));
        (verdicts, looked_up)
    });
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    if gix::interrupt::is_triggered() {
        bail!("Interrupted");
    }
    let mut rewrites = BTreeMap::new();
    for (ask, verdict) in asks.iter().zip(verdicts?) {
        match verdict {
            Verdict::Publish(published) if published != ask.url => {
                rewrites.insert(ask.url.clone(), published);
            }
            Verdict::Publish(_) => {}
            Verdict::Refuse(why) => {
                refused.push(format!("{}: {why}", ask.repos.join(", ")));
                refused_repos.extend(ask.names.iter().cloned());
            }
        }
    }
    Ok(Settled { refused, refused_repos, asks, rewrites, hidden, looked_up })
}

/// `text` with the credentials of `hidden` hidden and each `!URL` of `map`
/// (URL as written → URL to publish) rewritten, checked by parsing the
/// result back: every component must be what it was, but for naming the
/// URL it is published as. A hidden credential that changed a component
/// was fetched after all, and fails the check.
fn rewrite(text: &str, hidden: &[Spot], map: &BTreeMap<String, String>) -> Res<String> {
    let list = thornlist::parse(text).context("The thornlist does not parse")?;
    let rewritten = rewrite_urls(&hide_spots(text, hidden), map);
    let check = thornlist::parse(&rewritten).context("The thornlist with its URLs rewritten does not parse")?;
    let mut stuck = Vec::new();
    let mut changed = Vec::new();
    for (i, old) in list.components().iter().enumerate() {
        let want = old.url.as_ref().map(|u| map.get(u).unwrap_or(u));
        let got = check.components().get(i).filter(|c| c.checkout == old.checkout && c.repo == old.repo);
        let url = old.url.as_deref().unwrap_or("");
        match got {
            Some(got) if got.url.as_ref() == want || old.ty == ComponentType::Ignore => {
                if !same_but_url(old, got) {
                    changed.push(old.checkout.clone());
                }
            }
            Some(_) if !map.contains_key(url) => changed.push(old.checkout.clone()),
            _ => {
                let https = map.get(url).map_or(String::new(), |h| format!(" (as {h})"));
                stuck.push(format!("{}: {}{https}", old.checkout, Redacted(url)));
            }
        }
    }
    if !changed.is_empty() {
        bail!(
            "Hiding a credential would change these components:\n  {}\nRemove the credentials from the thornlist.\n\
             {LOCAL_HINT}",
            changed.join("\n  ")
        );
    }
    if !stuck.is_empty() || check.components().len() != list.components().len() {
        bail!(
            "No !URL line to rewrite as https for these ssh URLs:\n  {}\nWrite their https URLs in the \
             thornlist.\n{LOCAL_HINT}",
            if stuck.is_empty() { "(the list changed shape)".to_owned() } else { stuck.join("\n  ") }
        );
    }
    Ok(rewritten)
}

/// The line said for each of `hidden` in `text`, the text they were hidden
/// in: the line as it now reads. (Its number would be of the text with its
/// `!INCLUDE`s spliced in, not of any file the user has.)
fn hidden_notes(text: &str, hidden: &[Spot]) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    hidden
        .iter()
        .map(|spot| format!("Hid a credential in the thornlist: {}", lines.get(spot.line).map_or("", |l| l.trim())))
        .collect()
}

/// Whether `got` is `old` (a component, and the same one parsed back) but
/// for its URL; an `ignore` component's URLs are not fetched, and may
/// differ too.
fn same_but_url(old: &thornlist::Component, got: &thornlist::Component) -> bool {
    let mut got = got.clone();
    got.url.clone_from(&old.url);
    if old.ty == ComponentType::Ignore {
        got.auth_url.clone_from(&old.auth_url);
    }
    &got == old
}

/// Why `url`, a download's, an input's or another kind of repo's than git,
/// cannot be published, if it cannot: the shared crate's rule for
/// downloads (`url::download_shareability`).
fn download_refusal(url: &str) -> Option<String> {
    download_shareability(url).download_refusal(url)
}

/// Why the cvs root `root` (no URL) cannot be published, if it cannot: a
/// password in it ([`cvs_secrets`]), which the refusal shows as `***`. A
/// user name alone is how anonymous access is written
/// (`:pserver:anonymous@…`).
fn cvs_refusal(root: &str) -> Option<String> {
    let root = root.trim();
    let secrets = cvs_secrets(root);
    (!secrets.is_empty()).then(|| format!("{} carries a password", hide(root, &secrets)))
}

/// The byte ranges of the passwords in the cvs root `root`:
/// `:method[;option=value…]:[user[:password]@]host[:port]:path`, or
/// without the method. A password is the main one, an option whose key
/// holds `pass` (CVSNT's `;password=…`, `;proxypassword=…`), or the one in
/// a proxy option's userinfo (`;proxy=user:password@host`). A proxy's
/// value runs to the `:` after its `@`, so a proxy written without
/// userinfo but followed by a user name (`;proxy=p:anonymous@host`) counts
/// as one: refused, the safe side.
fn cvs_secrets(root: &str) -> Vec<Range<usize>> {
    let mut secrets = Vec::new();
    let mut at = 0;
    if let Some(after) = root.strip_prefix(':') {
        at = 1 + after.find([';', ',', ':']).unwrap_or(after.len());
        while root[at..].starts_with([';', ',']) {
            let start = at + 1;
            let rest = &root[start..];
            let Some(eq) = rest.find([';', ',', ':', '=']).filter(|&i| rest.as_bytes()[i] == b'=') else {
                at = start + rest.find([';', ',', ':']).unwrap_or(rest.len());
                continue;
            };
            let key = rest[..eq].to_ascii_lowercase();
            let value_start = start + eq + 1;
            let tail = &root[value_start..];
            let chunk = &tail[..tail.find([';', ',']).unwrap_or(tail.len())];
            let end = match chunk.find('@') {
                Some(a) if key == "proxy" => a + 1 + chunk[a + 1..].find(':').unwrap_or(chunk.len() - a - 1),
                _ => tail.find([';', ',', ':']).unwrap_or(tail.len()),
            };
            let value = &tail[..end];
            if key.contains("pass") && !value.is_empty() {
                secrets.push(value_start..value_start + end);
            }
            if key == "proxy"
                && let Some((userinfo, _)) = value.split_once('@')
                && let Some(colon) = userinfo.find(':')
                && colon + 1 < userinfo.len()
            {
                secrets.push(value_start + colon + 1..value_start + userinfo.len());
            }
            at = value_start + end;
        }
        match root[at..].strip_prefix(':') {
            Some(_) => at += 1,
            None => return secrets,
        }
    }
    let main = &root[at..];
    if let Some(a) = main.rfind('@')
        && let Some(colon) = main[..a].find(':')
        && colon + 1 < a
    {
        secrets.push(at + colon + 1..at + a);
    }
    secrets
}

/// [`download_refusal`] for an input's URL given at a prompt: the error,
/// or nothing.
pub fn check_input_url(knob: &str, url: &str) -> Res<()> {
    match download_refusal(url).or_else(|| anonymous::private_download(url)) {
        Some(why) => bail!("Input {knob}: {why}.\n{LOCAL_HINT}"),
        None => Ok(()),
    }
}

/// Probe each of `asks` anonymously ([`anonymous::check`]), all at once (up
/// to [`anonymous::PROBES_AT_ONCE`]) within `window`, with a progress line
/// for each under `progress`.
fn probe_all(progress: &prodash::tree::Root, asks: &[Ask], window: AskWindow) -> Res<Vec<Verdict>> {
    if asks.is_empty() {
        return Ok(Vec::new());
    }
    let item = progress.add_child("ask git remotes without credentials");
    item.init(Some(asks.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let verdicts = crate::fetch::git::ask_all_at(asks, anonymous::PROBES_AT_ONCE, |ask| {
        let current = item.lock().expect("probe progress poisoned").add_child(ask.repos.join(", "));
        let verdict = anonymous::check(&ask.url, &ask.candidate, window);
        drop(current);
        item.lock().expect("probe progress poisoned").inc();
        verdict
    });
    verdicts?.into_iter().collect()
}

/// `text` with each `!URL` that is one of `rewrites`' URLs (as written) set
/// to the URL it is published as; and with an ssh `!URL` built from
/// `$1`/`$2`, which several repos share, set to its own https equivalent
/// (the caller made sure every repo it names answered, and checks the
/// result), unless its host is built from a variable. Every other byte
/// stays as it was.
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
                    // A host built from a variable is lowercased with it
                    // (`$GH` → `$gh`, which nothing defines): left alone.
                    Shareability::Ssh { https: Some(https) } if url.contains('$') && !host_of(&https).contains('$') => {
                        Some(https)
                    }
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

/// The host of the https URL `https`.
fn host_of(https: &str) -> &str {
    let rest = https.strip_prefix("https://").unwrap_or(https);
    rest.split('/').next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A download's URL is judged by the crate's download rule: a query is
    /// fine unless a key looks like a credential, and a user name or
    /// password, ssh and a local path are refused.
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
            assert!(why.contains(" carries ") && !why.contains("s3cret") && !why.contains("TOKEN"), "{why}");
        }
        assert!(download_refusal("svn+ssh://svn.example.org/r").unwrap().contains("ssh"));
        assert!(download_refusal("file:///srv/svn/r").unwrap().contains("local path"));
        // A fragment's token is hidden too, and a percent-encoded key found.
        let why = download_refusal("https://example.org/f.h5#access_token=TOKEN").unwrap();
        assert!(why.starts_with("https://example.org/f.h5#*** carries"), "{why}");
        assert!(download_refusal("https://example.org/f.h5?%74oken=TOKEN").unwrap().contains(" carries "));
    }

    /// The whole text is searched, comments and unused lines included, for
    /// credentials in every form, each hidden in place: a URL's userinfo
    /// (an ssh URL's password), a credential-like query (percent-encoded
    /// too) or fragment, an scp-like password, a cvs root's password, a
    /// password with quotes or a backtick in it. Ssh user names, anonymous
    /// logins, download queries and URLs already refused by name are left
    /// alone, and so is every other byte.
    #[test]
    fn credentials_anywhere_in_the_text_are_hidden() {
        let cases = [
            ("# old: !URL = https://TOKEN1@github.com/o/a.git", "# old: !URL = https://***@github.com/o/a.git"),
            ("!DEFINE UNUSED = https://me:TOKEN2@example.org/x", "!DEFINE UNUSED = https://***@example.org/x"),
            ("!URL = https://gitlab.com/o/b.git?private_token=TOKEN3", "!URL = https://gitlab.com/o/b.git?***"),
            ("# https://example.org/f?%74oken=TOKEN4.", "# https://example.org/f?***."),
            ("!URL = https://example.org/g.tgz#access_token=TOKEN5", "!URL = https://example.org/g.tgz#***"),
            ("# (ssh://git:TOKEN6@github.com/o/c.git)", "# (ssh://git:***@github.com/o/c.git)"),
            ("# was git:TOKEN7@github.com:o/r.git", "# was git:***@github.com:o/r.git"),
            ("!DEFINE OLD=git:TOKEN8@github.com:o/r.git", "!DEFINE OLD=git:***@github.com:o/r.git"),
            ("# :pserver:me:TOKEN9@cvs.example.org:/cvsroot", "# :pserver:me:***@cvs.example.org:/cvsroot"),
            ("# :pserver;password=TOKENA:me@cvs.example.org:/r", "# :pserver;password=***:me@cvs.example.org:/r"),
            ("# https://me:to\"k`EN'B@example.org/x", "# https://***@example.org/x"),
            ("!URL = ssh://git@github.com/o/c.git", "!URL = ssh://git@github.com/o/c.git"),
            ("!URL = git@github.com:o/d.git", "!URL = git@github.com:o/d.git"),
            ("!URL = :pserver:anonymous@cvs.example.org:/cvsroot", "!URL = :pserver:anonymous@cvs.example.org:/cvsroot"),
            ("!ANON_USER = anonymous", "!ANON_USER = anonymous"),
            ("!ANON_PASS = anon", "!ANON_PASS = anon"),
            ("!URL = https://zenodo.org/f.tar.gz?download=1", "!URL = https://zenodo.org/f.tar.gz?download=1"),
            ("# mail max@example.org: see https://example.org/x#readme", "# mail max@example.org: see https://example.org/x#readme"),
            ("!URL = https://TOKENC@github.com/o/d.git", "!URL = https://TOKENC@github.com/o/d.git"),
        ];
        let text: String = cases.iter().map(|(line, _)| format!("{line}\r\n")).collect();
        let already = BTreeSet::from(["https://TOKENC@github.com/o/d.git".to_owned()]);
        let found = search_text(&text, &already);
        let hidden = hide_spots(&text, &found);
        let want: String = cases.iter().map(|(_, line)| format!("{line}\r\n")).collect();
        assert_eq!(hidden, want);
        assert_eq!(found.len(), 11, "{found:?}");
        assert_eq!(found[0], Spot { line: 0, secrets: std::iter::once(22..28).collect() });
    }

    /// A credential nothing fetches is hidden, and the list parsed back
    /// gives the same components; one in a fetched URL is refused (an
    /// active cvs root's option password included), not hidden.
    #[test]
    fn credentials_nothing_fetches_are_hidden_and_fetched_ones_refused() {
        let text = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!DEFINE OLD = https://me:TOKEN1@example.org/x\n\
                    # was git:TOKEN2@github.com:o/a.git\n\
                    !TARGET = $ROOT\n!TYPE = cvs\n!URL = :pserver:me:TOKEN3@cvs.example.org:/cvsroot\n\
                    !URL = :pserver:anonymous@cvs.example.org:/cvsroot\n!CHECKOUT = A/One\n\
                    !TARGET = $ROOT\n!TYPE = svn\n!URL = https://svn.example.org/b/trunk#token=TOKEN4\n!CHECKOUT = B/One\n";
        let list = thornlist::parse(text).unwrap();
        let checked = classify(&list, text, &[]);
        assert!(checked.refused.is_empty(), "{:?}", checked.refused);
        assert!(checked.asks.is_empty());
        let hidden = rewrite(text, &checked.hidden, &BTreeMap::new()).unwrap();
        assert!(!hidden.contains("TOKEN"), "{hidden}");
        assert_eq!(thornlist::parse(&hidden).unwrap().components(), list.components());
        assert_eq!(
            hidden_notes(&hidden, &checked.hidden),
            [
                "Hid a credential in the thornlist: !DEFINE OLD = https://***@example.org/x",
                "Hid a credential in the thornlist: # was git:***@github.com:o/a.git",
                "Hid a credential in the thornlist: !URL = :pserver:me:***@cvs.example.org:/cvsroot",
                "Hid a credential in the thornlist: !URL = https://svn.example.org/b/trunk#***",
            ]
        );

        // Fetched: refused by name, and not hidden.
        let text = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
                    !TARGET = $ROOT\n!TYPE = cvs\n!URL = :pserver;password=TOKEN5:me@cvs.example.org:/cvsroot\n\
                    !CHECKOUT = A/One\n";
        let checked = classify(&thornlist::parse(text).unwrap(), text, &[]);
        assert_eq!(checked.refused, ["cvsroot: :pserver;password=***:me@cvs.example.org:/cvsroot carries a password"]);
        assert!(checked.hidden.is_empty(), "{:?}", checked.hidden);
    }

    /// A hidden credential that a component turns out to use fails the
    /// parse-back check, rather than change what the list fetches.
    #[test]
    fn hiding_a_credential_a_component_uses_fails() {
        let text = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
                    !TARGET = $ROOT\n!TYPE = svn\n!URL = https://me:TOKEN@svn.example.org/b\n!CHECKOUT = B/One\n";
        let spot = search_text(text, &BTreeSet::new());
        assert_eq!(spot.len(), 1);
        let err = format!("{:#}", rewrite(text, &spot, &BTreeMap::new()).unwrap_err());
        assert!(err.contains("would change these components:\n  B/One\n"), "{err}");
    }

    /// A cvs root is refused for a password, written in any of its places,
    /// and shown with it hidden; a user name alone is anonymous access.
    #[test]
    fn a_cvs_root_is_refused_for_a_password_anywhere_in_it() {
        assert!(cvs_refusal(":pserver:anonymous@cvs.example.org:/cvsroot").is_none());
        assert!(cvs_refusal(":pserver;port=2401:anonymous@cvs.example.org:/cvsroot").is_none());
        assert!(cvs_refusal(":pserver;proxy=proxy.example.org;proxyport=8080:me@cvs.example.org:/r").is_none());
        for (root, shown) in [
            (":pserver:me:s3cret@cvs.example.org:/cvsroot", ":pserver:me:***@cvs.example.org:/cvsroot"),
            ("me:s3cret@cvs.example.org:/cvsroot", "me:***@cvs.example.org:/cvsroot"),
            (":pserver;password=s3cret:me@cvs.example.org:/r", ":pserver;password=***:me@cvs.example.org:/r"),
            (":pserver,PROXYPASSWORD=s3cret:me@cvs.example.org:/r", ":pserver,PROXYPASSWORD=***:me@cvs.example.org:/r"),
            (":pserver;proxy=u:s3cret@proxy.example.org:me@cvs.example.org:/r", ":pserver;proxy=u:***@proxy.example.org:me@cvs.example.org:/r"),
            (":pserver;proxy=u:s3cret@proxy.example.org;port=1:me@h:/r", ":pserver;proxy=u:***@proxy.example.org;port=1:me@h:/r"),
        ] {
            assert_eq!(cvs_refusal(root), Some(format!("{shown} carries a password")), "{root}");
        }
    }

    /// A repo named after a URL with a query (the parser keeps it in the
    /// name) is shown cut at the query, in a refusal and wherever else a
    /// repo name is shown.
    #[test]
    fn a_repo_name_is_shown_without_its_query() {
        let text = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
                    !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://github.com/o/r.git?private_token=SECRETQ\n\
                    !CHECKOUT = R/One\n\
                    !TARGET = $ROOT/dl\n!TYPE = https\n!URL = https://example.org/f.tar.gz?token=SECRETD\n!CHECKOUT = f.tgz\n";
        let list = thornlist::parse(text).unwrap();
        assert!(list.components().iter().any(|c| c.repo.contains("SECRETQ")), "the parser's key keeps the query");
        let checked = classify(&list, text, &[]);
        assert_eq!(checked.refused.len(), 2, "{:#?}", checked.refused);
        assert!(checked.refused[0].starts_with("r: "), "{:?}", checked.refused);
        assert!(checked.refused[1].starts_with("f.tar.gz: "), "{:?}", checked.refused);
        assert!(!checked.refused.concat().contains("SECRET"), "{:?}", checked.refused);
        assert_eq!(shown_repo("r.git?private_token=SECRETQ"), "r.git");
        assert_eq!(shown_repo("g.tar.gz#access_token=SECRETF"), "g.tar.gz");
        assert_eq!(shown_repo("zlib"), "zlib");
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
        assert!(checked.refused[1].starts_with("input bad: https://gitlab.com/api/v4/f.h5?*** carries `private_token` in its query"), "{:?}", checked.refused);
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

    /// Every remote is probed at once: forty that never answer leave the
    /// one behind them asked, and published, within the window.
    #[test]
    fn remotes_behind_silent_ones_are_still_probed() {
        use crate::fetch::anonymous::testserver::{serve, Mode};
        serve("silent.many.test", Mode::Silent);
        serve("public.many.test", Mode::Public);
        let ask = |url: String| Ask { repos: vec!["r".to_owned()], names: vec!["r".to_owned()], candidate: url.clone(), url };
        let mut asks: Vec<Ask> = (0..40).map(|i| ask(format!("https://silent.many.test/o/r{i}"))).collect();
        asks.push(ask("https://public.many.test/o/r".to_owned()));
        let window = AskWindow::from(std::time::Instant::now(), std::time::Duration::from_millis(1500));
        let verdicts = probe_all(&prodash::tree::Root::new(), &asks, window).unwrap();
        assert_eq!(verdicts[40], Verdict::Publish("https://public.many.test/o/r".to_owned()));
        assert!(matches!(&verdicts[0], Verdict::Refuse(why) if why.contains("no answer within")), "{:?}", verdicts[0]);
    }

    /// An ssh `!URL` is rewritten as written or as a template, but for one
    /// whose host is a variable; nothing else moves, spacing and line
    /// endings included.
    #[test]
    fn urls_are_rewritten_in_place() {
        let text = "!CRL_VERSION = 1.0\n# git@h.test:o/a.git\n!URL     = git@h.test:o/a.git\r\n!URL = git@h.test:o/$1.git\n\
                    !URL = git@other.test:o/b.git\n!URL = https://h.test/o/c\n!URL = git@$GH:o/$1.git\n";
        let rewrites = BTreeMap::from([("git@h.test:o/a.git".to_owned(), "https://h.test/o/a.git".to_owned())]);
        assert_eq!(
            rewrite_urls(text, &rewrites),
            "!CRL_VERSION = 1.0\n# git@h.test:o/a.git\n!URL     = https://h.test/o/a.git\r\n!URL = https://h.test/o/$1.git\n\
             !URL = git@other.test:o/b.git\n!URL = https://h.test/o/c\n!URL = git@$GH:o/$1.git\n"
        );
    }
}
