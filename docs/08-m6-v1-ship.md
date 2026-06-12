# 08 — M6: exposed-capability parity + warm-start cache (the v1 ship milestone)

**Status: SHIPPED — `v1.0.0` tagged 2026-06-10.** M6 is complete — every exposed-capability parity item (A–G) plus
autoload-singleton typing landed, the persistent warm-start cache is in (14.7× on a 3,000-file synthetic
project; multi-instance-safe), and an OSS capability walk against a real Godot 4.6.3 project
(Pixelorama) returns complete data on every row. Both ratchets hold (parser 186/186, analyzer 300/300).
`v1.0.0` is tagged from the integration branch on merge. The spec below is preserved as authored
(2026-06-01); see [`CHANGELOG.md`](../CHANGELOG.md) for what landed and the Phase-2 deferrals.

The M5 Phase H walk against a large real-world GDScript project returned GREEN **against the original bar** ("no crashes,
no wrong answers, safe-but-sometimes-incomplete is acceptable"). The bar has since been raised:

> Every **currently-exposed** LSP capability must be **fully** correct — no inaccurate or
> incomplete data for any input, with **no regressions against Godot's own GDScript LSP**. Any
> previously-deferred follow-up whose absence yields incomplete/inaccurate output is pulled into
> M6. Plus: the per-startup reindex cost must be **cached** so a long startup happens once, not
> every launch.

This doc enumerates what that bar adds — the **M6** scope — grounded in (a) what Godot's own LSP actually
does — inspected in Godot's `modules/gdscript/language_server/` — and
(b) gdls's current code paths. `v1.0.0` stays untagged until these land and a re-run of the Phase H
walk is clean.

---

## 1. Parity baseline — what Godot's own LSP actually does

This is the reference for "no regressions." Source: Godot's `modules/gdscript/language_server/`
(`godot_lsp.h`, `gdscript_text_document.cpp`, `gdscript_workspace.cpp`, `gdscript_extend_parser.cpp`).

| Capability | Godot's own LSP | gdls today | Verdict |
|---|---|---|---|
| `hover` | signature + doc + "Defined in" link, **including member/method signatures** (`text_document.cpp:347`, `godot_lsp.h:1284`) | type-name only for native/class_name identifiers; **member/call/preload show the base placeholder** | **regression → M6** |
| `definition` | `Location` at symbol; resolves `class_name` in expression position via `is_global_class` (`workspace.cpp:650`) | resolves in-file members + `class_name` in `extends`; **null for `class_name` in expression** | **regression → M6** |
| preload `res://` nav | via **`documentLink`** on any file-resolving string literal (`extend_parser.cpp:186-215`), *not* `definition` | not implemented (no documentLink; definition returns null) | **gap → M6** (mechanism TBD) |
| autoload nav | no dedicated autoload handling found *(unverified)* | null | exceeds-Godot completeness → M6 (cheap) |
| `references` | **project-wide**, textual name-scan + per-hit re-resolve (`workspace.cpp:472`) — finds cross-file callsites through typed vars | in-file + cross-file *class* refs; **misses cross-file method/signal callsites through typed vars** | **regression → M6** |
| `documentSymbol` | **hierarchical** (root Class → members, inner classes nested; `text_document.cpp:139`, `godot_lsp.h:1257`) | **flat** (drops the enclosing Class container) | **regression → M6** |
| `implementation` | **NOT supported** (`implementationProvider=false`, `godot_lsp.h:1768`) | exposed; subtype-nav for classes, null for methods | bonus (no parity bar) → M6 *optional* |
| `callHierarchy` | **NOT supported** (no field/handler) | exposed, works | bonus, no gap |
| `workspace/symbol` | **NOT supported** (`workspaceSymbolProvider=false`) | exposed, works | bonus, no gap |
| `completion` / `signatureHelp` | supported | not exposed | **Phase 2** (per `docs/00 §3`) |
| `rename` / `documentHighlight` / `declaration` / `documentLink` / onTypeFormatting | supported | not exposed | see §4 (decision) |

**Two headline facts that shape scope:**

1. gdls already **exceeds** Godot on `implementation`, `callHierarchy`, and `workspace/symbol` —
   Godot's own LSP advertises none of them. So those are not regressions; completing
   `implementation` for methods is optional polish, not a parity requirement.
2. gdls is **behind** Godot on `hover` (member signatures), `references` (cross-file callsites),
   `documentSymbol` (hierarchy), and resource-string navigation (Godot's `documentLink`). These are
   the genuine "incomplete data on an exposed capability" gaps the new bar targets.

---

## 2. M6 capability-completeness work items

Each item: the gap, the parity rationale, current behavior (`file:line`), the fix shape, and a
size/risk estimate. None touch the faithful-port analyzer/parser fidelity (all glue/projection),
so the 300/300 + 186/186 ratchets are not at risk — but every item must keep the full gate green.

### M6-A — `documentSymbol`: nest members under the enclosing Class  *(smallest, do first)*
- **Gap / parity:** Godot returns a hierarchical tree; gdls returns a flat member list with no Class
  container. A file with inner classes loses its nesting.
- **Current:** the parser projection `document_symbols` (`crates/gd_syntax/src/parser.rs:4380-4385`)
  calls `class_member_symbols(tree, tree.root)` — it emits the root class's *members* directly and
  drops the root Class node. The `DocumentSymbol { children }` type already supports nesting
  (inner classes/enums are nested at `parser.rs:4419-4458`); the server handler
  (`crates/gd_server/src/handlers.rs` `document_symbol` → `to_lsp_symbol`) preserves children
  faithfully. **The flattening is purely in the parser projection.**
- **Fix:** always wrap the script's members in a single root `Class` `DocumentSymbol` (range = whole
  script) — unconditionally, matching Godot's `parse_class_symbol` (`gdscript_extend_parser.cpp:240-252`).
  Named by `class_name` if present (selectionRange = identifier span); otherwise empty name + zero-width
  selectionRange at file start, with the server handler filling the file basename for unnamed scripts.
  Match Godot's SymbolKinds (Class 5, Method 6/Function 12, Variable 13/Property 7, Constant 14,
  Enum 10, Signal/Event 24) and drop `local` symbols as Godot does (`godot_lsp.h:1272`).
