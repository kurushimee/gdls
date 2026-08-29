//! The `ClassDB` half of `get_function_signature` (analyzer.cpp:5905-6015) feeding the
//! override-compatibility check (analyzer.cpp:1865-1960).
//!
//! Godot resolves an override's parent through one lookup that walks the script chain and then
//! falls into `ClassDB::get_method_info`, which returns engine *virtuals* too. So a script whose
//! only ancestor is native still has its `_ready` / `_process` overrides checked. The vendored
//! corpus reaches this path once (`errors/compat_get_property_list.gd`), through a typed-array
//! return gdls's native surface doesn't model yet, so the behavior is pinned here instead.
//!
//! Unguarded by dialect: both supported tags run the same check with the same ClassDB fallback.

use std::path::Path;

use gd_analyze::{analyze_with_options, AnalyzeOptions, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{Dialect, ParseOptions};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn errors(src: &str, dialect: Dialect) -> Vec<String> {
    let tree = gd_syntax::parse_with_options(
        src,
        &ParseOptions {
            dialect,
            script_path: "",
        },
    )
    .tree;
    let db = native_db();
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        dialect,
    );
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "native_override.gd",
        &db,
        &NoCrossFile,
        &policy,
        AnalyzeOptions {
            dialect,
            ..Default::default()
        },
    )
    .diagnostics
    .iter()
    .filter(|d| d.warning_code().is_none())
    .map(|d| d.message().to_string())
    .collect()
}

const TAGS: [Dialect; 2] = [Dialect::Godot4_6, Dialect::Godot4_7];

#[test]
fn a_narrower_return_on_a_native_virtual_is_rejected() {
    for d in TAGS {
        assert_eq!(
            errors("extends Node\n\nfunc _ready() -> int:\n\treturn 0\n", d),
            vec![
                r#"The function signature doesn't match the parent. Parent signature is "_ready() -> void"."#
                    .to_owned()
            ],
            "at {d:?}"
        );
    }
}

#[test]
fn dropping_a_native_virtuals_parameter_is_rejected() {
    for d in TAGS {
        assert_eq!(
            errors("extends Node\n\nfunc _process():\n\tpass\n", d),
            vec![
                r#"The function signature doesn't match the parent. Parent signature is "_process(float) -> void"."#
                    .to_owned()
            ],
            "at {d:?}"
        );
    }
}

#[test]
fn a_matching_native_override_is_silent() {
    for d in TAGS {
        for src in [
            "extends Node\n\nfunc _ready():\n\tpass\n",
            "extends Node\n\nfunc _ready() -> void:\n\tpass\n",
            "extends Node\n\nfunc _process(delta):\n\tprint(delta)\n",
            "extends Node\n\nfunc _process(delta: float) -> void:\n\tprint(delta)\n",
            // Contravariance: the override may widen a parameter.
            "extends Node\n\nfunc _input(event: Variant) -> void:\n\tprint(event)\n",
            // Extra parameters are fine as long as they carry defaults.
            "extends Node\n\nfunc _process(delta: float, extra := 1) -> void:\n\tprint(delta + extra)\n",
        ] {
            assert_eq!(errors(src, d), Vec::<String>::new(), "{src:?} at {d:?}");
        }
    }
}

/// A name no ancestor declares is not an override, so nothing is compared.
#[test]
fn a_plain_method_is_not_checked_against_the_engine() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nfunc totally_mine(a, b, c):\n\tprint([a, b, c])\n",
                d
            ),
            Vec::<String>::new(),
            "at {d:?}"
        );
    }
}

/// A script ancestor that declares the name owns the contract — the walk stops there and never
/// reaches ClassDB, so `Derived` is measured against `Base`'s `int`, not against `Node`'s `void`.
/// (`Base` itself still draws the native mismatch, one class up.)
#[test]
fn a_script_ancestor_shadows_the_native_signature() {
    for d in TAGS {
        assert_eq!(
            errors(
                "extends Node\n\nclass Base extends Node:\n\tfunc _ready() -> int:\n\t\treturn 0\n\nclass Derived extends Base:\n\tfunc _ready() -> String:\n\t\treturn \"\"\n",
                d
            ),
            vec![
                r#"The function signature doesn't match the parent. Parent signature is "_ready() -> void"."#
                    .to_owned(),
                r#"The function signature doesn't match the parent. Parent signature is "_ready() -> int"."#
                    .to_owned(),
            ],
            "at {d:?}"
        );
    }
}
