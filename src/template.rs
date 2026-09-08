//! Literal `@NAME@` substitution engine and the `.py` variant calling
//! convention (spec §6.1–§6.3, D7). No expression evaluation; the computed
//! token forms are `@ENV(…)@` and `@KNOB(…)@` with their `-OPTIONAL` variants.
//!
//! Substitution rules (§6.1, kind 1), applied in one left-to-right pass:
//! - `@NAME@` is replaced by the variable's canonical string value.
//! - `@ENV(NAME)@` is replaced by the environment variable `NAME`, read at
//!   substitution time; unset or empty is a hard error, never an empty splice.
//! - `@ENV-OPTIONAL(NAME)@` tolerates an unset/empty `NAME` and expands to
//!   the empty string; `@ENV-OPTIONAL(NAME, default)@` expands to `default`
//!   instead. The default is `"double-quoted"`, `'single-quoted'` (quotes
//!   dropped, `\"`/`\'`/`\\` escape) or a bare run of letters and digits.
//! - `@KNOB(name)@`, `@KNOB-OPTIONAL(name)@`, `@KNOB-OPTIONAL(name, default)@`
//!   are the same three forms over the knob snapshot the set carries (§5) —
//!   the effective knob values as of submit time, `-K` overlay included.
//! - `@@` collapses to a literal `@` and the result is never re-scanned, so
//!   `@@NAME@@` yields the literal `@NAME@`.
//! - An unknown `@NAME@` token is an error (fixes simfactory's `@QEUEUE@` bug).
//! - A lone `@` that is neither `@@` nor a well-formed token is an error.
//!
//! `.py` variants (§6.1, kind 2): `python3` runs the script with a fixed
//! preamble that reads one JSON object from stdin and binds every variable as
//! a module global (canonical string form), plus a `typed` dict carrying
//! native ints/bools and a `knobs` dict (with a `knob(name, default)` helper
//! mirroring the `@KNOB@` forms). The script's stdout is the produced artifact.

// Consumed by the Phase-2/3 streams (CFG, SIM, TEST); unused until then.

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
///
/// `knobs` is the effective knob snapshot `@KNOB(…)@` reads (§5, §6.1):
/// `None` in a context that has no knobs at all (MDB `[paths]` resolution,
/// ad-hoc sets), where any `@KNOB@` token is an error rather than "unset".
#[derive(Debug, Clone, Default)]
pub struct VarSet {
    vars: IndexMap<String, VarValue>,
    knobs: Option<IndexMap<String, String>>,
}

/// Prepended to every `.py` variant (§6.1): binds each variable as a module
/// global in canonical-string form and exposes the typed companions as
/// `typed`, the knob snapshot as `knobs`, and a `knob()` accessor.
/// Everything after this marker on stderr is a script's own refusal message,
/// reported to the user verbatim instead of as a Python crash (§6.1).
const PY_ERROR_MARKER: &str = "cactup-script-error:";

