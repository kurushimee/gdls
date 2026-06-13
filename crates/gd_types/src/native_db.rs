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

    /// How many `@GlobalScope` utility functions this DB ingested. Lets a consumer assert the
    /// ingest carried the full Variant utility set (parity against
    /// [`crate::VARIANT_UTILITY_FUNCTIONS`]).
    pub fn utility_count(&self) -> usize {
        self.utilities.len()
    }

    /// Enumerate every `@GlobalScope` utility function, **sorted by name**. The by-name
    /// [`Self::utility`] lookup is the targeted complement; this is the enumeration M8 completion
    /// lists for the IDENTIFIER context's "GDScript utilities" tier. The backing store is an
    /// [`FxHashMap`] (iteration order is nondeterministic), so the result is sorted to give
    /// completion a stable order to rank against. Count equals [`Self::utility_count`].
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn utilities(&self) -> impl Iterator<Item = &UtilityFn> + '_ {
        let mut all: Vec<&UtilityFn> = self.utilities.values().collect();
        all.sort_by(|a, b| self.name_of(a.name).cmp(self.name_of(b.name)));
        all.into_iter()
    }

    /// Enumerate every `@GlobalScope` constant as `(name, value)`, **sorted by name**. Note the
    /// `extension_api.json` `global_constants` array is empty on stock dumps — Godot exposes
    /// `OK`, `KEY_ESCAPE`, … as values of the `Error` / `Key` *global enums*, reachable through
    /// [`Self::global_enum_values`] / [`Self::global_enum_value`]. This iterator stays for the
    /// (rare) custom dump that does populate the array, and to mirror the by-name
    /// [`Self::global_constant`] lookup. Sorted for the same determinism reason as
    /// [`Self::utilities`].
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn global_constants(&self) -> impl Iterator<Item = (&str, i64)> + '_ {
        let mut all: Vec<(&str, i64)> = self
            .global_constants
            .iter()
            .map(|(s, v)| (self.name_of(*s), *v))
            .collect();
        all.sort_by(|a, b| a.0.cmp(b.0));
        all.into_iter()
    }

    /// Enumerate every `@GlobalScope` enum (`Error`, `Key`, `Side`, …), **sorted by name**. The
    /// by-name [`Self::global_enum`] lookup is the targeted complement; this is the enumeration
    /// M8 completion lists for the IDENTIFIER context's "global enums" tier. Sorted for
    /// determinism (see [`Self::utilities`]).
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn global_enums(&self) -> impl Iterator<Item = &NativeEnum> + '_ {
        let mut all: Vec<&NativeEnum> = self.global_enums.values().collect();
        all.sort_by(|a, b| self.name_of(a.name).cmp(self.name_of(b.name)));
        all.into_iter()
    }

    /// Enumerate every `@GlobalScope` enum *value* as `(value_name, owning_enum_name, value)`,
    /// **sorted by value name**. This is the flat bare-identifier set Godot's IDENTIFIER
    /// completion surfaces for the "global constants" tier — `OK` (of `Error`), `KEY_ESCAPE`
    /// (of `Key`), `SIDE_LEFT` (of `Side`), … — the reverse of [`Self::global_enum_value`]'s
    /// single lookup. Two enums could in principle declare the same bare value name; both rows
    /// are yielded (completion dedups/ranks). Sorted for determinism (see [`Self::utilities`]).
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn global_enum_values(&self) -> impl Iterator<Item = (&str, &str, i64)> + '_ {
        let mut all: Vec<(&str, &str, i64)> = self
            .global_enums
            .values()
            .flat_map(|ne| {
                let owner = self.name_of(ne.name);
                ne.values
                    .iter()
                    .map(move |(sym, val)| (self.name_of(*sym), owner, *val))
            })
            .collect();
        all.sort_by(|a, b| a.0.cmp(b.0));
        all.into_iter()
    }

    /// Enumerate every native class **name**, **sorted**. The by-name [`Self::class_named`] lookup
    /// is the targeted complement; this is the enumeration M8 completion lists in the IDENTIFIER and
    /// TYPE contexts (`Node`, `Timer`, …, the engine class set Godot's `get_global_map()` /
    /// `_list_available_types` surface). The backing store is an [`FxHashMap`] (nondeterministic
    /// iteration), so the result is sorted to give completion a stable order to rank against.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn class_names(&self) -> impl Iterator<Item = &str> + '_ {
        let mut all: Vec<&str> = self
            .classes
            .values()
            .map(|c| self.name_of(c.name))
            .collect();
        all.sort_unstable();
        all.into_iter()
    }

    /// Enumerate every builtin type **name**, **sorted** (`Vector2`, `Color`, `Array`, …). The
    /// complement of [`Self::builtin_named`]; sorted for the same determinism reason as
    /// [`Self::class_names`].
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> + '_ {
        let mut all: Vec<&str> = self
            .builtins
            .values()
            .map(|b| self.name_of(b.name))
            .collect();
        all.sort_unstable();
        all.into_iter()
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

    /// Enumerate the **value names** of an enum, **sorted**, for M8 call-argument / assignment
    /// enum-candidate completion (`ClassDB::get_enum_constants` / `_find_enumeration_candidates`).
    /// `scope` is the owning class for a class-scoped enum (`Input.MouseMode` → `Some("Input")`),
    /// or `None` for a `@GlobalScope` enum (`Error`). A class-scoped enum is looked up on the
    /// class **and inherited** up the `inherits` chain (an enum-typed param can name a base
    /// class's enum). Empty when the enum is unknown. Names only — completion adds the qualifier.
    #[must_use]
    pub fn enum_constants(&self, scope: Option<&str>, enum_name: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        match scope {
            None => {
                if let Some(ne) = self.global_enum(enum_name) {
                    out.extend(ne.values.iter().map(|(s, _)| self.name_of(*s)));
                }
            }
            Some(class) => {
                let target = self.interner.get(enum_name);
                let mut cur = self.class_named(class);
                for _ in 0..64 {
                    let Some(nc) = cur else { break };
                    if let Some(ne) = target.and_then(|t| nc.enums.iter().find(|e| e.name == t)) {
                        out.extend(ne.values.iter().map(|(s, _)| self.name_of(*s)));
                        break;
                    }
                    cur = nc.inherits.and_then(|s| self.classes.get(&s));
                }
            }
        }
        out.sort_unstable();
        out
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
        // Real chains are ~10 deep; the cap only turns a hand-edited dump's `inherits` cycle
        // into a miss instead of a hung request (never crash, never lie — never hang either).
        for _ in 0..64 {
            let Some(nc) = cur else { break };
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

    /// Collect **every member** reachable on `class` — its own plus all inherited up the
    /// `inherits` chain — the enumeration counterpart of the by-name [`Self::lookup_member`]
    /// walk (it serves `Class.<cursor>` / instance member completion). Returns each member
    /// alongside the class that **declares** it.
    ///
    /// **Shadowing / dedup rule (derived shadows base, by name):** the chain is walked
    /// derived-first (the named class, then each `inherits` parent). The **first** class to
    /// expose a given member name wins; a same-named member on any base class is skipped — the
    /// exact override semantics [`Self::lookup_member`] produces by stopping at its first hit.
    /// Within one class, members appear in dump order, grouped property → method → signal →
    /// enum → constant → enum value (mirroring [`member_of`]'s probe order so a one-by-one
    /// `lookup_member` and this enumeration agree on which entry a name resolves to). A named
    /// enum and its values are distinct names: the enum *itself* (e.g. `MouseMode`) and each of
    /// its values (e.g. `MOUSE_MODE_CAPTURED`) are separate entries.
    ///
    /// A hand-edited dump's `inherits` cycle is bounded the same way as [`Self::lookup_member`]
    /// (a fixed depth cap) — it yields a finite list instead of hanging (never crash, never
    /// hang).
    #[must_use]
    pub fn all_members<'a>(&'a self, class: &str) -> Vec<(&'a NativeClass, NativeMember<'a>)> {
        let mut out: Vec<(&'a NativeClass, NativeMember<'a>)> = Vec::new();
        let mut seen: rustc_hash::FxHashSet<Sym> = rustc_hash::FxHashSet::default();
        let mut cur = self.class_named(class);
        // Same cap as `lookup_member`: real chains are ~10 deep; the bound turns a malformed
        // cyclic `inherits` into a finite result rather than an unbounded walk.
        for _ in 0..64 {
            let Some(nc) = cur else { break };
            collect_class_members(nc, &mut seen, &mut out);
            cur = nc.inherits.and_then(|s| self.classes.get(&s));
        }
        out
    }

    /// [`Self::all_members`]'s builtin analog: every member of `builtin` (`Vector2`, `Array`,
    /// …). Builtins have **no inheritance chain** (and no signals — `members` plays the property
    /// role), so this is a single class's members in the same property → method → enum →
    /// constant → enum-value grouping. `None` when the builtin name is unknown.
    #[must_use]
    pub fn builtin_members<'a>(&'a self, builtin: &str) -> Option<Vec<NativeMember<'a>>> {
        let bt = self.builtin_named(builtin)?;
        let mut seen: rustc_hash::FxHashSet<Sym> = rustc_hash::FxHashSet::default();
        let mut out: Vec<(&'a BuiltinType, NativeMember<'a>)> = Vec::new();
        collect_builtin_members(bt, &mut seen, &mut out);
        Some(out.into_iter().map(|(_, m)| m).collect())
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

