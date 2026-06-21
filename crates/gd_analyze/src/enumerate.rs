//! Additive **symbol-enumeration** APIs for `textDocument/completion` (M8, #64).
//!
//! The analyzer resolves and the native DB looks up **by name**; completion instead needs to
//! *enumerate* — every member reachable through an `extends` chain, every value of an enum, the
//! full member set of whatever type the cursor expression has. This module wraps the existing
//! by-name walks with collect-all variants and a single [`DataType`]→members dispatcher that
//! mirrors Godot's `_find_identifiers_in_base` arms (`gdscript_editor.cpp`).
//!
//! Everything here is **strictly additive and side-effect-free**: unlike the reducer's
//! [`crate::reducer`] member walks (which record `Binding::Use` and mutate the
//! [`AnalysisContext`]), these functions take only read-only `&dyn CrossFileQuery` / `&NativeDb`
//! / `&ParseTree` and never touch analyzer state — completion must not perturb the analysis it
//! reads. The chain walks here are deliberately *not* memoized (a completion request is one-shot;
//! the per-pass `script_chains` cache belongs to an in-flight analyze).
//!
//! Ordering is deterministic so a later ranking phase has a stable base: members come out in
//! declaration order within a class/interface, derived classes before their bases.

use gd_project::{FileId, Interface, MemberKind as IfaceMemberKind};
use gd_syntax::ast::{ClassNode, Member, NodeId, NodeKind};
use gd_syntax::ParseTree;
use gd_types::{NativeDb, NativeMember};
use rustc_hash::FxHashSet;

use crate::cross_file::CrossFileQuery;
use crate::data_type::{DataType, DtKind, ScriptRef, VariantType};
use crate::script_chain::link_interface;

/// The kind of an enumerated member, unifying the three member-source models (native DB,
/// cross-file [`Interface`], in-file [`ClassNode`]) into one taxonomy completion can map to an
/// LSP `CompletionItemKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberItemKind {
    Method,
    /// A `var` / native property (with or without get/set).
    Property,
    Signal,
    /// A `const`, or a bare native class constant.
    Constant,
    /// A *named* enum (`enum E { … }` / a native `EnumName`) — the enum type itself.
    Enum,
    /// A value of an enum (`E.A`, or a native enum value such as `MOUSE_MODE_CAPTURED`).
    EnumValue,
    /// An inner GDScript class (reachable as `Outer.Inner`).
    Class,
}

/// Where an enumerated [`MemberItem`] was **declared** — enough for `completionItem/resolve` to
/// re-fetch its long-form documentation deterministically (carry-forward (b), M8 Phase 4). A
/// member enumerated through an `extends` chain may be declared on a *different* file/class than
/// the one the cursor sits in, so the requesting buffer alone can't find its doc; this names the
/// actual declarer.
///
/// Encoded as a serializable key (a native class **name** or a declaring **[`FileId`]**) rather
/// than a borrow, so it can ride a completion item's `data` field across the
/// completion→resolve round trip without a nondeterministic name-only re-search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberOwner {
    /// Declared on a native (engine / builtin) class — resolve via
    /// [`NativeDb::lookup_member`] / [`NativeDb::lookup_builtin_member`] on this class name.
    Native(String),
    /// Declared in a project GDScript file — resolve via that file's [`Interface`], descending
    /// `inner` to the declaring inner class. The [`FileId`] is the **declaring** file, not
    /// necessarily the requesting buffer; `inner` is its inner-class chain (empty for a top-level
    /// class), so a member declared on an inner-class instance resolves its doc on the inner
    /// interface, not the file root (#152).
    Script { file: FileId, inner: Vec<String> },
    /// No recoverable declarer (an in-file `Class`-node member enumerated without a finished
    /// analysis, an enum *value* flattened off a type) — resolve has no doc source.
    Unknown,
}

