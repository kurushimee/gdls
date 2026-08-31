//! CONFUSABLE_IDENTIFIER (`gdscript_parser.cpp:2822-2825` and `:3670-3677`) — #497.
//!
//! Godot's gate is `TS->spoof_check(name)`: ICU `uspoof_check` with the allowed set
//! `uspoof_getRecommendedSet() ∪ uspoof_getInclusionSet()` and restriction level
//! `USPOOF_MODERATELY_RESTRICTIVE` (`text_server_adv.cpp:7903-7928`). A single-script identifier
//! passes; what fails is mixing Latin with Cyrillic or Greek, and any character outside the allowed
//! set. The check runs on declarations and on each bare-identifier segment of a `$`/`%` path.
//!
//! Every row is pinned against `Godot_v4.7.2-stable --headless --check-only`, with the warning
//! escalated to an error so it prints.

use gd_analyze::warn_policy::{StrictSettings, WarnPolicy};
use gd_analyze::NoCrossFile;
use gd_project::WarningConfig;
use gd_syntax::Dialect;
use gd_types::NativeDb;

fn mini_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 7, "version_patch": 2},
            "classes": [{"name": "Object"}, {"name": "Node", "inherits": "Object"}]
        }"#,
    )
    .expect("valid mini dump")
}

/// The `(line, name)` of every CONFUSABLE_IDENTIFIER the analysis emits.
fn confusables(src: &str) -> Vec<(usize, String)> {
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
        .filter(|d| {
            d.warning_code() == Some(gd_analyze::warnings::WarningCode::ConfusableIdentifier)
        })
        .map(|d| {
            let line = src[..d.span().start as usize].matches('\n').count() + 1;
            (line, d.message().to_owned())
        })
        .collect()
}

fn warns(src: &str) -> bool {
    !confusables(src).is_empty()
}

fn decl(name: &str) -> String {
    format!("extends Node\n\nfunc f() -> void:\n\tvar {name} := 1\n")
}

/// A name written wholly in one script is ordinary code, whatever script that is. gdls used to
/// warn on every one of these — the false positive this issue is about.
#[test]
fn a_single_script_identifier_is_not_a_spoof() {
    for name in [
        "port",
        "café",
        "δοκιμή",
        "тест",
        "测试",
        "テスト",
        "한글",
        "اختبار",
        "जाँच",
        "ทดสอบ",
        "π",
    ] {
        assert!(!warns(&decl(name)), "{name}");
    }
}

/// Latin mixed with a CJK or Indic script passes at MODERATELY_RESTRICTIVE — those combinations
/// are how the scripts are ordinarily written.
#[test]
fn latin_mixed_with_a_cjk_or_indic_script_passes() {
    for name in ["ab漢", "abあ", "ab한", "abद"] {
        assert!(!warns(&decl(name)), "{name}");
    }
}

/// What the level actually rejects: Latin mixed with Cyrillic or Greek. `pοrt` is the corpus's
/// own case — Latin `p` and `rt` around a Greek omicron.
#[test]
fn latin_mixed_with_cyrillic_or_greek_is_a_spoof() {
    for name in ["pοrt", "Δt", "тестδ", "abcт"] {
        assert!(warns(&decl(name)), "{name}");
    }
}

/// The allowed-set half of the check, which comes with the restriction level rather than needing
/// its own pass: a character whose Identifier_Status is not Allowed fails whatever it mixes with.
#[test]
fn a_character_outside_the_allowed_set_is_a_spoof() {
    for name in ["µs", "ﬁle", "ᚱᚢᚾ", "a4٤"] {
        assert!(warns(&decl(name)), "{name}");
    }
}

/// The documented under-reports, pinned as silent so a future Unicode data bump is a conscious
/// decision rather than a surprise. Godot warns on all three; gdls does not, which is the safe
/// direction.
#[test]
fn the_known_under_reports_stay_silent() {
    for name in ["ㄥ", "abㄥ", "á́b"] {
        assert!(!warns(&decl(name)), "{name}");
    }
}

/// Each bare-identifier segment of a `$`/`%` path is checked, and a string-literal segment is not
/// — so `$"pοrt"` is silent where `$pοrt` warns.
#[test]
fn a_node_path_segment_is_checked_unless_it_is_a_string() {
    assert_eq!(
        confusables("extends Node\nfunc f():\n\tvar a = $pοrt\n").len(),
        1
    );
    assert_eq!(
        confusables("extends Node\nfunc f():\n\tvar a = %pοrt\n").len(),
        1
    );
    assert_eq!(
        confusables("extends Node\nfunc f():\n\tvar a = $a/pοrt/b\n").len(),
        1
    );
    assert!(confusables("extends Node\nfunc f():\n\tvar a = $\"pοrt\"\n").is_empty());
    assert!(confusables("extends Node\nfunc f():\n\tvar a = $тест\n").is_empty());
}

/// Two bad segments on one line warn twice, and both land on the line the access is written on.
#[test]
fn every_bad_segment_of_one_path_warns() {
    let got = confusables("extends Node\nfunc f():\n\tvar a = $pοrt/pοrt\n");
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().all(|(line, _)| *line == 3), "{got:?}");
}

/// Godot's vendored golden `parser/features/unicode_identifiers.gd`, which must be silent. gdls
/// emitted twelve warnings on it. #496 left the parser corpus's `GDTEST_OK` cases out of the
/// conformance harness, so it is asserted here directly.
#[test]
fn the_unicode_identifiers_golden_is_silent() {
    let src = "\
extends Node

func test():
	# Some examples of identifiers in different scripts.
	var փորձարկում := 1
	print(փորձարկում)
	var امتحان := 2
	print(امتحان)
	var পরীক্ষা := 3
	print(পরীক্ষা)
	var тест := 4
	print(тест)
	var जाँच := 5
	print(जाँच)
	var 기준 := 6
	print(기준)
	var 测试 := 7
	print(测试)
	var テスト := 8
	print(テスト)
	var 試験 := 9
	print(試験)
	var പരീക്ഷ := 10
	print(പരീക്ഷ)
	var ทดสอบ := 11
	print(ทดสอบ)
	var δοκιμή := 12
	print(δοκιμή)
";
    assert_eq!(confusables(src), Vec::new());
}

/// Godot's vendored golden `parser/warnings/confusable_identifier.gd`, which warns at lines 5 and
/// 12 — the declaration and the node path. gdls emitted only line 5.
#[test]
fn the_confusable_identifier_golden_warns_on_both_lines() {
    let src = "\
extends Node

func test():
	var port = 0 # Only latin characters.
	var pοrt = 1 # The \"ο\" is Greek omicron.

	prints(port, pοrt)

# Do not call this since nodes aren't in the tree. It is just a parser check.
func nodes():
	var _node1 = $port # Only latin characters.
	var _node2 = $pοrt # The \"ο\" is Greek omicron.
";
    let lines: Vec<usize> = confusables(src).into_iter().map(|(l, _)| l).collect();
    assert_eq!(lines, vec![5, 12]);
}
