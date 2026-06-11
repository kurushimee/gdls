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
    /// The dump's default-value literal, verbatim (`"0"`, `"null"`, `"&\"\""`), when the
    /// argument is optional. Interned — defaults are short, heavily repeated strings. Signal
    /// parameters never carry one (the dump has no such field for them).
    pub default_value: Option<Sym>,
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
    /// The constant's declared type name from the dump (`Vector3.UP` → `Vector3`,
    /// `Vector3.AXIS_X` → `int`). `Some` only for builtin-class constants — engine-class
    /// constants are bare integers and carry no type field in the API.
    pub ty: Option<Sym>,
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

/// One member found by [`NativeDb::lookup_member`] / [`NativeDb::lookup_builtin_member`],
/// borrowing from the declaring class. The variants cover every name a `Class.member` access can
/// reach in the dump's data model.
#[derive(Clone, Copy, Debug)]
pub enum NativeMember<'a> {
    Property(&'a Property),
    Method(&'a Method),
    Signal(&'a Signal),
    /// The enum *itself* (`Input.MouseMode`).
    Enum(&'a NativeEnum),
    /// A bare class constant (`Object.NOTIFICATION_READY`) — no enum membership in the dump.
    Constant(&'a NamedConst),
    /// A value declared inside a named enum (`Input.MOUSE_MODE_CAPTURED` → owner `MouseMode`).
    EnumValue {
        owner: &'a NativeEnum,
        name: Sym,
        value: i64,
    },
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

/// How well this DB is known to match the engine the project actually runs on. The analyzer
/// gates its *negative* claims on this: "type X does not exist" is only trustworthy when the
/// class surface came from the project's own engine (a project-context dump or a user-pinned
/// file). A bundled generic dump proves what *does* exist, never what doesn't — a project built
/// on a custom engine build legitimately names classes a stock dump has never heard of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApiProvenance {
    /// Project-derived: an auto-dump made with project context, a user `extensionApiPath`, or a
    /// project-root `extension_api.json`. Unknown-type errors are trustworthy.
    #[default]
    Exact,
    /// A bundled stock dump used as a last-resort fallback. Positive lookups are accurate for
    /// the stock surface; absence proves nothing.
    Generic,
    /// No API source at all (the empty DB). Every native lookup misses; absence proves nothing.
    Absent,
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
    /// See [`ApiProvenance`]. Constructors default to `Exact` (every pre-existing source is
    /// project-derived); [`NativeDb::empty`] is `Absent`; the embedded-fallback loader in
    /// `gd_server` downgrades its instance to `Generic`.
    provenance: ApiProvenance,
}

impl NativeDb {
    /// An empty DB: the graceful-degradation state when no dump is available. Provenance is
    /// [`ApiProvenance::Absent`] — the analyzer must not turn its misses into errors.
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
            provenance: ApiProvenance::Absent,
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
            provenance: ApiProvenance::Exact,
        }
    }

    /// True when no classes or builtins were ingested (the degraded state).
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty() && self.builtins.is_empty()
    }

    /// See [`ApiProvenance`].
    pub fn provenance(&self) -> ApiProvenance {
        self.provenance
    }

    /// Tag this DB's [`ApiProvenance`] — the `gd_server` loader marks its embedded-fallback
    /// instance `Generic` right after ingest.
    pub fn set_provenance(&mut self, provenance: ApiProvenance) {
        self.provenance = provenance;
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

    /// Find `member` on `class` or anywhere up its `inherits` chain, returning the **declaring**
    /// class alongside the member. Probe order mirrors the analyzer's attribute resolution
    /// (upstream `reduce_identifier_from_base`): property → method → signal → enum →
    /// constant / enum value. Server-side consumers only (hover, definition, stub rendering) —
    /// the analyzer keeps its own port-faithful walks in `gd_analyze`.
    pub fn lookup_member<'a>(
        &'a self,
        class: &str,
        member: &str,
    ) -> Option<(&'a NativeClass, NativeMember<'a>)> {
        // A name the interner never saw cannot exist anywhere in the DB.
        let target = self.interner.get(member)?;
        let mut cur = self.class_named(class);
        while let Some(nc) = cur {
            if let Some(m) = member_of(
                target,
                &nc.properties,
                &nc.methods,
                Some(&nc.signals),
                &nc.enums,
                &nc.constants,
            ) {
                return Some((nc, m));
            }
            cur = nc.inherits.and_then(|s| self.classes.get(&s));
        }
        None
    }

    /// [`Self::lookup_member`]'s builtin-type analog (`Vector3.ZERO`, `vec.length()`). Builtins
    /// have no inheritance chain and no signals; their `members` field plays the property role.
    pub fn lookup_builtin_member<'a>(
        &'a self,
        builtin: &str,
        member: &str,
    ) -> Option<(&'a BuiltinType, NativeMember<'a>)> {
        let target = self.interner.get(member)?;
        let bt = self.builtin_named(builtin)?;
        member_of(
            target,
            &bt.members,
            &bt.methods,
            None,
            &bt.enums,
            &bt.constants,
        )
        .map(|m| (bt, m))
    }

    /// Render a [`TypeRef`] the way the editor surfaces type names: `Array[int]`,
    /// `Dictionary[int, String]`, scoped enums as `Class.Name`. `trim_scope` drops a same-class
    /// enum qualifier (`Input.MouseMode` rendered from inside `Input` reads `MouseMode`),
    /// mirroring the class-reference convention Godot's own LSP details inherit from DocData.
    pub fn display_type(&self, ty: &TypeRef, trim_scope: Option<&str>) -> String {
        match ty {
            TypeRef::Variant => "Variant".to_owned(),
            TypeRef::Void => "void".to_owned(),
            TypeRef::Named(sym) => self.name_of(*sym).to_owned(),
            TypeRef::TypedArray(elem) => {
                format!("Array[{}]", self.display_type(elem, trim_scope))
            }
            TypeRef::TypedDict(k, v) => format!(
                "Dictionary[{}, {}]",
                self.display_type(k, trim_scope),
                self.display_type(v, trim_scope)
            ),
            TypeRef::Enum { scope, name } | TypeRef::Bitfield { scope, name } => {
                let name = self.name_of(*name);
                match scope {
                    Some(s) if trim_scope != Some(self.name_of(*s)) => {
                        format!("{}.{name}", self.name_of(*s))
                    }
                    _ => name.to_owned(),
                }
            }
            TypeRef::Pointer(inner) => format!("{}*", self.display_type(inner, trim_scope)),
        }
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
                ty: None,
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
                // Builtin-constant values are non-scalar literals ("Vector3(0, 1, 0)") the type
                // model doesn't evaluate; the declared type below is what the analyzer consumes.
                value: 0,
                ty: non_empty(&k.ty, it),
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
                default_value: a
                    .default_value
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| it.intern(s)),
            }
        })
        .collect()
}

