//! Machine discovery mechanics (spec §4.3): local-hostname resolution and
//! running a machine's `discover.py` `is_machine(hostname)` via `python3`.
//!
//! Policy (caching in the global DB, disambiguation prompts, the `generic`
//! zero-match fallback) lives in the MACH stream's `commands/machine.rs`;
//! this module only answers "which machines claim this host".

use crate::Res;
use anyhow::{bail, Context};
use std::path::Path;
use std::process::Command;

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

/// One `python3` invocation evaluating `is_machine(hostname)` from
/// `discover.py`. Loading and calling are batched into a single spawn (§4.3's
/// cost note). Errors (missing python3, module raised, non-bool-able result)
/// are returned; the caller decides whether they mean "did not match".
pub fn is_machine(discover_py: &Path, hostname: &str) -> Res<bool> {
    const PROBE: &str = "\
import runpy, sys
mod = runpy.run_path(sys.argv[1])
sys.stdout.write('1' if mod['is_machine'](sys.argv[2]) else '0')
";
    let mut command = Command::new("python3");
    command.args(["-c", PROBE]).arg(discover_py).arg(hostname);
    crate::shell::trace_command(&command);
    let output = command
        .output()
        .with_context(|| format!("Failed to run python3 for {}", discover_py.display()))?;

    if !output.status.success() {
        bail!(
            "{} raised while evaluating is_machine({hostname:?}): {}",
            discover_py.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    match output.stdout.as_slice() {
        b"1" => Ok(true),
        b"0" => Ok(false),
        other => bail!(
            "unexpected output from {}: {:?}",
            discover_py.display(),
            String::from_utf8_lossy(other)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn have_python3() -> bool {
        Command::new("python3").arg("--version").output().is_ok()
    }

    fn mel5_discover() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb/mel5/discover.py")
    }

    #[test]
    fn mel5_matches_its_fqdn_and_short_name_only() {
        if !have_python3() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        assert!(is_machine(&mel5_discover(), "melete05.cct.lsu.edu").unwrap());
        assert!(is_machine(&mel5_discover(), "melete05").unwrap());
        assert!(!is_machine(&mel5_discover(), "mike.hpc.lsu.edu").unwrap());
    }

    #[test]
    fn generic_never_matches() {
        if !have_python3() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let generic = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb/generic/discover.py");
        assert!(!is_machine(&generic, "anything.at.all").unwrap());
    }

    #[test]
    fn raising_discover_is_an_error_not_a_match() {
        if !have_python3() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("discover.py");
        std::fs::write(&bad, "def is_machine(hostname):\n    raise RuntimeError('boom')\n").unwrap();
        let err = is_machine(&bad, "host").unwrap_err();
        assert!(format!("{err:#}").contains("boom"), "{err:#}");
    }

    #[test]
    fn hostname_override_wins() {
        assert_eq!(resolve_hostname(Some("forced.example")), "forced.example");
        assert!(!resolve_hostname(None).is_empty());
    }
}
