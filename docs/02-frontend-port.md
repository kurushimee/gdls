# 02. Frontend port (tokenizer, parser, analyzer) and the type system

The core of gdls is a faithful Rust port of Godot's GDScript frontend, so diagnostics match Godot exactly. Only the frontend is ported; the compiler and bytecode half is out of scope. Two feature releases are supported from one binary, 4.6 and 4.7, chosen per project; the unguarded text of every ported function is the newest one, and §11c and §11d list everything that differs.

## 1. What is ported

| Godot source (`modules/gdscript/`) | ~Lines | Role in gdls |
|---|---:|---|
| `gdscript_tokenizer.cpp` | ~1,650 | Source to tokens, with exact positions |
| `gdscript_parser.cpp` | ~6,500 | Tokens to AST (recursive descent, single-token lookahead) |
| `gdscript_analyzer.cpp` | ~6,700 | Type checking and semantic analysis (the "reduce" and "resolve" passes) |
| `gdscript_warning.{h,cpp}` | ~280 | The warning codes (45 active plus 3 deprecated-gated at 4.6; 46 plus 3 at 4.7) and messages |

`gdscript_compiler.cpp`, `gdscript_byte_codegen.cpp`, and `gdscript_vm.cpp` are not ported. Bytecode and the VM are not needed for diagnostics.

Total ported logic is roughly 15k lines. The source of truth is the Godot source from a local checkout carrying both supported tags, never this document. Where a doc cites a concrete number it may have drifted; Godot's file wins.

Mirror Godot's structure function for function, so behavior, error and warning message strings, and source ranges all match, and so upstream changes stay easy to diff and re-apply. Resist improving the algorithms; fidelity is the requirement. Derive every enum, count, and message template mechanically from the Godot source, and grep Godot to confirm counts at port time.

## 2. Tokenizer

- The token kinds and the single-token design mirror Godot's. Every token keeps its exact byte offsets and line and column, since diagnostic ranges depend on it.
- Lexical error reporting is verbatim, including bad indentation and unterminated strings.
- GDScript is indentation-sensitive, so Godot's indent and dedent handling and its tab and space rules are reproduced. These drive `MIXED_TABS_AND_SPACES`-style diagnostics where they apply.
- The lexer is pull-based and parser-driven. Newline and indent suppression inside `()[]{}` and lambdas is toggled by the parser, so it is not a standalone pre-pass.

## 3. Parser

- Recursive descent, mirroring `GDScriptParser`. The Rust enums mirror the `GDScriptParser::Node` hierarchy (class, function, variable, expression nodes, and so on).
- Error recovery matches Godot closely enough to produce the same syntax-error set, and always yields a partial AST. The server must never fail to respond; see `06-testing-fidelity.md` §5.
- Syntax errors are emitted here through the diagnostics sink (§6).

## 4. Analyzer

Ported from `gdscript_analyzer.cpp`. It implements two pass families: the "reduce" functions, which type and fold expressions, and the "resolve" functions, which resolve declarations and statements (classes, functions, variables, signals, enums, inner classes).

Responsibilities:

- Assign types to every expression, and check assignability, call arity and argument types, return types, member access, indexing, operators, casts, and pattern matching.
- Emit errors (type and semantic) and warnings exactly where Godot does.
- Handle the semantic effects of annotations (§8).

**Resolution order is part of fidelity.** Several Godot behaviors hinge on the order the analyzer walks its work queues. Lambda bodies drain FIFO (`pending_lambda_bodies`, `analyzer.cpp:6536-6537`). `resolve_class_body_recursive` walks the root body, then inheritance, then inner-class recursion, in that order. The body pass runs only after `resolve_inheritance` and `resolve_interface` succeed. Re-ordering these queues diverges from the corpus even when the emission set is identical, so resolver code follows Godot's traversal order instead of the most natural Rust idiom.

## 5. Type model (`gd_types`)

The full GDScript 2.0 type space:

- Builtin and Variant types (int, float, String, Vector2, Array, Dictionary, and the rest).
- Typed collections, `Array[T]` and typed dictionaries.
- Native classes, engine and installed GDExtensions, from the API dump and `doc_classes` XML (`03-indexing-freshness.md` §1 and §2), with full inheritance.
- Script classes: `class_name` globals and script-path-identified classes, plus inner classes.
- Enums (named and anonymous) and constants.
- Callables and signals, with signals as a first-class type carrying argument signatures.
- A distinguished dynamic `Variant` type for untyped values.