/// Push every member of one native class into `out`, in [`member_of`]'s probe order
/// (property → method → signal → enum → constant → enum value), skipping any name already in
/// `seen`. `seen` carries across classes so a derived class's member shadows a base class's
/// same-named one (the enumeration twin of [`NativeDb::lookup_member`]'s first-hit stop).
fn collect_class_members<'a>(
    nc: &'a NativeClass,
    seen: &mut rustc_hash::FxHashSet<Sym>,
    out: &mut Vec<(&'a NativeClass, NativeMember<'a>)>,
) {
    for p in &nc.properties {
        if seen.insert(p.name) {
            out.push((nc, NativeMember::Property(p)));
        }
    }
    for m in &nc.methods {
        if seen.insert(m.name) {
            out.push((nc, NativeMember::Method(m)));
        }
    }
    for s in &nc.signals {
        if seen.insert(s.name) {
            out.push((nc, NativeMember::Signal(s)));
        }
    }
    for e in &nc.enums {
        if seen.insert(e.name) {
            out.push((nc, NativeMember::Enum(e)));
        }
    }
    for k in &nc.constants {
        if seen.insert(k.name) {
            out.push((nc, NativeMember::Constant(k)));
        }
    }
    for e in &nc.enums {
        for (name, value) in &e.values {
            if seen.insert(*name) {
                out.push((
                    nc,
                    NativeMember::EnumValue {
                        owner: e,
                        name: *name,
                        value: *value,
                    },
                ));
            }
        }
    }
}

