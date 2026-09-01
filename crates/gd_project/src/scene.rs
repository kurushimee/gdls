//! Tolerant text parser for Godot `.tscn` scene files + the per-scene query model.
//!
//! `.tscn` is Godot's `ConfigFile`-flavoured scene text format (`format=3` for 4.x): a sequence of
//! `[section ...]` headers, each optionally followed by `key = value` body lines, with
//! `config_version`-style scalars never appearing at top level. gdls reads scenes to learn the
//! *node tree shape* a `.gd` script sees through `$Node` / `%Unique` / `get_node()` — node names,
//! their types, the script attached to each node, and which sub-scenes are instanced where.
//!
//! **Scope.** This module owns parsing + an on-[`Scene`] query API. It does NOT feed the analyzer's
//! diagnostic path — a valid `$`/`%` types as bare `NATIVE Node` ([`gd_analyze`], faithful to Godot),
//! independent of the scene. This index is the substrate the precise NAVIGATION surfaces read
//! (hover / definition / completion). The parser is the foundation; correctness on realistic input
//! is the bar.
//!
//! **Never crash, never lie** (`CLAUDE.md`). [`parse_scene`] returns a (possibly partial) [`Scene`]
//! for *any* input — malformed, truncated, binary garbage — and never panics. Unknown sections and
//! keys (e.g. the fork-specific `unique_id=` Pixelorama writes) are tolerated without losing the
//! relations we do understand. The instanced-sub-scene resolver recurses into sub-scene *text*
//! (anti-catalog W16: no engine instantiation, never shell out to Godot) under a `visited` cycle
//! guard + a depth cap, so a cyclic or pathological instance graph terminates.
//!
//! **Node-path convention.** Paths are *root-relative*: the scene root's own name is the empty path
//! (its node is reached as [`Scene::root`]); a node with `parent="."` has path `"Child"`; a node
//! with `parent="A/B"` has path `"A/B/Leaf"`. This matches how a script attached to the scene root
//! resolves `$A/B/Leaf` (Godot resolves `$Rel` relative to the node owning the script — usually the
//! root). Phase 2 maps an arbitrary attachment node's relative `$` access against these paths.

use camino::Utf8Path;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

/// Maximum sub-scene instance recursion depth for [`Scene::resolve_instanced_root`]. A backstop
/// behind the `visited` cycle set: even an acyclic but pathologically deep instance chain (or one
/// the `visited` set somehow misses on a malformed input) terminates here rather than blowing the
/// stack. Real scene-instance nesting is shallow (single digits); 64 is far past any real project.
pub const MAX_INSTANCE_DEPTH: usize = 64;

/// One entry of a scene's `[ext_resource]` table.
///
/// `id` is the *string* key other sections reference via `ExtResource("<id>")` — Godot ids are
/// strings that are frequently numeric (`"2"`) but may be alphanumeric (`"13_4dhva"`). `path` is the
/// `res://…` target (preferred); `uid` is the `uid://…` it was written with. A scene saved by a
/// recent editor carries both, and `path` is what every consumer here reads; a `uid`-only entry is
/// not resolved back to a path.
#[derive(Clone, Debug, Eq, Serialize, Deserialize)]
pub struct ExtResource {
    /// `type="…"` — e.g. `"Script"`, `"PackedScene"`, `"Texture2D"`. May be empty if omitted.
    pub ty: String,
    /// `path="res://…"`, if present.
    pub path: Option<String>,
    /// `uid="uid://…"`, if present.
    pub uid: Option<String>,
    /// Byte offsets `(start, end)` into the parsed `.tscn` TEXT of the `path="…"` value BETWEEN its
    /// surrounding double-quotes (so `text[start..end] == path` verbatim), or `None` when the span
    /// could not be isolated as a plain single-line double-quoted string. This is the exact-span
    /// anchor `willRenameFiles` rewrites; it is the live-text-only spelling and is NOT serialized
    /// (a warm-loaded scene re-derives it on demand by reparsing), so the cache format is unchanged.
    #[serde(skip)]
    pub path_span: Option<(usize, usize)>,
}

// `path_span` is a LIVE-TEXT byte coordinate, not part of an ext_resource's identity (two scenes
// with the same `type`/`path`/`uid` are equal regardless of where in the file the path sits). It is
// also `#[serde(skip)]`, so a warm-loaded scene deserializes it as `None` while a freshly-parsed one
// holds `Some(span)` — deriving `PartialEq` would make a cache round-trip unequal to a fresh parse
// (the documented warm-load equality class). Excluding it here keeps `ExtResource` equality identity-
// based and the round-trip stable.
impl PartialEq for ExtResource {
    fn eq(&self, other: &Self) -> bool {
        self.ty == other.ty && self.path == other.path && self.uid == other.uid
    }
}

/// The resolved nature of a node's type.
///
/// A plain `[node type="Control"]` is [`Native`](NodeType::Native). A node with no `type=` but an
/// `instance=ExtResource(id)` is an instanced sub-scene ([`Instanced`](NodeType::Instanced)) — its
/// concrete root type comes from that PackedScene's own root, resolved lazily by
/// [`Scene::resolve_instanced_root`] (parsing the sub-scene text). A node with neither is
/// [`Unknown`](NodeType::Unknown) (tolerated, never an error).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeType {
    /// A native/engine class named directly via `type="Foo"`.
    Native(String),
    /// An instanced sub-scene: the `res://….tscn` PackedScene path its root type comes from.
    /// `None` when the `instance=ExtResource(id)` referenced an id not in the ext-resource table
    /// or a resource with no resolvable path (degrade, never lie).
    Instanced(Option<String>),
    /// Neither a `type=` nor a resolvable `instance=` — type unknown.
    Unknown,
}

/// A node within a parsed [`Scene`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneNode {
    /// The `name="…"` of this node (as written; may contain spaces / `%` / `/`-free arbitrary text).
    pub name: String,
    /// This node's resolved type (native, instanced sub-scene, or unknown).
    pub ty: NodeType,
    /// The `parent="…"` value verbatim: `None` for the root, `"."` for a direct child of the root,
    /// or a `/`-joined relative path (`"A/B"`) for a nested node.
    pub parent: Option<String>,
    /// Root-relative path of this node (see the module doc). Empty string for the root.
    pub path: String,
    /// `res://….gd` of the script attached to this node via `script = ExtResource(id)`, if any.
    pub script: Option<String>,
    /// `true` iff the node body carried `unique_name_in_owner = true` — i.e. it is reachable via
    /// `%Name` from a script in the same owner scene. The `%`-name is this node's [`Self::name`].
    pub unique_name_in_owner: bool,
}