A native property's type comes from its getter, and never from the property row. Godot's analyzer looks a property up to learn that it exists and then types it from `getter->get_return_info()` (`gdscript_analyzer.cpp:4343-4350`), a `PropertyInfo` that carries the enum class name and the container element types beside the plain `Variant::Type`. The JSON dump's `classes[].properties[].type` is only the flattened half, and the two disagree on 676 of the 4135 properties in the 4.7.2 dump: 502 enums written `int` (`Node.process_mode` is really `Node.ProcessMode`), 34 typed arrays written `Array` or `PackedStringArray`, 28 bitfields written `int` (which Godot also renders `int`), and 111 plain names that simply differ — the getter narrowing (`SceneTree.root` is `Node` in the table and `Window` from `get_root`), widening (`Camera2D.custom_viewport` is `Viewport` in the table and `Node` from its getter), or the `PropertyInfo` hint string leaking into the `type` field (`"Texture2D,-AtlasTexture,…"`, which is not a type name at all and which the getter spells `Texture2D`). `NativeDb` therefore takes the getter unconditionally, resolving it up the `inherits` chain as `ClassDB::get_method` does. The 27 properties whose getter is a `_`-prefixed internal the dump omits keep the table's flattened type: `ClassDB` has those methods at runtime and Godot types them from there, so the flattened name is a wrong rendering for a few of them and the only information the dump carries.

Gradual typing is modeled faithfully: the analyzer tracks when a value is statically known versus `Variant`, and downgrades to runtime-checked access exactly where Godot does, emitting `UNSAFE_*` warnings at those points. Getting "when is this `Variant`?" right is the central and most error-prone part of the port, which is why it is the main target of the conformance corpus (`06-testing-fidelity.md`).

## 6. Diagnostics sink

A single sink mirroring Godot's `push_error` and `push_warning`.

The active warning codes, plus the 3 deprecated-gated ones behind `#ifndef DISABLE_DEPRECATED`, are a Rust enum mirroring `GDScriptWarning::Code`, with the same message templates and default levels: 33 `WARN`, 8 `IGNORE`, 4 `ERROR` at 4.6. 4.7 inserts `CONFUSABLE_TEMPORARY_MODIFICATION` between `CONFUSABLE_CAPTURE_REASSIGNMENT` and `INFERENCE_ON_VARIANT` at level `WARN`, shifting every later ordinal. The enum carries the newest tag's order and `WARNING_SINCE` gates the rest, since `@warning_ignore`, `debug/gdscript/warnings/<name>`, the `.out` goldens, and the LSP diagnostic code are all keyed on the name, never the number. The whole `GDScriptWarning` class is `#ifdef DEBUG_ENABLED`. The 4 that are errors by default are `INFERENCE_ON_VARIANT`, `NATIVE_METHOD_OVERRIDE`, `GET_NODE_DEFAULT_WITHOUT_ONREADY`, and `ONREADY_WITH_EXPORT`.

Each diagnostic carries severity, code, message, and a source range. Ranges are stored as byte offsets internally and converted to LSP positions at the boundary (`05-lsp-cc-integration.md`).

**Emission order is observed by the conformance `.out` diff.** Godot's runner captures diagnostics in real time during analysis, so the `.out` golden files reflect the traversal sequence: interface-pass emissions before body-pass emissions, and so on. `DiagnosticSink::finish` preserves insertion order and never re-sorts. A locked unit test in `crates/gd_analyze/src/diagnostic.rs` pins this, so a "let's sort by line" refactor fails CI instead of the corpus.

**Optional line override.** Some Godot emission sites pass a `nullptr` source node to `push_error`, in which case `gdscript_parser.cpp:241-244` reads `previous.start_line` instead of deriving the line from a span. At end of parse the parser's `previous` token can sit on a synthetic post-EOF line (`match_with_subscript.gd`'s subscript-`Index` pattern, for one), which no byte span can express. gdls models this with `Diagnostic.line: Option<u32>` plus `ParseTree.eof_line: u32` (set in `Parser::into_parts`). The `DiagnosticSink::push_error_with_line` constructor stamps the override, and the conformance harness honors it for `.out`-diff fidelity, while the LSP boundary renders the byte span, which is what an editor needs anyway. This is Godot-fidelity plumbing only: new emission sites prefer the span-based `push_error` and use the line override only when mirroring a Godot null-source path.

## 7. Name resolution order

Godot's lookup precedence, matched exactly:

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

The semantic effects, not just the syntax, of `@onready`, `@export` and its `@export_*` variants, `@rpc`, `@tool`, `@static_unload`, `@abstract`, `@warning_ignore`, and `@icon`. These drive specific diagnostics such as `ONREADY_WITH_EXPORT` and `GET_NODE_DEFAULT_WITHOUT_ONREADY`. `@warning_ignore(code)` suppresses the named warning for its scope.

**Arguments are checked in two places, as upstream checks them.** The parser counts them against the annotation's registration (`Parser::validate_annotation_arguments`, gdscript_parser.cpp:4406, reading the transcribed `REGISTERED_ANNOTATIONS` table), and the analyzer reduces each one and checks the value it folded to against the parameter type the same registration declares (`resolve_annotation`, analyzer.cpp:1673). The parameter index sticks on the last entry, which is what makes every extra argument of a vararg annotation read against its final declared type. An apply callback then reads the resolved values rather than the argument expressions, so `@rpc(MODE)` with `const MODE = "any_peer"` is the keyword it folded to.

