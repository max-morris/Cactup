//! GCC `specs` files the cache accepts (§18.5): those that change only how
//! GCC links.
//!
//! A specs file can add flags to every compile behind the command line,
//! where the reader of the command line (`compile`) never sees them, so a
//! GCC that reads one is not cached — with one exception. Site-built GCCs
//! carry a specs file that adds an `-rpath` to their own libraries (qbd's
//! GCC 13.2 changes `*link_libgcc:` to `%(link_libgcc_rpath) %D` and adds
//! that section), and a `-c` compile never links. Such a file is read
//! against the driver's built-in specs (`gcc -dumpspecs`, which the file
//! does not affect) and accepted only when it defines every built-in
//! section (a GCC that reads a specs file does not set up its built-in ones
//! first) and every difference is in a section only the link command uses,
//! or in a new one only those refer to.
//!
//! GCC's own compile steps (`default_compilers` in its driver) are not in
//! `-dumpspecs`. They refer to built-in sections by name, and to none of
//! [`LINK_ONLY`]: that list is what the judgment rests on. A new section
//! could share its name with one those steps refer to and `-dumpspecs` does
//! not show; the driver's own bytes hold those steps as text, so a new name
//! that appears there as a reference is refused too.

use std::collections::{BTreeMap, BTreeSet};

/// The built-in sections only GCC's link command reads (`link_command`
/// itself, and what it refers to by name or by `%L %G %S %E %l`). Every
/// reference to them in GCC 14's driver is inside the link command, which
/// a `-c` compile does not run.
const LINK_ONLY: &[&str] = &[
    "link_command",
    "linker",
    "link",
    "lib",
    "libgcc",
    "link_libgcc",
    "link_gcc_c_sequence",
    "link_ssp",
    "link_gomp",
    "startfile",
    "endfile",
    "post_link",
    "linker_plugin_file",
    "lto_wrapper",
    "lto_gcc",
];

/// The sections of a specs text, in order: `*name:`, then the lines up to a
/// blank one. Anything else — a directive (`%include`, `%rename`), a
/// compiler for a suffix (`.c:`, `@c:`), a comment — is an error saying
/// what it is: this reader does not follow it, so it does not accept it.
fn sections(text: &str) -> Result<Vec<(&str, String)>, String> {
    let mut out = Vec::new();
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        let name = line.strip_prefix('*').and_then(|rest| rest.strip_suffix(':'));
        let Some(name) = name.filter(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')) else {
            return Err(match line.as_bytes()[0] {
                b'%' => format!("has a directive ({line})"),
                b'.' | b'@' => format!("defines a compiler ({line})"),
                _ => format!("has a line this reader does not follow ({line})"),
            });
        };
        // GCC skips blank lines after the name: an empty section is
        // written with two of them (`-dumpspecs` does), and after only one
        // GCC takes what follows for the section's text.
        let mut ahead = lines.clone();
        if ahead.next() == Some("") && ahead.next().is_some_and(|after| !after.is_empty()) {
            return Err(format!("has a section GCC would read differently ({line} followed by one blank line)"));
        }
        let mut body = Vec::new();
        while let Some(line) = lines.next_if(|line| !line.is_empty()) {
            body.push(line);
        }
        out.push((name, body.join("\n")));
    }
    Ok(out)
}

/// The sections `body` refers to by name: `%(name)` and `%[name]`.
fn references(body: &str) -> impl Iterator<Item = &str> {
    ["%(", "%["].into_iter().flat_map(move |open| {
        body.match_indices(open).filter_map(move |(at, _)| {
            let rest = &body[at + 2..];
            let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
            rest[end..].starts_with([')', ']']).then(|| &rest[..end])
        })
    })
}