/// One enumerated member — an **owned**, source-agnostic descriptor. Owning (rather than
/// borrowing the declaring `NativeClass` / `Interface` / tree) lets one uniform list mix members
/// from all three sources, which the [`members_of_type`] dispatcher requires (each arm borrows a
/// different backing store). The fields are the minimum completion needs to render an item plus
/// the [`MemberOwner`] resolve needs to re-find the long-form documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberItem {
    pub name: String,
    pub kind: MemberItemKind,
    /// A short, source-available signature/type rendering for the item's `detail`, when one is
    /// cheaply derivable (a native method/property's type, an interface member's annotation).
    /// `None` when no annotation is recorded — never a fabricated type.
    pub detail: Option<String>,
    /// The declaring class/file (carry-forward (b)): lets `completionItem/resolve` fetch the
    /// member's BBCode description from the native DB / the declaring file's interface.
    pub owner: MemberOwner,
    /// A native `static` method (or a builtin static like `Color.from_hsv`). Lets the
    /// `BuiltinTypeStatic` (`Color.`) render keep only statics, and the instance-member render
    /// drop them — Godot's `Variant::is_builtin_method_static` gate. `false` for every script
    /// member (GDScript statics aren't distinguished at enumeration here).
    pub is_static: bool,
    /// A native `virtual` method (`_ready`, `_process`, …) — the overridable set
    /// `OverrideMethod` completion offers from the native tail (Godot's
    /// `ClassDB::get_virtual_methods`). `false` for non-virtual / script members.
    pub is_virtual: bool,
}

impl MemberItem {
    /// A member with no known declarer / flags — the script-interface and in-file-class arms,
    /// where `is_static`/`is_virtual` aren't tracked at enumeration and the owner is set
    /// separately by the caller that knows the declaring file.
    fn new(name: impl Into<String>, kind: MemberItemKind, detail: Option<String>) -> Self {
        MemberItem {
            name: name.into(),
            kind,
            detail,
            owner: MemberOwner::Unknown,
            is_static: false,
            is_virtual: false,
        }
    }

    /// Set the declaring owner (builder style), for the arms that know it.
    fn with_owner(mut self, owner: MemberOwner) -> Self {
        self.owner = owner;
        self
    }
}

// ===================================================================================================
// Group 1 bridge — native class / builtin member enumeration projected into `MemberItem`.
// ===================================================================================================

/// Project a borrowed [`NativeMember`] (from [`NativeDb::all_members`] /
/// [`NativeDb::builtin_members`]) into an owned [`MemberItem`]. `declaring` is the class the
/// member resolves through (the chain link that exposes it) — it becomes the item's
/// [`MemberOwner::Native`] so resolve can re-fetch the BBCode description by name. A method's
/// `is_static`/`is_virtual` ride along (the builtin-static and override-virtual render gates).
fn native_member_item(db: &NativeDb, declaring: Option<&str>, m: &NativeMember) -> MemberItem {
    let owner = match declaring {
        Some(c) => MemberOwner::Native(c.to_owned()),
        None => MemberOwner::Unknown,
    };
    match m {
        NativeMember::Property(p) => MemberItem::new(
            db.name_of(p.name),
            MemberItemKind::Property,
            Some(db.display_type(&p.ty, declaring)),
        )
        .with_owner(owner),
        NativeMember::Method(meth) => {
            let mut it = MemberItem::new(
                db.name_of(meth.name),
                MemberItemKind::Method,
                Some(native_method_detail(db, declaring, meth)),
            )
            .with_owner(owner);
            it.is_static = meth.is_static;
            it.is_virtual = meth.is_virtual;
            it
        }
        NativeMember::Signal(s) => {
            MemberItem::new(db.name_of(s.name), MemberItemKind::Signal, None).with_owner(owner)
        }
        NativeMember::Enum(e) => {
            MemberItem::new(db.name_of(e.name), MemberItemKind::Enum, None).with_owner(owner)
        }
        NativeMember::Constant(k) => {
            MemberItem::new(db.name_of(k.name), MemberItemKind::Constant, None).with_owner(owner)
        }
        NativeMember::EnumValue { name, .. } => {
            MemberItem::new(db.name_of(*name), MemberItemKind::EnumValue, None).with_owner(owner)
        }
    }
}

/// A native method's `(params) -> Return` detail string, types rendered the editor way.
fn native_method_detail(db: &NativeDb, declaring: Option<&str>, m: &gd_types::Method) -> String {
    let params: Vec<String> = m
        .params
        .iter()
        .map(|p| {
            format!(
                "{}: {}",
                db.name_of(p.name),
                db.display_type(&p.ty, declaring)
            )
        })
        .collect();
    format!(
        "({}) -> {}",
        params.join(", "),
        db.display_type(&m.return_type, declaring)
    )
}

