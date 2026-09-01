//! Godot-editor-style declaration lines for native symbols — the one formatter behind native
//! hover (#35) and the API-stub renderer (#34), so a member reads byte-for-byte the same at the
//! use site and in its materialized API page.
//!
//! Formats are pinned to the editor LSP's `DocumentSymbol::detail` strings
//! (`gdscript_workspace.cpp:246-353` @ 4.6.3-stable):
//!
//! - class:    `<Native> class AudioStreamPlayer extends Node` (extends only when a parent exists)
//! - method:   `func AudioStreamPlayer.stop() -> void` — args render `name: Type`, optional args
//!   append ` = default` (matching upstream's `arg_default_value_started` output on real dumps,
//!   whose defaults are contiguous trailing); vararg appends `...`; an empty return type reads
//!   `void`.
//! - property: `var Input.mouse_mode: MouseMode` — enum-typed properties prefer the enum name,
//!   with a same-class scope trimmed (`Input.MouseMode` hovered on `Input` reads `MouseMode`).
//! - constant: `const Input.MOUSE_MODE_CAPTURED: MouseMode = 2` (`: Enum` only when enum-owned).
//!
//! Two deliberate deviations from upstream, documented here because gd_server is gdls's own
//! layer (the faithful-port discipline binds the frontend, not LSP rendering):
//!
//! - **Signals** render `signal Class.changed(args)` — the truthful GDScript declaration shape.
//!   Upstream lumps signals into its method-likes loop, so its detail reads
//!   `func Class.changed(...) -> void`, an artifact of DocData reuse rather than a design.
//! - **Builtin-class constants** (`Vector2.ZERO`) render without a value: their dump values are
//!   non-scalar constructor literals the ingest deliberately does not evaluate
//!   (`NamedConst::value` is a placeholder there), and a fabricated `= 0` would lie.

use gd_types::{NativeClass, NativeDb, NativeMember, Param, UtilityFn};

/// `<Native> class X extends Y` (gdscript_workspace.cpp:246-250).
pub fn class_detail(db: &NativeDb, class: &NativeClass) -> String {
    let name = db.name_of(class.name);
    match class.inherits {
        Some(parent) => format!("<Native> class {name} extends {}", db.name_of(parent)),
        None => format!("<Native> class {name}"),
    }
}

/// A member's declaration line, qualified with its declaring class (`func Class.name(...)`) —
/// the hover shape. `declaring` is the class `NativeDb::lookup_member` reported.
pub fn member_detail(db: &NativeDb, declaring: &str, member: &NativeMember) -> String {
    member_line(db, declaring, true, member)
}

/// A member's declaration line, unqualified (`func name(...)`) — the stub-body shape (#34).
pub fn member_decl(db: &NativeDb, declaring: &str, member: &NativeMember) -> String {
    member_line(db, declaring, false, member)
}

fn member_line(db: &NativeDb, declaring: &str, qualify: bool, member: &NativeMember) -> String {
    let q = |name: &str| {
        if qualify {
            format!("{declaring}.{name}")
        } else {
            name.to_owned()
        }
    };
    match member {
        NativeMember::Method(m) => {
            let args = args_list(db, &m.params, m.is_vararg, declaring);
            let ret = db.display_type(&m.return_type, Some(declaring));
            format!("func {}({args}) -> {ret}", q(db.name_of(m.name)))
        }
        NativeMember::Property(p) => {
            format!(
                "var {}: {}",
                q(db.name_of(p.name)),
                db.display_type(&p.ty, Some(declaring))
            )
        }
        NativeMember::Signal(s) => {
            let args = args_list(db, &s.params, false, declaring);
            format!("signal {}({args})", q(db.name_of(s.name)))
        }
        NativeMember::Enum(e) => format!("enum {}", q(db.name_of(e.name))),
        NativeMember::Constant(k) => match k.ty {
            // Builtin-class constant: the declared type is real, the ingested value is a
            // placeholder (see module docs) — render the type, omit the value.
            Some(ty) => format!("const {}: {}", q(db.name_of(k.name)), db.name_of(ty)),
            None => format!("const {} = {}", q(db.name_of(k.name)), k.value),
        },
        NativeMember::EnumValue {
            owner, name, value, ..
        } => {
            format!(
                "const {}: {} = {}",
                q(db.name_of(*name)),
                db.name_of(owner.name),
                value
            )
        }
    }
}

