//! The Cactarium registry's HTTP API, from cactup's side (spec §8.10). The
//! request and response shapes are the shared crate's (`api`), the very
//! structs the server serves, so the two cannot drift apart silently; this
//! module is only the transport: where the registry is, the token, the
//! generation check, and turning an error body into the message it carries.
//!
//! Everything here runs on the login node, before anything is built or
//! submitted; nothing on a compute node's path calls it (D11).

use crate::database::Database;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use cactup_ecosystem_common::api::{
    ErrorBody, PackageVersions, Published, Refreshed, Resolved, API_GENERATION, API_GENERATION_HEADER, API_PREFIX,
    CONTACT_HEADER, SHA256_HEADER, UNPINNED_HEADER,
};
use cactup_ecosystem_common::stack::FrozenStack;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

/// Where the registry is when nothing says otherwise.
pub const DEFAULT_URL: &str = "https://cactarium.cct.lsu.edu";

/// The knob naming another registry (a test instance, a mirror).
pub const URL_KNOB: &str = "cactarium-url";

/// A connection to one registry: its base URL and, for writes, the token.
pub struct Client {
    base: String,
}

impl Client {
    /// The registry `CACTARIUM_URL` names, else the `cactarium-url` knob,
    /// else the public one.
    pub fn new(db: &Database) -> Client {
        let base = std::env::var("CACTARIUM_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .or_else(|| db.knob(URL_KNOB).map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_URL.to_owned());
        Client::at(&base)
    }

    pub fn at(base: &str) -> Client {
        Client { base: base.trim_end_matches('/').to_owned() }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{API_PREFIX}{path}", self.base)
    }

    /// The newest release (or `@head`/`@master`, or an exact version)
    /// matching `req`, which is `owner/name[@…]` as a user writes it.
    pub fn resolve(&self, req: &str) -> Res<Resolved> {
        let url = format!("{}?req={}", self.url("/resolve"), query_escape(req));
        json(self.send(crate::fetch::download::client().get(&url))?)
    }

    /// A stack release (`version` exact, or `head`/`master`): its thornlist
    /// and pins, whether the server marked it unpinned, and where to report
    /// problems with it (its declared contact, else its repo).
    pub fn stack_release(&self, owner: &str, name: &str, version: &str) -> Res<(FrozenStack, bool, Option<String>)> {
        let url = self.url(&format!("/stacks/{owner}/{name}/releases/{version}"));
        let response = self.send(crate::fetch::download::client().get(&url))?;
        let unpinned = response.headers().get(UNPINNED_HEADER).is_some_and(|v| v.as_bytes() == b"true");
        let contact = response
            .headers()
            .get(CONTACT_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        Ok((json(response)?, unpinned, contact))
    }

    pub fn package_versions(&self, owner: &str, name: &str) -> Res<PackageVersions> {
        let url = self.url(&format!("/packages/{owner}/{name}/versions"));
        json(self.send(crate::fetch::download::client().get(&url))?)
    }

    /// Download sim file `owner/name@number` to `dest`, checked against the
    /// hash the server sends with it.
    pub fn download_sim(&self, owner: &str, name: &str, number: u32, dest: &Path) -> Res<()> {
        let url = self.url(&format!("/sims/{owner}/{name}/{number}/download"));
        let response = self.send(crate::fetch::download::client().get(&url))?;
        let expected = response.headers().get(SHA256_HEADER).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let bytes = response.bytes().with_context(|| format!("Failed to download {url}"))?;
        if let Some(expected) = expected {
            let actual = sha256_hex(&bytes);
            if !actual.eq_ignore_ascii_case(&expected) {
                bail!("The download of {owner}/{name}@{number} has SHA-256 {actual}, but the registry says {expected}");
            }
        }
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
        }
        std::fs::write(dest, &bytes).with_context(|| format!("Failed to write '{}'", dest.display()))
    }

    /// Publish a `.sim` file's bytes as the next version of sim `owner/name`.
    pub fn publish_sim(&self, owner: &str, name: &str, body: Vec<u8>) -> Res<Published> {
        let token = token_for(&self.base)?;
        let request = crate::fetch::download::client()
            .post(self.url(&format!("/sims/{owner}/{name}")))
            .bearer_auth(token)
            .header("content-type", "application/toml")
            .body(body);
        json(self.send(request)?)
    }

    /// Publish a stack release.
    pub fn publish_stack(
        &self,
        owner: &str,
        name: &str,
        request: &cactup_ecosystem_common::api::ReleaseRequest,
    ) -> Res<Published> {
        let token = token_for(&self.base)?;
        let body = serde_json::to_vec(request).context("Failed to encode the release request")?;
        let request = crate::fetch::download::client()
            .post(self.url(&format!("/stacks/{owner}/{name}/releases")))
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(body);
        json(self.send(request)?)
    }

    /// Have the registry check package (or repo-kept stack) `owner/name`'s
    /// repository for new release tags now, rather than at its next poll.
    pub fn refresh(&self, owner: &str, name: &str) -> Res<Refreshed> {
        let token = token_for(&self.base)?;
        let request = crate::fetch::download::client()
            .post(self.url(&format!("/packages/{owner}/{name}/refresh")))
            .bearer_auth(token);
        json(self.send(request)?)
    }

    /// Send, then refuse a registry newer than this cactup speaks, and turn
    /// an error status into the message the registry wrote for it.
    fn send(&self, request: reqwest::blocking::RequestBuilder) -> Res<reqwest::blocking::Response> {
        let response = request
            .header(API_GENERATION_HEADER, API_GENERATION.to_string())
            .send()
            .with_context(|| format!("Failed to reach the Cactarium at {}", self.base))?;
        if let Some(generation) = response.headers().get(API_GENERATION_HEADER) {
            let generation: u32 = generation.to_str().ok().and_then(|g| g.parse().ok()).unwrap_or(0);
            if generation > API_GENERATION {
                bail!(
                    "The Cactarium at {} speaks API generation {generation}, newer than this cactup's \
                     {API_GENERATION}; run `cactup update`",
                    self.base
                );
            }
        }
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let text = response.text().unwrap_or_default();
        // A missing or dead token: say how to get a working one.
        let login = if status == reqwest::StatusCode::UNAUTHORIZED {
            format!(" (run `cactup login` with a token from {}/settings)", self.base)
        } else {
            String::new()
        };
        match serde_json::from_str::<ErrorBody>(&text) {
            Ok(body) => Err(anyhow!("{}{login}", body.error.message)),
            Err(_) => Err(anyhow!("The Cactarium at {} answered {status}", self.base)),
        }
    }
}

/// A stack release for `req` (`owner/stack[@range|version|head|master]`):
/// a range resolved once to the exact release, and that release's thornlist
/// with every repo it pins pinned by `!REPO_COMMIT`.
pub struct StackRelease {
    /// The exact slug: `owner/stack@1.2.0`, or the `@head`/`@master` form.
    pub slug: String,
    /// The thornlist, pinned.
    pub text: String,
    /// The registry marked it unpinned (`@head`/`@master`).
    pub unpinned: bool,
    /// Where to report problems with the stack.
    pub contact: Option<String>,
}

/// See [`StackRelease`].
pub fn stack_release_text(db: &Database, req: &str) -> Res<StackRelease> {
    use cactup_ecosystem_common::slug::Slug;
    let slug: Slug = req.parse().map_err(|e| anyhow!("\"{req}\" is not a stack (owner/name[@version]): {e}"))?;
    let client = Client::new(db);
    let version = match slug.tip() {
        Some(_) => slug.at.clone().expect("a tip is an @ form"),
        None => {
            let resolved = client.resolve(req)?;
            if resolved.kind != "stack" {
                bail!("{} is a {}, not a stack", resolved.slug, resolved.kind);
            }
            resolved.version.version
        }
    };
    let (stack, unpinned, contact) = client.stack_release(slug.owner.as_str(), slug.name.as_str(), &version)?;
    let pin_failed = || format!("Failed to pin stack {}", stack.stack);
    let (pins, unchecked) = crate::thornlist::recorded_pins(&stack.thornlist.text, &stack.repos).with_context(pin_failed)?;
    if !unchecked.is_empty() {
        println!(
            "{}",
            format!(
                "Downloaded without checking their recorded hash, which names no single file: {}",
                unchecked.join(", ")
            )
            .yellow()
        );
    }
    let text = crate::thornlist::pin_text(&stack.thornlist.text, &pins).with_context(pin_failed)?;
    Ok(StackRelease { slug: stack.stack.to_string(), text, unpinned, contact })
}

fn json<T: DeserializeOwned>(response: reqwest::blocking::Response) -> Res<T> {
    let url = response.url().to_string();
    let text = response.text().with_context(|| format!("Failed to read the answer from {url}"))?;
    serde_json::from_str(&text).with_context(|| format!("The Cactarium's answer from {url} is not what cactup expects"))
}

/// `req` as a query-string value: everything outside the unreserved set
/// percent-encoded (a range's `^`, `:` and `@` included).
fn query_escape(req: &str) -> String {
    req.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// `~/.cactup/credentials`: one API token per registry, readable only by
/// its owner. Tokens are never knobs — knob values are frozen into every
/// restart's metadata, and a token there would travel with every simulation.
pub fn credentials_path() -> PathBuf {
    crate::CACTUP_ROOT.join("credentials")
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Credentials {
    /// Registry base URL → token.
    #[serde(default)]
    tokens: std::collections::BTreeMap<String, String>,
}

fn read_credentials(path: &Path) -> Res<Credentials> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("Failed to parse '{}'", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
        Err(e) => Err(e).with_context(|| format!("Failed to read '{}'", path.display())),
    }
}

/// The token for `base`: `CACTARIUM_TOKEN`, else the credentials file.
pub fn token_for(base: &str) -> Res<String> {
    if let Ok(token) = std::env::var("CACTARIUM_TOKEN")
        && !token.is_empty()
    {
        return Ok(token);
    }
    read_credentials(&credentials_path())?
        .tokens
        .remove(base)
        .ok_or_else(|| anyhow!("No token for the Cactarium at {base}; run `cactup login` first"))
}

/// Store `token` for `base` in `path`, written whole under a temporary name
/// created owner-only, then renamed into place.
pub fn store_token(path: &Path, base: &str, token: &str) -> Res<()> {
    let mut credentials = read_credentials(path)?;
    credentials.tokens.insert(base.to_owned(), token.to_owned());
    let dir = path.parent().expect("the credentials file has a directory");
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create '{}'", dir.display()))?;
    // tempfile creates its files 0600.
    let temp = tempfile::NamedTempFile::new_in(dir).context("Failed to create a temporary credentials file")?;
    std::fs::write(temp.path(), toml::to_string(&credentials)?)
        .with_context(|| format!("Failed to write '{}'", temp.path().display()))?;
    temp.persist(path).with_context(|| format!("Failed to save '{}'", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::download::test_server::serve_with;

    #[test]
    fn resolve_sends_the_request_escaped_and_reads_the_answer() {
        let answer = r#"{"slug":"max-morris/carpetx@mixed-precision:1.4.0","kind":"package","url":"https://x/carpetx.git",
            "version":"1.4.0","track":"mixed-precision","tag":"mixed-precision/v1.4.0","commit":"3f2c","tree":null,
            "yanked":false,"yank-reason":null,"state":"published","curated":false,"published":0}"#;
        let (base, heads) = serve_with(1, move |_| ("200 OK", "api-generation: 1\r\n".into(), answer.as_bytes().to_vec()));
        let resolved = Client::at(&base).resolve("max-morris/carpetx@mixed-precision:^1.2").unwrap();
        assert_eq!(resolved.version.tag.as_deref(), Some("mixed-precision/v1.4.0"));
        let head = &heads.join().unwrap()[0];
        assert!(head.starts_with("GET /api/v1/resolve?req=max-morris/carpetx%40mixed-precision%3A%5E1.2 "), "{head}");
        assert!(head.to_ascii_lowercase().contains("api-generation: 1"), "{head}");
    }

    #[test]
    fn errors_carry_the_registrys_message_and_newer_generations_are_refused() {
        let body = r#"{"error":{"code":"not-found","message":"No sim mmorris/nope"}}"#;
        let (base, _) = serve_with(1, move |_| ("404 Not Found", String::new(), body.as_bytes().to_vec()));
        let err = format!("{:#}", Client::at(&base).resolve("mmorris/nope").unwrap_err());
        assert_eq!(err, "No sim mmorris/nope");

        let (base, _) = serve_with(1, |_| ("200 OK", "api-generation: 99\r\n".into(), b"{}".to_vec()));
        let err = format!("{:#}", Client::at(&base).resolve("a/b").unwrap_err());
        assert!(err.contains("generation 99") && err.contains("cactup update"), "{err}");
    }

    #[test]
    fn a_sim_download_is_checked_against_the_registrys_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let good = sha256_hex(b"format = 1\n");
        let (base, _) = serve_with(2, move |path| {
            let hash = if path.contains("/1/") { good.clone() } else { "0".repeat(64) };
            ("200 OK", format!("x-sha256: {hash}\r\n"), b"format = 1\n".to_vec())
        });
        let client = Client::at(&base);
        client.download_sim("o", "n", 1, &tmp.path().join("ok.sim")).unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path().join("ok.sim")).unwrap(), "format = 1\n");
        let err = format!("{:#}", client.download_sim("o", "n", 2, &tmp.path().join("bad.sim")).unwrap_err());
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!tmp.path().join("bad.sim").exists());
    }

    #[test]
    fn tokens_are_stored_per_registry_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials");
        store_token(&path, "https://a", "cactarium_one").unwrap();
        store_token(&path, "https://b", "cactarium_two").unwrap();
        let credentials = read_credentials(&path).unwrap();
        assert_eq!(credentials.tokens["https://a"], "cactarium_one");
        assert_eq!(credentials.tokens["https://b"], "cactarium_two");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