/// Every member of a native class, **including inherited** (the [`NativeDb::all_members`] chain
/// walk), as owned [`MemberItem`]s — `Class.<cursor>` / instance completion's native arm.
#[must_use]
pub fn native_class_members(db: &NativeDb, class: &str) -> Vec<MemberItem> {
    db.all_members(class)
        .iter()
        .map(|(decl, m)| native_member_item(db, Some(db.name_of(decl.name)), m))
        .collect()
}

/// Every member of a builtin type (`Vector2`, `Array`, …) as owned [`MemberItem`]s. `None` when
/// the builtin name is unknown to the DB.
#[must_use]
pub fn builtin_members(db: &NativeDb, builtin: &str) -> Option<Vec<MemberItem>> {
    Some(
        db.builtin_members(builtin)?
            .iter()
            .map(|m| native_member_item(db, Some(builtin), m))
            .collect(),
    )
}

// ===================================================================================================
// Group 2 — script `extends`-chain member enumeration.
// ===================================================================================================

/// Collect the script links of `start`'s `extends` chain, innermost (the start) first — a
/// **side-effect-free** re-walk of the chain (the analyzer's [`crate::script_chain`] walk records
/// nothing here and is memo-free, since a completion request is one-shot). Stops at a native
/// root, an unresolvable link, or a cycle, exactly like the ported walk — it only collects the
/// `Interface`-bearing links the member enumeration needs.
fn script_chain_links(
    xfile: &dyn CrossFileQuery,
    native: &NativeDb,
    start: &ScriptRef,
) -> Vec<ScriptRef> {
    let mut links: Vec<ScriptRef> = Vec::new();
    let mut visited: FxHashSet<ScriptRef> = FxHashSet::default();
    let mut cur = start.clone();
    loop {
        if !visited.insert(cur.clone()) {
            return links; // cycle — stop (the cyclic file reports its own error)
        }
        let Some(iface) = link_interface(xfile, &cur) else {
            links.push(cur);
            return links;
        };
        let next = match &iface.extends {
            gd_project::Extends::None => {
                links.push(cur);
                return links;
            }
            gd_project::Extends::Path(p) => match xfile.resolve_path_from(cur.file, p) {
                Some(f) => ScriptRef {
                    file: f,
                    inner: Vec::new(),
                },
                None => {
                    links.push(cur);
                    return links;
                }
            },
            gd_project::Extends::Names(names) => {
                let Some(head) = names.first() else {
                    links.push(cur);
                    return links;
                };
                if let Some(f) = xfile.global_class_file(head) {
                    ScriptRef {
                        file: f,
                        inner: names[1..].to_vec(),
                    }
                } else if native.class_named(head).is_some() {
                    links.push(cur);
                    return links; // bottoms out in a native class
                } else {
                    let chain_names: Vec<&str> = names.iter().map(String::as_str).collect();
                    if xfile.resolve_inner_chain(cur.file, &chain_names).is_some() {
                        ScriptRef {
                            file: cur.file,
                            inner: names.clone(),
                        }
                    } else {
                        links.push(cur);
                        return links;
                    }
                }
            }
        };
        links.push(cur);
        cur = next;
    }
}

/// The native class a script chain bottoms out in (`Some("RefCounted")` for an `extends`-less
/// head; `None` when the root is unknown / a cycle), for completion to chain native-member
/// enumeration onto the script members. A side-effect-free re-walk (see [`script_chain_links`]).
#[must_use]
pub fn script_chain_native_root(
    xfile: &dyn CrossFileQuery,
    native: &NativeDb,
    start: &ScriptRef,
) -> Option<String> {
    let mut visited: FxHashSet<ScriptRef> = FxHashSet::default();
    let mut cur = start.clone();
    loop {
        if !visited.insert(cur.clone()) {
            return None;
        }
        let iface = link_interface(xfile, &cur)?;
        match &iface.extends {
            gd_project::Extends::None => return Some("RefCounted".to_owned()),
            gd_project::Extends::Path(p) => {
                cur = ScriptRef {
                    file: xfile.resolve_path_from(cur.file, p)?,
                    inner: Vec::new(),
                };
            }
            gd_project::Extends::Names(names) => {
                let head = names.first()?;
                if let Some(f) = xfile.global_class_file(head) {
                    cur = ScriptRef {
                        file: f,
                        inner: names[1..].to_vec(),
                    };
                } else if native.class_named(head).is_some() {
                    // `extends Native.Something` is malformed; only the bare form has a root.
                    return (names.len() == 1).then(|| head.clone());
                } else {
                    let chain_names: Vec<&str> = names.iter().map(String::as_str).collect();
                    xfile.resolve_inner_chain(cur.file, &chain_names)?;
                    cur = ScriptRef {
                        file: cur.file,
                        inner: names.clone(),
                    };
                }
            }
        }
    }
}

