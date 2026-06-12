# 02 — Frontend port (tokenizer · parser · analyzer) and the type system

The core deliverable: a faithful Rust port of Godot 4.6.3-stable's GDScript frontend, so diagnostics match
Godot "exactly." Only the frontend is ported — the compiler/bytecode half is out of scope.

## 1. What is ported, and how big it is

| Godot source (`modules/gdscript/`) | ~Lines | Ported? | Role |
|---|---:|---|---|
| `gdscript_tokenizer.cpp` | ~1,650 | ✅ | Source → tokens (exact positions) |
| `gdscript_parser.cpp` | ~6,500 | ✅ | Tokens → AST (recursive descent, single-token lookahead) |
| `gdscript_analyzer.cpp` | ~6,700 | ✅ | Type checking & semantic analysis ("reduce"/"resolve" passes) |
| `gdscript_warning.{h,cpp}` | ~280 | ✅ | The warning codes (45 active + 3 deprecated-gated) + messages |
| `gdscript_compiler.cpp`, `gdscript_byte_codegen.cpp`, `gdscript_vm.cpp` | — | ❌ | Bytecode/VM — not needed for diagnostics |

**Total ported logic ≈ 15k lines.** **The source of truth is the Godot 4.6.3-stable source**
(`version.py` = 4.6.3 stable), taken from a local Godot checkout. Where these docs cite a concrete
number it may be out of date, so **Godot's file wins**.

**Porting principle:** mirror Godot's structure function-for-function so (a) behavior and error/warning
**message strings and source ranges** match, and (b) Godot changes are easy to diff and re-apply. Resist
"improving" the algorithms — fidelity is the requirement. **Derive every enum, count, and message template
mechanically from the Godot source — never hard-code them from these docs, and grep Godot to confirm
counts at port time.** (Verified examples where these docs were stale: the analyzer warning set is **45
active + 3 deprecated-gated**, not "47"; `Token::Type` has **100** kinds.)

## 2. Tokenizer

- Port the token kinds and the single-token design. Preserve exact byte offsets and line/column for every
  token (diagnostics ranges depend on this).
- Reproduce Godot's lexical error reporting (e.g., bad indentation, unterminated strings) verbatim.
- GDScript is indentation-sensitive; replicate Godot's indent/dedent handling and tab/space rules
  (drives `MIXED_TABS_AND_SPACES`-style diagnostics where applicable).

## 3. Parser

- Recursive-descent, mirroring `GDScriptParser`. Define Rust enums mirroring the `GDScriptParser::Node`
  hierarchy (class, function, variable, expression nodes, etc.).
- **Error recovery** must match Godot closely enough to produce the same syntax-error set, and must always
  yield a partial AST (the server must never fail to respond — see `06-testing-fidelity.md` §5).
- Syntax errors are emitted here via the diagnostics sink (see §6).

## 4. Analyzer (the long pole)

Ported from `gdscript_analyzer.cpp`. Implements the two pass families:

- **"reduce" functions** — type and fold expressions.
- **"resolve" functions** — resolve declarations/statements (classes, functions, variables, signals,
  enums, inner classes).

Responsibilities:

- Assign types to every expression; check assignability, call arity/argument types, return types,
  member access, indexing, operators, casts, and pattern matching.
- Emit **errors** (type/semantic) and **warnings** (the 45 codes) exactly where Godot does.
- Handle annotations' semantic effects (§8).

**Resolution order is part of fidelity.** Several Godot behaviors hinge on the *order* the analyzer
walks its work queues: lambda bodies drain FIFO (`pending_lambda_bodies` — analyzer.cpp:6536-6537,
gdls WP-R1); `resolve_class_body_recursive` walks root body, then inheritance, then inner-class
recursion in that order (WP-R1 again); the body pass runs only after `resolve_inheritance` and
`resolve_interface` succeed. Re-ordering these queues — even with the same emission set — diverges
from the corpus. New resolver code follows Godot's traversal order rather than picking the most
"natural" Rust idiom.

## 5. Type model (`gd_types`)

Represent the full GDScript 2.0 type space:

- **Builtin / Variant types** (int, float, String, Vector2, Array, Dictionary, …).
- **Typed collections**: `Array[T]` (and future typed dictionaries if present in 4.6.3).
- **Native classes**: engine and installed GDExtensions, from the API dump and/or `doc_classes` XML
  (see `03-indexing-freshness.md` §1–§2), with full inheritance.
- **Script classes**: `class_name` globals and script-path-identified classes; **inner classes**.
- **Enums** (named and anonymous), **constants**.
- **Callables** and **signals** (first-class signal type with argument signatures).
- **A distinguished dynamic / `Variant` type** for untyped values.

**Gradual typing** is modeled faithfully: the analyzer tracks when a value is statically known vs `Variant`
and downgrades to runtime-checked access where Godot does, emitting `UNSAFE_*` warnings at exactly those
points. Getting "when is this `Variant`?" right is central and is the most error-prone part — it is the
primary target of the conformance corpus (`06-testing-fidelity.md`).