- **Size/risk:** small / low. Localized to one function + a documentSymbol test update.

### M6-B — `definition`: resolve `class_name` used in expression position
- **Gap / parity:** Godot resolves `Foo.bar()` → `Foo`'s script (`workspace.cpp:650`); gdls returns
  null (works only in `extends`).
- **Current:** `definition` (`handlers.rs:180-207`) takes `cursor_identifier(node)`; for `Utils` in
  `Utils.get_children_that(...)` the innermost node is the `Subscript` base, and
  `cursor_identifier` likely returns `None` for a non-bare-`Identifier` node → early null. `Utils`
  **is** a registered `class_name` (`src/utils/utils.gd:3`), so `find_global_class_definition`
  would succeed if it received the name.
- **Fix:** when the innermost node at the cursor is a `Subscript`/attribute whose **base** is an
  `Identifier` covering the cursor byte, resolve that base name through the existing
  `find_global_class_definition` (`handlers.rs:467`, backed by `ClassNameRegistry`). Confirm the
  exact node shape during implementation.
- **Size/risk:** small / low.

### M6-C — `definition` (or `documentLink`) on `preload("res://…")` / `load("res://…")` strings
- **Gap / parity:** Godot makes these clickable via **`documentLink`** (fires on *any* file-resolving
  string literal, `extend_parser.cpp:186-215`), not `definition`.
- **Current:** `definition` requires a bare `Identifier` (`handlers.rs:193`); a string-literal path
  node is not one → null. No `documentLink` capability is exposed.
