#![no_main]

use libfuzzer_sys::fuzz_target;

use camino::Utf8PathBuf;
use gd_project::{extract_interface, Index};
use gd_syntax::ParseOptions;

#[path = "common.rs"]
mod common;

// Layer-3 fuzz target (M4 WP-T2): drive `Index::on_file_changed` + `::on_file_removed` with random
// sequences and assert `Index::verify()` holds after every mutation. The contract under test is
// the M4 invariant guarantee: no mutation path can desynchronize `paths`/`ids`/`interfaces`/
// `registry`/`DepGraph`/`name_referencers`/`file_refs`/`path_referencers`/`file_path_ref` — if it
// does, IndexMutation::apply must
// catch it (in debug builds it would panic; the fuzz binary is release-ish so we call `verify()`
// directly and panic on Err so libfuzzer logs the trigger).
//
// Input bytes are split into a stream of operations:
//   first byte = op kind (0=change, 1=remove)
//   next byte = file slot (0..16, modulo)
//   remainder = source-text fragment chosen from a library of GDScript shapes selected to
//   exercise the trickier invariants:
//   - duplicate `class_name` declarations across slots → registry collision drift
//   - heavy cross-class member typing (X/Y) → DepGraph asymmetry surface
//   - rename cycles where a slot's class_name toggles between names already in use elsewhere
//     → name_referencers / file_refs inverse-pair drift
//   - self-extending classes → set_deps self-edge filter
//   - PATH-based `extends "res://fN.gd"` → the `path_referencers` reverse index: creating or
//     removing the target slot must keep `file_path_ref` ↔ `path_referencers` consistent
//     (invariants 6-7) and re-link the waiting consumer when the target appears
//   The point is mutation churn that hits invariants 3-7, not parse-error coverage — the parser
//   is layer-1's job.
//
// The op set is deliberately just change / remove. `Index::txn`'s post-verify + quarantine RECOVERY
// path is NOT fuzzed here, and cannot be: cargo-fuzz builds release-optimized but with
// debug-assertions ON, where `txn` answers ANY post-state violation — a real mutator desync OR a
// deliberately-injected one — with its debug-only `panic!` ("this is a bug; verify caught an
// inconsistency on_file_* should never produce"). libfuzzer-sys's panic hook treats that `panic!`
// as a crash *before* `txn`'s `catch_unwind` can reach the quarantine branch, so the recovery
// contract is unreachable through `txn` under fuzzing by every path (real desync → debug-panic;
// injected violation → debug-panic; panicking mutation → hook aborts before `catch_unwind`). That
// is by design and a feature for ops 0/1: a real invariant desync surfaces as an immediate
// libfuzzer crash with a minimal repro. The quarantine recovery itself (which only runs in
// debug-assertions-OFF builds) is covered directly by gd_project's RELEASE unit tests
// (`txn_quarantines_injected_verify_violation_without_panic`,
// `index_mutation_quarantines_the_offending_file_after_a_violating_panic` and siblings, run by
// `cargo test -p gd_project --release`), which don't sit under libfuzzer's abort-on-panic hook.

const TEMPLATES: &[&str] = &[
    // -- Baseline single-class shapes -----------------------------------------
    "class_name A\nextends Node\n",
    "class_name B\nextends Node\nvar x: int = 1\n",
    "class_name C\nextends A\n",
    "class_name D\nextends B\nfunc f() -> void:\n\tpass\n",
    "class_name E\nextends Node\nconst K = 5\n",
    "extends A\n",    // unnamed file extending A
    "extends Node\n", // unnamed Node-extender
    "class_name F\nextends C\nvar y: D\n",
    "class_name G\nextends D\nsignal hello()\n",
    "class_name H\nextends Node\nenum K { A, B }\n",
    // -- Adversarial: registry collisions -------------------------------------
    // Same class_name as templates 0 and 1. Loading templates 0 and 10 (both A)
    // into adjacent slots and then renaming one forces the registry's
    // remove_by_path + insert path to keep the inverse consistent.
    "class_name A\nextends Node2D\n",
    "class_name B\nextends Reference\nvar x: int = 2\n",
    // -- Adversarial: heavy cross-class member typing -------------------------
    // Each member references multiple other class_names — exercises set_deps
    // pruning and the file_refs ↔ name_referencers inverse on a wider front.
    "class_name X\nextends Node\nvar a: A\nvar b: B\nvar c: C\nvar d: D\n",
    "class_name Y\nextends X\nvar e: E\nvar f: F\nvar g: G\nvar h: H\n",
    // -- Adversarial: rename cycles -------------------------------------------
    // C toggling between extends-A and extends-X across mutations exercises
    // relink_referencers + recompute_edges.
    "class_name C\nextends X\nvar referee: Y\n",
    // -- Adversarial: self-extending class — set_deps must drop the self-edge.
    "class_name SelfTwist\nextends SelfTwist\n",
    // -- Adversarial: PATH-based extends → path_referencers reverse index (invariants 6-7).
    // These point at other slots' files by `res://` path; creating/removing the target slot must
    // keep file_path_ref ↔ path_referencers consistent and re-link the waiting consumer on add.
    // The target may never exist (a waiting referencer) or toggle in/out across mutations.
    "extends \"res://f0.gd\"\n",
    "class_name Pathy\nextends \"res://f1.gd\"\n",
    // -- Adversarial: deeply nested type args ---------------------------------
    "class_name Stack\nextends Node\nvar inner: Array[Array[Array[A]]]\n",
];

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    // The leading byte picks the dialect every template is parsed at, matching a session, where one
    // resolved dialect governs the whole index. The op stream starts after it.
    let dialect = common::dialect_from(data[0]);
    let data = &data[1..];
    let mut idx = Index::new(Utf8PathBuf::from("/proj"));
    let mut cursor = 0usize;

    // The op space is change / remove. The quarantine recovery path (an injected or panicking
    // violation) is unit-tested in gd_project, not fuzzed — under cargo-fuzz's debug-assertions-on
    // build `txn` answers any violation with its debug-`panic!`, and libfuzzer's panic hook aborts
    // before the quarantine branch runs (see the module header).
    const OP_COUNT: u8 = 2;

    while cursor + 2 < data.len() {
        let op = data[cursor] % OP_COUNT;
        let slot = (data[cursor + 1] as usize) % 16;
        let template_idx = (data[cursor + 2] as usize) % TEMPLATES.len();
        cursor += 3;

        let path = Utf8PathBuf::from(format!("/proj/f{slot}.gd"));
        match op {
            0 => {
                let text = TEMPLATES[template_idx];
                let tree = gd_syntax::parse_with_options(
                    text,
                    &ParseOptions {
                        dialect,
                        script_path: "",
                    },
                )
                .tree;
                let iface = extract_interface(&tree);
                idx.txn(&path, |i| i.on_file_changed(&path, iface));
            }
            1 => {
                idx.txn(&path, |i| i.on_file_removed(&path));
            }
            // `op = data % OP_COUNT` is in 0..=1; the explicit arms above are exhaustive.
            _ => unreachable!("op is data % OP_COUNT (2), so always 0 or 1"),
        }
        // Explicit verify on top of `Index::txn` — panics on any invariant violation libfuzzer can
        // then shrink down to a minimal repro. (`txn`'s own post-verify already debug-panics on a
        // desync under the debug-assertions-on fuzz build; this belt-and-suspenders re-verify pins
        // the contract independent of that cfg.)
        if let Err(violations) = idx.verify() {
            panic!("index invariant violated: {violations:?}");
        }
    }
});
