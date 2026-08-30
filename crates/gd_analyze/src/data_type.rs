//! The resolved GDScript type lattice — a faithful port of `GDScriptParser::DataType`
//! (Godot's `modules/gdscript/gdscript_parser.h:101-259`).
//!
//! This lives in `gd_analyze`, **not** `gd_types`, because the `Class`/`Script` kinds reference a
//! *project* GDScript class — a [`gd_project::FileId`] — and `gd_types` neither does nor (per the
//! crate DAG) may depend on `gd_project`. `gd_syntax`'s `Node::datatype` placeholder stays empty;
//! resolved types are written into a side table ([`crate::typetable::TypeTable`]) keyed by `NodeId`.

use std::collections::HashMap;

use gd_project::FileId;
use gd_syntax::ast::NodeId;

/// Mirror of Godot's `Variant::Type` (`core/variant/variant.h:102`), in declaration order.
///
/// Grepped from Godot at port time: 39 types (`Nil`=0 … `PackedVector4Array`=38), with
/// `VARIANT_MAX` = 39. (The design docs and an earlier sketch were off by one here — Godot has a
/// `PACKED_VECTOR4_ARRAY` the older numbering omitted; the `const` guard below pins the count.)
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VariantType {
    #[default]
    Nil = 0,
    // atomic types
    Bool,
    Int,
    Float,
    String,
    // math types
    Vector2,
    Vector2i,
    Rect2,
    Rect2i,
    Vector3,
    Vector3i,
    Transform2d,
    Vector4,
    Vector4i,
    Plane,
    Quaternion,
    Aabb,
    Basis,
    Transform3d,
    Projection,
    // misc types
    Color,
    StringName,
    NodePath,
    Rid,
    Object,
    Callable,
    Signal,
    Dictionary,
    Array,
    // packed arrays
    PackedByteArray,
    PackedInt32Array,
    PackedInt64Array,
    PackedFloat32Array,
    PackedFloat64Array,
    PackedStringArray,
    PackedVector2Array,
    PackedVector3Array,
    PackedColorArray,
    PackedVector4Array,
}

// Anchors the enum to Godot's `VARIANT_MAX` (39): the last type's discriminant is 38.
const _: () = assert!(VariantType::PackedVector4Array as u8 == 38);

/// `Variant::can_convert(from, to)` (`core/variant/variant.cpp:192`): whether a value of `from`
/// can be implicitly converted to `to`. Ported verbatim from Godot's per-target-type tables.
/// Used by `reduce_cast` (analyzer.cpp:3807) and by `is_type_compatible` when
/// `allow_implicit_conversion = true`.
pub fn variant_can_convert(from: VariantType, to: VariantType) -> bool {
    use VariantType::*;
    if from == to {
        return true;
    }
    if to == Nil {
        return true; // "nil can convert to anything" — variant.cpp:196.
    }
    if from == Nil {
        return to == Object;
    }

    // Per-target valid/invalid type tables. `Some(valid)` ⇒ accept only those; `Some(_, invalid)` ⇒
    // accept everything except those; `None` ⇒ no implicit conversion (just the identity above).
    //
    // NOTE: this is the LENIENT `can_convert` table (variant.cpp:192), used by `reduce_cast` per
    // analyzer.cpp:3807. The analyzer's *type-compatibility* path
    // (`check_type_compatibility` at analyzer.cpp:6328) uses the STRICTER
    // [`variant_can_convert_strict`] below, which omits the String/Int/Float/Bool cross-conversion
    // relaxations the runtime allows.
    let valid: &[VariantType] = match to {
        Bool => &[Int, Float, String],
        Int => &[Bool, Float, String],
        Float => &[Bool, Int, String],
        String => return !matches!(from, Object), // STRING uses an invalid-list at variant.cpp:241.
        Vector2 => &[Vector2i],
        Vector2i => &[Vector2],
        Rect2 => &[Rect2i],
        Rect2i => &[Rect2],
        Transform2d => &[Transform3d],
        Vector3 => &[Vector3i],
        Vector3i => &[Vector3],
        Vector4 => &[Vector4i],
        Vector4i => &[Vector4],
        Quaternion => &[Basis],
        Basis => &[Quaternion],
        Transform3d => &[Transform2d, Quaternion, Basis, Projection],
        Projection => &[Transform3d],
        Color => &[String, Int],
        Rid => &[Object],
        Object => &[],
        StringName => &[String],
        NodePath => &[String],
        Array => &[
            PackedByteArray,
            PackedInt32Array,
            PackedInt64Array,
            PackedFloat32Array,
            PackedFloat64Array,
            PackedStringArray,
            PackedColorArray,
            PackedVector2Array,
            PackedVector3Array,
            PackedVector4Array,
        ],
        PackedByteArray | PackedInt32Array | PackedInt64Array | PackedFloat32Array
        | PackedFloat64Array | PackedStringArray | PackedVector2Array | PackedVector3Array
        | PackedColorArray | PackedVector4Array => &[Array],
        // Targets not in Godot's switch (Plane, AABB, Callable, Signal, Dictionary, Nil) fall
        // through the C++ `default` arm — both valid_types and invalid_types stay null, and the
        // function returns false (variant.cpp:507-532).
        _ => return false,
    };
    valid.contains(&from)
}