- **Available:** `Index::resolve_res_path(res) -> Option<FileId>` (`crates/gd_project/src/index.rs:316`,
  already used by `reduce_preload` at `reducer.rs:4916`); `Index::path(fid)` → `path_to_file_uri`
  (`handlers.rs:470`). `reduce_preload` already pins a `Script` type with the resolved `FileId`.
- **Fix — two options (recommendation: start with the definition extension):**
  - **(C1, recommended)** Extend `definition`: if the cursor is inside a `Preload`/`load(...)`
    argument string, fold the path, `resolve_res_path` → `Location` at the target's root-class
    identifier (or file start). Minimal surface, directly answers "jump to the file."
  - **(C2, Godot-faithful)** Add a `documentLink` provider that turns every `res://` string literal
    resolving to an existing file into a link. Broader (matches Godot exactly) but a new exposed
    capability + server-capability flag. Can follow C1 later.
- **Size/risk:** C1 small / low; C2 medium / low.

### M6-D — `definition` on an autoload reference  *(exceeds Godot; cheap)*
- **Gap / parity:** Godot's LSP has no dedicated autoload resolution *(unverified)*; this would
  exceed parity. Included because the data exists and autoload nav is a common, expected jump.
- **Available:** `project.godot` is parsed into `ProjectGodot { autoloads: Vec<Autoload> }`, each
  `Autoload { name, target: ResTarget::Script("res://….gd") }`
  (`crates/gd_project/src/project_godot.rs:53-96,150-156`). *(Unverified: whether `Index`/`ProjectModel`
  re-exposes a name→path accessor at the server layer — may need a small accessor.)*
- **Fix:** add an autoload name→script-path lookup on `ProjectModel`/`Index`; in `definition`, map
  an identifier matching an autoload name → `resolve_res_path`/`file_id` → `Location`.
- **Size/risk:** small-medium / low. (Optional within M6 if time-boxed — flagged as
  exceeds-parity.)

### M6-E — `references`: include cross-file member/signal callsites through typed variables
- **Gap / parity:** Godot's references is project-wide and finds `c.get_current_value()` across
  files (textual scan + re-resolve, `workspace.cpp:472`). gdls misses them.
- **Current:** `references` (`handlers.rs:634-721`) projects only `Binding::Use` via
  `push_binding_locations` (`:729-759`, **`Binding::Call` deliberately skipped**) + a raw-name
  identifier scan, over candidates from `Index::name_referencers` (`:686`). The cross-file
  `c.get_current_value()` site is a `Binding::Call`, and `name_referencers` (an interface-level
  filter) won't list a file that only *calls* the method through a typed local.
- **Available:** `Binding::Call { callee_file, callee_name, call_site }`
  (`crates/gd_analyze/src/binding.rs:90`) holds exactly the cross-file callsite, dispatch-resolved by
  `resolve_callee_file` (`reducer.rs:73`); `find_incoming_calls` (callHierarchy) already filters
  Calls by `(callee_file, name)`.
- **Fix:** when the cursor target is a method/signal, also project matching `Binding::Call`
  call-site ranges (extract the callee-identifier sub-span to avoid wide dupes; de-dupe against the
  identifier scan), and broaden the candidate set beyond `name_referencers` for member targets
  (the analysis-cache / xref data already spans the project for open + analyzed files).
- **Size/risk:** medium / medium. Touches the references candidate-gathering; needs careful de-dup
  + a cross-file fixture test. Parity target: ⊇ what Godot's textual scan would return.

### M6-F — `hover`: render member/call/preload signatures
- **Gap / parity:** Godot's hover shows the member's full signature + doc (`godot_lsp.h:1284`); gdls
  shows the base type placeholder for member access / calls / preload.
- **Current:** `render_hover` (`handlers.rs:268-349`) renders a type *name* only for bare native/
  class_name identifiers (`:291-303`); otherwise it emits `analyzed.types.get(typed_id)` — the
  call's resolved return type or the widened ancestor's type — never a member signature
  (deferral noted at `:289-290`).