## 6. Diagnostics sink

- A single sink mirroring Godot's `push_error` / `push_warning`.
- The **45 active warning codes** (+ 3 deprecated-gated behind `#ifndef DISABLE_DEPRECATED`) are ported as
  a Rust enum mirroring `GDScriptWarning::Code`, with the same message templates and default levels
  (33 `WARN`, 8 `IGNORE`, 4 `ERROR`). The whole `GDScriptWarning` class is `#ifdef DEBUG_ENABLED`.
- The **4 warnings that are errors by default** are preserved as such: `INFERENCE_ON_VARIANT`,
  `NATIVE_METHOD_OVERRIDE`, `GET_NODE_DEFAULT_WITHOUT_ONREADY`, `ONREADY_WITH_EXPORT`.
- Each diagnostic carries: severity, code, message, and a source range. Ranges are stored as byte offsets
  internally and converted to LSP UTF-16 positions at the boundary (`05-lsp-cc-integration.md`).
- **Emission order is observed by the conformance `.out` diff.** Godot's runner captures
  diagnostics in real-time during analysis, so the `.out` golden files reflect the traversal
  sequence (interface-pass emissions before body-pass emissions, etc.). gdls's `DiagnosticSink::finish`
  preserves insertion order — never re-sorts. A locked unit test in `crates/gd_analyze/src/diagnostic.rs`
  pins this so a "let's sort by line" refactor fails CI rather than the corpus.
