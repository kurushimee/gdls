#![no_main]

use libfuzzer_sys::fuzz_target;

// Fuzz the `.tscn` scene parser (M11 #76). Taking `&str` makes libfuzzer-sys feed a valid-UTF-8
// view of the raw input (via the `arbitrary` crate), matching `gd_project::parse_scene`'s
// signature. The contract under test is "never crash, never lie": for ANY input — malformed,
// truncated, binary garbage, deeply-nested brackets — the parser must return a (possibly partial)
// `Scene` without panicking, overflowing the stack, or hanging (`CLAUDE.md`). We also exercise the
// derived query API on the result so a parse that produced an inconsistent path/unique-name index
// would surface as a panic here.
fuzz_target!(|data: &str| {
    let scene = gd_project::parse_scene(data);
    // Touch the derived lookups: these must be internally consistent for any parse output.
    let _ = scene.root_node();
    let _ = scene.root_script_path();
    let _ = scene.attached_scripts().count();
    let _ = scene.instanced_scenes().count();
    // The instance resolver must terminate (cycle set + depth cap) even when the lookup keeps
    // feeding scenes back — here we feed the SAME input for every sub-scene path, the worst case
    // for cycle detection.
    let _ = scene.resolve_root_type(&|_p: &str| Some(std::borrow::Cow::Borrowed(data)));
});