/// `Variant::can_convert_strict(from, to)` (`core/variant/variant.cpp:535`). The
/// **analyzer-side** check (analyzer.cpp:6328 `check_type_compatibility`) — tighter than
/// [`variant_can_convert`]: STRING isn't a valid source for BOOL/INT/FLOAT, STRING's valid
/// sources are limited to NODE_PATH and STRING_NAME, and the matrix tables drop most of the
/// runtime's permissive `can_convert` relaxations. Strict compatibility is what the corpus's
/// typed-collection family pins (`Cannot have an element of type "String" in an array of type
/// "Array[int]".`).
pub fn variant_can_convert_strict(from: VariantType, to: VariantType) -> bool {
    use VariantType::*;
    if from == to {
        return true;
    }
    if to == Nil {
        return true;
    }
    if from == Nil {
        return to == Object;
    }
    let valid: &[VariantType] = match to {
        Bool => &[Int, Float],
        Int => &[Bool, Float],
        Float => &[Bool, Int],
        String => &[NodePath, StringName],
        Vector2 => &[Vector2i],
        Vector2i => &[Vector2],
        Rect2 => &[Rect2i],
        Rect2i => &[Rect2],
        Transform2d => &[Transform3d],
        Vector3 => &[Vector3i],
        Vector3i => &[Vector3],
        Vector4 => &[Vector4i],
        Vector4i => &[Vector4],
        Quaternion => &[Basis],
        Basis => &[Quaternion],
        Transform3d => &[Transform2d, Quaternion, Basis, Projection],
        Projection => &[Transform3d],
        Color => &[String, Int],
        Rid => &[Object],
        Object => &[],
        StringName => &[String],
        NodePath => &[String],
        Array => &[
            PackedByteArray,
            PackedInt32Array,
            PackedInt64Array,
            PackedFloat32Array,
            PackedFloat64Array,
            PackedStringArray,
            PackedColorArray,
            PackedVector2Array,
            PackedVector3Array,
            PackedVector4Array,
        ],
        PackedByteArray | PackedInt32Array | PackedInt64Array | PackedFloat32Array
        | PackedFloat64Array | PackedStringArray | PackedVector2Array | PackedVector3Array
        | PackedColorArray | PackedVector4Array => &[Array],
        // Targets not in Godot's switch (Plane, AABB, Callable, Signal, Dictionary, Nil,
        // PackedByteArray's inverse pairs, ...) reach the C++ `default` arm at variant.cpp:850
        // — valid_types stays null and the function returns false.
        _ => return false,
    };
    valid.contains(&from)
}

/// `_variant_type_to_typed_array_element_type` (gdscript_parser.cpp:5508-5530), the table behind
/// `DataType::is_typed_container_type()` / `get_typed_container_type()` (gdscript_parser.cpp:5532/
/// 5536). A `Packed*Array`'s fixed element type; `None` for everything that isn't a packed array.
/// Consumed by `resolve_for`'s iterator typing (analyzer.cpp:2293-2295); the indexed-subscript
/// matrix (analyzer.cpp:5057-5101) shares the same mapping.
pub fn typed_container_element(t: VariantType) -> Option<VariantType> {
    use VariantType::*;
    Some(match t {
        PackedByteArray | PackedInt32Array | PackedInt64Array => Int,
        PackedFloat32Array | PackedFloat64Array => Float,
        PackedStringArray => String,
        PackedVector2Array => Vector2,
        PackedVector3Array => Vector3,
        PackedColorArray => Color,
        PackedVector4Array => Vector4,
        _ => return None,
    })
}