- **Optional line override.** Some Godot emission sites pass a `nullptr` source node to
  `push_error`, in which case `gdscript_parser.cpp:241-244` reads `previous.start_line` instead of
  deriving the line from a span. At end-of-parse the parser's `previous` token can be on a synthetic
  post-EOF line (e.g. `match_with_subscript.gd`'s subscript-`Index` pattern), which no byte span
  can express. gdls models this with `Diagnostic.line: Option<u32>` plus `ParseTree.eof_line: u32`
  (set in `Parser::into_parts`). The `DiagnosticSink::push_error_with_line` constructor stamps the
  override; the conformance harness honors it for `.out`-diff fidelity, while the LSP boundary
  renders the byte span (which is what an editor needs anyway). This is **Godot-fidelity-only
  plumbing** — adding new emission sites should prefer the span-based `push_error` and only use the
  line override when mirroring a Godot null-source path.

## 7. Name resolution order

Match Godot's lookup precedence exactly:

```
local scope (vars, params, for-loop vars, lambda captures)
  → current class members (incl. inherited via the base chain)
  → base chain (extends … up to the native root)
  → autoloads / singletons (from project.godot)
  → global class_name registry
  → native classes: engine, GDExtensions (from the API dump / doc XML)
  → global utility functions & global constants/enums (@GDScript, @GlobalScope)
```

## 8. Annotations

Port the semantic effects (not just syntax) of: `@onready`, `@export` and `@export_*` variants, `@rpc`,
`@tool`, `@static_unload`, `@abstract` (if present in 4.6.3), `@warning_ignore`, `@icon`. These drive specific
diagnostics — e.g., `ONREADY_WITH_EXPORT` and `GET_NODE_DEFAULT_WITHOUT_ONREADY`. `@warning_ignore(code)`
suppresses the named warning for its scope.

## 9. Cyclic dependency handling

Replicate Godot's **shallow vs full** resolution to break cycles: a class can be resolved to its *interface*
(members/signatures) without fully analyzing its bodies, which lets mutually-referencing scripts type-check.
This is the `GDScriptCache` analog and maps directly onto the eager-interface / lazy-body split in
`01-architecture.md` §4.

**Cross-file member-initializer cycles.** A second class of cycle Godot detects lives below the
script-class level: file A's `const X = B.Y`, file B's `const Y = A.X`. The interface tables don't
catch this (both files have interfaces), and the per-file analyzer doesn't see the cycle until it
chases `B.Y`'s initializer and finds it reading `A.X` which is itself currently resolving. gdls
models this with `AnalysisContext.current_resolving_member: Option<String>` plus the
[cross-file query seam](#10-cross-file-query-seam-crossfilequery)'s `member_initializer_xrefs(file,
member) -> Vec<(FileId, String)>` method, then emits the same two diagnostics Godot does
(`Could not resolve external class member` + `Cannot find member …`) when the walk closes a loop.
WP-R2 added this — corpus fixture `errors/cyclic_ref_external.gd` pins it.

## 10. Cross-file query seam — `CrossFileQuery`

The analyzer's only window onto the rest of the project is the
`gd_analyze::CrossFileQuery` trait. The seam exists so `gd_analyze` stays unit-testable in
isolation (it never reaches into `gd_project`'s `Index` directly), and so multiple resolution
strategies can plug in without diverging on the analyzer side.

| Implementation | Where | Resolution depth |
|---|---|---|
| `NoCrossFile` | `gd_analyze::cross_file` | Empty answers everywhere. For pure-frontend tests. |
| `SyntacticQuery` | `gd_analyze::cross_file` | M2's eager-interface tables only (no re-parse). The production server uses this in M3. |
| `CorpusQuery` | `crates/gd_analyze/tests/conformance.rs` | Walks the corpus, parses on demand. Used by the fidelity ratchet. |
| `WorkspaceXFileQuery` | `gd_server` (M4) | Thin wrapper holding `&SyntacticQuery + &Workspace::analysis_cache`. Delegates every method to the inner `SyntacticQuery` except `member_initializer_xrefs`, which reads the analyzer's recorded xrefs from `AnalysisResult.member_xrefs` on the cached analysis. No separate cache structure — invalidation comes free from the existing analysis cache lifecycle (`03-indexing-freshness.md §7.5`). |

Every cross-file method has a **default impl** that returns the empty / no-information answer,
which is correct (single-file analysis can't see cycles or external members anyway). This is what
lets new implementors opt in to richer behavior without rebuilding the trait. The same default
kept WP-R2's cross-file cycle detection inert in `SyntacticQuery` until M4, when the
`gd_server::xfile::WorkspaceXFileQuery` impl began overriding `member_initializer_xrefs` from the
analysis cache to make the diagnostic live in the LSP (see `03-indexing-freshness.md §5` and §7.5
for the inline-on-`AnalysisResult` design).

## 11. `$Node` / `%Unique` typing — v1 policy (deliberate deviation)

Godot's editor types `$Path` / `%Name` by reading the **`.tscn`** the script is attached to. v1 does **not**
parse scenes (Phase 2), so:

- `$NodePath` and `%UniqueName` expressions yield a **permissive deferred-node type**:
  - **Assignable to any explicitly-typed `Node`-derived variable without error.** Example that must NOT
    error in v1: `var enemy: Node3D = $Enemy`.
  - **Dynamic on member/method access** (no "unknown member" errors).
- This is a **conscious, documented deviation** from Godot's exact behavior, chosen so the tool **never emits
  a false positive** on node access before scene typing exists.
- Implementation: `gd_analyze::DataType::deferred_node` (`is_pseudo_type = true`, `native_type = "Node"`,
  `is_read_only = true`). Member-access reduction routes through the pseudo-type guard and yields
  `Variant` rather than emitting unknown-member errors.
- **Phase 2 (M11)** replaces this with precise scene-derived typing (parse the attached `.tscn`, map node
  names → node types → instanced-scene `class_name`s), at which point `$`/`%` diagnostics converge with
  Godot — while unresolvable paths stay permissive, preserving the no-false-positive guarantee. See
  `09-phase-2.md` §6-M11 and `07-milestones-risks.md`.

## 11b. Native-surface provenance gating (deliberate deviation, v1.0.2)

Godot can never run without its own ClassDB; gdls can — when no project-derived
`extension_api.json` resolves, it serves an **embedded stock 4.6.3 surface** (`Generic`
provenance) or, with the fallback disabled, an empty DB (`Absent`). In those states a *negative*
claim — "this type/member does not exist" — has no basis: a custom engine build's class is
indistinguishable from a typo.

- **Rule:** the analyzer's native-rooted negative diagnostics (`Could not find type "X" in the
  current scope.`, the super-call miss templates, `Cannot find member "X" in base "Y".` on
  native-rooted bases, and the `UNSAFE_PROPERTY_ACCESS` warning — promoted to an error under the
  strict profile — on native-rooted attribute misses, v1.0.4 #32) fire only under `Exact`
  provenance (a project-context dump, a pinned `extensionApiPath`, or a project-root
  `extension_api.json`). Under `Generic`/`Absent` the unknown degrades to a silent Variant — the
  docs/00 "unknown stays dynamic" rule. `UNSAFE_PROPERTY_ACCESS` adds two refinements: a Class
  base whose chain root is *unresolvable* (e.g. `extends ForkClass` under the stock fallback)
  never warns under any provenance — the member surface is incomplete; and a Class chain that
  crossed a file boundary and missed everywhere degrades to a silent set Variant (the SCRIPT
  branch's never-lie rule), so shallow-interface gaps can't surface as warnings either.
- Positive resolution is unaffected: under the embedded fallback, builtins resolve, hover works,
  and member checks against classes the stock surface *does* know remain faithful.
- The session upgrades itself: the background auto-dump's adoption mid-session swaps in an
  `Exact` DB, re-analyzes, and republishes — first-run diagnostics converge to exactly what a
  warm session reports. Implementation: `gd_types::ApiProvenance`, gates in
  `gd_analyze::resolver::resolve_datatype` + `gd_analyze::reducer`.

## 12. Sources

- Module pipeline & shallow/full cache — https://github.com/godotengine/godot/blob/master/modules/gdscript/README.md
- Warning codes — https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_warning.h
- Warning system (defaults, `@warning_ignore`, promote-to-error) — https://docs.godotengine.org/en/stable/tutorials/scripting/gdscript/warning_system.html
- `$` typing depends on the scene — https://docs.godotengine.org/en/stable/tutorials/scripting/nodes_and_scene_instances.html
