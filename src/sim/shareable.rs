//! What may go into a `.sim` that leaves this machine (spec §8.9): every
//! repo URL one anybody can fetch, and no URL that carries credentials.
//! `sim export` and `sim pack` check every repo and input URL before they
//! write one, unless `--local` says the file is for use here only.
//!
//! The rules are the shared crate's (`cactup_ecosystem_common::url`), so the
//! Cactarium refuses exactly what export does. A git repo's URL must be
//! [`Shareability::Shareable`]: no credentials or query string, no local
//! path, no ssh. An ssh remote is written as its https equivalent instead
//! when that answers with no credentials at all
//! ([`crate::fetch::anonymous`]), in the pins and in the thornlist alike,
//! since an import fetches what the thornlist names. A download's URL, or
//! another kind of repo's, is not asked about (it is not git) and may carry
//! a query (`?download=1` is how download links work); it is refused for
//! credentials and local paths only. So is an input's URL, for credentials.

use crate::fetch::anonymous::{self, Answer};
use crate::fetch::git::{AskWindow, REMOTE_ASK_TIMEOUT};
use crate::thornlist::{self, ComponentType};
use crate::Res;
use anyhow::{bail, Context};
use cactup_ecosystem_common::sim::RepoPin;
use cactup_ecosystem_common::url::{moved_refusal, shareability, Redacted, Shareability};
use std::collections::BTreeMap;

/// What every refusal ends with.
const LOCAL_HINT: &str = "or pass --local to export for use here only";