/// `_variant_type_to_typed_array_element_type` (`gdscript_parser.cpp:5496`) reduced to the
/// predicate `DataType::is_typed_container_type` (`:5520`) actually asks: is this one of the
/// packed arrays? Those are the types whose native-property getters hand back a *copy*.
pub fn is_packed_array(t: VariantType) -> bool {
    use VariantType::*;
    matches!(
        t,
        PackedByteArray
            | PackedInt32Array
            | PackedInt64Array
            | PackedFloat32Array
            | PackedFloat64Array
            | PackedStringArray
            | PackedVector2Array
            | PackedVector3Array
            | PackedColorArray
            | PackedVector4Array
    )
}

/// `Variant::get_type_name(p_type)` (`core/variant/variant.cpp:43`). Used in Godot's verbatim
/// "Invalid operands to operator …" error message (analyzer.cpp:3130). Lowercase for the atomic
/// types (`bool`/`int`/`float`/`Nil`) and capitalized for the rest, matching Godot exactly.
pub fn variant_type_name(t: VariantType) -> &'static str {
    use VariantType::*;
    match t {
        Nil => "Nil",
        Bool => "bool",
        Int => "int",
        Float => "float",
        String => "String",
        Vector2 => "Vector2",
        Vector2i => "Vector2i",
        Rect2 => "Rect2",
        Rect2i => "Rect2i",
        Vector3 => "Vector3",
        Vector3i => "Vector3i",
        Transform2d => "Transform2D",
        Vector4 => "Vector4",
        Vector4i => "Vector4i",
        Plane => "Plane",
        Quaternion => "Quaternion",
        Aabb => "AABB",
        Basis => "Basis",
        Transform3d => "Transform3D",
        Projection => "Projection",
        Color => "Color",
        StringName => "StringName",
        NodePath => "NodePath",
        Rid => "RID",
        Object => "Object",
        Callable => "Callable",
        Signal => "Signal",
        Dictionary => "Dictionary",
        Array => "Array",
        PackedByteArray => "PackedByteArray",
        PackedInt32Array => "PackedInt32Array",
        PackedInt64Array => "PackedInt64Array",
        PackedFloat32Array => "PackedFloat32Array",
        PackedFloat64Array => "PackedFloat64Array",
        PackedStringArray => "PackedStringArray",
        PackedVector2Array => "PackedVector2Array",
        PackedVector3Array => "PackedVector3Array",
        PackedColorArray => "PackedColorArray",
        PackedVector4Array => "PackedVector4Array",
    }
}

/// `DataType::Kind` (`gdscript_parser.h:105`), 8 variants in Godot's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DtKind {
    /// A `Variant` builtin type (`int`, `Vector3`, `Array`, …); see [`DataType::builtin_type`].
    Builtin,
    /// A `ClassDB` engine/GDExtension class; see [`DataType::native_type`].
    Native,
    /// An external GDScript file; see [`DataType::script_type`].
    Script,
    /// An in-file GDScript class. Held as a transient [`NodeId`] valid only during this file's
    /// analysis — rewritten to a [`ScriptRef`] (`kind = Script`) before the result escapes `analyze`.
    Class,
    /// A named or anonymous enum (`native_type` = base, `enum_type` = name; values in `enum_values`).
    Enum,
    /// Untyped / can be any type.
    Variant,
    /// Cycle-detection sentinel: this type is currently being resolved.
    Resolving,
    #[default]
    Unresolved,
}

/// `DataType::TypeSource` (`gdscript_parser.h:117`). `is_hard_type()` ⟺ `source > Inferred` — the
/// gradual-typing gate that drives every `UNSAFE_*` warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, PartialOrd, Ord)]
pub enum TypeSource {
    /// `has_no_type()` — can be any type.
    #[default]
    Undetected,
    /// Inferred, but still dynamic.
    Inferred,
    /// Static type derived from a `:=` assigned value.
    AnnotatedInferred,
    /// Explicit `: T` annotation.
    AnnotatedExplicit,
}

