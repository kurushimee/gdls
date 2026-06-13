//! Unit tests for the pure completion-rendering helpers (M8 #64, Phase 3). The end-to-end LSP
//! behaviour (the `CompletionList` JSON shape, ranking, resolve round-trip, capability gating) is
//! covered over a real in-memory `Connection` in `tests/completion.rs`; these pin the small
//! decision functions in isolation.

use super::*;

/// A `CompletionCaps` with everything off / default — the minimal-client baseline.
fn caps_off() -> CompletionCaps {
    CompletionCaps::default()
}

#[test]
fn clamp_kind_default_set_keeps_1_through_18_drops_the_rest() {
    let caps = caps_off(); // kind_value_set: None ⇒ the LSP default 1..=18 set.
                           // METHOD (2) is inside the default range — kept.
    assert_eq!(
        clamp_kind(CompletionItemKind::METHOD, &caps),
        Some(CompletionItemKind::METHOD)
    );
    // ENUM_MEMBER (20), CONSTANT (21), EVENT (23) are OUTSIDE the default range — dropped to None
    // so a minimal client never receives a kind number it didn't promise to render.
    for k in [
        CompletionItemKind::ENUM_MEMBER,
        CompletionItemKind::CONSTANT,
        CompletionItemKind::EVENT,
    ] {
        assert_eq!(clamp_kind(k, &caps), None, "{k:?} is outside 1..=18");
    }
}

#[test]
fn clamp_kind_explicit_value_set_is_honored_both_ways() {
    let caps = CompletionCaps {
        // A client that DOES support CONSTANT (21) but NOT METHOD (2).
        kind_value_set: Some(vec![CompletionItemKind::CONSTANT]),
        ..CompletionCaps::default()
    };
    assert_eq!(
        clamp_kind(CompletionItemKind::CONSTANT, &caps),
        Some(CompletionItemKind::CONSTANT),
        "a kind in the explicit set is kept even when outside 1..=18"
    );
    assert_eq!(
        clamp_kind(CompletionItemKind::METHOD, &caps),
        None,
        "a kind NOT in the explicit set is dropped even when inside 1..=18"
    );
}

#[test]
fn completion_data_round_trips_compactly_and_carries_no_request_params() {
    // The W18 contract: `data` is a compact symbol key, never the request position/params.
    let cases = [
        CompletionData::Member {
            file: "file:///p/a.gd".to_string(),
            name: "queue_free".to_string(),
            detail: Some("() -> void".to_string()),
        },
        CompletionData::Global {
            name: "print".to_string(),
        },
        CompletionData::NativeClass {
            class: "Node".to_string(),
        },
        CompletionData::Local,
    ];
    for original in cases {
        let json = serde_json::to_value(&original).expect("data serializes");
        // No request-shaped keys leaked in (anti-catalog W18).
        let s = json.to_string();
        for banned in ["position", "textDocument", "uri\":{", "line", "character"] {
            assert!(
                !s.contains(banned),
                "data {s} must not carry request param `{banned}`"
            );
        }
        // It is internally tagged on `k` and round-trips byte-for-byte.
        assert!(json.get("k").is_some(), "data {s} must be tagged on `k`");
        let back: CompletionData = serde_json::from_value(json).expect("data round-trips");
        assert_eq!(back, original);
    }
}

#[test]
fn member_detail_in_data_is_optional_and_omitted_when_absent() {
    // A member with no source-derived detail serializes without a `detail` key (skip_serializing).
    let no_detail = CompletionData::Member {
        file: "file:///a.gd".to_string(),
        name: "x".to_string(),
        detail: None,
    };
    let json = serde_json::to_value(&no_detail).unwrap();
    assert!(
        json.get("detail").is_none(),
        "absent detail must be omitted, got {json}"
    );
    let back: CompletionData = serde_json::from_value(json).unwrap();
    assert_eq!(back, no_detail);
}

#[test]
fn snippet_text_renders_per_style() {
    assert_eq!(
        snippet_text("foo", CallArgumentStyle::ParensWithCursor),
        "foo($0)"
    );
    assert_eq!(snippet_text("foo", CallArgumentStyle::Parens), "foo()");
    // NameOnly never reaches here under the gate, but renders a safe bare name if it does.
    assert_eq!(snippet_text("foo", CallArgumentStyle::NameOnly), "foo");
}

#[test]
fn member_kind_maps_each_variant() {
    use MemberItemKind::*;
    assert_eq!(member_kind(Method), CompletionItemKind::METHOD);
    assert_eq!(member_kind(Property), CompletionItemKind::PROPERTY);
    assert_eq!(member_kind(Signal), CompletionItemKind::EVENT);
    assert_eq!(member_kind(Constant), CompletionItemKind::CONSTANT);
    assert_eq!(member_kind(Enum), CompletionItemKind::ENUM);
    assert_eq!(member_kind(EnumValue), CompletionItemKind::ENUM_MEMBER);
    assert_eq!(member_kind(Class), CompletionItemKind::CLASS);
}

#[test]
fn default_kind_value_set_is_exactly_text_through_reference() {
    let set = default_kind_value_set();
    assert_eq!(set.len(), 18);
    assert_eq!(set[0], CompletionItemKind::TEXT);
    assert_eq!(set[17], CompletionItemKind::REFERENCE);
}

#[test]
fn commit_chars_gated_on_capability() {
    assert_eq!(
        commit_chars(&caps_off()),
        None,
        "no support ⇒ no commit chars"
    );
    let caps = CompletionCaps {
        commit_characters_support: true,
        ..CompletionCaps::default()
    };
    assert_eq!(
        commit_chars(&caps),
        Some(vec![".".to_string(), "(".to_string()])
    );
}
