use directories::BaseDirs;

/// Expand `$VAR`/`${VAR}` references against the current environment.
/// Undefined variables expand to the empty string, matching shell behaviour.
pub fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }

        match chars.peek() {
            // ${NAME}
            Some('{') => {
                chars.next(); // consume '{'
                let mut name = String::new();
                let mut closed = false;
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                if closed {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                } else {
                    // Unterminated `${...` — leave it untouched.
                    out.push_str("${");
                    out.push_str(&name);
                }
            }
            // $NAME (alphanumeric/underscore, not starting with a digit)
            Some(&c2) if c2 == '_' || c2.is_ascii_alphabetic() => {
                let mut name = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == '_' || nc.is_ascii_alphanumeric() {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            // A lone `$` (or `$` followed by punctuation) is emitted literally.
            _ => out.push('$'),
        }
    }

    out
}

/// Expand a user-supplied path string the way a shell would: a leading `~`
/// becomes the home directory and `$VAR`/`${VAR}` are substituted. Paths typed
/// at our prompts are read straight from stdin with no shell involved, so
/// without this a literal `~` directory would be created in the current
/// working directory. Values passed as flags are already shell-expanded, so
/// running them through this again is a harmless no-op.
pub fn expand_path(input: &str, base_dirs: &BaseDirs) -> String {
    let expanded = expand_env_vars(input);
    let home = base_dirs.home_dir();

    if expanded == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(rest) = expanded.strip_prefix("~/") {
        home.join(rest).to_string_lossy().into_owned()
    } else {
        expanded
    }
}