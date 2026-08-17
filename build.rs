//! Build script: parse and validate `resources/wisdom.txt` (spec §16) and
//! generate the static corpus slices the `wisdom` command embeds. A
//! malformed corpus fails the build itself, not just the test suite.

include!("src/wisdom_parse.rs");

fn main() {
    println!("cargo:rerun-if-changed=resources/wisdom.txt");
    println!("cargo:rerun-if-changed=src/wisdom_parse.rs");

    let raw = std::fs::read_to_string("resources/wisdom.txt")
        .expect("failed to read resources/wisdom.txt");
    let (tips, zens) = parse_wisdom(&raw);

    assert!(tips.len() >= 10, "wisdom.txt: suspiciously few tips ({})", tips.len());
    assert!(zens.len() >= 5, "wisdom.txt: suspiciously few zen entries ({})", zens.len());
    for entry in tips.iter().chain(&zens) {
        assert!(!entry.contains('\t'), "wisdom.txt: tabs render unpredictably: {entry:?}");
        for line in entry.lines() {
            assert!(line.chars().count() <= 100, "wisdom.txt: line too wide: {line:?}");
        }
    }
    for zen in &zens {
        assert!(zen.contains('—'), "wisdom.txt: zen entry lacks an — Name attribution: {zen:?}");
    }

    let literals = |entries: &[String]| {
        entries.iter().map(|e| format!("    {e:?},\n")).collect::<String>()
    };
    let generated = format!(
        "static TIPS: &[&str] = &[\n{}];\nstatic ZENS: &[&str] = &[\n{}];\n",
        literals(&tips),
        literals(&zens),
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set"));
    std::fs::write(out.join("wisdom_gen.rs"), generated).expect("failed to write wisdom_gen.rs");
}