/// A parsed `.tscn` scene: its ext-resource table, node tree, and the derived lookups Phase 2
/// scene-typing consumes.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    /// The scene's own `uid="uid://…"` from its `[gd_scene]` header, if present.
    pub uid: Option<String>,
    /// `ext_resource id → {type, path, uid}`. Keys are the string ids `ExtResource(...)` references.
    pub ext_resources: FxHashMap<String, ExtResource>,
    /// All nodes, in file order. Index `0` is the root iff [`Self::root`] is `Some(0)`.
    pub nodes: Vec<SceneNode>,
    /// Index into [`Self::nodes`] of the scene root (the node with no `parent`), if one was found.
    pub root: Option<usize>,
    /// `unique_name → root-relative node path`, for every node with `unique_name_in_owner = true`.
    /// This is the `%Name` lookup table.
    pub unique_names: FxHashMap<String, String>,
    /// Index: root-relative node path → index into [`Self::nodes`]. Built once at parse time so
    /// [`Self::node_at`] is O(1).
    path_to_node: FxHashMap<String, usize>,
}

impl Scene {
    /// The root node, if the scene had one (a node with no `parent`).
    #[must_use]
    pub fn root_node(&self) -> Option<&SceneNode> {
        self.root.and_then(|i| self.nodes.get(i))
    }

    /// The `res://….gd` script attached to the scene root, if any. This is the script Godot treats
    /// as the scene's "owner script" — the one whose `$`/`%` accesses resolve against this tree.
    #[must_use]
    pub fn root_script_path(&self) -> Option<&str> {
        self.root_node().and_then(|n| n.script.as_deref())
    }

    /// Look up a node by its root-relative path (`""` = root, `"A/B"` = nested). O(1).
    #[must_use]
    pub fn node_at(&self, path: &str) -> Option<&SceneNode> {
        self.path_to_node.get(path).and_then(|&i| self.nodes.get(i))
    }

    /// The resolved [`NodeType`] of the node at `path`, if that node exists.
    #[must_use]
    pub fn node_type(&self, path: &str) -> Option<&NodeType> {
        self.node_at(path).map(|n| &n.ty)
    }

    /// The node reachable as `%name` (a `unique_name_in_owner` node), by its unique name.
    #[must_use]
    pub fn node_by_unique_name(&self, name: &str) -> Option<&SceneNode> {
        let path = self.unique_names.get(name)?;
        self.node_at(path)
    }

    /// Every `res://….gd` script this scene attaches to one of its nodes (root or descendant), in
    /// node order, de-duplicated by first appearance. This is the set whose consumers a `.tscn`
    /// edit must re-diagnose (the scene→script invalidation edge).
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn attached_scripts(&self) -> impl Iterator<Item = &str> + '_ {
        let mut seen = FxHashSet::default();
        self.nodes
            .iter()
            .filter_map(|n| n.script.as_deref())
            .filter(move |s| seen.insert(s.to_owned()))
    }

    /// Every `res://….tscn` sub-scene this scene instances (via a node's
    /// `instance=ExtResource(id)`), de-duplicated. This is the scene→scene(instance) edge: editing
    /// one of these sub-scenes transitively affects this scene's consumers.
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub fn instanced_scenes(&self) -> impl Iterator<Item = &str> + '_ {
        let mut seen = FxHashSet::default();
        self.nodes
            .iter()
            .filter_map(|n| match &n.ty {
                NodeType::Instanced(Some(path)) => Some(path.as_str()),
                _ => None,
            })
            .filter(move |s| seen.insert(s.to_owned()))
    }

    /// Resolve the concrete *root type* a node contributes, following an instanced sub-scene into
    /// its `.tscn` text recursively (anti-catalog W16: text only, no engine).
    ///
    /// For a [`NodeType::Native`] node this is just its native type. For an instanced sub-scene,
    /// `lookup(res_path)` is asked for the sub-scene's source text; that scene's root is resolved,
    /// and if *its* root is itself an instanced sub-scene the walk continues — bounded by a
    /// `visited` set (the normalized sub-scene path is inserted *before* recursing) and
    /// [`MAX_INSTANCE_DEPTH`]. Returns:
    ///   * `Some(ResolvedRoot)` with the native type and/or attached script reached, or
    ///   * `None` when the chain can't be resolved (missing lookup, depth/cycle cap, no root) —
    ///     degrade to "unknown", never panic, never lie.
    ///
    /// `lookup` takes a `res://…` path and returns the sub-scene's text if available (the caller
    /// wires this to the scene index / VFS). Callers should normalize the paths they return text
    /// for consistently (e.g. via [`normalize_res`]) so two spellings of one path don't escape the
    /// `visited` cycle set.
    #[must_use]
    pub fn resolve_instanced_root<'a, F>(&self, path: &str, lookup: &F) -> Option<ResolvedRoot>
    where
        F: Fn(&str) -> Option<std::borrow::Cow<'a, str>>,
    {
        let node = self.node_at(path)?;
        let mut visited = FxHashSet::default();
        resolve_node_root(node, lookup, &mut visited, 0)
    }

    /// Resolve this scene's **own root** type, following an instanced root into its sub-scene text
    /// (the common Phase-2 query: "what concrete type/script does this scene present?"). Equivalent
    /// to [`Self::resolve_instanced_root`] on the root node's path, but it doesn't require the
    /// caller to know the root's name. `None` if the scene has no root or the chain can't resolve.
    #[must_use]
    pub fn resolve_root_type<'a, F>(&self, lookup: &F) -> Option<ResolvedRoot>
    where
        F: Fn(&str) -> Option<std::borrow::Cow<'a, str>>,
    {
        let root = self.root_node()?;
        let mut visited = FxHashSet::default();
        resolve_node_root(root, lookup, &mut visited, 0)
    }

    /// Construct a [`Scene`] from its source-of-truth parts (`uid`, ext-resource table, nodes) and
    /// rebuild every derived field (`root`, `unique_names`, the path index) via [`finalize`]. This
    /// is the single chokepoint both [`parse_scene`] and cache deserialization route through, so a
    /// warm-loaded scene is identical to a freshly-parsed one *by construction* — no derived map is
    /// ever served empty/stale (the documented warm-load failure class). `nodes` keeps the `path`
    /// each node was parsed with; `finalize` recomputes it the same way, so a round-trip is stable.
    #[must_use]
    pub fn from_parts(
        uid: Option<String>,
        ext_resources: FxHashMap<String, ExtResource>,
        nodes: Vec<SceneNode>,
    ) -> Self {
        let mut scene = Scene {
            uid,
            ext_resources,
            nodes,
            root: None,
            unique_names: FxHashMap::default(),
            path_to_node: FxHashMap::default(),
        };
        finalize(&mut scene);
        scene
    }
}

