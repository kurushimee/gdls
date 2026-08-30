//! `rpc_annotation` (gdscript_parser.cpp:5238-5298) — one `@rpc` per function, a vocabulary
//! check per argument, and a "no more than once" check per config axis. Each row is pinned
//! against `godot --headless --check-only` at 4.7.2-stable.

use gd_analyze::warn_policy::{StrictSettings, WarnPolicy};
use gd_analyze::NoCrossFile;
use gd_project::WarningConfig;
use gd_syntax::Dialect;
use gd_types::NativeDb;

const INVALID_ARG: &str = r#"Invalid RPC argument. Must be one of: "call_local"/"call_remote" (local calls), "any_peer"/"authority" (permission), "reliable"/"unreliable"/"unreliable_ordered" (transfer mode)."#;

fn mini_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 7, "version_patch": 2},
            "classes": [{"name": "Object"}, {"name": "Node", "inherits": "Object"}]
        }"#,
    )
    .expect("valid mini dump")
}

fn errors(src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    );
    let result = gd_analyze::analyze(&tree, None, "t.gd", &mini_native(), &NoCrossFile, &policy);
    result
        .diagnostics
        .iter()
        .filter(|d| d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect()
}

fn rpc(args: &str) -> Vec<String> {
    errors(&format!("extends Node\n@rpc{args}\nfunc f():\n\tpass\n"))
}

#[test]
fn a_bare_rpc_and_a_full_config_are_silent() {
    assert!(rpc("").is_empty());
    assert!(rpc(r#"("any_peer", "call_local", "reliable", 0)"#).is_empty());
}

#[test]
fn a_second_rpc_on_one_function_is_rejected() {
    assert_eq!(
        rpc(r#" @rpc"#),
        vec!["RPC annotations can only be used once per function."]
    );
    assert_eq!(
        rpc(r#"("any_peer") @rpc("call_local")"#),
        vec!["RPC annotations can only be used once per function."]
    );
}

#[test]
fn an_unknown_keyword_is_reported_once_per_argument() {
    assert_eq!(rpc(r#"("bogus")"#), vec![INVALID_ARG]);
    assert_eq!(rpc(r#"("bogus", "nope")"#), vec![INVALID_ARG, INVALID_ARG]);
}

#[test]
fn each_config_axis_may_be_named_only_once() {
    assert_eq!(
        rpc(r#"("call_local", "call_remote")"#),
        vec![
            r#"Invalid RPC config. The locality ("call_local"/"call_remote") must be specified no more than once."#
        ]
    );
    assert_eq!(
        rpc(r#"("any_peer", "authority")"#),
        vec![
            r#"Invalid RPC config. The permission ("any_peer"/"authority") must be specified no more than once."#
        ]
    );
    assert_eq!(
        rpc(r#"("reliable", "unreliable")"#),
        vec![
            r#"Invalid RPC config. The transfer mode ("reliable"/"unreliable"/"unreliable_ordered") must be specified no more than once."#
        ]
    );
}

#[test]
fn only_the_first_offending_axis_is_reported() {
    // cpp:5288-5294 is an else-if chain, so the permission clash stays quiet here.
    assert_eq!(
        rpc(r#"("call_local", "call_remote", "any_peer", 0)"#),
        vec![
            r#"Invalid RPC config. The locality ("call_local"/"call_remote") must be specified no more than once."#
        ]
    );
}

#[test]
fn the_fourth_argument_is_a_channel_not_a_keyword() {
    // "unreliable" in the channel slot is a typed-argument error, not a transfer-mode clash.
    assert_eq!(
        rpc(r#"("any_peer", "any_peer", "reliable", "unreliable")"#),
        vec![
            r#"Invalid RPC config. The permission ("any_peer"/"authority") must be specified no more than once."#
        ]
    );
}

#[test]
fn an_argument_that_is_not_a_string_literal_is_passed_over() {
    assert!(
        errors("extends Node\nconst M = \"any_peer\"\n@rpc(M)\nfunc f():\n\tpass\n").is_empty()
    );
}
