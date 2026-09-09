//! Machine discovery mechanics (spec §4.3): local-hostname resolution and the
//! two matchers a machine may ship — `hostname.regexp` (a Rust regex, tried
//! first) and `discover.py` (`is_machine(hostname)`, run via `python3` for
//! every machine that still needs it in a single interpreter).
//!
//! Policy (the verification stamp in the global DB, disambiguation prompts,
//! the `generic` zero-match fallback) lives in the MACH stream's
//! `commands/machine.rs`; this module only answers "does this machine claim
//! this host".

use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use regex::Regex;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Determine the hostname used for discovery: `--hostname` override →
/// `~/.hostname` → the system hostname (§4.3).
pub fn resolve_hostname(override_: Option<&str>) -> String {
    if let Some(h) = override_ {
        return h.to_owned();
    }
    if let Some(home_dir) = std::env::home_dir()
        && let Ok(contents) = std::fs::read_to_string(home_dir.join(".hostname"))
    {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The first label of a hostname: `mike1` for `mike1.hpc.lsu.edu`.
pub fn short_name(hostname: &str) -> &str {
    hostname.split('.').next().unwrap_or(hostname)
}

/// Load `<machine>/hostname.regexp`: the whole file, trimmed, is one pattern
/// (§4.3). There is no comment syntax — a `#` would be part of the regex.
pub fn load_regexp(path: &Path) -> Res<Regex> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let pattern = text.trim();
    if pattern.is_empty() {
        bail!("{} is empty", path.display());
    }
    Regex::new(pattern).with_context(|| format!("{} is not a valid regex", path.display()))
}

/// Does a `hostname.regexp` pattern claim `hostname`? Tested against the name
/// as resolved and then against its short form, so `^ln[1-4]$` claims both
/// `ln1` and `ln1.cosma.dur.ac.uk` — the same two probes the ported
/// simfactory patterns always made (§4.3).
pub fn regexp_claims(re: &Regex, hostname: &str) -> bool {
    re.is_match(hostname) || re.is_match(short_name(hostname))
}

/// The `hostname.regexp` verdict for one machine directory: `None` when the
/// file is absent, `Some(false)` when it is unreadable or not a regex (an
/// authoring bug — always warned about, since it silently hides a machine).
pub fn regexp_verdict(name: &str, dir: &Path, hostname: &str) -> Option<bool> {
    let path = dir.join("hostname.regexp");
    if !path.is_file() {
        return None;
    }
    match load_regexp(&path) {
        Ok(re) => Some(regexp_claims(&re, hostname)),
        Err(e) => {
            eprintln!("{}", format!("Warning: machine {name} is skipped in discovery: {e:#}").yellow());
            Some(false)
        }
    }
}

/// Evaluate `is_machine(hostname)` from every `discover.py` in `scripts`,
/// in **one** `python3` process (the interpreter start-up is the whole cost:
/// ~15 ms per spawn against well under a millisecond per module — §4.3's
/// cost note). Each script runs in its own namespace via `runpy`, its stdout
/// diverted to stderr so it cannot corrupt the protocol, and a script that
/// raises (or calls `sys.exit`) yields an `Err` for that one entry only.
///
/// A whole-batch failure (no `python3`, an interrupt, output that is not the
/// protocol) is the outer `Err`; the caller decides what a per-script error
/// means (discovery treats it as "did not match").
pub fn probe_all(scripts: &[PathBuf], hostname: &str) -> Res<Vec<Res<bool>>> {
    if scripts.is_empty() {
        return Ok(Vec::new());
    }
    // One line per script, `<index> 1|0|E<message>`, written to the real
    // stdout captured before any script can rebind it. `BaseException`
    // rather than `Exception`: a `sys.exit()` in one script must not end the
    // batch. Ctrl-C is the exception to that — it is ours to handle.
    const PROBE: &str = "\
import contextlib, runpy, sys
host = sys.argv[1]
out = sys.stdout
for i, path in enumerate(sys.argv[2:]):
    try:
        with contextlib.redirect_stdout(sys.stderr):
            mod = runpy.run_path(path)
            line = '1' if mod['is_machine'](host) else '0'
    except KeyboardInterrupt:
        raise
    except BaseException as e:
        line = 'E' + ('%s: %s' % (type(e).__name__, e)).replace('\\n', ' ')
    out.write('%d %s\\n' % (i, line))
";
    let mut command = Command::new("python3");
    command.args(["-c", PROBE]).arg(hostname).args(scripts);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::shell::trace_command(&command);
    let mut child = command.spawn().context("Failed to run python3 for discover.py")?;

    // Drain both pipes on threads so a chatty script cannot deadlock the
    // wait, and poll instead of blocking so Ctrl-C lands promptly
    // (interrupt contract): the child is killed, and the interrupt is an
    // error rather than "zero matches".
    let stdout = drain(child.stdout.take().expect("stdout piped"));
    let stderr = drain(child.stderr.take().expect("stderr piped"));
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if gix::interrupt::is_triggered() {
            let _ = child.kill();
            let _ = child.wait();
            bail!("interrupted");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if gix::interrupt::is_triggered() {
        bail!("interrupted");
    }
    if !status.success() {
        bail!(
            "python3 failed while evaluating discover.py scripts ({status}): {}",
            String::from_utf8_lossy(&stderr).trim()
        );
    }

    let mut results: Vec<Option<Res<bool>>> = (0..scripts.len()).map(|_| None).collect();
    for line in String::from_utf8_lossy(&stdout).lines() {
        let Some((index, verdict)) = line.split_once(' ') else { continue };
        let Some(slot) = index.parse::<usize>().ok().and_then(|i| results.get_mut(i)) else { continue };
        *slot = Some(match verdict {
            "1" => Ok(true),
            "0" => Ok(false),
            other => Err(anyhow::anyhow!(
                "{} raised while evaluating is_machine({hostname:?}): {}",
                scripts[index.parse::<usize>().unwrap_or(0)].display(),
                other.strip_prefix('E').unwrap_or(other)
            )),
        });
    }
    Ok(results
        .into_iter()
        .zip(scripts)
        .map(|(r, path)| {
            r.unwrap_or_else(|| Err(anyhow::anyhow!("no verdict from python3 for {}", path.display())))
        })
        .collect())
}

/// One `discover.py` against one hostname — `probe_all` for a single script.
pub fn is_machine(discover_py: &Path, hostname: &str) -> Res<bool> {
    probe_all(std::slice::from_ref(&discover_py.to_path_buf()), hostname)?
        .pop()
        .expect("one script, one verdict")
}

fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have_python3() -> bool {
        Command::new("python3").arg("--version").output().is_ok()
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn regexp_claims_fqdn_and_short_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hostname.regexp"), "^ln[1-4]$\n").unwrap();
        assert_eq!(regexp_verdict("m", dir.path(), "ln1"), Some(true));
        assert_eq!(regexp_verdict("m", dir.path(), "ln3.cosma.dur.ac.uk"), Some(true));
        assert_eq!(regexp_verdict("m", dir.path(), "ln5"), Some(false));
        assert_eq!(regexp_verdict("m", dir.path(), "xln1"), Some(false));
        assert_eq!(regexp_verdict("m", tempfile::tempdir().unwrap().path(), "ln1"), None);
    }

    #[test]
    fn invalid_or_empty_regexp_is_a_nonmatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hostname.regexp"), "^ln[1-4\n").unwrap();
        assert_eq!(regexp_verdict("m", dir.path(), "ln1"), Some(false));
        std::fs::write(dir.path().join("hostname.regexp"), "  \n").unwrap();
        assert_eq!(regexp_verdict("m", dir.path(), "ln1"), Some(false));
    }

    #[test]
    fn single_script_matches_fqdn_and_short_name_only() {
        if !have_python3() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let py = script(
            dir.path(),
            "discover.py",
            "def is_machine(hostname):\n    return hostname == 'melete05.cct.lsu.edu' or hostname.split('.')[0] == 'melete05'\n",
        );
        assert!(is_machine(&py, "melete05.cct.lsu.edu").unwrap());
        assert!(is_machine(&py, "melete05").unwrap());
        assert!(!is_machine(&py, "mike.hpc.lsu.edu").unwrap());
    }

    #[test]
    fn batch_isolates_raising_printing_and_exiting_scripts() {
        if !have_python3() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let yes = script(dir.path(), "yes.py", "def is_machine(h):\n    return True\n");
        let boom = script(dir.path(), "boom.py", "def is_machine(h):\n    raise RuntimeError('boom')\n");
        let noisy = script(
            dir.path(),
            "noisy.py",
            "print('chatter on stdout')\ndef is_machine(h):\n    print('more')\n    return False\n",
        );
        let exits = script(dir.path(), "exits.py", "import sys\nsys.exit(3)\n");
        let no_fn = script(dir.path(), "nofn.py", "x = 1\n");
        let results = probe_all(&[yes, boom, noisy, exits, no_fn], "host").unwrap();
        assert_eq!(results.len(), 5);
        assert!(results[0].as_ref().unwrap());
        let err = results[1].as_ref().unwrap_err();
        assert!(format!("{err:#}").contains("boom"), "{err:#}");
        assert!(!results[2].as_ref().unwrap());
        assert!(format!("{:#}", results[3].as_ref().unwrap_err()).contains("SystemExit"));
        assert!(format!("{:#}", results[4].as_ref().unwrap_err()).contains("KeyError"));
    }

    #[test]
    fn empty_batch_spawns_nothing() {
        assert!(probe_all(&[], "host").unwrap().is_empty());
    }

    #[test]
    fn hostname_override_wins() {
        assert_eq!(resolve_hostname(Some("forced.example")), "forced.example");
        assert!(!resolve_hostname(None).is_empty());
        assert_eq!(short_name("a.b.c"), "a");
        assert_eq!(short_name("bare"), "bare");
    }
}
