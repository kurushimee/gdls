#![no_main]

use libfuzzer_sys::fuzz_target;

use gd_analyze::{NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::WarningConfig;
use gd_types::NativeDb;

// Layer 2 of the fuzz gate, covering the M3 analyzer: any input that successfully parses through
// `gd_syntax::parse` (the layer-1 fuzz target's contract) is fed through `gd_analyze::analyze` with
// the production-shape stubs — empty native DB, no cross-file resolver, default warning config.
// The contract under test is the same as the parser's: "never crash, never lie" (`CLAUDE.md`) —
// any analyzer panic is a release blocker. We don't assert anything about the diagnostics produced;
// the corpus + differential oracle (`docs/06`) handle fidelity. This target is solely about
// panic-freedom across the resolver/reducer paths the M3 work added.
fuzz_target!(|data: &str| {
    let tree = gd_syntax::parse(data).tree;
    let native = NativeDb::empty();
    let xfile = NoCrossFile;
    let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default());
    // `file: None` is the orphan-buffer shape gd_server uses for files not yet in the index; the
    // analyzer's `file` parameter only feeds ScriptRef self-identity, so the orphan shape is the
    // right production-faithful stub for single-buffer fuzzing.
    let _ = gd_analyze::analyze(&tree, None, "fuzz.gd", &native, &xfile, &policy);
});