// `Scene` serializes only its source-of-truth (uid, ext_resources, nodes); the derived lookups
// (`root`, `unique_names`, `path_to_node`) are rebuilt via `from_parts`/`finalize` on deserialize,
// mirroring `IndexCache`'s "store sources, rebuild inverses" discipline. This makes a cache
// round-trip identical to a fresh parse by construction.
#[derive(Serialize, Deserialize)]
struct SceneRepr {
    uid: Option<String>,
    ext_resources: FxHashMap<String, ExtResource>,
    nodes: Vec<SceneNode>,
}

impl Serialize for Scene {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SceneRepr {
            uid: self.uid.clone(),
            ext_resources: self.ext_resources.clone(),
            nodes: self.nodes.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Scene {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = SceneRepr::deserialize(deserializer)?;
        Ok(Scene::from_parts(repr.uid, repr.ext_resources, repr.nodes))
    }
}

/// The concrete root a node resolves to once instanced sub-scenes are followed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRoot {
    /// The deepest native/engine type reached (e.g. `"Control"`), if any.
    pub native_type: Option<String>,
    /// The `res://….gd` script attached at the resolved root, if any.
    pub script: Option<String>,
}

/// Resolve one node's contributed root, recursing through instanced sub-scenes via `lookup`.
fn resolve_node_root<'a, F>(
    node: &SceneNode,
    lookup: &F,
    visited: &mut FxHashSet<String>,
    depth: usize,
) -> Option<ResolvedRoot>
where
    F: Fn(&str) -> Option<std::borrow::Cow<'a, str>>,
{
    match &node.ty {
        NodeType::Native(ty) => Some(ResolvedRoot {
            native_type: Some(ty.clone()),
            // A node can carry BOTH a local `script=` and (rarely) an `instance=`; for a plain
            // native node the local script is the attachment.
            script: node.script.clone(),
        }),
        NodeType::Instanced(Some(sub_path)) => {
            if depth >= MAX_INSTANCE_DEPTH {
                return None; // depth backstop — degrade, never recurse unbounded
            }
            // Insert BEFORE recursing so a self- or mutually-instancing scene graph terminates.
            // (We key on the raw res:// string; callers normalize consistently, and a `.tscn`'s
            // own ext_resource paths are already plain `res://…` as Godot writes them.)
            if !visited.insert(sub_path.clone()) {
                return None; // cycle — already on the current resolution path
            }
            let text = lookup(sub_path)?;
            let sub = parse_scene(&text);
            let sub_root = sub.root_node()?;
            // The local `script=` on the *instancing* node overrides the sub-scene's root script
            // (Godot: a script set on an instance placeholder replaces the packed root's script).
            let local_script = node.script.clone();
            let mut resolved = resolve_node_root(sub_root, lookup, visited, depth + 1)?;
            if local_script.is_some() {
                resolved.script = local_script;
            }
            Some(resolved)
        }
        // An instance whose ExtResource id didn't resolve to a path, or a typeless/instanceless
        // node: the best we can say is whatever local script it had.
        NodeType::Instanced(None) | NodeType::Unknown => Some(ResolvedRoot {
            native_type: None,
            script: node.script.clone(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Parser.
// ---------------------------------------------------------------------------

/// Parse `.tscn` text into a [`Scene`]. Panic-free and partial-on-error: any input yields a Scene
/// (possibly empty); unknown sections/keys are tolerated. See the module doc for the contract.
#[must_use]
pub fn parse_scene(text: &str) -> Scene {
    let mut scene = Scene::default();
    // We hold the node currently accumulating body lines as an index into `scene.nodes` plus the
    // pending `instance=` ext-resource id (resolved to a NodeType only once the body is seen, so a
    // `type=`-less instanced node and a `script=` line are both available).
    let mut cur_node: Option<usize> = None;

    for (line_offset, logical) in logical_lines(text) {
        let line = logical.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if let Some(inner) = section_header(line) {
            cur_node = None; // leaving any node body
                             // Absolute byte offset of `inner` (the text between the `[` `]`) within the original
                             // `text`: the logical line's own offset plus the distance from the logical line's start
                             // to where `inner` begins (the leading whitespace `trim` removed, then the `[`). Lets the
                             // `ext_resource` arm anchor its `path="…"` value span back to absolute `text` coordinates.
            let inner_offset = line_offset + (inner.as_ptr() as usize - logical.as_ptr() as usize);
            handle_section(&mut scene, inner, inner_offset, &mut cur_node);
            continue;
        }
        // A body `key = value` line. Only `[node]` bodies carry relations we keep.
        if let Some((key, val)) = split_key_value(line) {
            if let Some(node_idx) = cur_node {
                apply_node_body(&mut scene, node_idx, key, val);
            }
        }
    }

    finalize(&mut scene);
    scene
}

/// Dispatch a `[section ...]` header (the text between the brackets) into the scene. `inner_offset`
/// is the absolute byte offset of `inner` within the original `.tscn` text, used only to anchor the
/// `ext_resource` `path="…"` value span.
fn handle_section(
    scene: &mut Scene,
    inner: &str,
    inner_offset: usize,
    cur_node: &mut Option<usize>,
) {
    let (kind, attrs_text) = split_section_kind(inner);
    match kind {
        "gd_scene" => {
            let attrs = parse_attrs(attrs_text);
            if let Some(uid) = attrs.get("uid") {
                scene.uid = Some(uid.clone());
            }
        }
        "ext_resource" => {
            let attrs = parse_attrs(attrs_text);
            // An ext_resource with no id is unreferenceable; skip it rather than invent a key.
            if let Some(id) = attrs.get("id") {
                let path = attrs.get("path").cloned();
                // Anchor the `path="…"` value's byte span (between the quotes) back to absolute
                // `text` coordinates, fail-closed (None unless it is a plain double-quoted string
                // whose inner equals the parsed value). `inner` is a verbatim substring of `text`
                // at `inner_offset`, so a span found within `inner` shifts by `inner_offset`.
                let path_span = path
                    .as_deref()
                    .and_then(|p| ext_path_value_span(inner, p))
                    .map(|(s, e)| (s + inner_offset, e + inner_offset));
                scene.ext_resources.insert(
                    id.clone(),
                    ExtResource {
                        ty: attrs.get("type").cloned().unwrap_or_default(),
                        path,
                        uid: attrs.get("uid").cloned(),
                        path_span,
                    },
                );
            }
        }
        "node" => {
            let attrs = parse_attrs(attrs_text);
            // A node with no name can't be addressed; still record it so paths of *its* children
            // (which reference it by name) don't silently misparent — but an empty name is the
            // best we can do.
            let name = attrs.get("name").cloned().unwrap_or_default();
            let parent = attrs.get("parent").cloned();
            // type= (native) takes precedence; else instance=ExtResource(id) (sub-scene); else
            // Unknown. The instance ref is resolved against the ext-resource table to a .tscn path.
            let ty = if let Some(t) = attrs.get("type") {
                NodeType::Native(t.clone())
            } else if let Some(inst) = attrs.get("instance") {
                NodeType::Instanced(resolve_ext_ref_path(scene, inst))
            } else {
                NodeType::Unknown
            };
            let idx = scene.nodes.len();
            scene.nodes.push(SceneNode {
                name,
                ty,
                parent,
                path: String::new(), // filled in `finalize`
                script: None,
                unique_name_in_owner: false,
            });
            *cur_node = Some(idx);
        }
        // `sub_resource`, `connection`, `editable`, and any unknown/extra section: tolerated, no
        // relations kept. `cur_node` was already cleared by the caller so their body lines (if any)
        // are ignored.
        _ => {}
    }
}

/// Apply one `key = value` line of a `[node]` body to that node. A no-op if `node_idx` is somehow
/// out of range (defensive — the caller only ever passes a freshly-pushed index).
fn apply_node_body(scene: &mut Scene, node_idx: usize, key: &str, val: &str) {
    match key {
        "script" => {
            // `script = ExtResource("2")` → resolve to its res:// path against the ext table. A
            // `script = null` (clearing an instance's script) resolves to None and is left as-is.
            // Resolve first (immutable borrow of the table), then write (mutable borrow of nodes).
            if let Some(path) = resolve_ext_ref_path(scene, val) {
                if let Some(node) = scene.nodes.get_mut(node_idx) {
                    node.script = Some(path);
                }
            }
        }
        "unique_name_in_owner" => {
            if let Some(node) = scene.nodes.get_mut(node_idx) {
                node.unique_name_in_owner = val.trim() == "true";
            }
        }
        // Any other body key (layout_mode, theme, the fork's unique_id, groups, …) is irrelevant
        // to the node tree shape — tolerate and discard.
        _ => {}
    }
}

/// Finalize derived state after all sections are parsed: assign each node its root-relative path,
/// pick the root, and build the path + unique-name lookups.
fn finalize(scene: &mut Scene) {
    // The root is the first node with no `parent` (Godot writes exactly one; tolerate >1 by taking
    // the first and treating the rest as if rooted too).
    scene.root = scene.nodes.iter().position(|n| n.parent.is_none());

    // Compute each node's root-relative path from its `parent` + `name`.
    //   parent == None        → root        → path = ""              (the root itself)
    //   parent == "."         → child of root → path = name
    //   parent == "A/B"       → nested       → path = "A/B/name"
    // We resolve in file order; Godot always writes parents before children, so a single forward
    // pass suffices for the path string (which is purely lexical — it does not require the parent
    // node to exist).
    for i in 0..scene.nodes.len() {
        let path = match &scene.nodes[i].parent {
            None => String::new(),
            Some(p) if p == "." => scene.nodes[i].name.clone(),
            Some(p) => {
                if scene.nodes[i].name.is_empty() {
                    p.clone()
                } else {
                    format!("{p}/{}", scene.nodes[i].name)
                }
            }
        };
        scene.nodes[i].path = path;
    }

    // Build the path → index map (last writer wins on a duplicate path — a malformed scene with two
    // identical paths is degraded, not rejected) and the unique-name table.
    scene.path_to_node.clear();
    scene.unique_names.clear();
    for (i, node) in scene.nodes.iter().enumerate() {
        scene.path_to_node.insert(node.path.clone(), i);
        if node.unique_name_in_owner && !node.name.is_empty() {
            scene
                .unique_names
                .insert(node.name.clone(), node.path.clone());
        }
    }
}

/// Resolve an `ExtResource("<id>")` / `ExtResource(<id>)` reference value through the scene's
/// ext-resource table. Returns `None` only if the value isn't an `ExtResource(...)` ref or the id
/// is unknown.
///
/// A `path`-less entry yields its `uid://…` VERBATIM rather than nothing. Parsing is pure — it has
/// no project uid map to consult — so the uid is carried through for [`crate::SceneIndex`] to
/// canonicalize at insert time. Nothing downstream of the index ever sees a `uid://` here. #484.
fn resolve_ext_ref_path(scene: &Scene, value: &str) -> Option<String> {
    let id = parse_ext_resource_id(value)?;
    let res = scene.ext_resources.get(&id)?;
    res.path.clone().or_else(|| res.uid.clone())
}

/// Extract the id from an `ExtResource("13_4dhva")` or `ExtResource(2)` value. The id is returned
/// without surrounding quotes. `None` if the value isn't an `ExtResource(...)` reference.
fn parse_ext_resource_id(value: &str) -> Option<String> {
    let inner = value
        .trim()
        .strip_prefix("ExtResource(")?
        .strip_suffix(')')?
        .trim();
    Some(unquote(inner).to_owned())
}

/// The byte span (relative to `inner`) of the `path="<value>"` attribute VALUE — the bytes *between*
/// the double-quotes — for an `[ext_resource …]` header whose inner text is `inner`, or `None`
/// (fail-closed) when the `path` attribute is absent, isn't a plain double-quoted string, or its
/// quoted content doesn't equal `expected_value` verbatim.
///
/// This is the single-source-of-truth span the `willRenameFiles` `.tscn` rewrite (#131) edits. It is
/// computed by the SAME tolerant attribute walk [`parse_attrs`] uses — quote/bracket/escape aware —
/// so the located `path` key is the real attribute, never a substring of some other value (e.g. a
/// `uid="…path…"`). The span covers ONLY the inner bytes (quotes excluded), and only when the raw
/// inner slice equals `expected_value` (no escapes / continuation), so a caller can replace exactly
/// those bytes without touching the quotes. Mirrors `file_operations::inner_string_span`'s discipline.
fn ext_path_value_span(inner: &str, expected_value: &str) -> Option<(usize, usize)> {
    // Walk `inner` as (byte_offset, char) so every slice point is a valid boundary (multibyte-safe,
    // matching `parse_attrs`). For each `key=value` pair, when the key is `path`, capture the value's
    // byte range; require it to be a plain `"…"` double-quoted string whose content == expected.
    let chars: Vec<(usize, char)> = inner.char_indices().collect();
    let len = inner.len();
    let byte_at = |k: usize| chars.get(k).map_or(len, |&(b, _)| b);
    let m = chars.len();
    let mut k = 0usize;
    while k < m {
        while k < m && chars[k].1.is_whitespace() {
            k += 1;
        }
        if k >= m {
            break;
        }
        // Read a key: up to `=` or whitespace.
        let key_start = byte_at(k);
        while k < m {
            let c = chars[k].1;
            if c == '=' || c.is_whitespace() {
                break;
            }
            k += 1;
        }
        let key = inner[key_start..byte_at(k)].trim();
        while k < m && chars[k].1.is_whitespace() {
            k += 1;
        }
        if k >= m || chars[k].1 != '=' {
            continue; // a bare token with no `=` — skip, mirroring parse_attrs
        }
        k += 1; // consume '='
        while k < m && chars[k].1.is_whitespace() {
            k += 1;
        }
        // Read the value with the same quote/bracket/escape awareness as parse_attrs, but remember
        // the value's start so we can return a span (parse_attrs only keeps the unquoted string).
        let val_start = byte_at(k);
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escaped = false;
        while k < m {
            let c = chars[k].1;
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_str = false;
                }
                k += 1;
                continue;
            }
            match c {
                '"' => in_str = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ if c.is_whitespace() && depth == 0 => break,
                _ => {}
            }
            k += 1;
        }
        let val_end = byte_at(k);
        if key != "path" {
            continue;
        }
        // The value must be a plain double-quoted string `"…"` whose inner equals expected verbatim.
        let raw = &inner[val_start..val_end];
        let stripped = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"'))?;
        if stripped != expected_value {
            return None; // escapes / unexpected spelling — refuse (caller falls back to no rewrite)
        }
        // Inner span excludes the two quote bytes (`"` is one byte each).
        return Some((val_start + 1, val_end - 1));
    }
    None
}

// ---------------------------------------------------------------------------
// Lexical helpers.
// ---------------------------------------------------------------------------

/// Is `line` (already trimmed) a `[section ...]` header? Returns the text between the outer
/// brackets. A header is a single complete `[...]`; the depth-aware [`logical_lines`] merge upstream
/// guarantees a wrapped property value never reaches here as a stray `[`-leading line.
fn section_header(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    rest.strip_suffix(']')
}

/// Split a section's inner text into `(kind, rest)` where kind is the leading bareword
/// (`gd_scene`, `ext_resource`, `node`, …) and rest is the attribute text after it.
fn split_section_kind(inner: &str) -> (&str, &str) {
    let inner = inner.trim_start();
    match inner.find(char::is_whitespace) {
        Some(i) => (&inner[..i], inner[i..].trim_start()),
        None => (inner, ""),
    }
}

/// Split a body line into `(key, value)` on the first top-level `=` (one not inside quotes or
/// brackets). `None` if there is no such `=`.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    // Iterate by `char_indices` so every slice point is a valid UTF-8 boundary — a `.tscn` value or
    // key can contain multibyte text, and byte-index slicing would panic mid-character ("never
    // crash"). `=` is ASCII, so `i + 1` after a boundary `=` is itself a boundary.
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '=' if depth == 0 => {
                let key = line[..i].trim();
                let val = line[i + 1..].trim();
                if key.is_empty() {
                    return None;
                }
                return Some((key, val));
            }
            _ => {}
        }
    }
    None
}