- **Available:** `Binding::Call { callee_file, callee_name, call_site }` per resolved call;
  `reduce_subscript` types attribute access (`reducer.rs:3939`); the callee's params + return live
  in `MemberDecl { kind, ty, params, .. }` (`crates/gd_analyze/src/interface.rs:84-90`), reachable
  via `CrossFileQuery::interface(file)`; native member docs via `NativeClass`
  (already used by `append_class_docs`, `handlers.rs:351`).
- **Fix:** when the leaf is the callee/attribute identifier of a `Call`/`Subscript`, resolve the
  member's `MemberDecl` (cross-file or native) and format `func name(p: T, …) -> R` (+ doc for
  native). For `preload(...)`, render the resolved script path/type. Keep the existing type-name
  branch for declaration-anchor identifiers.
- **Size/risk:** medium / low-medium. Pure projection; no analyzer change.

### M6-G — `implementation` for method overrides  *(bonus capability; optional)*
- **Gap / parity:** Godot has **no** `implementation`, so there is no parity bar. gdls exposes it;
  on a method cursor it returns null (it does class-subtype nav only). `workspace/symbol` already
  surfaces every same-named method decl across files, so the information is reachable today.
- **Current:** `implementation` (`handlers.rs:811-931`) resolves a class name and BFSes the
  inverse-extends closure, emitting each subclass's root-class identifier.
- **Fix:** when the cursor is on a `func` identifier, compute the enclosing class, reuse the existing
  inverse-extends BFS to get subclasses, then for each subclass `Interface` emit a `Location` for any
  `MemberDecl { kind: Func, name == target }` (using `member.line`).