/// Identifies an external GDScript class: the file plus an optional inner-class chain
/// (`extends Outer.Inner` → `inner = ["Inner"]`). Replaces Godot's `Ref<Script>` + `script_path`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScriptRef {
    pub file: FileId,
    /// Inner-class chain; empty for the file's top-level class.
    pub inner: Vec<String>,
}

/// A method signature carried inline by `Callable`/`Signal` types (Godot's `MethodInfo` slice).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MethodSig {
    pub name: String,
    pub params: Vec<(String, DataType)>,
    pub return_type: Box<DataType>,
}

/// The resolved type of a node — the analyzer's central currency.
///
/// Equality is **not** derived: Godot's `operator==` deliberately treats `Undetected`/`Inferred`
/// sources as "equal for parsing purposes", which is not a lawful `Eq`. [`DataType::equiv`] ports
/// that loose comparison and backs the `PartialEq` impl; the type is intentionally not `Eq`.
#[derive(Clone, Debug, Default)]
pub struct DataType {
    pub kind: DtKind,
    pub type_source: TypeSource,

    pub is_constant: bool,
    pub is_read_only: bool,
    /// The type *itself* as a value (the class object), not an instance of it.
    pub is_meta_type: bool,
    /// A global name that can't be used standalone (e.g. a native enum metatype).
    pub is_pseudo_type: bool,
    /// A call result that is a coroutine (drives `await`).
    pub is_coroutine: bool,

    /// Meaningful when `kind == Builtin`.
    pub builtin_type: VariantType,
    /// The class name for `Native`/`Enum` kinds (Godot's `StringName native_type`).
    pub native_type: String,
    /// The enum name (or value name) for `kind == Enum`.
    pub enum_type: String,
    /// External-script payload for `kind == Script`.
    pub script_type: Option<ScriptRef>,
    /// In-file class payload for `kind == Class` — transient (see [`DtKind::Class`]).
    pub class_node: Option<NodeId>,
    /// Signature payload for `Callable`/`Signal` builtins.
    pub method_sig: Option<Box<MethodSig>>,
    /// Members of an `Enum` kind.
    pub enum_values: HashMap<String, i64>,
    /// `Enum` kind only: at least one of [`Self::enum_values`] is a placeholder, not the real
    /// declared integer (a cross-file enum whose value expression the interface extractor could
    /// not read). Value-dependent diagnostics (INT_AS_ENUM_WITHOUT_MATCH,
    /// ENUM_VARIABLE_WITHOUT_DEFAULT) must skip when set — membership lookups stay valid.
    /// Analyzer-internal; never serialized.
    pub enum_values_inexact: bool,
    /// `Array[T]` → `[T]`; `Dictionary[K, V]` → `[K, V]`. Empty = unparameterized.
    pub container_element_types: Vec<DataType>,
    /// #355: the name [`Display`] renders for a `Script`/`Class` kind — the payload Godot's
    /// `DataType` carries directly (`class_type->identifier->name`, else `class_type->fqcn`;
    /// `gdscript_parser.cpp:5354-5358`) and gdls's `FileId`/`NodeId` cannot reach.
    ///
    /// Godot's `to_string()` is total on the value because the value owns what it needs to render.
    /// gdls substituted opaque ids for those pointers, and every message that names a project type
    /// then had to route through a context-aware helper to get a real name back — a fail-open
    /// arrangement that leaked `<Script #3>` into user-facing errors four separate times. Carrying
    /// the name restores Godot's property.
    ///
    /// Empty for every other kind, and for a type built outside an analysis (`gd_server`'s
    /// navigation-only types), where [`Display`] keeps its bracketed placeholder — a value with no
    /// name shows its seams rather than inventing one.
    pub display_name: String,
}

impl DataType {
    /// `DataType::is_typed_container_type` (`gdscript_parser.cpp:5520`): a builtin packed array.
    pub fn is_typed_container_type(&self) -> bool {
        self.kind == DtKind::Builtin && is_packed_array(self.builtin_type)
    }

    /// Godot `is_set()`: a determined kind (not `Resolving`/`Unresolved`).
    #[inline]
    pub fn is_set(&self) -> bool {
        !matches!(self.kind, DtKind::Resolving | DtKind::Unresolved)
    }

    /// Godot `is_resolving()`.
    #[inline]
    pub fn is_resolving(&self) -> bool {
        self.kind == DtKind::Resolving
    }