/// Parse a header's attribute text — a whitespace-separated list of `key=value` pairs, values being
/// either `"quoted strings"`, `ExtResource(...)`/`Object(...)` calls, or barewords — into a map.
/// Tolerant: a malformed pair is skipped, never fatal.
///
/// UTF-8-safe: it walks a `(byte_offset, char)` table so every slice point is a valid char boundary
/// — a header value can carry multibyte text, and naive byte-index slicing would panic mid-character
/// ("never crash"). All slice ends come from a real char boundary or `text.len()`.
fn parse_attrs(text: &str) -> FxHashMap<String, String> {
    let mut map = FxHashMap::default();
    // (byte_offset, char) for each char, so `chars[k].0` is always a valid boundary.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let len = text.len();
    // Byte offset of char index `k` (or end-of-string when `k` is past the last char).
    let byte_at = |k: usize| chars.get(k).map_or(len, |&(b, _)| b);
    let m = chars.len();
    let mut k = 0usize;
    while k < m {
        // Skip leading whitespace / stray separators.
        while k < m && chars[k].1.is_whitespace() {
            k += 1;
        }
        if k >= m {
            break;
        }
        // Read a key: up to `=` or whitespace.
        let key_start = byte_at(k);
        while k < m {
            let c = chars[k].1;
            if c == '=' || c.is_whitespace() {
                break;
            }
            k += 1;
        }
        let key = text[key_start..byte_at(k)].trim();
        // Skip whitespace before a possible `=`.
        while k < m && chars[k].1.is_whitespace() {
            k += 1;
        }
        if k >= m || chars[k].1 != '=' {
            // A bare token with no `=` (rare in headers); ignore it.
            continue;
        }
        k += 1; // consume '='
        while k < m && chars[k].1.is_whitespace() {
            k += 1;
        }
        // Read the value, honoring quotes + nested brackets so `Object(...,"x":1)` stays one token.
        let val_start = byte_at(k);
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escaped = false;
        while k < m {
            let c = chars[k].1;
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_str = false;
                }
                k += 1;
                continue;
            }
            match c {
                '"' => in_str = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ if c.is_whitespace() && depth == 0 => break,
                _ => {}
            }
            k += 1;
        }
        let val = text[val_start..byte_at(k)].trim();
        if !key.is_empty() {
            map.insert(key.to_owned(), unquote(val).to_owned());
        }
    }
    map
}