const PY_PREAMBLE: &str = "\
import sys as _cactup_sys, json as _cactup_json
_cactup_d = _cactup_json.load(_cactup_sys.stdin)
globals().update(_cactup_d[\"vars\"])
typed = _cactup_d[\"typed\"]
knobs = _cactup_d[\"knobs\"]

class CactupError(Exception):
    \"\"\"Raise to refuse the run with a message shown to the user.

    For a request this machine cannot serve — a scheduler rule the topology
    violates, an unsupported combination of variables. cactup prints the
    message and stops; nothing is submitted. Any OTHER exception is treated
    as a bug in the script and reported with its full traceback.
    \"\"\"

_cactup_required = object()

def knob(name, default=_cactup_required):
    \"\"\"The knob's value, like @KNOB(name)@ / @KNOB-OPTIONAL(name, default)@.

    With no default, an unset or empty knob refuses the run (CactupError).
    With one, it is returned instead.
    \"\"\"
    value = knobs.get(name, \"\")
    if value != \"\":
        return value
    if default is _cactup_required:
        raise CactupError(
            f\"knob {name} is unset or empty; set it with `cactup knob {name} VALUE` \"
            f\"(`-c` first for a custom knob) or pass -K {name}=VALUE\"
        )
    return default

def _cactup_excepthook(kind, exc, tb):
    if issubclass(kind, CactupError):
        _cactup_sys.stderr.write(\"cactup-script-error:\" + str(exc))
    else:
        import traceback as _tb
        _tb.print_exception(kind, exc, tb, file=_cactup_sys.stderr)
_cactup_sys.excepthook = _cactup_excepthook
del _cactup_json, _cactup_d
";

/// Where a computed token (§6.1) gets its value: the process environment or
/// the set's knob snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Env,
    Knob,
}

impl Source {
    fn func(self) -> &'static str {
        match self {
            Source::Env => "ENV",
            Source::Knob => "KNOB",
        }
    }

    /// Bytes an argument may consist of: `UPPER_SNAKE` for an environment
    /// variable, the kebab-case knob alphabet for a knob (§5).
    fn is_arg_byte(self, b: u8) -> bool {
        match self {
            Source::Env => VarSet::is_name_byte(b),
            Source::Knob => b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-',
        }
    }

    fn check_arg(self, arg: &str, at: usize, form: &str) -> Res<()> {
        match self {
            Source::Env if arg.is_empty() => {
                bail!("malformed @{form}(NAME)@ token at byte {at} (NAME must be UPPER_SNAKE)")
            }
            Source::Env => Ok(()),
            Source::Knob => crate::database::validate_knob_name(arg)
                .with_context(|| format!("malformed @{form}(name)@ token at byte {at}")),
        }
    }

    /// "environment variable FOO" / "knob foo" — for messages.
    fn what(self, arg: &str) -> String {
        match self {
            Source::Env => format!("environment variable {arg}"),
            Source::Knob => format!("knob {arg}"),
        }
    }

    /// How to supply a missing required value — appended to the error.
    fn hint(self, arg: &str) -> String {
        match self {
            Source::Env => String::new(),
            Source::Knob => format!(
                "; set it with `cactup knob {arg} VALUE` (`-c` first for a new custom knob) or \
                 pass -K {arg}=VALUE for this command"
            ),
        }
    }
}

/// A byte cursor over the template text. Every delimiter the grammar cares
/// about (`@ ( ) , " ' \`) is ASCII, so scanning bytes is safe: cuts only
/// ever land on ASCII bytes, never inside a multi-byte character.
struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_str(&mut self, s: &str) -> bool {
        if self.rest().starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
            self.pos += 1;
        }
    }

    fn take_while(&mut self, f: impl Fn(u8) -> bool) -> &'a str {
        let start = self.pos;
        while self.peek().is_some_and(&f) {
            self.pos += 1;
        }
        &self.text[start..self.pos]
    }
}

/// Parse the default value of an `-OPTIONAL` token (§6.1): `"…"` or `'…'`
/// with `\"`, `\'` and `\\` escapes (any other backslash is literal), or a
/// bare run of ASCII letters and digits. `cur` sits on its first byte.
fn parse_default(cur: &mut Cursor<'_>, at: usize, form: &str, arg: &str) -> Res<String> {
    match cur.peek() {
        Some(quote @ (b'"' | b'\'')) => {
            cur.bump();
            let mut bytes = Vec::new();
            loop {
                match cur.bump() {
                    None => bail!(
                        "unterminated quoted default value in @{form}({arg}, …)@ at byte {at}"
                    ),
                    Some(b'\\') => match cur.bump() {
                        Some(c @ (b'"' | b'\'' | b'\\')) => bytes.push(c),
                        Some(c) => {
                            bytes.push(b'\\');
                            bytes.push(c);
                        }
                        None => bail!(
                            "unterminated quoted default value in @{form}({arg}, …)@ at byte {at}"
                        ),
                    },
                    Some(b) if b == quote => break,
                    Some(b) => bytes.push(b),
                }
            }
            // Only ASCII bytes were ever removed from valid UTF-8, so the rest
            // is still valid UTF-8.
            String::from_utf8(bytes).context("default value is not valid UTF-8")
        }
        _ => {
            let bare = cur.take_while(|b| b.is_ascii_alphanumeric());
            // Whatever follows a bare default must close the token (trailing
            // blanks allowed); anything else is a character that needed
            // quoting — a blank included, when more text follows it.
            let next = cur.rest().chars().next();
            let closes = match next {
                Some(')') => true,
                Some(' ' | '\t') => {
                    let mut probe = Cursor { text: cur.text, pos: cur.pos };
                    probe.skip_ws();
                    probe.peek() == Some(b')')
                }
                _ => false,
            };
            if !closes {
                match next {
                    None => bail!(
                        "malformed @{form}({arg}, …)@ token at byte {at}: expected ')@' to close it"
                    ),
                    Some(c) => bail!(
                        "unquoted default value in @{form}({arg}, …)@ at byte {at} contains \
                         '{c}'; only letters and digits may go unquoted — quote it: \
                         @{form}({arg}, \"…\")@"
                    ),
                }
            }
            if bare.is_empty() {
                bail!(
                    "empty default value in @{form}({arg}, )@ at byte {at}: write \"\" for an \
                     empty default, or drop the comma"
                );
            }
            Ok(bare.to_owned())
        }
    }
}

impl VarSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: &str, value: impl Into<VarValue>) {
        self.vars.insert(name.to_owned(), value.into());
    }

    // Pinned foundation API; production code substitutes whole templates,
    // tests inspect individual values.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, name: &str) -> Option<&VarValue> {
        self.vars.get(name)
    }

    /// Iterate the variables in insertion order (used to freeze the set into
    /// `restart.toml` — §9.3).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &VarValue)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Attach the effective knob snapshot `@KNOB(…)@` reads (§5, §6.1) —
    /// `Database::knob_snapshot()` at submit time, or the table thawed from
    /// the restart/build/test metadata on the compute node (D11).
    pub fn set_knobs(&mut self, knobs: IndexMap<String, String>) {
        self.knobs = Some(knobs);
    }

    /// The knob snapshot, if this set carries one (frozen alongside the
    /// variables — §9.3).
    pub fn knobs(&self) -> Option<&IndexMap<String, String>> {
        self.knobs.as_ref()
    }

    fn is_name_byte(b: u8) -> bool {
        b == b'_' || b.is_ascii_uppercase() || b.is_ascii_digit()
    }

    /// Substitute every `@NAME@` token in `text` per the module-level rules.
    pub fn substitute(&self, text: &str) -> Res<String> {
        self.substitute_impl(text, false)
    }

    /// Dry-run `text` against this set: every error `substitute` would raise
    /// — a stray `@`, an unknown variable, a malformed token, a required
    /// knob that is unset — except an unset `@ENV(…)@`, which is judged on
    /// the environment the real substitution will run in (a compute node's,
    /// typically — §6.2), not this one's. Lets `sim submit` reject a parfile
    /// before the job is queued rather than hours later.
    pub fn check(&self, text: &str) -> Res<()> {
        self.substitute_impl(text, true).map(drop)
    }

    fn substitute_impl(&self, text: &str, env_lenient: bool) -> Res<String> {
        let mut out = String::with_capacity(text.len());
        let mut cur = Cursor { text, pos: 0 };

        loop {
            // Copy everything up to the next `@` verbatim.
            let rest = cur.rest();
            let Some(off) = rest.find('@') else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..off]);
            cur.pos += off;
            let at = cur.pos;
            cur.bump();

            // `@@` → literal `@`, never re-scanned.
            if cur.eat(b'@') {
                out.push('@');
                continue;
            }

            let name = cur.take_while(Self::is_name_byte);

            // `@NAME@` — a variable.
            if cur.eat(b'@') {
                if name.is_empty() {
                    bail!(
                        "stray '@' at byte {at}: not an '@@' escape nor a well-formed @NAME@ \
                         token (write a literal '@' as '@@')"
                    );
                }
                match self.vars.get(name) {
                    Some(v) => out.push_str(&v.canonical()),
                    None => bail!("unknown substitution variable @{name}@"),
                }
                continue;
            }

            // `@ENV(…)@` / `@KNOB(…)@` and their `-OPTIONAL` forms — the
            // computed tokens (§6.1).
            let source = match name {
                "ENV" => Some(Source::Env),
                "KNOB" => Some(Source::Knob),
                _ => None,
            };
            if let Some(source) = source
                && matches!(cur.peek(), Some(b'(') | Some(b'-'))
            {
                let value = self.computed_token(&mut cur, at, source, env_lenient)?;
                out.push_str(&value);
                continue;
            }

            bail!(
                "stray '@' at byte {at}: not an '@@' escape nor a well-formed @NAME@ token \
                 (write a literal '@' as '@@')"
            );
        }

        Ok(out)
    }

    /// Parse and resolve one computed token (§6.1) — `cur` sits right after
    /// the `ENV`/`KNOB` word, `at` is the opening `@`'s byte offset (for
    /// error messages). Consumes through the closing `@`. With
    /// `env_lenient` (see [`Self::check`]) an unset required `@ENV(…)@`
    /// expands empty instead of failing.
    fn computed_token(
        &self,
        cur: &mut Cursor<'_>,
        at: usize,
        source: Source,
        env_lenient: bool,
    ) -> Res<String> {
        let func = source.func();
        let optional = cur.eat(b'-');
        if optional && !cur.eat_str("OPTIONAL") {
            bail!(
                "malformed @{func}-…@ token at byte {at}: the only suffix is -OPTIONAL \
                 (@{func}-OPTIONAL(…)@)"
            );
        }
        let form = if optional { format!("{func}-OPTIONAL") } else { func.to_owned() };
        if !cur.eat(b'(') {
            bail!("malformed @{form}(…)@ token at byte {at}: expected '(' after {form}");
        }

        cur.skip_ws();
        let arg = cur.take_while(|b| source.is_arg_byte(b));
        source.check_arg(arg, at, &form)?;
        cur.skip_ws();

        let default = if cur.eat(b',') {
            if !optional {
                bail!(
                    "@{func}({arg})@ at byte {at} takes no default value — the required form \
                     fails when {} is unset; write @{func}-OPTIONAL({arg}, …)@ to fall back \
                     to a default",
                    source.what(arg)
                );
            }
            cur.skip_ws();
            let d = parse_default(cur, at, &form, arg)?;
            cur.skip_ws();
            Some(d)
        } else {
            None
        };

        if !cur.eat(b')') || !cur.eat(b'@') {
            bail!("malformed @{form}({arg}…)@ token at byte {at}: expected ')@' to close it");
        }

        let value = match source {
            Source::Env => std::env::var(arg).ok(),
            Source::Knob => {
                let knobs = self.knobs.as_ref().ok_or_else(|| {
                    anyhow!(
                        "@{form}({arg})@ at byte {at}: knob values are not available in this \
                         context (knobs can be read in scripts, optionlists and parfiles, not \
                         in machine paths)"
                    )
                })?;
                knobs.get(arg).cloned()
            }
        };
        match value.filter(|v| !v.is_empty()) {
            Some(v) => Ok(v),
            None if optional => Ok(default.unwrap_or_default()),
            None if env_lenient && source == Source::Env => Ok(String::new()),
            None => bail!("@{func}({arg})@: {} is unset or empty{}", source.what(arg), source.hint(arg)),
        }
    }

    /// The JSON object handed to `.py` scripts on stdin: `vars` maps every
    /// variable to its canonical string; `typed` carries native ints/bools for
    /// the Int/Bool variables (§6.1 "Types"); `knobs` is the knob snapshot
    /// (empty when the context carries none).
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
        let knobs: serde_json::Map<String, serde_json::Value> = self
            .knobs
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        serde_json::json!({ "vars": vars, "typed": typed, "knobs": knobs })
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

        let mut command = Command::new("python3");
        command
            .arg(tmp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::shell::trace_command(&command);
        let mut child = command
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
            let stderr = String::from_utf8_lossy(&output.stderr);
            // A `raise CactupError(...)` is the script refusing the run on
            // purpose (§6.1): report its message as the error, with no Python
            // wrapping — the user asked cactup for something this machine
            // cannot do, which is not a crash.
            if let Some((_, message)) = stderr.split_once(PY_ERROR_MARKER) {
                bail!("{} ({})", message.trim(), script.display());
            }
            bail!(
                "Python variant {} exited unsuccessfully ({}):\n{}",
                script.display(),
                output.status,
                stderr.trim_end()
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
    fn env_token_reads_the_environment() {
        // PATH is set and non-empty in any sane environment.
        let path = std::env::var("PATH").unwrap();
        assert_eq!(vars().substitute("p=@ENV(PATH)@!").unwrap(), format!("p={path}!"));
        // Unset (and empty — same match arm) env vars are a hard error,
        // never an empty splice.
        let err =
            vars().substitute("@ENV(CACTUP_TEST_SURELY_UNSET)@").unwrap_err().to_string();
        assert!(err.contains("CACTUP_TEST_SURELY_UNSET"), "{err}");
        // Malformed forms are errors, not pass-through.
        assert!(vars().substitute("@ENV()@").is_err());
        assert!(vars().substitute("@ENV(lower)@").is_err());
        assert!(vars().substitute("@ENV(PATH@").is_err());
        // Without parens, ENV is an ordinary (here unknown) variable.
        assert!(vars().substitute("@ENV@").is_err());
    }

    /// `vars()` plus a knob snapshot: one standard, one custom, one empty.
    fn vars_with_knobs() -> VarSet {
        let mut v = vars();
        let mut knobs = IndexMap::new();
        knobs.insert("allocation".to_owned(), "hpc_xxx".to_owned());
        knobs.insert("kadath-initial-data".to_owned(), "/scratch/id/bhns.info".to_owned());
        knobs.insert("email".to_owned(), String::new());
        v.set_knobs(knobs);
        v
    }

    #[test]
    fn env_optional_tolerates_unset() {
        let v = vars();
        let path = std::env::var("PATH").unwrap();
        // Set: identical to the required form, default ignored.
        assert_eq!(v.substitute("@ENV-OPTIONAL(PATH)@").unwrap(), path);
        assert_eq!(v.substitute("@ENV-OPTIONAL(PATH, nope)@").unwrap(), path);
        // Unset: empty, or the default.
        assert_eq!(v.substitute("[@ENV-OPTIONAL(CACTUP_TEST_SURELY_UNSET)@]").unwrap(), "[]");
        assert_eq!(
            v.substitute("@ENV-OPTIONAL(CACTUP_TEST_SURELY_UNSET, fallback)@").unwrap(),
            "fallback"
        );
        // Whitespace around the pieces is tolerated.
        assert_eq!(v.substitute("@ENV-OPTIONAL( CACTUP_TEST_SURELY_UNSET , 42 )@").unwrap(), "42");
        // The required form takes no default — that would silently defeat it.
        let err = v.substitute("@ENV(CACTUP_TEST_SURELY_UNSET, x)@").unwrap_err().to_string();
        assert!(err.contains("takes no default"), "{err}");
        // Only -OPTIONAL is a suffix.
        assert!(v.substitute("@ENV-MAYBE(PATH)@").is_err());
        assert!(v.substitute("@ENV-OPTIONAL PATH)@").is_err());
        assert!(v.substitute("@ENV-OPTIONAL(PATH, x@").is_err());
    }

    #[test]
    fn default_values_quoted_and_bare() {
        let v = vars();
        let t = |d: &str| v.substitute(&format!("@ENV-OPTIONAL(CACTUP_TEST_SURELY_UNSET, {d})@"));
        // Quotes are dropped; either kind works and may hold anything.
        assert_eq!(t(r#""/path with spaces/x.info""#).unwrap(), "/path with spaces/x.info");
        assert_eq!(t("'a, b) c'").unwrap(), "a, b) c");
        assert_eq!(t(r#""it's""#).unwrap(), "it's");
        assert_eq!(t(r#""ünïcödé →""#).unwrap(), "ünïcödé →");
        // Escapes: \" \' \\ inside quotes; any other backslash is literal.
        assert_eq!(t(r#""say \"hi\"""#).unwrap(), r#"say "hi""#);
        assert_eq!(t(r#"'it\'s'"#).unwrap(), "it's");
        assert_eq!(t(r#""back\\slash""#).unwrap(), r"back\slash");
        assert_eq!(t(r#""tab\tkept""#).unwrap(), r"tab\tkept");
        // Empty default only in quotes.
        assert_eq!(t(r#""""#).unwrap(), "");
        let err = t("").unwrap_err().to_string();
        assert!(err.contains("empty default"), "{err}");
        // Bare: letters and digits only; anything else must be quoted, and
        // the error says which character tripped it.
        assert_eq!(t("abc123").unwrap(), "abc123");
        let err = t("foo bar").unwrap_err().to_string();
        assert!(err.contains("contains ' '") && err.contains("quote it"), "{err}");
        let err = t("/tmp/x").unwrap_err().to_string();
        assert!(err.contains("contains '/'"), "{err}");
        let err = t("a_b").unwrap_err().to_string();
        assert!(err.contains("contains '_'"), "{err}");
        // Unterminated quotes are an error, not a silent swallow.
        assert!(t(r#""open"#).is_err());
        assert!(t(r#""trailing\"#).is_err());
    }

    #[test]
    fn knob_tokens_read_the_snapshot() {
        let v = vars_with_knobs();
        assert_eq!(v.substitute("-A @KNOB(allocation)@").unwrap(), "-A hpc_xxx");
        assert_eq!(
            v.substitute("f = \"@KNOB(kadath-initial-data)@\"").unwrap(),
            "f = \"/scratch/id/bhns.info\""
        );
        // Unset or empty: the required form fails loudly, naming the fix.
        let err = v.substitute("@KNOB(mail)@").unwrap_err().to_string();
        assert!(err.contains("knob mail is unset or empty") && err.contains("-K mail=VALUE"), "{err}");
        let err = v.substitute("@KNOB(email)@").unwrap_err().to_string();
        assert!(err.contains("knob email is unset or empty"), "{err}");
        // -OPTIONAL: empty, or the default.
        assert_eq!(v.substitute("[@KNOB-OPTIONAL(mail)@]").unwrap(), "[]");
        assert_eq!(v.substitute("@KNOB-OPTIONAL(email, \"a@@b\")@").unwrap(), "a@@b");
        assert_eq!(v.substitute("@KNOB-OPTIONAL(allocation, other)@").unwrap(), "hpc_xxx");
        // Knob names follow the kebab-case rules.
        assert!(v.substitute("@KNOB(Allocation)@").is_err());
        assert!(v.substitute("@KNOB(-x)@").is_err());
        assert!(v.substitute("@KNOB(x-)@").is_err());
        assert!(v.substitute("@KNOB(1x)@").is_err());
        assert!(v.substitute("@KNOB()@").is_err());
        assert!(v.substitute("@KNOB(under_score)@").is_err());
        // Without parens, KNOB is an ordinary (here unknown) variable.
        assert!(v.substitute("@KNOB@").is_err());
        // A set without a snapshot cannot answer any KNOB token — not even
        // an optional one, which would otherwise quietly read as "unset".
        let err = vars().substitute("@KNOB-OPTIONAL(allocation, x)@").unwrap_err().to_string();
        assert!(err.contains("not available in this context"), "{err}");
    }

    #[test]
    fn check_is_substitute_minus_the_environment() {
        let v = vars_with_knobs();
        // An unset required ENV passes the check (the run-time environment
        // is not this one's) …
        v.check("x=@ENV(CACTUP_TEST_SURELY_UNSET)@ n=@NODES@").unwrap();
        // … but every other failure is caught now.
        assert!(v.check("@KNOB(mail)@").is_err());
        assert!(v.check("@NOPE@").is_err());
        assert!(v.check("a @ b").is_err());
        assert!(v.check("@ENV(lower)@").is_err());
        assert!(v.check("@ENV-OPTIONAL(X, a b)@").is_err());
        assert!(vars().check("@KNOB-OPTIONAL(mail)@").is_err(), "no snapshot at all");
    }

    #[test]
    fn py_json_shape() {
        let json = vars().to_py_json();
        assert_eq!(json["vars"]["NODES"], "4");
        assert_eq!(json["typed"]["NODES"], 4);
        assert_eq!(json["typed"]["GPU"], false);
        assert!(json["typed"].get("QUEUE").is_none());
        // No snapshot → an empty knobs dict, never a missing key.
        assert_eq!(json["knobs"], serde_json::json!({}));
        let json = vars_with_knobs().to_py_json();
        assert_eq!(json["knobs"]["allocation"], "hpc_xxx");
        assert_eq!(json["knobs"]["kadath-initial-data"], "/scratch/id/bhns.info");
    }

    #[test]
    fn py_variant_sees_knobs() {
        if Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 not found; skipping");
            return;
        }
        let mut f = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        writeln!(
            f,
            "print(knobs['allocation'], knob('kadath-initial-data'), knob('mail', 'none'), \
             repr(knob('email', '')))"
        )
        .unwrap();
        f.flush().unwrap();
        let out = vars_with_knobs().run_py_script(f.path()).unwrap();
        assert_eq!(out.trim(), "hpc_xxx /scratch/id/bhns.info none ''");
        // A required knob that is unset refuses the run like @KNOB(name)@.
        let mut f = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        writeln!(f, "print(knob('mail'))").unwrap();
        f.flush().unwrap();
        let err = vars_with_knobs().run_py_script(f.path()).unwrap_err().to_string();
        assert!(err.starts_with("knob mail is unset or empty"), "{err}");
        assert!(!err.contains("Traceback"), "{err}");
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

    #[test]
    fn py_variant_can_refuse_the_run() {
        if Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 not found; skipping");
            return;
        }
        // A deliberate refusal reaches the user as its own message — no
        // traceback, no "exited unsuccessfully" wrapper.
        let mut f = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        writeln!(f, "raise CactupError(f\"queue {{QUEUE}} allows at most 2 nodes, got {{NODES}}\")").unwrap();
        f.flush().unwrap();
        let err = vars().run_py_script(f.path()).unwrap_err().to_string();
        assert!(err.starts_with("queue checkpt allows at most 2 nodes, got 4"), "{err}");
        assert!(!err.contains("Traceback"), "{err}");

        // Multi-line messages survive intact — a refusal usually wants to say
        // what to do instead.
        let mut f = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        writeln!(f, "raise CactupError('no good\\ntry --tpn 1')").unwrap();
        f.flush().unwrap();
        let err = vars().run_py_script(f.path()).unwrap_err().to_string();
        assert!(err.contains("no good\ntry --tpn 1"), "{err}");

        // A genuine bug is NOT a refusal: it keeps its traceback, so a typo in
        // a submitscript is debuggable rather than disguised as a policy error.
        let mut f = tempfile::NamedTempFile::with_suffix(".py").unwrap();
        writeln!(f, "print(NO_SUCH_VARIABLE)").unwrap();
        f.flush().unwrap();
        let err = vars().run_py_script(f.path()).unwrap_err().to_string();
        assert!(err.contains("Traceback") && err.contains("NameError"), "{err}");
    }
}