/// Project one [`Interface`]'s own members (not its bases) into [`MemberItem`]s, appending only
/// names not already in `seen` (derived-shadows-base when called down a chain). Named enums and
/// their value identifiers are distinct entries; an inner class is a [`MemberItemKind::Class`].
/// `owner_file` is the file this interface belongs to — recorded on each item as
/// [`MemberOwner::Script`] so `completionItem/resolve` re-fetches the doc from the **declaring**
/// file's interface (carry-forward (b): a base-class member's doc lives in the base file, not the
/// requesting buffer).
fn collect_interface_members(
    iface: &Interface,
    owner_file: FileId,
    owner_inner: &[String],
    seen: &mut FxHashSet<String>,
    out: &mut Vec<MemberItem>,
) {
    let owner = MemberOwner::Script {
        file: owner_file,
        inner: owner_inner.to_vec(),
    };
    for m in &iface.members {
        if !seen.insert(m.name.clone()) {
            continue;
        }
        let kind = match m.kind {
            IfaceMemberKind::Const => MemberItemKind::Constant,
            IfaceMemberKind::Var | IfaceMemberKind::Property => MemberItemKind::Property,
            IfaceMemberKind::Func => MemberItemKind::Method,
            IfaceMemberKind::Signal => MemberItemKind::Signal,
            IfaceMemberKind::Enum => MemberItemKind::Enum,
        };
        out.push(
            MemberItem::new(m.name.clone(), kind, interface_member_detail(m))
                .with_owner(owner.clone()),
        );
    }
    // Named-enum value identifiers (`E.A`) — reachable as bare members through the enum, but
    // also surfaced flat the way Godot hoists them for completion.
    for e in &iface.enums {
        for v in &e.values {
            if seen.insert(v.name.clone()) {
                out.push(
                    MemberItem::new(v.name.clone(), MemberItemKind::EnumValue, None)
                        .with_owner(owner.clone()),
                );
            }
        }
    }
    // Inner classes.
    for inner in &iface.inner {
        if let Some(name) = &inner.class_name {
            if seen.insert(name.clone()) {
                out.push(
                    MemberItem::new(name.clone(), MemberItemKind::Class, None)
                        .with_owner(owner.clone()),
                );
            }
        }
    }
}

/// A short detail string for an interface member, from its written type annotation (no lattice
/// re-resolution — Phase 1 enumerates). `None` when untyped.
fn interface_member_detail(m: &gd_project::MemberDecl) -> Option<String> {
    match &m.ty {
        gd_project::TypeExpr::Named { path, .. } => Some(path.join(".")),
        gd_project::TypeExpr::None => None,
    }
}

/// Every member reachable from a script/class `start` through its `extends` chain, as owned
/// [`MemberItem`]s — methods, properties, signals, constants, named enums (+ their values), and
/// inner classes — **derived shadows base** (the first chain link to expose a name wins, the
/// enumeration twin of [`crate::reducer`]'s first-hit member walk). The chain's native tail is
/// **not** appended here; a caller wanting the full instance surface enumerates
/// [`native_class_members`] for [`script_chain_native_root`] and concatenates (native members are
/// lower-priority, so completion ranks them after the script members).
#[must_use]
pub fn script_chain_members(
    xfile: &dyn CrossFileQuery,
    native: &NativeDb,
    start: &ScriptRef,
) -> Vec<MemberItem> {
    script_chain_members_seen(xfile, native, start).0
}