Godot decides "not a constant expression" from `make_expression_reduced_value`, which materializes any `Variant`. gdls's fold table is narrower — `absi(-10)` and `Vector3.UP` are both constant to Godot and unrepresentable here — so the blame is gated on the same never-constant walk a `const` initializer uses, and anything the walk cannot place stays silent. An argument gdls could not materialize, or a conversion it cannot perform, ends the resolve without a message and truncates the resolved list, so an apply callback can never read a value that was not checked.

## 9. Cyclic dependency handling

Godot's shallow-versus-full resolution breaks cycles: a class resolves to its interface (members and signatures) without fully analyzing its bodies, which lets mutually-referencing scripts type-check. This is the `GDScriptCache` analog, and it maps directly onto the eager-interface, lazy-body split in `01-architecture.md` §4.

**Cross-file member-initializer cycles.** Godot detects a second class of cycle below the script-class level: file A's `const X = B.Y` against file B's `const Y = A.X`. The interface tables miss it, since both files have interfaces, and the per-file analyzer does not see the cycle until it chases `B.Y`'s initializer and finds it reading `A.X`, which is itself mid-resolution. gdls models this with `AnalysisContext.current_resolving_member: Option<String>` plus the [cross-file query seam](#10-cross-file-query-seam-crossfilequery)'s `member_initializer_xrefs(file, member) -> Vec<(FileId, String)>` method, then emits the same two diagnostics Godot does (`Could not resolve external class member` and `Cannot find member …`) when the walk closes a loop. Corpus fixture `errors/cyclic_ref_external.gd` pins it.

The `InitShape` resolver has its own guard, `AnalysisContext.init_shape_stack`, seeded from `current_resolving_member` so a mutual pair closes even when the reader is one of the two members. It stays silent on a cycle rather than emitting either message: Godot reaches those cycles by really recursing into the other file's analysis, and a structural walk over recorded shapes has not been shown to see the same set. A cycle gdls sees and Godot does not would be a false error, and that is the one direction the port does not take.

## 10. Cross-file query seam: `CrossFileQuery`

The analyzer's only window onto the rest of the project is the `gd_analyze::CrossFileQuery` trait. The seam exists so `gd_analyze` stays unit-testable in isolation (it never reaches into `gd_project`'s `Index` directly), and so multiple resolution strategies can plug in without diverging on the analyzer side.

| Implementation | Where | Resolution depth |
|---|---|---|
| `NoCrossFile` | `gd_analyze::cross_file` | Empty answers everywhere. For pure-frontend tests. |
| `SyntacticQuery` | `gd_analyze::cross_file` | The eager-interface tables only, no re-parse. |
| `CorpusQuery` | `crates/gd_analyze/tests/conformance.rs` | Walks the corpus, parses on demand. Used by the fidelity ratchet. |
| `WorkspaceXFileQuery` | `gd_server::xfile` | The production impl. A thin wrapper holding `&SyntacticQuery` plus `&Workspace::analysis_cache`. Delegates every method to the inner `SyntacticQuery` except `member_initializer_xrefs`, which reads the analyzer's recorded xrefs from `AnalysisResult.member_xrefs` on the cached analysis, and `scene_node_facts`, which reads the project `SceneIndex`. No separate cache structure, so invalidation comes free from the existing analysis cache lifecycle (`03-indexing-freshness.md` §7.5). |

Every cross-file method has a default impl returning the empty answer, which is correct: single-file analysis cannot see cycles or external members anyway. That default is what lets a new implementor opt in to richer behavior without rebuilding the trait. See `03-indexing-freshness.md` §5 and §7.5 for the inline-on-`AnalysisResult` design.

