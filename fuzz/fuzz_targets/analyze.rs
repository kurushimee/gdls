#![no_main]

use libfuzzer_sys::fuzz_target;

use gd_analyze::{analyze_with_options, AnalyzeOptions, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::WarningConfig;
use gd_syntax::ParseOptions;
use gd_types::NativeDb;

#[path = "common.rs"]
mod common;

// Layer 2 of the fuzz gate, covering the M3 analyzer: any input that successfully parses through
// `gd_syntax::parse` (the layer-1 fuzz target's contract) is fed through `gd_analyze::analyze` with
// the production-shape stubs — empty native DB, no cross-file resolver, default warning config.
// The contract under test is the same as the parser's: "never crash, never lie" (`CLAUDE.md`) —
// any analyzer panic is a release blocker. We don't assert anything about the diagnostics produced;
// the corpus + differential oracle (`docs/06`) handle fidelity. This target is solely about
// panic-freedom across the resolver/reducer paths the M3 work added.
//
// The leading byte picks the dialect, and it has to reach BOTH the parse and the analyze — a tree
// built at one tag and analyzed at another is a shape no session can produce, so fuzzing it would
// chase crashes that cannot happen. The warning policy takes it too, since the active warning set
// differs between releases.
fuzz_target!(|input: (u8, &str)| {
    let (tag, data) = input;
    let dialect = common::dialect_from(tag);
    let tree = gd_syntax::parse_with_options(
        data,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let native = NativeDb::empty();
    let xfile = NoCrossFile;
    let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default(), dialect);
    // `file: None` is the orphan-buffer shape gd_server uses for files not yet in the index; the
    // analyzer's `file` parameter only feeds ScriptRef self-identity, so the orphan shape is the
    // right production-faithful stub for single-buffer fuzzing.
    let _ = analyze_with_options(
        &tree,
        None,
        "fuzz.gd",
        &native,
        &xfile,
        &policy,
        AnalyzeOptions {
            dialect,
            ..Default::default()
        },
    );
});