/// Like [`script_chain_members`] but also returns the `seen` name set the walk accumulated, so a
/// caller appending the chain's native tail can de-dup it against the script-overridden names (the
/// dispatcher's Script arm needs this so a script overriding a native method does not yield that
/// name twice — once as the override, once as the native base). Mirrors how
/// [`script_parent_members`] threads one shared `seen` across both the script links and the native
/// tail.
fn script_chain_members_seen(
    xfile: &dyn CrossFileQuery,
    native: &NativeDb,
    start: &ScriptRef,
) -> (Vec<MemberItem>, FxHashSet<String>) {
    let mut out: Vec<MemberItem> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for link in script_chain_links(xfile, native, start) {
        if let Some(iface) = link_interface(xfile, &link) {
            collect_interface_members(iface, link.file, &link.inner, &mut seen, &mut out);
        }
    }
    (out, seen)
}

/// The members of `start`'s **parent** chain — everything strictly above `start` in the `extends`
/// chain (the immediate base script, its bases, and the native tail they ultimately inherit), as
/// owned [`MemberItem`]s. This is the `super.<cursor>` set (Godot's
/// `_find_identifiers_in_class(..., p_parent_only = true)`): `start`'s **own** members are
/// excluded by *enumerating from the parent*, so a method `start` overrides is still offered
/// (its parent declares it) — the opposite of filtering after a derived-shadows-base dedup, which
/// would wrongly drop the very method `super.method()` targets.
///
/// Side-effect-free, like [`script_chain_members`]. The native tail is appended (lower priority);
/// when `start` extends a native class directly (no script parent) the result is exactly that
/// native class's members.
#[must_use]
pub fn script_parent_members(
    xfile: &dyn CrossFileQuery,
    native: &NativeDb,
    start: &ScriptRef,
) -> Vec<MemberItem> {
    let mut out: Vec<MemberItem> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    // Skip the first link (`start` itself); collect the script links strictly above it.
    for link in script_chain_links(xfile, native, start).into_iter().skip(1) {
        if let Some(iface) = link_interface(xfile, &link) {
            collect_interface_members(iface, link.file, &link.inner, &mut seen, &mut out);
        }
    }
    // The native class the parent chain bottoms out in — the same root `start` reaches (a script
    // parent doesn't change the native root). Appended de-duped, lower priority.
    if let Some(root) = script_chain_native_root(xfile, native, start) {
        for m in native_class_members(native, &root) {
            if seen.insert(m.name.clone()) {
                out.push(m);
            }
        }
    }
    out
}

// ===================================================================================================
// Group 2b — in-file class member enumeration (the `Class` arm source).
// ===================================================================================================

/// Every member declared directly on an in-file [`ClassNode`] (one class node, no base walk), as
/// owned [`MemberItem`]s. `class_id` must name a `NodeKind::Class` in `tree`; a non-class id
/// yields an empty list (never panics). In-file base-chain walking is the analyzer's
/// [`crate::resolver::scope_classes`] job — a caller with a live context concatenates the
/// per-class results; from a finished [`crate::AnalysisResult`] an in-file class type has already
/// been rewritten to a `Script` ref (see [`crate::context::AnalysisContext::finish`]), so the
/// [`script_chain_members`] path covers it instead.
#[must_use]
pub fn class_node_members(tree: &ParseTree, class_id: NodeId) -> Vec<MemberItem> {
    let NodeKind::Class(class) = &tree.get(class_id).kind else {
        return Vec::new();
    };
    let mut out: Vec<MemberItem> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    collect_class_node_members(tree, class, &mut seen, &mut out);
    out
}

/// Push one [`ClassNode`]'s members into `out` in declaration order, skipping names in `seen`
/// (so a caller can walk a base chain derived-first). Mirrors the member taxonomy the interface
/// extractor records; member names come from each declaration's identifier child.
fn collect_class_node_members(
    tree: &ParseTree,
    class: &ClassNode,
    seen: &mut FxHashSet<String>,
    out: &mut Vec<MemberItem>,
) {
    for member in &class.members {
        let (kind, id) = match member {
            Member::Function(id) => (MemberItemKind::Method, *id),
            Member::Variable(id) => (MemberItemKind::Property, *id),
            Member::Constant(id) => (MemberItemKind::Constant, *id),
            Member::Signal(id) => (MemberItemKind::Signal, *id),
            Member::Enum(id) => (MemberItemKind::Enum, *id),
            Member::Class(id) => (MemberItemKind::Class, *id),
            Member::EnumValue(v) => {
                if let Some(name) = v.identifier.and_then(|i| node_identifier_name(tree, i)) {
                    if seen.insert(name.clone()) {
                        out.push(MemberItem::new(name, MemberItemKind::EnumValue, None));
                    }
                }
                continue;
            }
            // `@export_group` / category markers carry no addressable member name.
            Member::Group(_) => continue,
        };
        if let Some(name) = decl_identifier_name(tree, id) {
            if seen.insert(name.clone()) {
                out.push(MemberItem::new(name, kind, None));
            }
        }
    }
}

