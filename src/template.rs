//! Literal `@NAME@` substitution engine and the `.py` variant calling
//! convention (spec §6.1–§6.3, D7). No expression evaluation, no `@ENV()@`.
//!
//! Substitution rules (§6.1, kind 1), applied in one left-to-right pass:
//! - `@NAME@` is replaced by the variable's canonical string value.
//! - `@@` collapses to a literal `@` and the result is never re-scanned, so
//!   `@@NAME@@` yields the literal `@NAME@`.
//! - An unknown `@NAME@` token is an error (fixes simfactory's `@QEUEUE@` bug).
//! - A lone `@` that is neither `@@` nor a well-formed token is an error.
//!
//! `.py` variants (§6.1, kind 2): `python3` runs the script with a fixed
//! preamble that reads one JSON object from stdin and binds every variable as
//! a module global (canonical string form), plus a `typed` dict carrying
//! native ints/bools. The script's stdout is the produced artifact.

// Consumed by the Phase-2/3 streams (CFG, SIM, TEST); unused until then.
#![allow(dead_code)]

use crate::Res;
use anyhow::{Context, anyhow, bail};
use indexmap::IndexMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// A substitution variable value. Every value has a canonical string form
/// (what `@NAME@` expands to in `.sh`/TOML templates); Int/Bool additionally
/// provide the typed companion for the `.py` convention (§6.1 `typed` dict).
#[derive(Debug, Clone, PartialEq)]
pub enum VarValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl VarValue {
    /// Canonical string form: `Str` verbatim, `Int` plain decimal,
    /// `Bool` `"1"`/`"0"` (matching the §6.3 `GPU` convention).
    pub fn canonical(&self) -> String {
        match self {
            VarValue::Str(s) => s.clone(),
            VarValue::Int(v) => v.to_string(),
            VarValue::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
        }
    }
}

impl From<&str> for VarValue {
    fn from(s: &str) -> Self {
        VarValue::Str(s.to_owned())
    }
}
impl From<String> for VarValue {
    fn from(s: String) -> Self {
        VarValue::Str(s)
    }
}
impl From<i64> for VarValue {
    fn from(v: i64) -> Self {
        VarValue::Int(v)
    }
}
impl From<u64> for VarValue {
    fn from(v: u64) -> Self {
        VarValue::Int(v as i64)
    }
}
impl From<bool> for VarValue {
    fn from(v: bool) -> Self {
        VarValue::Bool(v)
    }
}

/// The active variable set for one substitution context (§6.3). Variable names
/// are `UPPER_SNAKE` — the one identifier namespace that keeps underscores.
#[derive(Debug, Clone, Default)]
pub struct VarSet {
    vars: IndexMap<String, VarValue>,
}