fn non_empty(s: &str, it: &mut Interner) -> Option<Sym> {
    (!s.is_empty()).then(|| it.intern(s))
}

/// Probe one class's (or builtin's) member lists for `target`, in the shared lookup order:
/// property → method → signal → enum → constant → enum value. Builtins pass `signals: None`.
fn member_of<'a>(
    target: Sym,
    properties: &'a [Property],
    methods: &'a [Method],
    signals: Option<&'a [Signal]>,
    enums: &'a [NativeEnum],
    constants: &'a [NamedConst],
) -> Option<NativeMember<'a>> {
    if let Some(p) = properties.iter().find(|p| p.name == target) {
        return Some(NativeMember::Property(p));
    }
    if let Some(m) = methods.iter().find(|m| m.name == target) {
        return Some(NativeMember::Method(m));
    }
    if let Some(s) = signals.and_then(|sigs| sigs.iter().find(|s| s.name == target)) {
        return Some(NativeMember::Signal(s));
    }
    if let Some(e) = enums.iter().find(|e| e.name == target) {
        return Some(NativeMember::Enum(e));
    }
    if let Some(k) = constants.iter().find(|k| k.name == target) {
        return Some(NativeMember::Constant(k));
    }
    for e in enums {
        if let Some((name, value)) = e.values.iter().find(|(n, _)| *n == target) {
            return Some(NativeMember::EnumValue {
                owner: e,
                name: *name,
                value: *value,
            });
        }
    }
    None
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
