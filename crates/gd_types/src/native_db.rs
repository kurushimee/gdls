//! The native-class database: the interned, decoded form of `extension_api.json`.
//!
//! Ingestion decodes every type string into a [`TypeRef`] and interns every name into a [`Sym`],
//! producing O(1) class lookup and cheap inheritance walks. A missing/unreadable/malformed dump
//! degrades to [`NativeDb::empty`] — every lookup returns `None`, the analyzer treats unknown natives
//! as dynamic, and the server surfaces one notice. The DB never panics on bad input.

use std::hash::{Hash, Hasher};

use rustc_hash::FxHashMap;

use crate::api::{self, ExtensionApi};
use crate::intern::{Interner, Sym};
use crate::type_ref::{self, TypeRef};

/// `api_type` of a class — whether it ships in the editor build only, comes from a GDExtension, etc.
/// Retained so M3 can, if needed, distinguish editor-only symbols.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiType {
    Core,
    Editor,
    Extension,
    EditorExtension,
    Other,
}

impl ApiType {
    fn parse(s: &str) -> Self {
        match s {
            "core" => ApiType::Core,
            "editor" => ApiType::Editor,
            "extension" => ApiType::Extension,
            "editor_extension" => ApiType::EditorExtension,
            _ => ApiType::Other,
        }
    }
}

/// A named parameter with a decoded type.
#[derive(Clone, Debug)]
pub struct Param {
    pub name: Sym,
    pub ty: TypeRef,
}

/// A native method (engine or — via the XML reader — GDExtension).
#[derive(Clone, Debug)]
pub struct Method {
    pub name: Sym,
    pub is_const: bool,
    pub is_static: bool,
    pub is_vararg: bool,
    pub is_virtual: bool,
    pub return_type: TypeRef,
    pub params: Vec<Param>,
    /// Long-form docstring from `--dump-extension-api-with-docs` or the class-reference XML. Empty
    /// when the source dump omitted docs (older or stripped builds). Held as an owned `String`
    /// rather than interned — descriptions are unique long text, so the interner would waste memory.
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct Property {
    pub name: Sym,
    pub ty: TypeRef,
    pub setter: Option<Sym>,
    pub getter: Option<Sym>,
    /// One-line member description (see [`Method::description`]).
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct Signal {
    pub name: Sym,
    pub params: Vec<Param>,
    /// Signal description (see [`Method::description`]).
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct NativeEnum {
    pub name: Sym,
    pub is_bitfield: bool,
    pub values: Vec<(Sym, i64)>,
}

#[derive(Clone, Debug)]
pub struct NamedConst {
    pub name: Sym,
    pub value: i64,
}

/// A native class and its members.
#[derive(Clone, Debug)]
pub struct NativeClass {
    pub name: Sym,
    pub inherits: Option<Sym>,
    pub is_refcounted: bool,
    pub is_instantiable: bool,
    pub api_type: ApiType,
    pub methods: Vec<Method>,
    pub properties: Vec<Property>,
    pub signals: Vec<Signal>,
    pub enums: Vec<NativeEnum>,
    pub constants: Vec<NamedConst>,
    /// One-line summary from `--dump-extension-api-with-docs`'s `brief_description`. Empty when
    /// the dump didn't include docs.
    pub brief_description: String,
    /// Long-form class description (see [`Method::description`] for the source-availability
    /// contract).
    pub description: String,
}

/// A builtin / Variant type (`int`, `Vector2`, `Array`, …).
#[derive(Clone, Debug)]
pub struct BuiltinType {
    pub name: Sym,
    pub is_keyed: bool,
    pub indexing_return: Option<TypeRef>,
    pub members: Vec<Property>,
    pub methods: Vec<Method>,
    pub enums: Vec<NativeEnum>,
    pub constants: Vec<NamedConst>,
}

/// A `@GlobalScope` utility function (`abs`, `print`, …).
#[derive(Clone, Debug)]
pub struct UtilityFn {
    pub name: Sym,
    pub return_type: TypeRef,
    pub is_vararg: bool,
    pub params: Vec<Param>,
}

/// Failure modes of [`NativeDb::load`]. The *caller* decides whether to degrade to
/// [`NativeDb::empty`] (it should) — the DB does not swallow errors silently.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("could not read extension_api.json at {0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("could not parse extension_api.json at {0}: {1}")]
    Parse(String, #[source] serde_json::Error),
}

/// The native-class database.
#[derive(Debug)]
pub struct NativeDb {
    interner: Interner,
    classes: FxHashMap<Sym, NativeClass>,
    builtins: FxHashMap<Sym, BuiltinType>,
    global_enums: FxHashMap<Sym, NativeEnum>,
    global_constants: FxHashMap<Sym, i64>,
    utilities: FxHashMap<Sym, UtilityFn>,
    /// Singleton name → its class type.
    singletons: FxHashMap<Sym, Sym>,
    header: api::Header,
    /// Hash of the source text — lets the M4 watcher skip reloads when the dump is unchanged.
    content_hash: u64,
}

impl NativeDb {
    /// An empty DB: the graceful-degradation state when no dump is available.
    pub fn empty() -> Self {
        NativeDb {
            interner: Interner::new(),
            classes: FxHashMap::default(),
            builtins: FxHashMap::default(),
            global_enums: FxHashMap::default(),
            global_constants: FxHashMap::default(),
            utilities: FxHashMap::default(),
            singletons: FxHashMap::default(),
            header: api::Header::default(),
            content_hash: 0,
        }
    }

    /// Load and ingest a dump from disk. Returns an error the caller should log before degrading to
    /// [`NativeDb::empty`].
    pub fn load(path: &str) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path).map_err(|e| LoadError::Io(path.to_owned(), e))?;
        Self::from_json(&text).map_err(|e| LoadError::Parse(path.to_owned(), e))
    }

    /// Ingest a dump from its JSON text.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        let api: ExtensionApi = serde_json::from_str(text)?;
        let mut db = Self::from_api(api);
        db.content_hash = hash_str(text);
        Ok(db)
    }