A member's type crosses the seam as a `TypeExpr` — the declaration as written, not a resolved `DataType` — and `reducer::resolve_interface_type_expr` turns it into one against the reading file's own context. A member with no annotation the shallow pass could type crosses as an `InitShape` instead: still syntax, still resolved on the reading side, by `reducer::resolve_member_init_shape` (`01-architecture.md` §4). Nothing resolved ever crosses, which is what keeps the reader from having to re-stamp hardness, scrub constancy, or reconcile the two spellings an enum's declaring class has. That function is the cross-file twin of `resolver::resolve_datatype` (analyzer.cpp:654-900) and answers the same heads in the same order: builtins, native classes, project `class_name`s, the declaring file's own enums and inner classes, a constant of the declaring class that names a script, `@GlobalScope` enums, and autoload singletons. That constant head is what makes `const Other = preload("other.gd")` followed by `func f() -> Other` readable from another file: Godot resolves it through the class's own constants (analyzer.cpp:847-900), and the interface path reaches the same answer through the constant's recorded `InitShape`. Trailing segments walk exactly as a global class's do, so `A.APrime` finds an inner class of the preloaded script and `A.Mode` finds one of its enums, and the metatype lowers to the instance like every other type position. A `Call` link answers with its callee's declared return type, taken from the same three places a call site takes it: the declaring script's own interface, `NativeDb`'s method table, and the builtin method table. Two of those need a guard the value path does not. A bare native class name is a legal call base only for a static method or a singleton, so an instance method named off one is refused rather than typed off an instance that is not there (analyzer.cpp:6036-6039). And a return of `Variant`, or of nothing at all, degrades to `Variant` instead of being recorded, since the only thing a reader could do with either is claim a type the call never had. It never diagnoses and never returns `Unresolved`; a head it cannot place degrades to `Variant`, because an interface gap must never read as a type error. What it cannot recover on its own is hardness, since a `TypeExpr` does not say whether it was written as an annotation, as `:=`, or as a plain `=`, so the interface carries that separately (`MemberFlags::ty_is_soft`, `01-architecture.md` §4) and the `var` arm applies it. A `func` parameter carries the same distinction one step finer, per parameter (`ParamTyping`), because hardness there decides whether a call draws `Invalid argument` or only `UNSAFE_CALL_ARGUMENT` — and because a parameter whose default the shallow pass could not decode has to read as "no type", not as `Variant`. `validate_call_arg`'s first arm goes quiet for exactly one parameter shape, a *hard* `Variant` (analyzer.cpp:6097), which is what every dump-declared `Variant` parameter is (`type_from_property(.., p_is_arg=true)`), so getting that hardness wrong turns every `Array.append` in a project into a warning.

## 11. `$Node` and `%Unique` typing: bare `Node`, with precise scene types for navigation only

**Godot's frontend analyzer does not read the scene to type `$` and `%`.** `GDScriptAnalyzer::reduce_get_node` (`gdscript_analyzer.cpp:3882-3886`, verified at `4.6.3-stable`) types every `$Path` and `%Name` as a hard `NATIVE` `Node` (`ANNOTATED_EXPLICIT`, `native_type = "Node"`, builtin `OBJECT`). The precise per-node type the *editor* surfaces in completion and hover comes from a separate mechanism, not from the type the analyzer assigns. Because the analyzer's `$` type is a bare `Node`, assigning or passing it to any Node-derived subtype or sibling is an unsafe downcast, which Godot tolerates (the `UNSAFE_*` warnings are `Ignore` by default). So `var c: Control = $Health` and `wants($Health)` (with `func wants(c: Control)`) both pass with no error even when `$Health` is a `Node2D`.

gdls types a valid `$` or `%` as bare `NATIVE Node` in `gd_analyze::reduce_get_node`, matching Godot function for function. The resulting behaviors, each confirmed against the real 4.6.3 binary:

- A member miss raises `UNSAFE_PROPERTY_ACCESS` (`$x.bogus`, read or write), the same as any typed `Node` base. `UNSAFE_METHOD_ACCESS` on a method miss is the matching expectation; gdls under-emits it on *every* native-base method miss, a general analyzer gap that is not specific to `$`.
- A valid `Node` method (`$x.get_parent()`) is silent.
- A sibling or subtype downcast (`var c: Control = $x`, `$x as Control`) is silent, the unsafe downcast Godot tolerates.
- `wants($x)` where `func wants(p: Control)` raises `UNSAFE_CALL_ARGUMENT`, since a bare `Node` supertype is passed where a `Control` subtype is required.

**Why precise scene types stay out of the diagnostic path.** Resolving `$Path` to the node's precise `.tscn` type (`Node2D`, say, or the Script instance of a node's attached script) and feeding it to the analyzer would turn those Godot-tolerated sibling downcasts into false-positive errors: `var c: Control = $Health` would report "Cannot assign a value of type Node2D…" and `$Health as Control` would report "Invalid cast…", neither of which Godot emits. A `DataType` is used symmetrically in compatibility checks, so there is no "precise for navigation, bare `Node` for assignment" without decoupling the navigation type from the compatibility type. Precise scene-derived node types are therefore navigation-only, deliberately kept out of the diagnostic path.

