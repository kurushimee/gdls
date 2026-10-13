//! The analyzer recurses once per nesting level of the source. Each recursion site runs through
//! `gd_syntax::stack::ensure`, so a deep file analyzes on any thread; these run on a thread whose
//! stack holds only a few hundred levels without it.

use gd_analyze::{analyze_with_options, AnalyzeOptions, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::WarningConfig;
use gd_syntax::{Dialect, ParseOptions};
use gd_types::NativeDb;

fn analyze_on_small_stack(expr: String) -> usize {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let src = format!("var b\n\nfunc f():\n\tvar x = {expr}\n\tprint(x)\n");
            let tree = gd_syntax::parse_with_options(
                &src,
                &ParseOptions {
                    dialect: Dialect::DEFAULT,
                    script_path: "",
                },
            )
            .tree;
            let policy = WarnPolicy::build(
                &WarningConfig::default(),
                &StrictSettings::default(),
                Dialect::DEFAULT,
            );
            analyze_with_options(
                &tree,
                None,
                "deep.gd",
                &NativeDb::empty(),
                &NoCrossFile,
                &policy,
                AnalyzeOptions::default(),
            )
            .diagnostics
            .len()
        })
        .unwrap()
        .join()
        .expect("analysis must not overflow")
}

#[test]
fn deep_expressions_analyze_on_a_small_stack() {
    let d = 3_000;
    for expr in [
        format!("{}{}", "[".repeat(d), "]".repeat(d)),
        format!("{}true", "not ".repeat(d)),
        format!("1{}", " + 1".repeat(d)),
        format!("b{}", ".c".repeat(d)),
        format!("b{}", "[0]".repeat(d)),
        format!("{}b", "func(): return ".repeat(d)),
    ] {
        analyze_on_small_stack(expr);
    }
}