/// Make `repos` and the thornlist `text` they came from fit to leave this
/// machine, with `input_urls` (each input's KNOB and URL) checked too:
/// every ssh remote whose https equivalent answers anonymously is rewritten
/// to it (one line said for each), and anything that cannot be published
/// fails the call, every offending URL named in one error. The probes run
/// on the network fan-out within one [`REMOTE_ASK_TIMEOUT`], a line each;
/// Ctrl-C fails the call ("Interrupted").
pub fn make_shareable(repos: &mut [RepoPin], text: &mut String, input_urls: &[(String, String)]) -> Res<()> {
    let list = thornlist::parse(text).context("The thornlist does not parse")?;
    let mut refused = Vec::new();
    let mut asks = Vec::new();
    for pin in repos.iter() {
        let Some(c) = list.components().iter().find(|c| c.repo == pin.name) else { continue };
        if c.ty == ComponentType::Ignore || pin.url.trim().is_empty() {
            continue;
        }
        if c.ty != ComponentType::Git {
            if let Some(why) = download_refusal(&pin.url, true) {
                refused.push(format!("{}: {why}", pin.name));
            }
            continue;
        }
        match shareability(&pin.url) {
            Shareability::Shareable => {}
            Shareability::Ssh { https: Some(https) } => asks.push((pin.name.clone(), pin.url.clone(), https)),
            other => refused.push(format!("{}: {}", pin.name, other.refusal(&pin.url).expect("not shareable"))),
        }
    }
    for (knob, url) in input_urls {
        if let Some(why) = download_refusal(url, false) {
            refused.push(format!("input {knob}: {why}"));
        }
    }

    let answers = probe_all(&asks)?;
    let mut rewrites = BTreeMap::new();
    for ((name, ssh, https), answer) in asks.into_iter().zip(answers) {
        match answer {
            Answer::Answered => {
                rewrites.insert(name, (ssh, https));
            }
            Answer::Moved(to) => refused.push(format!("{name}: {}", moved_refusal(&ssh, &https, to.as_deref()))),
            Answer::Refused(how) => {
                let why = Shareability::Ssh { https: Some(https.clone()) }.refusal(&ssh).expect("not shareable");
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
    if rewrites.is_empty() {
        return Ok(());
    }

    let rewritten = rewrite_urls(text, &rewrites.values().cloned().collect());
    let check = thornlist::parse(&rewritten).context("The thornlist with its ssh URLs rewritten does not parse")?;
    let mut stuck = Vec::new();
    for c in check.components() {
        let Some(old) = list.components().iter().find(|o| o.checkout == c.checkout && o.repo == c.repo) else {
            stuck.push(c.checkout.clone());
            continue;
        };
        let want = rewrites.get(&old.repo).map(|(_, https)| Some(https.clone())).unwrap_or_else(|| old.url.clone());
        if c.url != want {
            stuck.push(c.checkout.clone());
        }
    }
    if !stuck.is_empty() || check.components().len() != list.components().len() {
        let names: Vec<String> =
            rewrites.iter().map(|(name, (ssh, https))| format!("{name}: {} as {https}", Redacted(ssh))).collect();
        bail!(
            "The thornlist's ssh URLs answer as https, but its !URL lines could not all be rewritten to them (a URL \
             built from a !DEFINE, perhaps; {}):\n  {}\nWrite the https URLs in the thornlist, {LOCAL_HINT}",
            if stuck.is_empty() { "the list changed shape".to_owned() } else { format!("for {}", stuck.join(", ")) },
            names.join("\n  ")
        );
    }
    *text = rewritten;
    for pin in repos.iter_mut() {
        if let Some((ssh, https)) = rewrites.get(&pin.name) {
            println!("{}: written as {https}, which anyone can fetch, in place of the ssh URL {}", pin.name, Redacted(ssh));
            pin.url = https.clone();
        }
    }
    Ok(())
}

/// Why `url`, an input's (when not `repo`) or a repo's that is not git,
/// cannot be published, if it cannot: credentials in it (a user name or
/// password), or, for a repo, a local path. A query string is no reason:
/// download links carry them.
pub fn download_refusal(url: &str, repo: bool) -> Option<String> {
    match shareability(without_query(url)) {
        s @ Shareability::Credentials => s.refusal(url),
        s @ Shareability::Local if repo => s.refusal(url),
        _ => None,
    }
}

/// [`download_refusal`] for an input's URL given at a prompt: the error, or
/// nothing.
pub fn check_input_url(knob: &str, url: &str) -> Res<()> {
    match download_refusal(url, false) {
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

/// Probe each https URL of `asks` (name, ssh URL, https URL) for an
/// anonymous answer, on the network fan-out within one window, with a
/// progress line for each.
fn probe_all(asks: &[(String, String, String)]) -> Res<Vec<Answer>> {
    if asks.is_empty() {
        return Ok(Vec::new());
    }
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let item = progress.add_child("ask ssh remotes' https URLs");
    item.init(Some(asks.len()), Some(prodash::unit::label("repos")));
    let item = std::sync::Mutex::new(item);
    let window = AskWindow::from(std::time::Instant::now(), REMOTE_ASK_TIMEOUT);
    let answers = crate::fetch::git::ask_all(asks, |(name, _, https)| {
        let current = item.lock().expect("probe progress poisoned").add_child(name.clone());
        let answer = anonymous::probe(https, window);
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

    /// Credentials are refused in a download's URL, and a local path in a
    /// repo's; a query string is not, nor is an ordinary URL.
    #[test]
    fn a_download_is_refused_for_credentials_not_for_a_query() {
        assert!(download_refusal("https://zenodo.org/records/1/files/id.tar.gz?download=1", false).is_none());
        assert!(download_refusal("https://example.org/f.h5", false).is_none());
        for url in ["https://max:s3cret@example.org/f.h5?download=1", "https://TOKEN@example.org/f.h5"] {
            let why = download_refusal(url, false).unwrap();
            assert!(why.contains("credentials") && !why.contains("s3cret") && !why.contains("TOKEN"), "{why}");
        }
        assert!(download_refusal("/data/f.h5", false).is_none(), "an input's local path fails on download");
        assert!(download_refusal("file:///srv/svn/r", true).unwrap().contains("local path"));
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