/// `func print(...)`-style declaration for a `@GlobalScope` utility function.
pub fn utility_detail(db: &NativeDb, u: &UtilityFn) -> String {
    let args = args_list(db, &u.params, u.is_vararg, "");
    let ret = db.display_type(&u.return_type, None);
    format!("func {}({args}) -> {ret}", db.name_of(u.name))
}

/// The [`utility_detail`] twin for a GDScript-only utility (`len`, `range`, …). Those functions
/// are compiled into the engine and absent from every dump, so the declaration comes from the
/// transcribed table rather than from `db` (#584).
pub fn gdscript_utility_detail(u: &gd_types::GdScriptUtility) -> String {
    format!("func {}({}) -> {}", u.name, u.params, u.return_type)
}

/// `name: Type = default, …` — every argument carrying a dump default renders it. Real dumps
/// have contiguous trailing defaults, so this emits byte-for-byte what upstream's
/// `arg_default_value_started` loop (gdscript_workspace.cpp:323-341) does, without its
/// stateful flag. Vararg appends `...`.
fn args_list(db: &NativeDb, params: &[Param], is_vararg: bool, declaring: &str) -> String {
    let trim = (!declaring.is_empty()).then_some(declaring);
    let mut parts: Vec<String> = Vec::with_capacity(params.len() + 1);
    for p in params {
        let mut part = format!("{}: {}", db.name_of(p.name), db.display_type(&p.ty, trim));
        if let Some(dv) = p.default_value {
            part.push_str(" = ");
            part.push_str(db.name_of(dv));
        }
        parts.push(part);
    }
    if is_vararg {
        parts.push("...".to_owned());
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One dump exercising every format arm: defaults-tail, vararg, enum-typed property and
    /// return, enum-owned constant, builtin constant, signal args.
    fn db() -> NativeDb {
        NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "builtin_classes": [
                    {"name": "Vector2", "is_keyed": false,
                     "constants": [{"name": "ZERO", "type": "Vector2", "value": "Vector2(0, 0)"}],
                     "methods": [{"name": "length", "is_const": true, "is_static": false,
                                  "is_vararg": false, "return_type": "float", "arguments": []}]}
                ],
                "classes": [
                    {"name": "Object"},
                    {"name": "Input", "inherits": "Object",
                     "properties": [{"name": "mouse_mode", "type": "enum::Input.MouseMode",
                                     "setter": "set_mouse_mode", "getter": "get_mouse_mode"}],
                     "enums": [{"name": "MouseMode", "is_bitfield": false,
                                "values": [{"name": "MOUSE_MODE_CAPTURED", "value": 2}]}],
                     "constants": [{"name": "NOTIF", "value": 7}],
                     "signals": [{"name": "joy_connection_changed",
                                  "arguments": [{"name": "device", "type": "int"},
                                                 {"name": "connected", "type": "bool"}]}],
                     "methods": [
                        {"name": "play", "is_const": false, "is_static": false, "is_vararg": false,
                         "is_virtual": false, "hash": 1,
                         "arguments": [{"name": "from", "type": "float", "default_value": "0.0"}]},
                        {"name": "call_group", "is_const": false, "is_static": false,
                         "is_vararg": true, "is_virtual": false, "hash": 2,
                         "arguments": [{"name": "group", "type": "StringName"}]},
                        {"name": "get_mode", "is_const": true, "is_static": false,
                         "is_vararg": false, "is_virtual": false, "hash": 3,
                         "return_value": {"type": "enum::Input.MouseMode"}, "arguments": []}
                     ]}
                ],
                "utility_functions": [
                    {"name": "print", "category": "general", "is_vararg": true, "arguments": []},
                    {"name": "clampi", "return_type": "int", "category": "math", "is_vararg": false,
                     "arguments": [{"name": "value", "type": "int"}, {"name": "min", "type": "int"},
                                    {"name": "max", "type": "int"}]}
                ]
            }"#,
        )
        .expect("valid render-test dump")
    }

    fn detail(db: &NativeDb, class: &str, member: &str) -> String {
        let (decl, m) = db.lookup_member(class, member).expect("member resolves");
        member_detail(db, db.name_of(decl.name), &m)
    }

    #[test]
    fn class_line_with_and_without_parent() {
        let db = db();
        assert_eq!(
            class_detail(&db, db.class_named("Input").unwrap()),
            "<Native> class Input extends Object"
        );
        assert_eq!(
            class_detail(&db, db.class_named("Object").unwrap()),
            "<Native> class Object"
        );
    }

    #[test]
    fn method_lines_cover_void_defaults_vararg_and_enum_return() {
        let db = db();
        assert_eq!(
            detail(&db, "Input", "play"),
            "func Input.play(from: float = 0.0) -> void"
        );
        assert_eq!(
            detail(&db, "Input", "call_group"),
            "func Input.call_group(group: StringName, ...) -> void"
        );
        // Same-class enum scope trims on the declaring class.
        assert_eq!(
            detail(&db, "Input", "get_mode"),
            "func Input.get_mode() -> MouseMode"
        );
    }

    #[test]
    fn property_signal_enum_and_constant_lines() {
        let db = db();
        assert_eq!(
            detail(&db, "Input", "mouse_mode"),
            "var Input.mouse_mode: MouseMode"
        );
        assert_eq!(
            detail(&db, "Input", "joy_connection_changed"),
            "signal Input.joy_connection_changed(device: int, connected: bool)"
        );
        assert_eq!(detail(&db, "Input", "MouseMode"), "enum Input.MouseMode");
        assert_eq!(
            detail(&db, "Input", "MOUSE_MODE_CAPTURED"),
            "const Input.MOUSE_MODE_CAPTURED: MouseMode = 2"
        );
        assert_eq!(detail(&db, "Input", "NOTIF"), "const Input.NOTIF = 7");
    }

    #[test]
    fn builtin_constant_renders_type_without_placeholder_value() {
        let db = db();
        let (bt, m) = db.lookup_builtin_member("Vector2", "ZERO").expect("ZERO");
        assert_eq!(
            member_detail(&db, db.name_of(bt.name), &m),
            "const Vector2.ZERO: Vector2"
        );
    }

    #[test]
    fn unqualified_decl_drops_the_class_prefix() {
        let db = db();
        let (decl, m) = db.lookup_member("Input", "play").expect("play");
        assert_eq!(
            member_decl(&db, db.name_of(decl.name), &m),
            "func play(from: float = 0.0) -> void"
        );
    }

    #[test]
    fn utility_lines_cover_vararg_and_typed() {
        let db = db();
        assert_eq!(
            utility_detail(&db, db.utility("print").unwrap()),
            "func print(...) -> void"
        );
        assert_eq!(
            utility_detail(&db, db.utility("clampi").unwrap()),
            "func clampi(value: int, min: int, max: int) -> int"
        );
    }

    #[test]
    fn gdscript_utility_lines_read_like_the_dump_backed_ones() {
        let line = |name: &str| {
            gdscript_utility_detail(gd_types::gdscript_utility(name).expect("registered"))
        };
        // Vararg, no-arg-with-a-value, trailing default, and the registered `char` name that
        // `REGISTER_FUNC` derived by stripping `_char`'s underscore.
        assert_eq!(line("range"), "func range(...) -> Array");
        assert_eq!(line("get_stack"), "func get_stack() -> Array");
        assert_eq!(line("print_stack"), "func print_stack() -> void");
        assert_eq!(
            line("Color8"),
            "func Color8(r8: int, g8: int, b8: int, a8: int = 255) -> Color"
        );
        assert_eq!(line("char"), "func char(code: int) -> String");
        assert_eq!(line("len"), "func len(var: Variant) -> int");
        assert!(
            gd_types::gdscript_utility("_char").is_none(),
            "`_char` is the C++ symbol, never a callable name"
        );
    }
}