/// Does the specs file `file` change only how GCC links, against the
/// built-in specs `builtin` of the driver whose bytes are `driver`? `Err`
/// says what else it does.
pub fn link_only(builtin: &str, file: &[u8], driver: &[u8]) -> Result<(), String> {
    let builtin: BTreeMap<&str, String> =
        sections(builtin).map_err(|why| format!("built-in specs that cannot be read: {why}"))?.into_iter().collect();
    let file = std::str::from_utf8(file).map_err(|_| "is not text".to_owned())?;
    let file = sections(file)?;
    let mut seen = BTreeSet::new();
    for (name, _) in &file {
        if !seen.insert(*name) {
            return Err(format!("defines {name} twice"));
        }
    }
    // A GCC that finds its specs file does not set up its built-in
    // sections first: one the file leaves out is not there at all.
    if let Some(left_out) = builtin.keys().find(|name| !seen.contains(*name)) {
        return Err(format!("leaves out {left_out}, which GCC then does not have"));
    }
    let new: BTreeSet<&str> = file.iter().map(|(name, _)| *name).filter(|name| !builtin.contains_key(name)).collect();
    for (name, body) in &file {
        let changed = builtin.get(name).is_some_and(|built_in| built_in != body);
        if changed && !LINK_ONLY.contains(name) {
            return Err(format!("changes {name}, which compiling reads"));
        }
    }
    // What GCC ends up with: the built-in sections, as the file leaves them.
    let mut effective: BTreeMap<&str, &str> = builtin.iter().map(|(name, body)| (*name, body.as_str())).collect();
    effective.extend(file.iter().map(|(name, body)| (*name, body.as_str())));
    for (name, body) in &effective {
        if LINK_ONLY.contains(name) || new.contains(name) {
            continue;
        }
        if let Some(added) = references(body).find(|reference| new.contains(reference)) {
            return Err(format!("adds {added}, which {name} refers to"));
        }
    }
    let contains = |needle: &[u8]| driver.windows(needle.len()).any(|window| window == needle);
    for name in &new {
        if [format!("%({name})"), format!("%[{name}]")].iter().any(|reference| contains(reference.as_bytes())) {
            return Err(format!("adds {name}, which the driver's own compile steps may refer to"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A few built-in sections, as `-dumpspecs` prints them.
    const BUILTIN: &str = "*asm:\n--64\n\n*cpp:\n%{posix:-D_POSIX_SOURCE}\n\n*cc1:\n%{profile:-p}\n\n\
        *link_libgcc:\n%D\n\n*empty:\n\n\n*self_spec:\n\n\n*link_command:\n%{!c:%(linker) %(link_libgcc) %L}\n\n";

    /// qbd's GCC 13.2 specs file, in miniature: the built-in specs with an
    /// rpath added to the link.
    fn qbd() -> String {
        BUILTIN.replace("*link_libgcc:\n%D\n", "*link_libgcc:\n%(link_libgcc_rpath) %D\n")
            + "*link_libgcc_rpath:\n-rpath /usr/local/packages/compilers/gcc/13.2.0/lib64\n\n"
    }

    #[test]
    fn reads_sections_as_gcc_writes_them() {
        let read = sections(BUILTIN).unwrap();
        let names: Vec<&str> = read.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["asm", "cpp", "cc1", "link_libgcc", "empty", "self_spec", "link_command"]);
        assert_eq!((read[4].1.as_str(), read[5].1.as_str()), ("", ""));
        assert_eq!(sections("*a:\nline one\nline two\n\n*b:\nx").unwrap()[0].1, "line one\nline two");
        assert_eq!(references("%(a) %[b] %(c %d %(e_1)").collect::<Vec<_>>(), ["a", "e_1", "b"]);
    }

    #[test]
    fn a_file_that_changes_only_the_link_is_accepted() {
        assert_eq!(link_only(BUILTIN, qbd().as_bytes(), b"driver"), Ok(()));
        // Or that repeats the built-in specs.
        assert_eq!(link_only(BUILTIN, BUILTIN.as_bytes(), b""), Ok(()));
        let link = BUILTIN.replace("%{!c:%(linker) %(link_libgcc) %L}", "%{!c:%(linker) -rpath /x %(link_libgcc) %L}");
        assert_eq!(link_only(BUILTIN, link.as_bytes(), b""), Ok(()));
    }

    #[test]
    fn anything_else_is_refused() {
        for (file, why) in [
            (BUILTIN.replace("%{profile:-p}", "%{profile:-p} -DSNEAKY"), "changes cc1"),
            (BUILTIN.replace("--64", "--32"), "changes asm"),
            (BUILTIN.replace("*self_spec:\n\n\n", "*self_spec:\n-O2\n\n"), "changes self_spec"),
            (qbd().replace("%{profile:-p}", "%{profile:-p} %(link_libgcc_rpath)"), "changes cc1"),
            ("%include <other.specs>\n".to_owned(), "has a directive"),
            ("%rename cc1 old_cc1\n*cc1:\n%(old_cc1) -DX\n".to_owned(), "has a directive"),
            (".f90:\n@f95\n".to_owned(), "defines a compiler"),
            ("# a comment\n".to_owned(), "does not follow"),
            ("*link:\n-x\n\n*link:\n-y\n".to_owned(), "defines link twice"),
            // Only some of the sections, or none: GCC then lacks the rest.
            ("*cc1:\n%{profile:-p}\n".to_owned(), "leaves out asm"),
            (String::new(), "leaves out asm"),
            // An empty section with one blank line after it: GCC reads the
            // next section's name as its text.
            (BUILTIN.replace("*empty:\n\n\n", "*empty:\n\n"), "read differently"),
        ] {
            let err = link_only(BUILTIN, file.as_bytes(), b"").unwrap_err();
            assert!(err.contains(why), "{file:?}: {err}");
        }
        // A new section the driver's compile steps name.
        let driver = b"...%{!E:%(cc1_extra) %(cc1_options)}...";
        let file = format!("{BUILTIN}*cc1_extra:\n-DX\n");
        let file = file.as_str();
        assert!(link_only(BUILTIN, file.as_bytes(), driver).unwrap_err().contains("own compile steps"));
        assert_eq!(link_only(BUILTIN, file.as_bytes(), b"..."), Ok(()), "named by nothing, it does nothing");
        assert_eq!(link_only(BUILTIN, &[0xff, 0xfe], b"").unwrap_err(), "is not text");
    }
}
