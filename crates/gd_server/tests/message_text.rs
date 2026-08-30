//! #420 — no user-facing message carries a run of stray spaces mid-sentence.
//!
//! Three refusal messages shipped with a run of spaces in the middle of a sentence, left behind
//! when a long string literal was wrapped by hand and the halves were never joined properly. The
//! text reaches the user's editor as-is, so it is a real defect and an easy one to reintroduce:
//! the source looks fine at a glance because the spaces line up under the indentation.
//!
//! The guard is a scan of the crate's own sources rather than an assertion on any one message,
//! because the slip is an authoring slip. A message that no test happens to exercise is exactly
//! the one that ships broken.

use std::path::Path;

/// A run of three or more spaces inside a string literal, sitting between two ordinary sentence
/// characters. Deliberate runs are all alignment, and they come in two shapes that this skips: a
/// run just behind an escape (`\n` for an embedded code block, `\x20` for a leading column), and a
/// run on a source line that ends in a `\` continuation, which is how the `--help` block lays its
/// columns out. Prose has neither.
fn stray_space_runs(text: &str) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_end().ends_with('\\') {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            if chars[i] != ' ' {
                i += 1;
                continue;
            }
            let start = i;
            while i < chars.len() && chars[i] == ' ' {
                i += 1;
            }
            if i - start < 3 || start == 0 || i >= chars.len() {
                continue;
            }
            let before = chars[start - 1];
            let follows_an_escape = chars[start.saturating_sub(8)..start].contains(&'\\');
            if follows_an_escape || !chars[i].is_alphanumeric() {
                continue;
            }
            if before.is_alphanumeric() || matches!(before, ',' | ';' | ')' | ':' | '.') {
                let from = start.saturating_sub(40);
                hits.push((n + 1, chars[from..i].iter().collect()));
            }
        }
    }
    hits
}

#[test]
fn no_source_string_carries_a_run_of_stray_spaces() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut bad: Vec<String> = Vec::new();
    let mut files = vec![src];
    while let Some(dir) = files.pop() {
        for entry in std::fs::read_dir(&dir).expect("read the crate's src tree") {
            let path = entry.expect("a readable dir entry").path();
            if path.is_dir() {
                files.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read a source file");
            for (line, context) in stray_space_runs(&text) {
                bad.push(format!("{}:{line}: …{context}…", path.display()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a message carries a run of stray spaces:\n{}",
        bad.join("\n")
    );
}

#[test]
fn the_scan_tells_a_broken_wrap_from_deliberate_alignment() {
    // The shape this exists to catch, verbatim from what shipped.
    assert!(!stray_space_runs(
        r#"format!("Cannot rename `{name}`: its chain is unknown, so the         rename cannot apply")"#
    )
    .is_empty());
    // Embedded code-block indentation, a leading `\x20` column, a `\` continuation line, and
    // ordinary source indentation are all fine.
    assert!(stray_space_runs(r#"md("if x:\n    pass")"#).is_empty());
    assert!(stray_space_runs(r#"let m = "one\n\x20   two";"#).is_empty());
    assert!(
        stray_space_runs("let h = \"\\\n    gdls --help          Print this\\n\\\n\";").is_empty()
    );
    assert!(stray_space_runs("let x = 1;\n    let y = 2;").is_empty());
}