- **Size/risk:** small-medium / low. **Recommended to include** (removes the "null where overrides
  exist" surprise on an exposed capability), but droppable to Phase 2 if time-boxed since Godot has
  no equivalent.

---

## 3. Persistent per-project index cache  *(the startup-cost item — required)*

**Problem.** Every launch pays the full cost twice: `Index::build` reads + `gd_syntax::parse`es +
interface-extracts **every** `.gd` (`crates/gd_project/src/index.rs:143-194`) — the ~3.4 s
cold-index — and then `Workspace::reconcile` (`crates/gd_server/src/workspace.rs:485-649`)
**re-reads + re-parses every file again** to diff `signature_hash` (`:578-585`) — the ~8.5 s on
a large real-world GDScript project. The event loop only arms after both (`server.rs` constructs the watcher + runs
reconcile before `loop {`), so the editor is unresponsive for ~12 s on **every** start. A one-time
warm-up is fine; paying it every launch is not.

**Goal (was a Phase 2 exit criterion, pulled forward into M6):** warm start of a large project from on-disk cache
is **> 5× faster** than a cold scan (`docs/07 §1` Phase-2 row). On a large real-world GDScript project that means a
~stat-only warm start in well under ~2 s.

**Design.**

- **What to persist:** the eager-interface `Index` for the project — at minimum the per-file
  `Interface` table + `ClassNameRegistry` (the reverse indexes `name_referencers`,
  `path_referencers`, `deps.reverse`, `file_refs` are all derivable, so store forward data and
  rebuild edges via `recompute_edges`/`finish_cold_index` on load, OR store whole and let
  `verify()` check it). The `Index` is **all plain data** today (`index.rs:41-79`,
  `interface.rs:108-138`, `depgraph.rs`, `registry.rs`), so this is feasible.
- **Cache key (whole-file validity):** `(cache_format_version, gdls_version,
  NativeDb::content_hash, project.godot fingerprint)`. `NativeDb::content_hash` already exists
  (`crates/gd_types/src/native_db.rs:158,249`) and is the documented top-level key
  (`docs/01:112-114`, `docs/07 §1`). Any mismatch ⇒ discard the whole cache (native lattice / config
  changed ⇒ all interfaces stale).
- **Per-file validity (net-new):** store `(path, size, mtime_ns)` per file. On load, **stat** each
  file (no read): unchanged ⇒ reuse the cached `Interface`; changed/new ⇒ re-parse just those;
  missing ⇒ drop. This is the 5× win — warm start becomes a stat sweep + a handful of re-parses
  instead of 2338 full parses. (mtime+size is the fast, read-free check; a content-hash fallback can
  be added if mtime proves unreliable on some FS, but mtime+size matches the freshness model Godot
  and most LSPs use.)
- **Storage location:** **recommendation — project-local `<root>/.gdls/index.<format-ver>.bin`**.
  Simple, discoverable, user-clearable, no new OS-dirs dependency. **Requires adding `.gdls/` to the
  index exclusion set** (`docs/03 §6` exclusions: `.godot/ .import/ .git/ target/ node_modules/` —
  `.gdls/` is *not* currently excluded) so the cache file never re-enters the index. *Alternative:* a
  global cache dir (`%LOCALAPPDATA%`/XDG via the `directories` crate) keyed by hashed root path —
  avoids writing into the project tree but adds a dependency + key management. Decision point for the
  user; default to project-local.
- **Serialization:** `serde` derives on `Index`/`Interface`/`MemberDecl`/`EnumDecl`/`DepGraph`/
  `ClassNameRegistry`/`FileId`/`ByteSpan` (all **net-new** — none derive serde today; `camino` needs
  its `serde1` feature, `ByteSpan` is gd_syntax-owned). Format: `serde_json` is already a workspace
  dep (zero new dep) and deserializing a few-MB interface table is tens of ms — acceptable for v1;
  a compact binary format (`postcard`/`bincode`) is a possible optimization, noted not required.
- **Load path safety (non-negotiable — "never crash, never lie"):**
  1. Read cache; on any parse/IO error ⇒ log + fall back to full cold-index (never trust blind).
  2. Validate the whole-file key; mismatch ⇒ discard + cold-index.
  3. Preserve `FileId` stability: the `paths` arena is append-only and ids must not shift
     (`index.rs` invariant 1) — deserialize `paths` in exact stored order; new files append.
  4. Per-file stat validation (above); re-parse the deltas.
  5. Run `Index::verify()` + quarantine violators exactly as `Index::build` does
     (`index.rs:181-192`, unconditional, not `debug_assert!`) — a corrupt cache degrades, it never
     poisons the session.
  6. **Still run a (cheap, mtime-based) reconcile** as the drift backstop for changes made while the
     server was off — see M6-H.
- **Write timing:** write the cache after the initial index is built + reconciled (one-time
  ~few-ms serialize), guarded so a write failure only logs. Do not block the event loop; if needed,
  write on shutdown as well. Single-threaded loop means no locking concern (`docs/03 §6.1`).

**Convergence with reconcile (M6-H):** the same per-file `(size, mtime_ns)` table lets
`reconcile` skip the full re-parse — it can classify added/modified/removed by **stat diff** and only
re-parse the changed files, instead of re-parsing all 2338 (`workspace.rs:578-585`). This both
removes the ~8.5 s startup block *and* makes the cache's drift-backstop cheap. (Note doc-rot:
`docs/03 §6.1` already *claims* reconcile hashes `(path, mtime, size)` but the code does a full
re-parse — this work makes the doc true.)

### M6-I — Multi-instance / concurrent-process safety  *(load-bearing for the cache)*

**Today gdls is already safe for concurrent use, and the cache must not break that.** Verified by
grep: every filesystem write in production code is inside `#[cfg(test)]`; the *only* non-test write
path is `gdls bench --record <path>` (`crates/gd_server/src/bench.rs:220-225`), an opt-in debug
reproducer to a user-chosen path. A normal LSP session writes **nothing** — it only reads `.gd`
files, `project.godot`, and `extension_api.json`.

So the real-world scenario — **Claude Code's gdls process and an IDE's gdls process running against
the same project at once** — is fine as-is: each editor spawns its own server (the standard LSP
model), each holds an independent in-memory `Index` + VFS + LRU caches, the `notify` watchers
coexist (multiple `ReadDirectoryChangesW`/inotify watches on one tree are supported), and there is
no shared mutable state and no project write to race on. This is exactly how every per-editor LSP
server already behaves; nothing special is required *today*.