    /// Ingest an already-parsed [`ExtensionApi`].
    pub fn from_api(api: ExtensionApi) -> Self {
        let mut it = Interner::new();
        let mut classes = FxHashMap::default();
        for c in api.classes {
            let class = ingest_class(c, &mut it);
            classes.insert(class.name, class);
        }
        let mut builtins = FxHashMap::default();
        for b in api.builtin_classes {
            let bt = ingest_builtin(b, &mut it);
            builtins.insert(bt.name, bt);
        }
        let mut global_enums = FxHashMap::default();
        for e in api.global_enums {
            let ne = ingest_enum(e, &mut it);
            global_enums.insert(ne.name, ne);
        }
        let mut global_constants = FxHashMap::default();
        for gc in api.global_constants {
            global_constants.insert(it.intern(&gc.name), gc.value);
        }
        let mut utilities = FxHashMap::default();
        for u in api.utility_functions {
            let uf = ingest_utility(u, &mut it);
            utilities.insert(uf.name, uf);
        }
        let mut singletons = FxHashMap::default();
        for s in api.singletons {
            singletons.insert(it.intern(&s.name), it.intern(&s.ty));
        }
        NativeDb {
            interner: it,
            classes,
            builtins,
            global_enums,
            global_constants,
            utilities,
            singletons,
            header: api.header,
            content_hash: 0,
        }
    }

