//! The `extension_api.json` deserialization model.
//!
//! Field names mirror the JSON keys Godot emits (`core/extension/extension_api_dump.cpp`); only
//! the keys M2 consumes are modeled — serde ignores the rest (`hash`, `meta`, ABI size tables, …).
//! These are the raw, *undecoded* shapes; [`crate::native_db`] turns the type strings into
//! [`crate::type_ref::TypeRef`]s and interns names.

use serde::Deserialize;

/// Top-level `extension_api.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct ExtensionApi {
    pub header: Header,
    #[serde(default)]
    pub global_constants: Vec<GlobalConstant>,
    #[serde(default)]
    pub global_enums: Vec<EnumDef>,
    #[serde(default)]
    pub utility_functions: Vec<UtilityFunction>,
    #[serde(default)]
    pub builtin_classes: Vec<BuiltinClass>,
    #[serde(default)]
    pub classes: Vec<ClassDef>,
    #[serde(default)]
    pub singletons: Vec<Singleton>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Header {
    pub version_major: u32,
    pub version_minor: u32,
    pub version_patch: u32,
    #[serde(default)]
    pub version_status: String,
    #[serde(default)]
    pub version_full_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClassDef {
    pub name: String,
    #[serde(default)]
    pub inherits: Option<String>,
    #[serde(default)]
    pub is_refcounted: bool,
    #[serde(default)]
    pub is_instantiable: bool,
    #[serde(default)]
    pub api_type: String,
    #[serde(default)]
    pub methods: Vec<MethodDef>,
    #[serde(default)]
    pub properties: Vec<PropertyDef>,
    #[serde(default)]
    pub signals: Vec<SignalDef>,
    #[serde(default)]
    pub enums: Vec<EnumDef>,
    #[serde(default)]
    pub constants: Vec<ClassConstant>,
    /// One-line summary populated by `godot --dump-extension-api-with-docs`
    /// (`core/extension/extension_api_dump.cpp` writes the `brief_description` key when the build
    /// has the editor doc-strings linked in). Absent in the stock `--dump-extension-api` dump and in
    /// every doc-XML fallback — defaults to empty so the deserializer stays back-compatible with
    /// fixtures that pre-date the WP-H doc-string pull-through.
    #[serde(default)]
    pub brief_description: String,
    /// Long-form class description from the with-docs dump. Same back-compat story as
    /// `brief_description`.
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MethodDef {
    pub name: String,
    #[serde(default)]
    pub is_const: bool,
    #[serde(default)]
    pub is_static: bool,
    #[serde(default)]
    pub is_vararg: bool,
    #[serde(default)]
    pub is_virtual: bool,
    /// Absent ⇒ the method returns `void`.
    #[serde(default)]
    pub return_value: Option<ValueType>,
    #[serde(default)]
    pub arguments: Vec<ArgumentDef>,
    /// Method description from the with-docs dump (see [`ClassDef::description`]).
    #[serde(default)]
    pub description: String,
}

/// `{ "type": "..." }` — used by `return_value`. `type` is a Rust keyword, so it is renamed.
#[derive(Clone, Debug, Deserialize)]
pub struct ValueType {
    #[serde(rename = "type")]
    pub ty: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ArgumentDef {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub default_value: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PropertyDef {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub setter: String,
    #[serde(default)]
    pub getter: String,
    /// Member description from the with-docs dump (see [`ClassDef::description`]).
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SignalDef {
    pub name: String,
    #[serde(default)]
    pub arguments: Vec<ArgumentDef>,
    /// Signal description from the with-docs dump (see [`ClassDef::description`]).
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EnumDef {
    pub name: String,
    #[serde(default)]
    pub is_bitfield: bool,
    /// The enum's own description from the with-docs dump (see [`ClassDef::description`]).
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub values: Vec<EnumValue>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EnumValue {
    pub name: String,
    /// No `#[serde(default)]` here on purpose: every enum value in a real dump carries
    /// `value` (verified across 4.7.2's 5380), and defaulting a malformed dump would
    /// silently fold every constant to `0` — exactly the failure mode the `Exact`
    /// provenance machinery exists to avoid. A missing key is a load error, loudly.
    pub value: i64,
    /// This value's description from the with-docs dump (see [`ClassDef::description`]).
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClassConstant {
    pub name: String,
    #[serde(default)]
    pub value: i64,
    /// The constant's description from the with-docs dump (see [`ClassDef::description`]).
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GlobalConstant {
    pub name: String,
    #[serde(default)]
    pub value: i64,
    #[serde(default)]
    pub is_bitfield: bool,
    /// The constant's description from the with-docs dump (see [`ClassDef::description`]).
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UtilityFunction {
    pub name: String,
    /// Bare type string (not a `{ "type": .. }` object). Absent ⇒ `void`.
    #[serde(default)]
    pub return_type: Option<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub is_vararg: bool,
    #[serde(default)]
    pub arguments: Vec<ArgumentDef>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Singleton {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
}

/// A builtin / Variant type (`Vector2`, `Array`, …). Shape differs from `ClassDef`: bare
/// `return_type` strings, direct `members`, `operators`, `constructors`. `operators` stays
/// unmodeled (serde ignores it); `constructors` feeds the completion arghint surface (#194).
#[derive(Clone, Debug, Deserialize)]
pub struct BuiltinClass {
    pub name: String,
    /// From `--dump-extension-api-with-docs`, exactly as `ClassDef` carries it. Empty when the
    /// dump was taken without docs.
    #[serde(default)]
    pub brief_description: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub is_keyed: bool,
    #[serde(default)]
    pub indexing_return_type: Option<String>,
    #[serde(default)]
    pub members: Vec<BuiltinMember>,
    #[serde(default)]
    pub constants: Vec<BuiltinConstant>,
    #[serde(default)]
    pub enums: Vec<EnumDef>,
    #[serde(default)]
    pub methods: Vec<BuiltinMethod>,
    #[serde(default)]
    pub constructors: Vec<BuiltinConstructor>,
}

/// One constructor overload of a builtin type, as the dump emits it: an optional `arguments` list
/// (the no-arg default constructor omits `arguments`). Mirrors the per-overload `MethodInfo`
/// Godot's `Variant::get_constructor_list` builds (`core/variant/variant_construct.cpp`).
#[derive(Clone, Debug, Deserialize)]
pub struct BuiltinConstructor {
    #[serde(default)]
    pub arguments: Vec<ArgumentDef>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BuiltinMember {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BuiltinConstant {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BuiltinMethod {
    pub name: String,
    /// Bare type string. Absent ⇒ `void`.
    #[serde(default)]
    pub return_type: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub is_const: bool,
    #[serde(default)]
    pub is_static: bool,
    #[serde(default)]
    pub is_vararg: bool,
    #[serde(default)]
    pub arguments: Vec<ArgumentDef>,
}
