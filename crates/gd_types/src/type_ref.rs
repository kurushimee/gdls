//! `TypeRef` — a structured but **unresolved** reference to a GDScript type.
//!
//! The dump (and `doc_classes` XML) encode every type as a string with a small set of prefixes.
//! M2 decodes each once into a `TypeRef` that captures *exactly what the string said* — no more.
//! It deliberately does **not** classify a bare name as builtin-vs-class (that needs the DB, which
//! the analyzer has in M3) and never checks assignability. M3 resolves these against [`NativeDb`]
//! and the project registry.
//!
//! [`NativeDb`]: crate::native_db::NativeDb

use crate::intern::{Interner, Sym};

/// A decoded, unresolved type reference. See module docs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeRef {
    /// `"Variant"` — an untyped value (the dump's `NIL` + `NIL_IS_VARIANT`).
    Variant,
    /// `"void"`, or an absent `return_value` (a method that returns nothing).
    Void,
    /// A bare name: a builtin (`"int"`, `"Vector3"`) **or** a class (`"Node"`). Which one is decided
    /// at resolution time (M3) against the DB — the string alone cannot tell them apart.
    Named(Sym),
    /// `"typedarray::ELEM"` — a typed `Array[ELEM]`.
    TypedArray(Box<TypeRef>),
    /// `"typeddictionary::KEY;VALUE"` — a typed `Dictionary[KEY, VALUE]`.
    TypedDict(Box<TypeRef>, Box<TypeRef>),
    /// `"enum::Name"` (global) or `"enum::Class.Name"` (class-scoped).
    Enum { scope: Option<Sym>, name: Sym },
    /// `"bitfield::Name"` / `"bitfield::Class.Name"`.
    Bitfield { scope: Option<Sym>, name: Sym },
    /// `"void*"` / `"AudioFrame*"` — a pointer to a native struct (rare; appears in low-level APIs).
    Pointer(Box<TypeRef>),
}

/// Decode one dump/XML type string into a [`TypeRef`], interning any names into `it`.
pub fn decode(s: &str, it: &mut Interner) -> TypeRef {
    if let Some(elem) = s.strip_prefix("typedarray::") {
        return TypeRef::TypedArray(Box::new(decode(elem, it)));
    }
    if let Some(kv) = s.strip_prefix("typeddictionary::") {
        // Confirmed `KEY;VALUE` in real data (e.g. `typeddictionary::int;String`).
        let (k, v) = kv.split_once(';').unwrap_or((kv, "Variant"));
        return TypeRef::TypedDict(Box::new(decode(k, it)), Box::new(decode(v, it)));
    }
    if let Some(rest) = s.strip_prefix("enum::") {
        let (scope, name) = split_scoped(rest, it);
        return TypeRef::Enum { scope, name };
    }
    if let Some(rest) = s.strip_prefix("bitfield::") {
        let (scope, name) = split_scoped(rest, it);
        return TypeRef::Bitfield { scope, name };
    }
    if let Some(inner) = s.strip_suffix('*') {
        return TypeRef::Pointer(Box::new(decode(inner, it)));
    }
    match s {
        "Variant" => TypeRef::Variant,
        "void" | "" => TypeRef::Void,
        name => TypeRef::Named(it.intern(name)),
    }
}

/// Split `"Class.Name"` → `(Some(Class), Name)` or `"Name"` → `(None, Name)`.
fn split_scoped(s: &str, it: &mut Interner) -> (Option<Sym>, Sym) {
    match s.split_once('.') {
        Some((scope, name)) => (Some(it.intern(scope)), it.intern(name)),
        None => (None, it.intern(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(it: &mut Interner, s: &str) -> TypeRef {
        TypeRef::Named(it.intern(s))
    }

    #[test]
    fn plain_and_special() {
        let mut it = Interner::new();
        assert_eq!(decode("Variant", &mut it), TypeRef::Variant);
        assert_eq!(decode("void", &mut it), TypeRef::Void);
        assert_eq!(decode("", &mut it), TypeRef::Void);
        assert_eq!(decode("int", &mut it), named(&mut it, "int"));
        assert_eq!(decode("Node", &mut it), named(&mut it, "Node"));
    }

    #[test]
    fn typed_array() {
        let mut it = Interner::new();
        let expected = TypeRef::TypedArray(Box::new(named(&mut it, "Node")));
        assert_eq!(decode("typedarray::Node", &mut it), expected);
    }

    #[test]
    fn typed_dict() {
        let mut it = Interner::new();
        let expected = TypeRef::TypedDict(
            Box::new(named(&mut it, "int")),
            Box::new(named(&mut it, "String")),
        );
        assert_eq!(decode("typeddictionary::int;String", &mut it), expected);
    }

    #[test]
    fn scoped_and_global_enum() {
        let mut it = Interner::new();
        let scoped = decode("enum::AESContext.Mode", &mut it);
        assert_eq!(
            scoped,
            TypeRef::Enum {
                scope: Some(it.intern("AESContext")),
                name: it.intern("Mode"),
            }
        );
        let global = decode("enum::Error", &mut it);
        assert_eq!(
            global,
            TypeRef::Enum {
                scope: None,
                name: it.intern("Error"),
            }
        );
    }

    #[test]
    fn bitfield_and_pointer() {
        let mut it = Interner::new();
        assert_eq!(
            decode("bitfield::Mesh.ArrayFormat", &mut it),
            TypeRef::Bitfield {
                scope: Some(it.intern("Mesh")),
                name: it.intern("ArrayFormat"),
            }
        );
        // `void*` → pointer to void.
        assert_eq!(
            decode("void*", &mut it),
            TypeRef::Pointer(Box::new(TypeRef::Void))
        );
        assert_eq!(
            decode("AudioFrame*", &mut it),
            TypeRef::Pointer(Box::new(named(&mut it, "AudioFrame")))
        );
    }
}