    /// True when no classes or builtins were ingested (the degraded state).
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty() && self.builtins.is_empty()
    }

    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    pub fn header(&self) -> &api::Header {
        &self.header
    }

    pub fn content_hash(&self) -> u64 {
        self.content_hash
    }

    /// Resolve an interned handle back to its string (for handles this DB minted).
    pub fn name_of(&self, sym: Sym) -> &str {
        self.interner.resolve(sym)
    }

    /// The handle for `name`, if this DB interned it. Does not mint new handles.
    pub fn sym(&self, name: &str) -> Option<Sym> {
        self.interner.get(name)
    }

    pub fn class(&self, sym: Sym) -> Option<&NativeClass> {
        self.classes.get(&sym)
    }

    pub fn class_named(&self, name: &str) -> Option<&NativeClass> {
        self.interner.get(name).and_then(|s| self.classes.get(&s))
    }

    pub fn builtin_named(&self, name: &str) -> Option<&BuiltinType> {
        self.interner.get(name).and_then(|s| self.builtins.get(&s))
    }

    /// Walk the `inherits` chain to decide whether `sub` is `sup` or a (transitive) subclass of it.
    pub fn is_subclass_of(&self, sub: Sym, sup: Sym) -> bool {
        let mut cur = Some(sub);
        while let Some(c) = cur {
            if c == sup {
                return true;
            }
            cur = self.classes.get(&c).and_then(|class| class.inherits);
        }
        false
    }

    /// Convenience wrapper over [`Self::is_subclass_of`] keyed by name.
    pub fn is_subclass_of_named(&self, sub: &str, sup: &str) -> bool {
        match (self.interner.get(sub), self.interner.get(sup)) {
            (Some(a), Some(b)) => self.is_subclass_of(a, b),
            _ => false,
        }
    }

    pub fn singleton_type(&self, name: &str) -> Option<&NativeClass> {
        let ty = self
            .interner
            .get(name)
            .and_then(|s| self.singletons.get(&s))?;
        self.classes.get(ty)
    }

    /// A `@GlobalScope` enum (`Error`, `Key`, …).
    pub fn global_enum(&self, name: &str) -> Option<&NativeEnum> {
        self.interner
            .get(name)
            .and_then(|s| self.global_enums.get(&s))
    }

    /// If `name` is a value of any `@GlobalScope` enum, return that enum's name + the
    /// numeric value. `@GlobalScope` value lookups bypass the explicit `EnumName.` prefix
    /// — e.g. `CLOCKWISE` resolves the same as `ClockDirection.CLOCKWISE`.
    pub fn global_enum_value(&self, name: &str) -> Option<(String, i64)> {
        let target = self.interner.get(name)?;
        for ne in self.global_enums.values() {
            for (sym, val) in &ne.values {
                if *sym == target {
                    return Some((self.name_of(ne.name).to_owned(), *val));
                }
            }
        }
        None
    }

    /// A `@GlobalScope` constant's value.
    pub fn global_constant(&self, name: &str) -> Option<i64> {
        self.interner
            .get(name)
            .and_then(|s| self.global_constants.get(&s))
            .copied()
    }

    /// A `@GlobalScope` utility function (`abs`, `print`, …).
    pub fn utility(&self, name: &str) -> Option<&UtilityFn> {
        self.interner.get(name).and_then(|s| self.utilities.get(&s))
    }

    /// Merge a class parsed from doc XML as a fallback tier. Returns `false` and changes nothing if a
    /// class of that name already exists — the JSON dump always wins (`docs/03-indexing-freshness.md`
    /// §2). The XML reader normalizes its type strings into the dump's encoding, so ingestion is
    /// shared with the JSON path.
    pub fn merge_doc_class(&mut self, class: api::ClassDef) -> bool {
        if let Some(sym) = self.interner.get(&class.name) {
            if self.classes.contains_key(&sym) {
                return false;
            }
        }
        let nc = ingest_class(class, &mut self.interner);
        self.classes.insert(nc.name, nc);
        true
    }
}

fn ingest_class(c: api::ClassDef, it: &mut Interner) -> NativeClass {
    NativeClass {
        name: it.intern(&c.name),
        inherits: c.inherits.as_deref().map(|s| it.intern(s)),
        is_refcounted: c.is_refcounted,
        is_instantiable: c.is_instantiable,
        api_type: ApiType::parse(&c.api_type),
        methods: c
            .methods
            .into_iter()
            .map(|m| ingest_method(m, it))
            .collect(),
        properties: c
            .properties
            .into_iter()
            .map(|p| ingest_property(p, it))
            .collect(),
        signals: c
            .signals
            .into_iter()
            .map(|s| Signal {
                name: it.intern(&s.name),
                params: ingest_args(s.arguments, it),
                description: s.description,
            })
            .collect(),
        enums: c.enums.into_iter().map(|e| ingest_enum(e, it)).collect(),
        constants: c
            .constants
            .into_iter()
            .map(|k| NamedConst {
                name: it.intern(&k.name),
                value: k.value,
            })
            .collect(),
        brief_description: c.brief_description,
        description: c.description,
    }
}