**The navigation half.** `hover`, `definition`, `typeDefinition`, `completion`, and `signatureHelp` answer a `$Path`, `%Name`, or `get_node("literal")` access with the scene-precise node type: the engine class of the node the access reaches, or the `class_name` of the script attached to it. It is built in `gd_server::scene_nav` and handed straight to the renderers. The projection carries past the dot as well (#349): `scene_type_ending_at` gives a member access whose BASE is such an expression the precise type to resolve the member on, matched at the base's end so a `$X` nested inside a larger base cannot hijack it. That decoupling is the point: the precise type is a display and jump vehicle that never enters an `AnalysisResult`, so the compatibility checks keep seeing bare `Node` and the tolerated downcasts stay silent. Anything the scene index cannot resolve unanimously (an absolute `$/root/…` path, a script no scene attaches, an absent node, two attaching scenes disagreeing) falls back to the bare-`Node` answer rather than guessing.

**The substrate both halves read**, which `reduce_get_node` and the diagnostic path never consult:

- `gd_analyze::CrossFileQuery::scene_node_facts`, a project-fact seam returning a `SceneNodeFacts` (a native class name or an attached-script `FileId`, never a `DataType`). The default impl returns `None`; only `gd_server`'s `WorkspaceXFileQuery` overrides it, over the project `SceneIndex`, resolving conservatively: any uncertainty or cross-scene disagreement gives `None`, script wins when a node carries both `type=` and `script=`, and instanced sub-scenes are walked through the index's own parsed scenes. A wrong navigation result is a defect, so resolution fails closed.
- `gd_project::SceneIndex::resolve_relative_from` and `resolve_unique_in`, the index-backed node-path resolution (relative path from the attachment node, owner-scoped unique name, instanced-sub-scene recursion). `join_node_path` returns `None` on a `..` that escapes above the scene root, so a root child never gets a spurious match.

A `.tscn` edit keeps the scene index live (`reindex_scene`, `remove_scene`) but does not re-diagnose the scene's attached scripts. A `$` or `%` type is scene-independent (bare `Node` from the enclosing class alone), so re-publishing would be byte-identical churn, and precise navigation types are pull-based.

The static-function and non-Node-class `$`/`%` context errors (`reduce_get_node`'s two `push_error`s) fire exactly as Godot's do. On either, the result is the default `VARIANT`/`UNDETECTED`, so a `:=` infer off the failed `$` reports its companion infer error, matching `gdscript_analyzer.cpp:3870` and `:3876`.

## 11b. Native-surface provenance gating (a deliberate deviation)

Godot can never run without its own ClassDB. gdls can: when no project-derived `extension_api.json` resolves, it serves an embedded stock surface for the project's own release (`Generic` provenance) or, with the fallback disabled, an empty DB (`Absent`). In those states a *negative* claim, "this type or member does not exist", has no basis, because a custom engine build's class is indistinguishable from a typo.

**The rule.** The analyzer's native-rooted negative diagnostics fire only under `Exact` provenance, meaning a project-context dump, a pinned `extensionApiPath`, or a project-root `extension_api.json`. Those diagnostics are `Could not find type "X" in the current scope.`, the super-call miss templates, `Cannot find member "X" in base "Y".` on native-rooted bases, `Function "X()" not found in base self.` on a self-call whose whole chain was walked, and the `UNSAFE_PROPERTY_ACCESS` warning (promoted to an error under the strict profile) on native-rooted attribute misses. Under `Generic` or `Absent` the unknown degrades to a silent Variant, the "unknown stays dynamic" rule from `00-overview.md` §4.

`UNSAFE_PROPERTY_ACCESS` adds one refinement, on the same footing as the self-call rule below: the negative is sound only when the base's chain was walked end to end. A Class base whose chain root is unresolvable (`extends ForkClass` under the stock fallback, say) never warns under any provenance, because the member surface is incomplete. Crossing a file boundary is not itself a hole, though: a chain whose every cross-file link has a present, parse-clean interface and whose native root the dump knows proves a miss exactly as a same-file one does. gdls once degraded every such chain to a silent set Variant, which suppressed the warning for every project class that does not extend a native class directly — most of them, in a real codebase (#448).

**Provenance is necessary, not sufficient: the chain has to have been walked.** A self-call miss is a claim about the *whole* ancestry, so the chain it was measured against must be introspectable end to end — every link resolved to an indexed file, every one of those files parsed without errors, and the last link landing on a native class the dump knows (`gd_analyze::script_chain::ResolvedChain::introspectable`). Any hole and the miss proves nothing, so it stays silent: an `extends` that resolves to nothing, a link the workspace never indexed, an `extends` cycle, or a base whose parse failed and whose error recovery may simply have dropped the method being called. That last case is why an interface records `parse_clean` (`gd_project::Interface`), stamped from the tree's own `had_parse_errors`; it also gates `Member "X" is not a function.`, which is a claim about a member's *shape* and is equally unsafe to make from a partial extract. One more suppression sits on top: if the chain's native root carries the called name as a property, constant, signal, or enum value, Godot answers with the value-callable pair rather than not-found, so gdls must not claim not-found there. The pair itself comes from the miss branch's own callee probe, which resolves the name against the base and reports `Name "X" called as a function but is a "T".`; the suppression stays as the fail-closed backstop for anything the probe cannot reach. A `super.` call is exempt from both, since Godot's parent lookup runs on its own and does emit not-found.

**A bare callee is typeless until the miss branch looks at it.** Godot never reduces a bare identifier callee (`gdscript_analyzer.cpp:3556-3559` builds the base and goes straight to `get_function_signature`), so the miss branch's `reduce_identifier_from_base` is the first thing to type it, and that function opens with a guard that only passes on a typeless node (`analyzer.cpp:4025-4027`). gdls does pre-reduce such a callee, for reasons of its own — the access-check split behind `reducing_callee`, and #333's super-callee binding — and `reduce_expression`'s tail-guard then stamped `Variant` over exactly the state the probe needs. So the probe was a no-op for a bare call, and a name the chain carries as a value (`name()`, `hp()`) drew neither half of Godot's pair nor not-found. One node is therefore exempt from the tail-guard, named on the context as `reducing_callee_node`, and `reduce_call`'s dispatcher arm re-stamps it afterwards if nothing determined it, so every node still carries a type by the time nav reads it. The exemption is a `NodeId` and not the `reducing_callee` flag, because that flag stays raised across the whole callee subtree and this must reach one node.

**The one negative gated at `Absent`, not `Exact`.** `Identifier "X" not declared in the current scope.` is gated the other way: it fires under `Generic` too, and is silenced only by `Absent`. The line the rule above draws is how much of the answer the dump owns. `Cannot find member "X" in base "Y".` asks the dump about one class and believes its silence, so a stock surface missing a GDExtension's class answers wrongly. A bare identifier is resolved against locals, parameters, class members, the `class_name` registry, builtins, global enums, utilities, and autoloads *first* — the dump is one contributor among many, and since v3.0.0 the stock surface is the complete official API for the project's own declared release, so its silence about an engine name means something. `Absent` is the state where nothing is knowable: every native lookup misses, and an ungated check would report `position` on a `Node2D` as undeclared. That is the one provenance this negative must stay quiet under.

This diagnostic used to be suppressed for every identifier starting with an uppercase letter, on the theory that such a name was probably an engine symbol the DB lacked. That covered most of the names a Godot script reaches for, so no misspelled `Vector4l`, `Nod`, or `TYPE_OBJEKT` was ever reported. What the hedge was really compensating for turned out to be two test-harness gaps rather than a missing port, both since fixed (#300, #312, #313).

Positive resolution is unaffected: under the embedded fallback, builtins resolve, hover works, and member checks against classes the stock surface does know stay faithful.

**The embedded asset carries documentation.** It is built with `--dump-extension-api-with-docs` and regenerated by `scripts/regen-stock-api.py`, which keeps exactly the fields `gd_types::api` deserializes and drops the GDExtension ABI sections gdls never reads (`builtin_class_sizes`, `builtin_class_member_offsets`, `native_structures`, and per-method `hash`/`hash_compatibility`). That trade costs about 690 KB of gzipped asset (396 KB to 1.09 MB, an 8% release-binary increase) and buys the whole engine's prose on the first-run path: a fresh install with no Godot on `PATH`, which is the headless case this project exists for. A docs-free asset would give that user correct signatures and empty hovers with nothing saying why. `embedded_stock_db_loads` pins the prose in CI, so a regeneration with the wrong flag fails rather than silently emptying every hover.

**The fallback announces itself.** A session that resolves to it sends one `window/showMessage(Info)` at startup naming the stock surface and how to replace it (`godotBinaryPath`, `GDLS_GODOT`, `extensionApiPath`). A stderr warn line alone is invisible to most clients, and "never lie" covers a degraded surface the user cannot see: the prose is there, but the user's own engine build and every GDExtension class are not.

The session upgrades itself. When the background auto-dump is adopted mid-session it swaps in an `Exact` DB, re-analyzes, and republishes, so first-run diagnostics converge to exactly what a warm session reports. Implementation: `gd_types::ApiProvenance`, with gates in `gd_analyze::resolver::resolve_datatype` and `gd_analyze::reducer`.

**The miss branch is one branch, and each arm proves its own claim.** Godot's `reduce_call` decides between `Function "%s()" not found in base %s.`, `Static function "%s()" not found in base "%s".`, and `UNSAFE_METHOD_ACCESS` inside a single `!found` branch (`gdscript_analyzer.cpp:3722-3774`), and its gates are kind-agnostic — any non-`self` base that is not a *hard* `BUILTIN` warns, and any non-super hard metatype gets the static error. gdls reaches the branch only where the miss is provable, and what "provable" costs depends on what the warning claims. On a `Class`, `Script`, or non-meta `Native` base it asserts that a walked surface lacks the name, so the ancestry must be introspectable end to end (`base_is_introspectable` — every in-file link resolved, every cross-file link's interface present and `parse_clean`, no cycle, a native root the DB knows) and the name must not shadow a metatype value. On a `Variant` base there is nothing to walk — upstream has no surface there either — so the miss holds by construction and the warning fires with no chain firewall: a hard `v: Variant`, an untyped local or parameter, the meta Variant a pseudo-type error leaves, and the dummy an undeclared identifier leaves all print `"Variant"`, exactly as upstream does. Hardness is not a gate of its own, since upstream's only hardness test excludes hard builtins; a soft builtin miss therefore warns under the builtin's own name, provided the dump carries that builtin's method table, and `Nil` stays out because it would render `"null"`, a string this warning never prints. `Exact` provenance gates every kind: for the walked ones because the claim bottoms out in the dump, and for `Variant` and enum values because under `Generic` or `Absent` gdls manufactures Variants upstream never has, each of which would launder into a phantom `"Variant"` row one call later. A `Native` metatype is left silent because Godot renders it as `GDScriptNativeClass`, a string gdls does not produce.

**A metatype resolves more than the script chain holds.** Two surfaces upstream consults through a class's own name have no metatype gate at all: the native tail of `reduce_identifier_from_base` (`analyzer.cpp:4333-4386`), where a native property, signal, constant, or enum resolves, and `get_function_signature`'s `ClassDB::get_method_info("GDScript", name)` fallback (`analyzer.cpp:6013`), where `reload`, `duplicate`, and `connect` resolve and then draw `Cannot call non-static function "%s()" on the class "%s" directly.`. `metatype_value_shadow` covers both before either miss arm may speak. Suppressing the second is a deliberate under-report — the code is erroneous either way in Godot, and the alternative is emitting a diagnostic upstream never prints.

## 11c. The 4.6 → 4.7 tokenizer and parser delta

Every frontend difference between the two supported tags is either a guard or a deliberate no-op, and the two halves together are the audit surface for the next version bump. The guards are greppable: `grep -rn "DIALECT("`. The no-ops are only here, so this table is what stops a future auditor from "fixing" one.

| Upstream change | Where it lands in gdls |
|---|---|
| A tab advances `column` by 1 instead of `tab_size` | Guarded, `lexer.rs` `check_indent` and `skip_whitespace`. `indent_count` is unaffected, so indent depth does not move. |
| Token position fields default to `1` instead of `0`/`-1` | Guarded, `parser.rs` `empty_token`. |
| `Token::cursor_position` removed; the multi-line-interior branch sets `cursor_place = CURSOR_MIDDLE` | **No-op.** gdls never ported Godot's cursor machinery. Completion contexts are classified from the token frame plus the cursor byte in `gd_server::completion_context`, not from `make_completion_context` hooks inside the parser, so there is no `cursor_place` to fix and no dead field to delete. |
| `ParserError` carries a full range; three tokenizer-error sites point at the bad token rather than `previous` | **No-op, plus one accepted divergence.** gdls has always carried a `ByteSpan` range on every diagnostic. It has also always pointed those three sites at the bad token, so it matched 4.7 before 4.7 existed. Restoring 4.6's off-by-one token would be replicating a fixed engine bug, and it would cascade into worse follow-on ranges for 4.6 users. It is invisible to the corpus, since `GDTEST_PARSER_ERROR` prints only the first message and no position. |
| `Node` extents default to `-1` as a recovery-node sentinel | **No-op.** gdls stores extents as `u32`, which cannot hold the sentinel, and it is unobservable anyway: positions are clamped at the LSP boundary and the `.out` format never prints an extent. |
| New error `"class_name" isn't allowed in built-in scripts.` | Guarded, `parser.rs` `parse_class_name`, keyed on `ParseOptions::script_path`. |
| The `super` branch enters multiline mode only when `(` is really there | Guarded, `parser.rs` `parse_call`. This one changes which follow-on errors cascade after a malformed `super`, so the 4.6 arm restores the old unconditional `push_multiline`. |
| `COMPLETION_DECLARATION` promoted from a TODO, with six emission sites | **No-op across the two tags, but gdls changed anyway.** 4.7 handles the new context with a bare `break` — no completions in a declaration's name position — and 4.6 reaches the same empty result incidentally, because `parse_identifier` deliberately never opens a context. So there is nothing to gate. gdls was offering identifiers at `var spe`, `class_name Fo`, and their four siblings; it now offers nothing at both tags. `func <name>` stays an override-method completion. |
| `GDScriptWarning` gained start and end columns, moving Godot's own LSP off whole-line ranges | **No-op.** gdls already emits node ranges from `ByteSpan`. Porting the fields would add dead state to the Godot-column space, which exists only for `.out` printing, and `.out` prints no columns. |
| Doc comments use `lstrip`/`rstrip(" \t")` instead of `strip_edges`, and `[br][br]` becomes a paragraph break (which also suppresses the space join on the next line) | Guarded, `doc_comments.rs`. User-visible in hover, and no diagnostics gate would catch a regression, so it carries its own dialect tests. The ported string keeps Godot's lone `\n`; widening it to a Markdown blank line is `gd_server::docs`' job, not the port's. |

## 11d. The 4.6 → 4.7 analyzer delta

Same rule as §11c: every difference is a `DIALECT(4.7)` guard or a documented no-op. The analyzer diff is the larger half by line count and the smaller half by consequence, because most of what moved lives in machinery gdls never ported.

| Upstream change | Where it lands in gdls |
|---|---|
| An untyped override inherits the parent's return type; an untyped `_get_property_list` gets bare `Array` (GH-118877) | Guarded, `resolver.rs` `adopt_parent_return_type`. gdls already did the *script*-parent half at both tags, which was a 4.6 fidelity bug; the native half, walking on into ClassDB, is new. |
| `resolve_return` gains an `expected_type.is_hard_type()` gate | Guarded, `resolver.rs` `check_return_compatibility`. Currently unreachable: nothing in gdls produces a function return type that is both soft and non-Variant. Kept so the guard is already right if one ever does. |
| `reduce_type_test`'s constant arm switches to `is_type_compatible_strict_collections` | Guarded, `reducer.rs`. Also currently unreachable: gdls's fold model has no `Array` value, so `const A = []` is never a constant operand. gdls had the strict version at both tags, which was another 4.6 fidelity bug. |
| New warning `CONFUSABLE_TEMPORARY_MODIFICATION` | Guarded, `reducer.rs` `warn_confusable_temporary_modification`, with the 4.7 corpus fixture and its own dialect tests. |
| `resolve_class_inheritance` accumulates inner-class errors | Guarded, `resolver.rs`. 4.6 returned on the first failure, hiding the siblings' own errors. |
| `type_from_script` replaces `make_script_meta_type` on the three typed-container element paths | **No-op.** Those are the `ARRAY`/`DICTIONARY`/`OBJECT` arms of `type_from_variant`, which gdls stops before: its `FoldedValue` has no array, dictionary, or object value to carry a script reference. |
| Four new fallback reducers (binary op, ternary, cast, type test), and annotation arguments routed through `make_expression_reduced_value` | **No-op.** gdls does not port `make_expression_reduced_value` or any of the `make_*_reduced_value` family — that is Godot's second, separate constant evaluator for annotation arguments and container defaults. `resolve_annotation` is ported and reads the ordinary fold table instead, so the four new fallbacks change nothing it can already see. What that costs is written down in §8. |
| Constant folding skips shared operands; the hardcoded 13-case switch becomes `Variant::is_type_shared`; the `< PACKED_BYTE_ARRAY` clause is dropped | **No-op, and a no-op upstream too.** `is_type_shared` is true for exactly the 13 types the switch listed, and it is true for every type at or past `PACKED_BYTE_ARRAY`, so the dropped clause was already redundant. gdls folds no shared value either, through `constructor_fold_blocked` in `reducer.rs`, which is unguarded for the same reason: the two tags agree on the set. |
| `Variant::is_type_shared` gains the ten `Packed*Array` types | Guarded, `reducer.rs` `variant_is_type_shared`, which the nested read-only subscript walk reads. Currently unreachable: `is_read_only` is only ever stamped from a setter-less native property, and the dump has no such property of an affected type that can be subscripted. The table is pinned at both tags by a unit test rather than a `.gd` fixture. |
| `reduce_identifier` skips the globals lookup when `ClassDB::class_exists(name)` | **No-op.** The `TODO` above it wants `globals` to hold only publicly available things; gdls's global-constant arm already holds exactly `PI`, `TAU`, `INF`, `NAN`, none of which is a class. |
| `type_from_property` deduplicated into `type_from_property_hint_string`, which accepts `"Variant"` and yields a `VARIANT` element type on failure | **No-op.** gdls reads element types from the dump's structured `TypeRef` (`typedarray::T`, `typeddictionary::K;V`), never from a `PropertyInfo` hint string, so there is no parse to share and no failure path to change. |
| `Failed to construct "%s".` removed | **No-op.** It lived in `make_call_reduced_value`, which gdls does not port. |
| `type_exists` moved behind `DISABLE_DEPRECATED` | **No-op** for a default build, which is what gdls mirrors. |
| `resolve_for` / `resolve_while` call `reduce_expression` instead of `resolve_node(…, false)` | **No-op.** A `for` list and a `while` condition are always expressions, and `resolve_node`'s expression arm is `reduce_expression(node, p_is_root)` with the same argument. |
| `resolve_pending_lambda_bodies` moves the pending list instead of copying it | **No-op.** A C++ allocation detail with no Rust analogue. |

Two of the guards above are inert today, and both are marked so rather than dropped: an inert guard costs one comparison and is the difference between "handled" and "missed" the next time someone audits the port against a new tag.

## 12. Sources

- [Module pipeline and the shallow/full cache](https://github.com/godotengine/godot/blob/master/modules/gdscript/README.md)
- [Warning codes](https://github.com/godotengine/godot/blob/master/modules/gdscript/gdscript_warning.h)
- [Warning system: defaults, `@warning_ignore`, promote-to-error](https://docs.godotengine.org/en/stable/tutorials/scripting/gdscript/warning_system.html)
- [`$` typing depends on the scene](https://docs.godotengine.org/en/stable/tutorials/scripting/nodes_and_scene_instances.html)