/// Strip one layer of surrounding double-quotes from `s` if present; otherwise return `s` as-is.
fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
}

/// Merge physical lines into logical lines so a property value that opens a bracket/quote and wraps
/// across physical lines stays one logical line — and, crucially, a `[` at the start of such a
/// continuation line is NOT mistaken for a section header. Mirrors `project_godot::logical_lines`:
/// section headers, comments, and blanks at depth 0 are atomic.
///
/// Each logical line is returned with the absolute byte offset of its FIRST physical line's start
/// within `text`, so a span found inside a (single-physical-line) section header can be anchored
/// back to absolute `text` coordinates — the `ext_resource` `path="…"` value span needs this. A
/// section header is always atomic (a single physical line, pushed verbatim), so its returned string
/// is byte-identical to `text[offset..offset + line.len()]`.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut buf_offset = 0usize; // byte offset of `buf`'s first physical line within `text`
    let mut depth = 0i32;
    let mut in_str = false;

    // `str::lines` strips the terminator and hides byte offsets, so walk the byte offset manually.
    let mut offset = 0usize;
    for line in text.lines() {
        let line_offset = offset;
        // Advance past this physical line plus its terminator. `lines` accepts both `\n` and
        // `\r\n`; recover the terminator width from the gap to the next line (or end of text).
        offset += line.len();
        if offset < text.len() {
            // The next byte is `\n` (LF) or `\r\n` (CRLF) — `lines` consumed exactly one of them.
            offset += if text.as_bytes().get(offset) == Some(&b'\r') {
                2
            } else {
                1
            };
        }

        if buf.is_empty() && depth == 0 && !in_str {
            let head = line.trim_start();
            if head.is_empty() || head.starts_with(';') || head.starts_with('[') {
                out.push((line_offset, line.to_owned()));
                continue;
            }
        }
        if buf.is_empty() {
            buf_offset = line_offset;
        } else {
            buf.push('\n');
        }
        buf.push_str(line);
        scan_depth(line, &mut depth, &mut in_str);
        if depth <= 0 && !in_str {
            depth = 0;
            out.push((buf_offset, std::mem::take(&mut buf)));
        }
    }
    if !buf.is_empty() {
        out.push((buf_offset, buf));
    }
    out
}