/// Prepended to every `.py` variant (§6.1): binds each variable as a module
/// global in canonical-string form and exposes the typed companions as `typed`.
const PY_PREAMBLE: &str = "\
import sys as _cactup_sys, json as _cactup_json
_cactup_d = _cactup_json.load(_cactup_sys.stdin)
globals().update(_cactup_d[\"vars\"])
typed = _cactup_d[\"typed\"]
del _cactup_sys, _cactup_json, _cactup_d
";

impl VarSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: &str, value: impl Into<VarValue>) {
        self.vars.insert(name.to_owned(), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&VarValue> {
        self.vars.get(name)
    }

    /// Iterate the variables in insertion order (used to freeze the set into
    /// `restart.toml` — §9.3).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &VarValue)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v))
    }

    fn is_name_char(c: char) -> bool {
        c == '_' || c.is_ascii_uppercase() || c.is_ascii_digit()
    }

    /// Substitute every `@NAME@` token in `text` per the module-level rules.
    pub fn substitute(&self, text: &str) -> Res<String> {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.char_indices().peekable();

        while let Some((pos, c)) = chars.next() {
            if c != '@' {
                out.push(c);
                continue;
            }

            // `@@` → literal `@`, never re-scanned.
            if let Some(&(_, '@')) = chars.peek() {
                chars.next();
                out.push('@');
                continue;
            }

            // Otherwise this must open a well-formed `@NAME@` token.
            let mut name = String::new();
            let mut closed = false;
            while let Some(&(_, nc)) = chars.peek() {
                if nc == '@' {
                    chars.next();
                    closed = true;
                    break;
                }
                if Self::is_name_char(nc) {
                    name.push(nc);
                    chars.next();
                } else {
                    break;
                }
            }

            if !closed || name.is_empty() {
                bail!(
                    "stray '@' at byte {pos}: not an '@@' escape nor a well-formed @NAME@ token \
                     (write a literal '@' as '@@')"
                );
            }

            match self.vars.get(&name) {
                Some(v) => out.push_str(&v.canonical()),
                None => bail!("unknown substitution variable @{name}@"),
            }
        }

        Ok(out)
    }

    /// The JSON object handed to `.py` scripts on stdin: `vars` maps every
    /// variable to its canonical string; `typed` carries native ints/bools for
    /// the Int/Bool variables (§6.1 "Types").
    pub fn to_py_json(&self) -> serde_json::Value {
        let mut vars = serde_json::Map::new();
        let mut typed = serde_json::Map::new();
        for (name, value) in &self.vars {
            vars.insert(name.clone(), serde_json::Value::String(value.canonical()));
            match value {
                VarValue::Int(v) => {
                    typed.insert(name.clone(), serde_json::json!(v));
                }
                VarValue::Bool(b) => {
                    typed.insert(name.clone(), serde_json::json!(b));
                }
                VarValue::Str(_) => {}
            }
        }
        serde_json::json!({ "vars": vars, "typed": typed })
    }

    /// Run a `.py` variant per the §6.1 calling convention and return its
    /// stdout. The variable set travels as JSON on stdin (keeping values out
    /// of the process table and argv limits); a non-zero exit is an error
    /// surfacing the script's stderr.
    pub fn run_py_script(&self, script: &Path) -> Res<String> {
        let body = std::fs::read_to_string(script)
            .with_context(|| format!("Failed to read Python variant {}", script.display()))?;

        // The preamble + body run as one program from a temp file (stdin must
        // stay free for the JSON payload).
        let mut tmp = tempfile::NamedTempFile::with_suffix(".py")
            .context("Failed to create temporary file for Python variant")?;
        tmp.write_all(PY_PREAMBLE.as_bytes())?;
        tmp.write_all(body.as_bytes())?;
        tmp.flush()?;

        let mut child = Command::new("python3")
            .arg(tmp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to run python3 for {}", script.display()))?;

        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open python3 stdin"))?
            .write_all(self.to_py_json().to_string().as_bytes())
            .with_context(|| format!("Failed to send variables to {}", script.display()))?;

        let output = child
            .wait_with_output()
            .with_context(|| format!("Failed to wait for python3 running {}", script.display()))?;

        if !output.status.success() {
            bail!(
                "Python variant {} exited unsuccessfully ({}):\n{}",
                script.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim_end()
            );
        }

        String::from_utf8(output.stdout)
            .with_context(|| format!("{} produced non-UTF-8 output", script.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> VarSet {
        let mut v = VarSet::new();
        v.set("NODES", 4u64);
        v.set("QUEUE", "checkpt");
        v.set("GPU", false);
        v
    }

    #[test]
    fn substitutes_tokens() {
        let out = vars().substitute("srun -N @NODES@ -p @QUEUE@ gpu=@GPU@").unwrap();
        assert_eq!(out, "srun -N 4 -p checkpt gpu=0");
    }

    #[test]
    fn double_at_escapes_and_never_rescans() {
        assert_eq!(vars().substitute("user@@host").unwrap(), "user@host");
        assert_eq!(vars().substitute("@@NODES@@").unwrap(), "@NODES@");
        assert_eq!(vars().substitute("@@@NODES@").unwrap(), "@4");
    }

    #[test]
    fn unknown_token_and_stray_at_error() {
        assert!(vars().substitute("@QEUEUE@").is_err());
        assert!(vars().substitute("a @ b").is_err());
        assert!(vars().substitute("trailing@").is_err());
        assert!(vars().substitute("@lower@").is_err());
    }

    #[test]
    fn py_json_shape() {
        let json = vars().to_py_json();
        assert_eq!(json["vars"]["NODES"], "4");
        assert_eq!(json["typed"]["NODES"], 4);
        assert_eq!(json["typed"]["GPU"], false);
        assert!(json["typed"].get("QUEUE").is_none());
    }

    #[test]
    fn runs_py_variant() {
        // Skip gracefully where python3 isn't available.
        if Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 not found; skipping");
            return;
        }
        let mut f = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        writeln!(f, "print(f\"N={{NODES}} n={{typed['NODES'] + 1}} q={{QUEUE}}\")").unwrap();
        f.flush().unwrap();
        let out = vars().run_py_script(f.path()).unwrap();
        assert_eq!(out.trim(), "N=4 n=5 q=checkpt");
    }
}