/// The `name` of an `Identifier` node, or `None` for any other node kind.
fn node_identifier_name(tree: &ParseTree, id: NodeId) -> Option<String> {
    match &tree.get(id).kind {
        NodeKind::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

/// The identifier-name of a declaration node (variable/constant/function/signal/enum/class).
fn decl_identifier_name(tree: &ParseTree, id: NodeId) -> Option<String> {
    let ident = match &tree.get(id).kind {
        NodeKind::Variable(v) => v.identifier,
        NodeKind::Constant(c) => c.identifier,
        NodeKind::Function(f) => f.identifier,
        NodeKind::Signal(s) => s.identifier,
        NodeKind::Class(c) => c.identifier,
        NodeKind::Enum(e) => e.identifier,
        _ => None,
    }?;
    node_identifier_name(tree, ident)
}

// ===================================================================================================
// Group 3 — the DataType→member-source dispatcher.
// ===================================================================================================

/// **The load-bearing member-completion primitive.** Given the resolved [`DataType`] of an
/// expression (what `smallest_typed_containing` yields for `expr` in `expr.<cursor>`), return the
/// uniform member set to complete, dispatching on [`DtKind`] exactly like Godot's
/// `_find_identifiers_in_base` arms (`gdscript_editor.cpp`):
///
/// - **Builtin** → the builtin type's members ([`builtin_members`]).
/// - **Native** → the native class's members incl. inherited ([`native_class_members`]).
/// - **Script** → the script `extends` chain's members ([`script_chain_members`]); the chain's
///   native tail is appended (lower priority) so `obj.<cursor>` on a typed script also offers the
///   engine surface it ultimately inherits.
/// - **Class** → the in-file class node's members ([`class_node_members`]). NB: a finished
///   [`crate::AnalysisResult`] never carries a `Class` type (it is rewritten to `Script` in
///   [`crate::context::AnalysisContext::finish`]), so in practice the `Script` arm serves in-file
///   classes too; this arm covers a directly-constructed `Class` `DataType`.
/// - **Enum** → the enum's values, from the type's own [`DataType::enum_values`] (no DB walk).
/// - **Variant / Resolving / Unresolved** → empty (dynamic — every member is plausible; offer
///   nothing rather than a wrong set).
///
/// Side-effect-free and read-only. `tree` is consulted only for the `Class` arm; pass the parse
/// tree the `DataType`'s `class_node` id belongs to.
#[must_use]
pub fn members_of_type(
    dt: &DataType,
    native: &NativeDb,
    xfile: &dyn CrossFileQuery,
    tree: &ParseTree,
) -> Vec<MemberItem> {
    match dt.kind {
        DtKind::Builtin => members_of_builtin(dt, native),
        DtKind::Native => native_class_members(native, &dt.native_type),
        DtKind::Script => match &dt.script_type {
            Some(sr) => {
                let (mut members, mut seen) = script_chain_members_seen(xfile, native, sr);
                // Append the native tail de-duped against the script-overridden names, so a method
                // a script overrides (`_ready`, `queue_free`, …) stays the user's own entry and is
                // not also emitted as the native base (which would point resolve at the wrong doc).
                if let Some(root) = script_chain_native_root(xfile, native, sr) {
                    for m in native_class_members(native, &root) {
                        if seen.insert(m.name.clone()) {
                            members.push(m);
                        }
                    }
                }
                members
            }
            None => Vec::new(),
        },
        DtKind::Class => match dt.class_node {
            Some(id) => class_node_members(tree, id),
            None => Vec::new(),
        },
        DtKind::Enum => enum_value_items(dt),
        DtKind::Variant | DtKind::Resolving | DtKind::Unresolved => Vec::new(),
    }
}

/// The Builtin arm: a parameterized `Array[T]` / `Dictionary[K, V]` still has the base builtin's
/// members, so we enumerate by the builtin's name. `Nil` (the untyped builtin) has no members.
fn members_of_builtin(dt: &DataType, native: &NativeDb) -> Vec<MemberItem> {
    if dt.builtin_type == VariantType::Nil {
        return Vec::new();
    }
    let name = crate::data_type::variant_type_name(dt.builtin_type);
    builtin_members(native, name).unwrap_or_default()
}

/// The Enum arm: the enum's values are already on the type ([`DataType::enum_values`]); emit them
/// sorted by name (deterministic) — no native-DB walk. Values whose integer is a placeholder
/// (`enum_values_inexact`) still enumerate: membership is known even when the value isn't.
fn enum_value_items(dt: &DataType) -> Vec<MemberItem> {
    let mut names: Vec<&String> = dt.enum_values.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|n| MemberItem::new(n.clone(), MemberItemKind::EnumValue, None))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gd_types::NativeDb;

    /// A tiny dump with a static, an instance, and a virtual method — to pin the M8 Phase 4
    /// `MemberItem` flags (`is_static`/`is_virtual`) and the `MemberOwner::Native` declaring class.
    fn db() -> NativeDb {
        NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "classes": [
                    {"name": "Object", "is_instantiable": true, "methods": [
                        {"name": "get_class", "is_const": true, "is_static": false,
                         "is_vararg": false, "is_virtual": false,
                         "return_value": {"type": "String"}}
                    ]},
                    {"name": "Node", "inherits": "Object", "is_instantiable": true, "methods": [
                        {"name": "_ready", "is_const": false, "is_static": false,
                         "is_vararg": false, "is_virtual": true,
                         "return_value": {"type": "void"}},
                        {"name": "make", "is_const": false, "is_static": true,
                         "is_vararg": false, "is_virtual": false,
                         "return_value": {"type": "Node"}}
                    ]}
                ]
            }"#,
        )
        .expect("flags dump")
    }

    #[test]
    fn native_members_carry_flags_and_native_owner() {
        let db = db();
        let members = native_class_members(&db, "Node");
        let by_name = |n: &str| members.iter().find(|m| m.name == n).cloned().unwrap();

        // A virtual method is flagged `is_virtual`, not `is_static`.
        let ready = by_name("_ready");
        assert!(ready.is_virtual && !ready.is_static, "_ready is a virtual");
        assert_eq!(ready.owner, MemberOwner::Native("Node".to_owned()));

        // A static method is flagged `is_static`, not `is_virtual`.
        let make = by_name("make");
        assert!(make.is_static && !make.is_virtual, "make is static");
        assert_eq!(make.owner, MemberOwner::Native("Node".to_owned()));

        // An inherited member's owner is the DECLARING class (Object), not the queried class.
        let get_class = by_name("get_class");
        assert_eq!(
            get_class.owner,
            MemberOwner::Native("Object".to_owned()),
            "inherited member's owner is the declaring class"
        );
        assert!(!get_class.is_virtual && !get_class.is_static);
    }

    #[test]
    fn builtin_members_carry_static_flag_and_owner() {
        let db = NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "builtin_classes": [
                    {"name": "Color", "is_keyed": false,
                     "constants": [{"name": "RED", "type": "Color", "value": "Color(1,0,0,1)"}],
                     "methods": [
                        {"name": "from_hsv", "is_const": false, "is_static": true,
                         "is_vararg": false, "return_type": "Color", "arguments": []},
                        {"name": "lerp", "is_const": true, "is_static": false,
                         "is_vararg": false, "return_type": "Color", "arguments": []}
                     ]}
                ]
            }"#,
        )
        .expect("builtin dump");
        let members = builtin_members(&db, "Color").expect("Color members");
        let from_hsv = members.iter().find(|m| m.name == "from_hsv").unwrap();
        assert!(from_hsv.is_static, "from_hsv is a static builtin method");
        let lerp = members.iter().find(|m| m.name == "lerp").unwrap();
        assert!(!lerp.is_static, "lerp is an instance builtin method");
    }
}