    /// Godot `has_no_type()`: an `Undetected` source.
    #[inline]
    pub fn has_no_type(&self) -> bool {
        self.type_source == TypeSource::Undetected
    }

    /// Godot `is_variant()`: `Variant`/`Resolving`/`Unresolved` all read as dynamic.
    #[inline]
    pub fn is_variant(&self) -> bool {
        matches!(
            self.kind,
            DtKind::Variant | DtKind::Resolving | DtKind::Unresolved
        )
    }

    /// Godot `is_hard_type()`: `source > Inferred`. Only hard types trigger compatibility errors;
    /// soft types emit `UNSAFE_*` warnings instead.
    #[inline]
    pub fn is_hard_type(&self) -> bool {
        self.type_source > TypeSource::Inferred
    }

    /// Payload-by-kind consistency check. Each [`DtKind`] is associated with a specific subset of
    /// the payload fields ([`Self::builtin_type`] for `Builtin`, [`Self::script_type`] for `Script`,
    /// etc.). This helper checks the inverse direction — for a given kind, **no out-of-band payload
    /// is set** — and returns `true` when the type's shape is consistent with its kind.
    ///
    /// The check is intentionally permissive about `Variant`/`Resolving`/`Unresolved` shapes, since
    /// Godot constructs these mid-pass with stale payload from a previous kind assignment that
    /// hasn't been cleared yet (e.g. `kind = Resolving` placeholders, the `Class → Script` rewrite
    /// in the final pass). It is **not** wired into a `debug_assert!` for that reason — adding one
    /// would risk false trips during faithful-port intermediate states. Use it in new tests that
    /// want to pin a *finished* type's shape, or in future refactors that want to enforce the
    /// invariant at a known-safe call site.
    #[must_use]
    pub fn kind_consistent(&self) -> bool {
        use DtKind::*;
        match self.kind {
            Builtin => {
                // `Builtin` carries `builtin_type`; the script/class/enum payloads should be empty.
                self.script_type.is_none()
                    && self.class_node.is_none()
                    && self.enum_values.is_empty()
            }
            Native => {
                // `Native` carries `native_type`; not `script_type`/`class_node`.
                !self.native_type.is_empty()
                    && self.script_type.is_none()
                    && self.class_node.is_none()
            }
            Script => self.script_type.is_some() && self.class_node.is_none(),
            Class => self.class_node.is_some(),
            Enum => {
                // `Enum` carries either a name on `native_type` (Global.Enum) or just `enum_type`.
                !(self.native_type.is_empty() && self.enum_type.is_empty())
            }
            // Variant / Resolving / Unresolved are placeholders — no payload invariants to assert.
            Variant | Resolving | Unresolved => true,
        }
    }

    /// `GDScriptParser::DataType::to_string()` for diagnostic messages. The full Godot rendering
    /// handles container element types, scripts, enums; this WP-E3c slice covers the cases the
    /// analyzer emits errors on (builtin, native, script, class metatype, enum, variant) — enough
    /// to format the cast/operator error families. Container element types (`Array[T]`) join with
    /// the typed-collection reducers later in E3.
    /// `get_variant_type()` — the default element type for an unparameterized container.
    pub fn variant() -> Self {
        DataType {
            kind: DtKind::Variant,
            type_source: TypeSource::Inferred,
            ..Default::default()
        }
    }

    /// Godot `operator==` (`gdscript_parser.h:196`): `Undetected`/`Inferred` compare equal to anything
    /// "for parsing purposes"; otherwise compare by kind-specific payload.
    pub fn equiv(&self, other: &DataType) -> bool {
        if self.has_no_type() || other.has_no_type() {
            return true;
        }
        if self.type_source == TypeSource::Inferred || other.type_source == TypeSource::Inferred {
            return true;
        }
        if self.kind != other.kind {
            return false;
        }
        match self.kind {
            DtKind::Variant => true,
            DtKind::Builtin => self.builtin_type == other.builtin_type,
            DtKind::Native | DtKind::Enum => self.native_type == other.native_type,
            DtKind::Script => self.script_type == other.script_type,
            DtKind::Class => self.class_node == other.class_node,
            DtKind::Resolving | DtKind::Unresolved => false,
        }
    }
}

impl PartialEq for DataType {
    fn eq(&self, other: &Self) -> bool {
        self.equiv(other)
    }
}