**The cache (§3) is the one thing that introduces a shared writable artifact** — both processes
would read *and* write `<root>/.gdls/index.bin`. Without care that means torn reads (one process
reads while the other writes) and write clobbering. Requirements the cache implementation MUST meet
so concurrent use stays safe:

- **Atomic writes, last-writer-wins.** Serialize to a unique temp file in the same dir, then
  atomically rename over the target (`tempfile::NamedTempFile::persist` — `tempfile` is already a
  workspace dep — or an equivalent atomic replace). A reader therefore only ever sees a complete old
  *or* complete new file, never a torn one. Two concurrent writers ⇒ both write a *valid* full cache
  and the last rename wins; since the cache is a derived artifact of identical on-disk project state,
  either is correct. On Windows the replace must tolerate the target being open by another reader
  (retry / `ReplaceFileW` semantics) — don't fail the session on a replace error, just skip the
  write (next start cold-indexes).
- **No hard lock.** A cross-process advisory lock is unnecessary and fragile (stale-lock deadlock if
  a process is killed). The cache is throwaway: a lost or skipped write costs exactly one cold-index
  next launch. Atomic-rename + tolerant-read is sufficient and self-healing.
- **Tolerant reads (already in §3's safety list).** A read/parse failure or a key/`verify()`
  mismatch ⇒ fall back to cold-index. A briefly-inconsistent file (shouldn't happen with atomic
  rename, but belt-and-suspenders) degrades, never crashes.
- **Per-process temp-file names** so two simultaneous writers don't collide on the temp path
  (include the PID or a random suffix; `NamedTempFile` already randomizes).

**Minor, non-blocking footgun (document, don't fix):** two sessions both told to
`gdls bench --record <same-path>` (or both with `$GDLS_BENCH_RECORD_TO` pointing at one file) would
clobber each other's trace. That's an opt-in debug feature, not the LSP path; a one-line README note
("`--record` is single-process") suffices.

---

## 4. Out of scope / decisions (resolved)

> **Superseded note (2026-06-12):** everything deferred to "Phase 2" below is now fully specified —
> milestones **M7–M11** in [`09-phase-2.md`](09-phase-2.md) (preemption → M7; completion/signatureHelp
> → M8; rename/documentHighlight/declaration → M9; onTypeFormatting → skipped with rationale;
> `.tscn` typing → M11). This section is retained as the v1-era decision record.

**Confirmed Phase 2 (per `docs/00 §3`; unchanged):** `.tscn` node typing for `$`/`%` precise types,
`completion`, `signatureHelp`.

**Confirmed Phase 2 (raised here):**
- `$/cancelRequest` *preemption* of in-flight requests. The single-threaded loop dispatches a request
  to completion before reading the next message, so a cancel can't interrupt running work. This is a
  *latency* property, not a data-correctness one (no wrong/incomplete result — the request still
  returns a correct response), and Godot's LSP is no better here. Needs concurrent dispatch ⇒ Phase 2.

**Godot capabilities gdls does not expose — decided for v1: all deferred to Phase 2.** Godot's LSP also offers
`rename`/`prepareRename`, `documentHighlight`, `declaration` (definition + jump to native docs), and
`onTypeFormatting`. None are "incomplete data on an exposed capability," so the M6 bar doesn't
require them. `rename` and `documentHighlight` are the two a user would most notice, so they are the
first Phase-2 candidates:
- **`rename`** — substantial (workspace-wide edits, must reuse the M6-E reference graph). **Phase 2.**
- **`documentHighlight`** — in-file usages of the symbol under the cursor; cheap once M6-E's
  reference machinery exists (in-file subset), so a **low-cost early Phase-2 add-on** — but out of v1 scope.
- **`declaration`, onTypeFormatting** — low value for this tool; Phase 2 / never.

---

## 5. Suggested sequencing

1. **M6-A** documentSymbol nesting (smallest, isolated, immediate parity win).
2. **M6-B** definition on class_name-in-expression (small, high-frequency).
3. **M6-C1** definition on preload strings; **M6-D** autoload definition (share the `resolve_res_path`
   plumbing).
4. **M6-E** references cross-file callsites (unlocks **M6-G** implementation overrides and a future
   `documentHighlight`/`rename`, which reuse the same reference/override machinery).
5. **M6-F** hover member/preload signatures.
6. **M6-G** implementation method overrides (optional; reuses M6-E + the existing BFS).
7. **§3 cache + M6-H reconcile-by-stat + M6-I atomic/multi-instance-safe writes** (the largest; do as
   a focused unit — serde derives, per-file `(size,mtime)` table, `.gdls/` exclusion,
   load+verify+reconcile path, atomic write-on-build via `tempfile` persist).

Each step keeps `cargo fmt --check` + `cargo lint -D warnings` + `cargo test --workspace` green and
holds both ratchets (1.0000 / 1.0000). Capability fixes are glue/projection only — no analyzer/parser
fidelity changes.

---

## 6. M6 exit criteria — the v1 ship gate

`v1.0.0` ships when, in addition to the existing gate (ratchets 1.0/1.0, empty `known_failures`,
green CI, observability/memory/governor wired):

- **Exposed-capability parity:** hover (incl. member/call/preload signatures), definition
  (in-file + cross-file class + class_name-in-expression + preload-string + autoload), references
  (incl. cross-file member/signal callsites), documentSymbol (hierarchical) all return
  complete/accurate output ⊇ Godot's own LSP on the same inputs. `implementation` (M6-G) returns
  method overrides or is explicitly scoped as class-subtype-only.
- **Warm-start cache:** a second launch of a large real-world GDScript project is **> 5× faster** than the cold scan
  (stat-only warm start), the cache validates against `NativeDb::content_hash` + per-file
  `(size,mtime)`, degrades safely on corruption (verify + quarantine + cold fallback), and reconcile
  no longer re-parses unchanged files.
- **Multi-instance safe:** two gdls processes on the same project (e.g. Claude Code + an IDE) run
  concurrently with no corruption — cache writes are atomic (temp + rename, last-writer-wins,
  no lock), torn/!mismatched reads fall back to cold-index. (The non-cache LSP path is already
  concurrent-safe — read-only, no shared state.)
- **Re-run Phase H walk clean:** repeat the `scripts/lsp-poke.py` capability walk; every row Pass
  (no "limited"/"null where data exists"); confirm a GREEN recommendation under the raised bar.

---

### Appendix — grounding citations

Godot LSP: capabilities `godot_lsp.h:1739-1888`; hover `gdscript_text_document.cpp:347` +
`godot_lsp.h:1284`; definition `text_document.cpp:376` + `workspace.cpp:650`; preload via
documentLink `gdscript_extend_parser.cpp:186-215`; references `text_document.cpp:256` +
`workspace.cpp:472`; documentSymbol `text_document.cpp:139` + `godot_lsp.h:1257`;
implementation off `godot_lsp.h:1768`; no callHierarchy / workspaceSymbol.
gdls: hover `handlers.rs:268-349`; definition `handlers.rs:180-207,467`; references
`handlers.rs:634-784`; implementation `handlers.rs:811-931`; documentSymbol projection
`gd_syntax/src/parser.rs:4380-4458`; `resolve_res_path` `gd_project/src/index.rs:316`;
`Binding::Call` `gd_analyze/src/binding.rs:90`; autoloads `gd_project/src/project_godot.rs:53-156`;
`Index` shape `gd_project/src/index.rs:41-194`; `NativeDb::content_hash`
`gd_types/src/native_db.rs:158,249`; reconcile `gd_server/src/workspace.rs:485-649`; cache spec
`docs/01:112-114`, `docs/07 §1` Phase-2 row.