/// Update bracket depth / in-string state across one physical line (for [`logical_lines`]).
fn scan_depth(line: &str, depth: &mut i32, in_str: &mut bool) {
    let mut escaped = false;
    for c in line.chars() {
        if *in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                *in_str = false;
            }
        } else {
            match c {
                '"' => *in_str = true,
                '(' | '[' | '{' => *depth += 1,
                ')' | ']' | '}' => *depth -= 1,
                _ => {}
            }
        }
    }
}

/// Normalize a `res://` path for use as a scene-index / cycle-detection key, matching the
/// crate's [`crate::index::normalize`] discipline for filesystem paths. For a `res://…` string we
/// only need a stable spelling: trim, and collapse `\` → `/` (Godot writes `/`, but a hand-edited
/// or fork file might not). Non-`res://` strings pass through trimmed.
#[must_use]
pub fn normalize_res(res: &str) -> String {
    res.trim().replace('\\', "/")
}

/// Convenience: does this path name a scene file gdls indexes? **`.tscn` only** — gdls parses scene
/// TEXT (anti-catalog W16), and `.scn` is Godot's *binary* scene form with no text to parse. Keeping
/// this to `.tscn` aligns the cold/warm-start scan (`SceneIndex::build`, the warm stat-diff) with the
/// watcher, whose glob is `**/*.tscn` and which deliberately excludes `.scn` for the same reason — so
/// a `.scn` is never indexed at startup only to go forever-unmaintained by the watcher.
#[must_use]
pub fn is_scene_path(path: &Utf8Path) -> bool {
    path.extension() == Some("tscn")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    const REALISTIC: &str = r#"[gd_scene load_steps=4 format=3 uid="uid://dbylw5k04ulp8"]

[ext_resource type="Theme" uid="uid://dog5j8wjiwikc" path="res://assets/theme.tres" id="1"]
[ext_resource type="Script" uid="uid://dlxqc0hc51xu4" path="res://src/Main.gd" id="2"]
[ext_resource type="PackedScene" uid="uid://bsgwar3l6qtgv" path="res://src/UI/TopMenuContainer.tscn" id="3"]

[node name="Control" type="Control"]
layout_mode = 3
theme = ExtResource("1")
script = ExtResource("2")

[node name="MenuAndUI" type="VBoxContainer" parent="."]
layout_mode = 2

[node name="UniqueLabel" type="Label" parent="MenuAndUI"]
unique_name_in_owner = true
text = "hi"

[node name="TopMenuContainer" parent="MenuAndUI" instance=ExtResource("3")]
layout_mode = 2
"#;

    #[test]
    fn realistic_scene_every_relation_extracted() {
        let s = parse_scene(REALISTIC);
        // Scene uid from the header.
        assert_eq!(s.uid.as_deref(), Some("uid://dbylw5k04ulp8"));
        // ext_resource table.
        assert_eq!(s.ext_resources.len(), 3);
        assert_eq!(
            s.ext_resources.get("2").unwrap().path.as_deref(),
            Some("res://src/Main.gd")
        );
        // Root + root script.
        let root = s.root_node().unwrap();
        assert_eq!(root.name, "Control");
        assert_eq!(root.ty, NodeType::Native("Control".into()));
        assert_eq!(root.path, "");
        assert_eq!(s.root_script_path(), Some("res://src/Main.gd"));
        // Node types + paths.
        assert_eq!(
            s.node_type("MenuAndUI"),
            Some(&NodeType::Native("VBoxContainer".into()))
        );
        assert_eq!(
            s.node_type("MenuAndUI/UniqueLabel"),
            Some(&NodeType::Native("Label".into()))
        );
        // Unique name table (keyed off unique_name_in_owner, NOT a `%` prefix).
        let uniq = s.node_by_unique_name("UniqueLabel").unwrap();
        assert_eq!(uniq.path, "MenuAndUI/UniqueLabel");
        // Instanced sub-scene: type is the PackedScene path; no native type.
        assert_eq!(
            s.node_type("MenuAndUI/TopMenuContainer"),
            Some(&NodeType::Instanced(Some(
                "res://src/UI/TopMenuContainer.tscn".into()
            )))
        );
        // attached_scripts iterates the one script.
        let scripts: Vec<&str> = s.attached_scripts().collect();
        assert_eq!(scripts, vec!["res://src/Main.gd"]);
        // instanced_scenes lists the sub-scene.
        let inst: Vec<&str> = s.instanced_scenes().collect();
        assert_eq!(inst, vec!["res://src/UI/TopMenuContainer.tscn"]);
    }

    #[test]
    fn malformed_and_truncated_never_panic() {
        // Truncated mid-header.
        let _ = parse_scene("[gd_scene format=3 uid=\"uid://ab");
        // Binary-ish garbage.
        let _ = parse_scene("\u{0}\u{1}\u{2}not a scene at all %%%\n[[[]]]\n=== ===");
        // Header with no closing bracket then a real node.
        let s = parse_scene("[node name=\"A\" type=\"Node\"\n[node name=\"B\" type=\"Node2D\"]");
        // The unterminated first header is not a valid section; B should still parse.
        assert!(s.node_at("B").is_some() || s.nodes.iter().any(|n| n.name == "B"));
        // Empty input.
        let e = parse_scene("");
        assert!(e.nodes.is_empty());
        assert!(e.root.is_none());
    }

    #[test]
    fn unknown_sections_and_keys_tolerated() {
        let s = parse_scene(
            r#"[gd_scene format=3]
[some_future_section foo="bar" id="9"]
[node name="Root" type="Node" unique_id=123456 groups=["a","b"]]
weird_key = Object(Nonsense, "x":1)
script = ExtResource("nope_unknown_id")
[connection signal="pressed" from="Root" to="." method="_x"]
"#,
        );
        // Root parsed despite the fork-specific unique_id= and groups= header keys.
        let root = s.root_node().unwrap();
        assert_eq!(root.name, "Root");
        assert_eq!(root.ty, NodeType::Native("Node".into()));
        // An unknown ExtResource id resolves to no script (degrade, never lie).
        assert_eq!(root.script, None);
    }

    #[test]
    fn node_paths_with_slashes() {
        let s = parse_scene(
            r#"[gd_scene format=3]
[node name="Root" type="Node"]
[node name="A" type="Node" parent="."]
[node name="B" type="Node" parent="A"]
[node name="C" type="Node" parent="A/B"]
"#,
        );
        assert_eq!(s.node_at("A").unwrap().name, "A");
        assert_eq!(s.node_at("A/B").unwrap().name, "B");
        assert_eq!(s.node_at("A/B/C").unwrap().name, "C");
        assert_eq!(s.node_type("A/B/C"), Some(&NodeType::Native("Node".into())));
    }

    #[test]
    fn names_with_special_chars() {
        // Real Pixelorama node names include spaces and `%`.
        let s = parse_scene(
            r#"[gd_scene format=3]
[node name="Root" type="Node"]
[node name="100%" type="Button" parent="."]
[node name="3D Object Tree" type="Tree" parent="."]
"#,
        );
        assert_eq!(s.node_at("100%").unwrap().name, "100%");
        assert_eq!(s.node_at("3D Object Tree").unwrap().name, "3D Object Tree");
    }

    #[test]
    fn bare_and_quoted_ext_resource_ids() {
        // ExtResource(2) (bare) and ExtResource("2") (quoted) both resolve.
        let s = parse_scene(
            r#"[gd_scene format=3]
[ext_resource type="Script" path="res://a.gd" id="2"]
[node name="Root" type="Node"]
script = ExtResource(2)
"#,
        );
        assert_eq!(s.root_script_path(), Some("res://a.gd"));
    }

    #[test]
    fn multiline_value_does_not_desync_sections() {
        // A property value with a bracketed continuation: the `[` starting line 2 must NOT be read
        // as a section header, so the [node] after it still parses.
        let s = parse_scene(
            "[gd_scene format=3]\n\
             [sub_resource type=\"Animation\" id=\"A\"]\n\
             tracks = [{\n\
             \"path\": NodePath(\"x\")\n\
             }]\n\
             [node name=\"Root\" type=\"Node\"]\n",
        );
        assert!(
            s.root_node().is_some(),
            "the node after a multi-line sub_resource value must still parse"
        );
        assert_eq!(s.root_node().unwrap().name, "Root");
    }

    #[test]
    fn instanced_root_resolves_through_subscene_text() {
        // Parent scene instances child.tscn; child's root is a Panel with a script.
        let parent = r#"[gd_scene format=3]
[ext_resource type="PackedScene" path="res://child.tscn" id="1"]
[node name="Root" type="Node"]
[node name="Sub" parent="." instance=ExtResource("1")]
"#;
        let child = r#"[gd_scene format=3]
[ext_resource type="Script" path="res://child.gd" id="1"]
[node name="ChildRoot" type="Panel"]
script = ExtResource("1")
"#;
        let s = parse_scene(parent);
        let lookup = |p: &str| -> Option<Cow<'static, str>> {
            if p == "res://child.tscn" {
                Some(Cow::Borrowed(child))
            } else {
                None
            }
        };
        let resolved = s.resolve_instanced_root("Sub", &lookup).unwrap();
        assert_eq!(resolved.native_type.as_deref(), Some("Panel"));
        assert_eq!(resolved.script.as_deref(), Some("res://child.gd"));
    }

    #[test]
    fn cyclic_instance_graph_terminates() {
        // a.tscn instances b.tscn whose root instances a.tscn — a cycle.
        let a = r#"[gd_scene format=3]
[ext_resource type="PackedScene" path="res://b.tscn" id="1"]
[node name="ARoot" parent="." instance=ExtResource("1")]
"#;
        let b = r#"[gd_scene format=3]
[ext_resource type="PackedScene" path="res://a.tscn" id="1"]
[node name="BRoot" parent="." instance=ExtResource("1")]
"#;
        // Each scene's ROOT is itself the instanced node (no parent), so this is a root↔root
        // instance cycle: a's root instances b, b's root instances a.
        let a = a.replace("parent=\".\" ", "");
        let b = b.replace("parent=\".\" ", "");
        let sa = parse_scene(&a);
        let lookup = move |p: &str| -> Option<Cow<'static, str>> {
            match p {
                "res://a.tscn" => Some(Cow::Owned(a.clone())),
                "res://b.tscn" => Some(Cow::Owned(b.clone())),
                _ => None,
            }
        };
        // Must terminate (return None at the cycle), not hang or overflow. `resolve_root_type`
        // resolves a's root (the instanced node) directly.
        let resolved = sa.resolve_root_type(&lookup);
        assert!(
            resolved.is_none(),
            "a cyclic instance graph must terminate (None), not hang or overflow; got {resolved:?}"
        );
    }

    #[test]
    fn deep_instance_chain_hits_depth_cap() {
        // Each scene N instances scene N+1, acyclically and unboundedly — only the depth cap (not
        // the visited set, since every path is distinct) can stop it. Must return None at the cap.
        let sa = parse_scene(
            "[gd_scene format=3]\n\
             [ext_resource type=\"PackedScene\" path=\"res://s1.tscn\" id=\"1\"]\n\
             [node name=\"R\" instance=ExtResource(\"1\")]\n",
        );
        let lookup = |p: &str| -> Option<Cow<'static, str>> {
            // s<N>.tscn instances s<N+1>.tscn, forever (distinct paths each time).
            let n: usize = p
                .strip_prefix("res://s")
                .and_then(|s| s.strip_suffix(".tscn"))
                .and_then(|s| s.parse().ok())?;
            Some(Cow::Owned(format!(
                "[gd_scene format=3]\n\
                 [ext_resource type=\"PackedScene\" path=\"res://s{}.tscn\" id=\"1\"]\n\
                 [node name=\"R{n}\" instance=ExtResource(\"1\")]\n",
                n + 1
            )))
        };
        // Terminates at MAX_INSTANCE_DEPTH rather than recursing forever.
        assert!(sa.resolve_root_type(&lookup).is_none());
    }

    #[test]
    fn instance_with_local_script_overrides_subscene_root_script() {
        let parent = r#"[gd_scene format=3]
[ext_resource type="PackedScene" path="res://child.tscn" id="1"]
[ext_resource type="Script" path="res://override.gd" id="2"]
[node name="Root" type="Node"]
[node name="Sub" parent="." instance=ExtResource("1")]
script = ExtResource("2")
"#;
        let child = r#"[gd_scene format=3]
[ext_resource type="Script" path="res://base.gd" id="1"]
[node name="ChildRoot" type="Panel"]
script = ExtResource("1")
"#;
        let s = parse_scene(parent);
        let lookup = |p: &str| -> Option<Cow<'static, str>> {
            (p == "res://child.tscn").then_some(Cow::Borrowed(child))
        };
        let resolved = s.resolve_instanced_root("Sub", &lookup).unwrap();
        assert_eq!(resolved.native_type.as_deref(), Some("Panel"));
        // The local script on the instancing node wins.
        assert_eq!(resolved.script.as_deref(), Some("res://override.gd"));
    }

    #[test]
    fn serde_round_trip_preserves_query_api() {
        // The round-trip must be identical THROUGH THE QUERY API, not just field equality — a
        // deserialized Scene with empty derived maps would pass a nodes-vector compare but lie on
        // every node_at/node_by_unique_name lookup. This pins the finalize-on-deserialize chokepoint.
        let original = parse_scene(REALISTIC);
        let json = serde_json::to_string(&original).unwrap();
        let restored: Scene = serde_json::from_str(&json).unwrap();

        // Derived state rebuilt: root, root script, node-by-path, node type, unique name.
        assert_eq!(
            restored.root_node().map(|n| &n.name),
            Some(&"Control".into())
        );
        assert_eq!(restored.root_script_path(), Some("res://src/Main.gd"));
        assert_eq!(
            restored.node_type("MenuAndUI/UniqueLabel"),
            Some(&NodeType::Native("Label".into()))
        );
        let uniq = restored.node_by_unique_name("UniqueLabel").unwrap();
        assert_eq!(uniq.path, "MenuAndUI/UniqueLabel");
        assert_eq!(
            restored.node_type("MenuAndUI/TopMenuContainer"),
            Some(&NodeType::Instanced(Some(
                "res://src/UI/TopMenuContainer.tscn".into()
            )))
        );
        // Source-of-truth preserved.
        assert_eq!(restored.uid, original.uid);
        assert_eq!(restored.nodes, original.nodes);
        assert_eq!(restored.ext_resources, original.ext_resources);
        // And the rebuilt derived maps match the fresh ones exactly.
        assert_eq!(restored.unique_names, original.unique_names);
        assert_eq!(restored.root, original.root);
    }

    #[test]
    fn multibyte_chars_do_not_panic() {
        // Regression (fuzz-found): byte-index slicing in parse_attrs / split_key_value panicked
        // when a slice point landed mid-UTF-8-character. A node name, attr value, or body value
        // with multibyte text must parse without panic. `\u{360}` is the exact 2-byte char the
        // fuzzer hit; add CJK + emoji for breadth.
        let s = parse_scene(
            "[gd_scene format=3 uid=\"uid://\u{360}xyz\"]\n\
             [ext_resource type=\"Script\" path=\"res://\u{360}.gd\" id=\"1\"]\n\
             [node name=\"日本語ノード\" type=\"Node\"]\n\
             script = ExtResource(\"1\")\n\
             label = \"emoji 🎮 value\"\n\
             [node name=\"Child\u{360}\" type=\"Label\" parent=\".\"]\n\
             unique_name_in_owner = true\n",
        );
        // The multibyte-named root parsed and got its script.
        assert_eq!(s.root_node().map(|n| n.name.as_str()), Some("日本語ノード"));
        assert_eq!(s.root_script_path(), Some("res://\u{360}.gd"));
        // The multibyte-named child resolved by its path and is a unique node.
        assert!(s.node_at("Child\u{360}").is_some());
        assert!(s.node_by_unique_name("Child\u{360}").is_some());
    }

    #[test]
    fn is_scene_path_classifies() {
        assert!(is_scene_path(Utf8Path::new("res/Main.tscn")));
        // `.scn` is binary (no text to parse) and the watcher excludes it, so it is NOT indexed —
        // keeping scan aligned with watch.
        assert!(!is_scene_path(Utf8Path::new("a/b.scn")));
        assert!(!is_scene_path(Utf8Path::new("a/b.gd")));
        assert!(!is_scene_path(Utf8Path::new("a/b.tres")));
    }

    #[test]
    fn ext_resource_path_span_anchors_value_between_quotes() {
        // The span the #131 .tscn rewrite edits: byte offsets into the TEXT of the `path="…"` value,
        // quotes excluded, so `text[start..end]` is the path verbatim — for the script and the
        // instanced-PackedScene ext_resources alike.
        let text = "[gd_scene format=3]\n\
            [ext_resource type=\"Script\" path=\"res://player.gd\" id=\"1\"]\n\
            [ext_resource type=\"PackedScene\" uid=\"uid://x\" path=\"res://child.tscn\" id=\"2\"]\n\
            [node name=\"Player\" type=\"Node\"]\n";
        let s = parse_scene(text);

        let script = s.ext_resources.get("1").expect("ext_resource 1");
        let (a, b) = script.path_span.expect("script path span");
        assert_eq!(&text[a..b], "res://player.gd");
        assert_eq!(script.path.as_deref(), Some("res://player.gd"));

        // The PackedScene's `path` span is found even though a `uid="…"` precedes it (the attribute
        // walk locates the real `path` key, never a substring of another value).
        let packed = s.ext_resources.get("2").expect("ext_resource 2");
        let (c, d) = packed.path_span.expect("packed path span");
        assert_eq!(&text[c..d], "res://child.tscn");
    }

    #[test]
    fn ext_resource_path_span_none_when_uid_only() {
        // A uid-only ext_resource (no `path=`) has no span to rewrite — fail-closed None.
        let text = "[gd_scene format=3]\n\
            [ext_resource type=\"Script\" uid=\"uid://abc\" id=\"1\"]\n";
        let s = parse_scene(text);
        let r = s.ext_resources.get("1").expect("ext_resource 1");
        assert_eq!(r.path, None);
        assert_eq!(r.path_span, None);
    }
}