impl std::fmt::Display for DataType {
    /// `GDScriptParser::DataType::to_string()` for diagnostic messages. The full Godot rendering
    /// handles container element types, scripts, enums; this WP-E3c slice covers the cases the
    /// analyzer emits errors on (builtin, native, script, class metatype, enum, variant) — enough
    /// to format the cast/operator error families. Container element types (`Array[T]`) join with
    /// the typed-collection reducers later in E3.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            DtKind::Variant => f.write_str("Variant"),
            DtKind::Builtin => {
                // gdscript_parser.cpp:5327-5331 — Array/Dictionary with container element types
                // render as `Array[T]` / `Dictionary[K, V]`. Unparameterized variants fall through
                // to the bare builtin name.
                match self.builtin_type {
                    // gdscript_parser.cpp:5341 — a builtin `NIL` renders as `null`, not as
                    // `Variant::get_type_name(NIL)`'s `"Nil"`. Same at both supported tags.
                    VariantType::Nil => f.write_str("null"),
                    VariantType::Array if !self.container_element_types.is_empty() => {
                        write!(f, "Array[{}]", self.container_element_types[0])
                    }
                    // A `Dictionary` renders parameterized as soon as EITHER slot is set, and
                    // an unset slot reads back as `Variant`
                    // (`get_container_element_type_or_variant`), so
                    // `Dictionary[int, Variant]` — which only ever fills slot 0 — still prints
                    // both halves.
                    VariantType::Dictionary if !self.container_element_types.is_empty() => {
                        let slot = |i: usize| match self.container_element_types.get(i) {
                            Some(t) => t.to_string(),
                            None => "Variant".to_owned(),
                        };
                        write!(f, "Dictionary[{}, {}]", slot(0), slot(1))
                    }
                    _ => f.write_str(variant_type_name(self.builtin_type)),
                }
            }
            // gdscript_parser.cpp:5351-5353 — the class OBJECT, not an instance of it, renders as
            // the engine's own wrapper class name. `var e: int = Node` reads "a value of type
            // GDScriptNativeClass", never "of type Node" (which would describe an instance).
            DtKind::Native if self.is_meta_type => f.write_str("GDScriptNativeClass"),
            DtKind::Native => f.write_str(&self.native_type),
            // gdscript_parser.cpp:5354-5358, the CLASS arm — which is what gdls's `Script` kind is:
            // `make_global_class_meta_type` hands back the depended parser's head CLASS type, and
            // Godot's SCRIPT kind proper only arises for non-GDScript scripts, which gdls has none
            // of. `display_name` carries the identifier (else the fqcn); the bracketed placeholders
            // remain for a type built with no name to render.
            DtKind::Script if !self.display_name.is_empty() => f.write_str(&self.display_name),
            DtKind::Script => match &self.script_type {
                Some(s) if s.inner.is_empty() => write!(f, "<Script #{}>", s.file.get()),
                Some(s) => write!(f, "<Script #{}>.{}", s.file.get(), s.inner.join(".")),
                None => f.write_str("<Script>"),
            },
            DtKind::Class if !self.display_name.is_empty() => f.write_str(&self.display_name),
            DtKind::Class => f.write_str("<Class>"),
            DtKind::Enum => {
                // analyzer.cpp:5361 — `String(native_type).get_file()`. Godot's `String::get_file()`
                // strips any leading `<dir>/` and keeps the basename, so `res://outer.gd.Inner` reads
                // back as `outer.gd.Inner` while bare names (`TileSet.TileShape`) pass through unchanged.
                let name = if self.native_type.is_empty() {
                    self.enum_type.as_str()
                } else {
                    let raw = self.native_type.as_str();
                    match raw.rfind('/') {
                        Some(i) => &raw[i + 1..],
                        None => raw,
                    }
                };
                f.write_str(name)
            }
            DtKind::Resolving | DtKind::Unresolved => f.write_str("<unresolved>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_type_gate() {
        let mut dt = DataType {
            kind: DtKind::Builtin,
            builtin_type: VariantType::Int,
            type_source: TypeSource::AnnotatedExplicit,
            ..Default::default()
        };
        assert!(dt.is_hard_type());
        dt.type_source = TypeSource::Inferred;
        assert!(!dt.is_hard_type());
        dt.type_source = TypeSource::AnnotatedInferred;
        assert!(dt.is_hard_type(), "`:=` inference is still a hard type");
    }