/// [`collect_class_members`]'s builtin analog: builtins carry no signals (the `None` slot in
/// [`member_of`]) and `members` plays the property role.
fn collect_builtin_members<'a>(
    bt: &'a BuiltinType,
    seen: &mut rustc_hash::FxHashSet<Sym>,
    out: &mut Vec<(&'a BuiltinType, NativeMember<'a>)>,
) {
    for p in &bt.members {
        if seen.insert(p.name) {
            out.push((bt, NativeMember::Property(p)));
        }
    }
    for m in &bt.methods {
        if seen.insert(m.name) {
            out.push((bt, NativeMember::Method(m)));
        }
    }
    for e in &bt.enums {
        if seen.insert(e.name) {
            out.push((bt, NativeMember::Enum(e)));
        }
    }
    for k in &bt.constants {
        if seen.insert(k.name) {
            out.push((bt, NativeMember::Constant(k)));
        }
    }
    for e in &bt.enums {
        for (name, value) in &e.values {
            if seen.insert(*name) {
                out.push((
                    bt,
                    NativeMember::EnumValue {
                        owner: e,
                        name: *name,
                        value: *value,
                    },
                ));
            }
        }
    }
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

    /// The vendored trimmed dump (`Node` inherits `Object`); the same fixture `gd_analyze`'s
    /// cross-file tests load. Portable: in-crate, no absolute paths.
    fn trimmed_db() -> NativeDb {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/trimmed_api.json");
        NativeDb::load(path.to_str().expect("utf-8 path"))
            .unwrap_or_else(|e| panic!("load trimmed native DB fixture: {e}"))
    }

    /// The member-name set of one `NativeMember`, for cross-checking enumeration against
    /// one-by-one `lookup_member`.
    fn member_name(db: &NativeDb, m: &NativeMember) -> String {
        match m {
            NativeMember::Property(p) => db.name_of(p.name).to_owned(),
            NativeMember::Method(m) => db.name_of(m.name).to_owned(),
            NativeMember::Signal(s) => db.name_of(s.name).to_owned(),
            NativeMember::Enum(e) => db.name_of(e.name).to_owned(),
            NativeMember::Constant(k) => db.name_of(k.name).to_owned(),
            NativeMember::EnumValue { name, .. } => db.name_of(*name).to_owned(),
        }
    }

    #[test]
    fn all_members_of_node_includes_inherited_object_members() {
        let db = trimmed_db();
        let members = db.all_members("Node");
        assert!(!members.is_empty(), "Node has members");
        // Plausible count: Node + its base chain up to Object carries well over 50 names in the
        // trimmed dump (the real dump is far larger); the upper bound just catches a runaway.
        assert!(
            members.len() > 50 && members.len() < 5000,
            "Node member count {} is implausible",
            members.len()
        );

        let names: std::collections::HashSet<String> =
            members.iter().map(|(_, m)| member_name(&db, m)).collect();
        // Node's own members.
        assert!(names.contains("get_parent"), "Node::get_parent enumerated");
        assert!(names.contains("queue_free"), "Node::queue_free enumerated");
        assert!(names.contains("name"), "Node::name property enumerated");
        assert!(
            names.contains("process_mode"),
            "Node::process_mode enumerated"
        );
        // Inherited from Object (proves the chain walk).
        assert!(
            names.contains("get_class"),
            "inherited Object::get_class enumerated"
        );

        // Every enumerated name resolves via the by-name walk to the SAME member kind — the
        // enumeration and the one-by-one lookup agree.
        for (decl, m) in &members {
            let looked = db
                .lookup_member("Node", &member_name(&db, m))
                .unwrap_or_else(|| panic!("lookup_member finds {}", member_name(&db, m)));
            assert_eq!(
                std::mem::discriminant(m),
                std::mem::discriminant(&looked.1),
                "kind of {} agrees between enumerate and lookup",
                member_name(&db, m)
            );
            assert_eq!(
                db.name_of(decl.name),
                db.name_of(looked.0.name),
                "declaring class of {} agrees (derived shadows base)",
                member_name(&db, m)
            );
        }

        // No duplicate names (the dedup held).
        assert_eq!(names.len(), members.len(), "enumeration is name-unique");
    }

    #[test]
    fn all_members_dedup_is_derived_shadows_base() {
        // `Derived::shared` shadows `Base::shared`; the chain walk yields the DERIVED one and
        // never the base's, matching `lookup_member`'s first-hit stop.
        let db = NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "classes": [
                    {"name": "Base", "is_instantiable": true,
                     "methods": [{"name": "shared", "is_const": false, "is_static": false,
                                  "is_vararg": false, "is_virtual": false, "hash": 1,
                                  "return_value": {"type": "int"}}],
                     "properties": [{"name": "only_base", "type": "int", "setter": "", "getter": ""}]},
                    {"name": "Derived", "inherits": "Base", "is_instantiable": true,
                     "methods": [{"name": "shared", "is_const": false, "is_static": false,
                                  "is_vararg": false, "is_virtual": false, "hash": 2,
                                  "return_value": {"type": "String"}}]}
                ]
            }"#,
        )
        .expect("shadowing dump");
        let members = db.all_members("Derived");
        let shared: Vec<&NativeMember> = members
            .iter()
            .filter(|(_, m)| member_name(&db, m) == "shared")
            .map(|(_, m)| m)
            .collect();
        assert_eq!(
            shared.len(),
            1,
            "shared appears once (derived shadows base)"
        );
        let NativeMember::Method(m) = shared[0] else {
            panic!("shared is a method");
        };
        // The DERIVED override returns String; the base returned int. The chain kept the derived.
        assert!(
            matches!(m.return_type, TypeRef::Named(s) if db.name_of(s) == "String"),
            "the derived override (String return) shadows the base (int return)"
        );
        // The base-only member still comes through (inheritance, not replacement).
        assert!(
            members
                .iter()
                .any(|(_, m)| member_name(&db, m) == "only_base"),
            "base-only members are inherited"
        );
    }

    #[test]
    fn builtin_members_enumerated() {
        let db = trimmed_db();
        let members = db
            .builtin_members("Vector2")
            .expect("Vector2 builtin exists");
        let names: std::collections::HashSet<String> =
            members.iter().map(|m| member_name(&db, m)).collect();
        assert!(names.contains("x"), "Vector2.x member enumerated");
        assert!(names.contains("y"), "Vector2.y member enumerated");
        // Every enumerated builtin member resolves by-name to the same kind.
        for m in &members {
            let (_, looked) = db
                .lookup_builtin_member("Vector2", &member_name(&db, m))
                .expect("builtin member resolves by name");
            assert_eq!(
                std::mem::discriminant(m),
                std::mem::discriminant(&looked),
                "{} kind agrees",
                member_name(&db, m)
            );
        }
        assert!(db.builtin_members("NotAType").is_none());
    }

    /// An inline dump with two utilities and the `Error` / `Key` global enums — the portable way
    /// to pin `print`/`randi` (utilities) and `OK`/`KEY_ESCAPE` (global-enum values), neither of
    /// which the trimmed fixture carries in full (and which the stock dump exposes through enums,
    /// not the empty `global_constants` array).
    fn mini_globals_db() -> NativeDb {
        NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "utility_functions": [
                    {"name": "print", "category": "general", "is_vararg": true, "hash": 1,
                     "arguments": []},
                    {"name": "randi", "category": "random", "is_vararg": false, "hash": 2,
                     "return_type": "int"}
                ],
                "global_enums": [
                    {"name": "Error", "is_bitfield": false,
                     "values": [{"name": "OK", "value": 0}, {"name": "FAILED", "value": 1}]},
                    {"name": "Key", "is_bitfield": false,
                     "values": [{"name": "KEY_NONE", "value": 0},
                                {"name": "KEY_ESCAPE", "value": 4194305}]}
                ]
            }"#,
        )
        .expect("mini globals dump")
    }

    #[test]
    fn utilities_enumerable_and_count_matches() {
        let db = mini_globals_db();
        let names: Vec<&str> = db.utilities().map(|u| db.name_of(u.name)).collect();
        assert!(names.contains(&"print"), "print is a utility");
        assert!(names.contains(&"randi"), "randi is a utility");
        assert_eq!(
            db.utilities().count(),
            db.utility_count(),
            "enumeration count equals utility_count()"
        );
        // Sorted (deterministic) order.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "utilities() yields a name-sorted order");
    }

    #[test]
    fn class_and_builtin_names_enumerated_sorted() {
        let db = trimmed_db();
        let classes: Vec<&str> = db.class_names().collect();
        assert!(classes.contains(&"Node"), "Node enumerated: {classes:?}");
        assert!(classes.contains(&"Object"), "Object enumerated");
        let mut sorted = classes.clone();
        sorted.sort_unstable();
        assert_eq!(classes, sorted, "class_names() is name-sorted");

        let builtins: Vec<&str> = db.builtin_names().collect();
        assert!(builtins.contains(&"Color"), "Color builtin enumerated");
        assert!(builtins.contains(&"Vector2"), "Vector2 builtin enumerated");
        let mut bsorted = builtins.clone();
        bsorted.sort_unstable();
        assert_eq!(builtins, bsorted, "builtin_names() is name-sorted");

        // Empty DB yields nothing, never panics.
        let empty = NativeDb::empty();
        assert_eq!(empty.class_names().count(), 0);
        assert_eq!(empty.builtin_names().count(), 0);
    }

    #[test]
    fn enum_constants_global_and_scoped() {
        // Global enum (`Error`) values via `enum_constants(None, "Error")`.
        let db = mini_globals_db();
        let mut err = db.enum_constants(None, "Error");
        err.sort_unstable();
        assert_eq!(
            err,
            vec!["FAILED", "OK"],
            "global Error enum values, sorted"
        );
        assert!(
            db.enum_constants(None, "NotAnEnum").is_empty(),
            "unknown enum → empty"
        );

        // Class-scoped enum, including inherited up the chain.
        let db = NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "classes": [
                    {"name": "Base", "is_instantiable": true,
                     "enums": [{"name": "Mode", "is_bitfield": false,
                                "values": [{"name": "MODE_A", "value": 0},
                                           {"name": "MODE_B", "value": 1}]}]},
                    {"name": "Derived", "inherits": "Base", "is_instantiable": true}
                ]
            }"#,
        )
        .expect("scoped enum dump");
        // Looked up on the declaring class.
        assert_eq!(
            db.enum_constants(Some("Base"), "Mode"),
            vec!["MODE_A", "MODE_B"]
        );
        // Inherited: `Derived` reaches `Base.Mode` up the `inherits` chain.
        assert_eq!(
            db.enum_constants(Some("Derived"), "Mode"),
            vec!["MODE_A", "MODE_B"],
            "a class-scoped enum is found up the inherits chain"
        );
    }

    #[test]
    fn global_enum_values_enumerable() {
        let db = mini_globals_db();
        let vals: std::collections::HashMap<&str, (&str, i64)> = db
            .global_enum_values()
            .map(|(name, owner, v)| (name, (owner, v)))
            .collect();
        assert_eq!(
            vals.get("OK"),
            Some(&("Error", 0)),
            "OK is a value of the Error global enum"
        );
        assert_eq!(
            vals.get("KEY_ESCAPE").map(|(o, _)| *o),
            Some("Key"),
            "KEY_ESCAPE is a value of the Key global enum"
        );
        // Cross-check against the single reverse lookup the analyzer already uses.
        assert_eq!(db.global_enum_value("OK"), Some(("Error".to_owned(), 0)));

        // The enums themselves enumerate too.
        let enum_names: Vec<&str> = db.global_enums().map(|e| db.name_of(e.name)).collect();
        assert_eq!(
            enum_names,
            vec!["Error", "Key"],
            "global enums, name-sorted"
        );

        // `global_constants` is genuinely empty on this (and the stock) dump — the iterator is
        // present and correct, it just has nothing to yield.
        assert_eq!(db.global_constants().count(), 0);
    }
}