fn ingest_method(m: api::MethodDef, it: &mut Interner) -> Method {
    let return_type = match m.return_value {
        Some(rv) => type_ref::decode(&rv.ty, it),
        None => TypeRef::Void,
    };
    Method {
        name: it.intern(&m.name),
        is_const: m.is_const,
        is_static: m.is_static,
        is_vararg: m.is_vararg,
        is_virtual: m.is_virtual,
        return_type,
        params: ingest_args(m.arguments, it),
        description: m.description,
    }
}

fn ingest_property(p: api::PropertyDef, it: &mut Interner) -> Property {
    let ty = type_ref::decode(&p.ty, it);
    Property {
        name: it.intern(&p.name),
        ty,
        setter: non_empty(&p.setter, it),
        getter: non_empty(&p.getter, it),
        description: p.description,
    }
}

fn ingest_builtin(b: api::BuiltinClass, it: &mut Interner) -> BuiltinType {
    BuiltinType {
        name: it.intern(&b.name),
        is_keyed: b.is_keyed,
        indexing_return: b.indexing_return_type.map(|s| type_ref::decode(&s, it)),
        members: b
            .members
            .into_iter()
            .map(|m| {
                let ty = type_ref::decode(&m.ty, it);
                Property {
                    name: it.intern(&m.name),
                    ty,
                    setter: None,
                    getter: None,
                    // Builtin-class members (e.g. `Vector2.x`) aren't documented per-member in the
                    // extension-API dump; descriptions are class-level only.
                    description: String::new(),
                }
            })
            .collect(),
        methods: b
            .methods
            .into_iter()
            .map(|m| {
                let return_type = m
                    .return_type
                    .map_or(TypeRef::Void, |s| type_ref::decode(&s, it));
                Method {
                    name: it.intern(&m.name),
                    is_const: m.is_const,
                    is_static: m.is_static,
                    is_vararg: m.is_vararg,
                    is_virtual: false,
                    return_type,
                    params: ingest_args(m.arguments, it),
                    description: String::new(),
                }
            })
            .collect(),
        enums: b.enums.into_iter().map(|e| ingest_enum(e, it)).collect(),
        constants: b
            .constants
            .into_iter()
            .map(|k| NamedConst {
                name: it.intern(&k.name),
                value: 0,
            })
            .collect(),
    }
}

fn ingest_utility(u: api::UtilityFunction, it: &mut Interner) -> UtilityFn {
    let return_type = u
        .return_type
        .map_or(TypeRef::Void, |s| type_ref::decode(&s, it));
    UtilityFn {
        name: it.intern(&u.name),
        return_type,
        is_vararg: u.is_vararg,
        params: ingest_args(u.arguments, it),
    }
}

fn ingest_enum(e: api::EnumDef, it: &mut Interner) -> NativeEnum {
    NativeEnum {
        name: it.intern(&e.name),
        is_bitfield: e.is_bitfield,
        values: e
            .values
            .into_iter()
            .map(|v| (it.intern(&v.name), v.value))
            .collect(),
    }
}

fn ingest_args(args: Vec<api::ArgumentDef>, it: &mut Interner) -> Vec<Param> {
    args.into_iter()
        .map(|a| {
            let ty = type_ref::decode(&a.ty, it);
            Param {
                name: it.intern(&a.name),
                ty,
            }
        })
        .collect()
}

fn non_empty(s: &str, it: &mut Interner) -> Option<Sym> {
    (!s.is_empty()).then(|| it.intern(s))
}

fn hash_str(s: &str) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_db_degrades() {
        let db = NativeDb::empty();
        assert!(db.is_empty());
        assert!(db.class_named("Node").is_none());
        assert!(!db.is_subclass_of_named("Node", "Object"));
    }
}