    #[test]
    fn unresolved_and_variant_read_as_dynamic() {
        assert!(DataType::default().is_variant()); // default kind = Unresolved
        assert!(!DataType::default().is_set());
        assert!(DataType::variant().is_variant());
        assert!(DataType::variant().is_set());
    }

    #[test]
    fn equiv_is_loose_on_soft_types() {
        let int = DataType {
            kind: DtKind::Builtin,
            builtin_type: VariantType::Int,
            type_source: TypeSource::AnnotatedExplicit,
            ..Default::default()
        };
        let float = DataType {
            kind: DtKind::Builtin,
            builtin_type: VariantType::Float,
            type_source: TypeSource::AnnotatedExplicit,
            ..Default::default()
        };
        assert_ne!(int, float); // two hard, distinct builtins differ
        assert_eq!(int, int.clone());
        // An undetected/inferred operand compares equal to anything (Godot operator==).
        assert_eq!(int, DataType::default());
        assert_eq!(int, DataType::variant());
    }

    #[test]
    fn kind_consistent_accepts_well_formed_types() {
        // Variant placeholders are always consistent.
        assert!(DataType::variant().kind_consistent());
        assert!(DataType::default().kind_consistent());

        // A finished Builtin: only the builtin_type slot populated.
        let int = DataType {
            kind: DtKind::Builtin,
            builtin_type: VariantType::Int,
            type_source: TypeSource::AnnotatedExplicit,
            ..Default::default()
        };
        assert!(int.kind_consistent());

        // Native carries a native_type.
        let node = DataType {
            kind: DtKind::Native,
            type_source: TypeSource::AnnotatedExplicit,
            builtin_type: VariantType::Object,
            native_type: "Node".to_owned(),
            ..Default::default()
        };
        assert!(node.kind_consistent());

        // Script carries a script_type.
        let script = DataType {
            kind: DtKind::Script,
            type_source: TypeSource::AnnotatedExplicit,
            script_type: Some(ScriptRef {
                file: FileId::new(1),
                inner: vec![],
            }),
            ..Default::default()
        };
        assert!(script.kind_consistent());
    }

    #[test]
    fn kind_consistent_catches_payload_leaks() {
        // A `Native` without a native_type isn't consistent.
        let bad_native = DataType {
            kind: DtKind::Native,
            type_source: TypeSource::AnnotatedExplicit,
            ..Default::default()
        };
        assert!(!bad_native.kind_consistent());

        // A `Script` without a script_type.
        let bad_script = DataType {
            kind: DtKind::Script,
            type_source: TypeSource::AnnotatedExplicit,
            ..Default::default()
        };
        assert!(!bad_script.kind_consistent());

        // A `Class` without a class_node.
        let bad_class = DataType {
            kind: DtKind::Class,
            ..Default::default()
        };
        assert!(!bad_class.kind_consistent());
    }

    /// #355 — every `Display` arm that reads [`DataType::display_name`], plus the placeholder a
    /// nameless value keeps.
    #[test]
    fn script_and_class_kinds_render_their_carried_name() {
        let named = |kind, name: &str| DataType {
            kind,
            script_type: Some(ScriptRef {
                file: FileId::new(7),
                inner: Vec::new(),
            }),
            display_name: name.to_owned(),
            ..Default::default()
        };
        assert_eq!(named(DtKind::Script, "Lib1").to_string(), "Lib1");
        assert_eq!(
            named(DtKind::Script, "res://src/probe.gd").to_string(),
            "res://src/probe.gd"
        );
        assert_eq!(named(DtKind::Class, "In").to_string(), "In");

        // A value built outside an analysis carries no name, so it shows its seams rather than
        // inventing one.
        assert_eq!(named(DtKind::Script, "").to_string(), "<Script #7>");
        assert_eq!(named(DtKind::Class, "").to_string(), "<Class>");
    }

    /// A native class as a *value* is `GDScriptNativeClass`; as a type it is its own name.
    #[test]
    fn a_native_metatype_renders_as_gdscriptnativeclass() {
        let mut dt = DataType {
            kind: DtKind::Native,
            native_type: "Node".to_owned(),
            ..Default::default()
        };
        assert_eq!(dt.to_string(), "Node");
        dt.is_meta_type = true;
        assert_eq!(dt.to_string(), "GDScriptNativeClass");
    }
}
