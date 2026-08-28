# Changelog

All notable changes to `gdls` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

M7 — Phase 2 protocol foundations (#57–#63), merged 2026-06-13. No release cut: Phase 2
milestones land on `main` and release together once the phase matures.

### Added
- **True `$/cancelRequest` preemption** (#57): a router thread drains the wire and flips
  cancellation tokens the moment a cancel arrives, so the analyzer's cooperative checkpoints
  abort mid-handler; results invalidated by an intervening edit return `ContentModified`
  (-32801). The shutdown handshake no longer risks lsp-server's 30 s `handle_shutdown` stall.
- **`workDoneProgress`** (#58): one progress token spans the cold start (exact percentages on
  the cold-index walk); mid-session re-index/reconcile arcs; client `workDoneToken` honored on
  `references` and `workspace/symbol`. Nothing is ever sent without `window.workDoneProgress`.
- **Runtime re-configuration** (#59): `workspace/didChangeConfiguration` re-reads the
  `initializationOptions` schema (pulling via `workspace/configuration` when advertised);
  malformed payloads keep the previous config (+ `window/showMessage` warning); sparse payloads
  keep absent groups; strict/analyzer changes republish open buffers live.
- **Dynamic `didChangeWatchedFiles`** (#60): the one dynamic registration gdls performs;
  client events merge into the native watcher's mutation funnel with content-fingerprint
  dedupe — Helix's only watch path now keeps the index fresh on its own.
- **Pull diagnostics** (#61): `textDocument/diagnostic` shares push's computation (items
  byte-identical), with `unchanged` short-circuits via a `version:hash:epoch:generation`
  resultId; `workspace/diagnostic` is a documented skip.
- **Documentation pipeline** (#62): the lexer records `##` doc comments in a side-channel
  (token stream untouched; both ratchets hold at 1.0); Godot's association rules ported
  post-parse; docs ride the `Interface` outside the signature hash; one BBCode→GFM converter
  serves all outgoing prose (hover now; completion/signatureHelp in M8); `hover.contentFormat`
  honored with a plaintext downgrade.
- **`codeDescription` gating + per-code anchors** (#63): emission now requires
  `publishDiagnostics.codeDescriptionSupport`; each warning links its own
  `debug/gdscript/warnings/*` ProjectSettings anchor (deprecated trio → overview page).
- M7 §7.4 editor-profile harness: vendored real client-capability JSONs replayed per-profile
  (`tests/editor_profiles.rs`, self-extending); `scripts/lsp-poke.py` gained a `capabilities`
  session key. Helix 25.07.1 captured; remaining editors land with the milestone exit walk.

### Changed
- The warm-start cache format bumped (v4 → v5) for the doc-carrying `Interface` shape — one
  cold re-index per project on first launch after upgrading, self-healing.

M8 — Phase 2 editing core (#64–#65), merged 2026-06-13. No release cut.

### Added
- **`textDocument/completion` + `completionItem/resolve`** (#64): `CompletionList` (never a bare
  array); member-access / identifier / type-position contexts driven token-primary; `textEdit`,
  `sortText` (fixed-width rank), `filterText`, snippet + `InsertReplaceEdit` + `commitCharacters`
  all capability-gated; lazy `documentation`/`detail` via resolve with a compact self-sufficient
  `data` key. 99.6% member-access agreement vs Godot's headless LSP differential.
- **`textDocument/signatureHelp`** (#65): triggers `(`/`,`, retrigger `)`; `activeParameter` per
  spec; label offsets gated on `labelOffsetSupport`; native + project signatures with default
  values; string-safe backward bracket/comma scan.

M9 — Phase 2 navigation & refactoring completeness (#66–#71), merged 2026-06-14. No release cut.

### Added
- **`textDocument/documentHighlight`** (#67): in-file Read/Write highlights reusing the references
  engine's binding-backed resolution; Write derived from assignment + compound-assignment LHS,
  excluding attribute-position identifiers; identifier-token ranges.
- **`textDocument/foldingRange` + `selectionRange`** (#70): AST suites + `#region`/`#endregion`
  pairs + comment runs with `FoldingRangeKind`, honoring `rangeLimit` + `lineFoldingOnly`; the AST
  ancestor chain for selectionRange. `ParseResult` gains an additive `comments` field (token stream
  untouched — both fidelity ratchets hold at 1.0000).
- **`textDocument/declaration` + `typeDefinition`** (#68): declaration = the definition target
  (GDScript has no separate declare/define); typeDefinition resolves the symbol's type to its
  declaring script `class_name` site or native stub header (Builtin/Variant/unresolved → `null`,
  never a guess).
- **`typeHierarchy` prepare/supertypes/subtypes** (#69): the `class_name` registry + extends graph
  as a class-tree navigator; supertypes cross the project→native boundary (stub-anchored), subtypes
  reuse the `implementation` walk; compact `data` blobs so expansion survives past depth 2.
  `typeHierarchyProvider` injected as the standard wire key (lsp-types 0.97 lacks the server field).
- **`workspaceSymbol/resolve`** (#71): lazy `WorkspaceSymbol[]` (location-sans-range + a compact
  `data` key) when the client advertises `workspace.symbol.resolveSupport`, resolving the precise
  range on demand; eager `SymbolInformation[]` otherwise (byte-identical to before).
- **`textDocument/rename` + `prepareRename`** (#66): workspace-wide semantic rename reusing the
  references engine for the edit set; versioned `documentChanges` (zero stale-version edits) gated
  on `workspace.workspaceEdit.documentChanges`, legacy `changes` map otherwise; `prepareRename`
  `{range, placeholder}` gated on `rename.prepareSupport`. A fail-closed corruption firewall
  (positive-project-resolution: native engine symbols, native methods on non-Native bases, stub
  files, and new-name engine/registry collisions all refuse with a typed error and zero edits —
  refuse-rather-than-corrupt); the local edit set is binding-correct (excludes `self.x`-attribute
  over-capture). Round-trips on the Pixelorama acceptance project with zero stale-version edits.
- M9 §7.4 editor-profile walk extended over the rename/foldingRange/workspaceSymbol gated
  projections (`tests/editor_profiles.rs`).

M10 — Phase 2 presentation & code actions (#72–#75), merged 2026-06-14. No release cut.

### Added
- **`textDocument/semanticTokens` full/delta/range (+ refresh)** (#72): syntax-aware highlighting
  over the **STANDARD LSP legend only — zero `gdscript/`-prefixed or custom token names** (the #30
  generic-LSP highlighting target; every theme already maps the 10 standard types + 6 standard
  modifiers). The advertised legend is always full-width for stable wire indices; the client's
  advertised legend is a pure ALLOW-FILTER applied at emit time (LSP 3.17: the wire integers index
  the server-advertised legend) — gdls always emits its own server-legend indices/modifier bits and
  drops any type/modifier the client didn't declare, never by shrinking the advertised legend.
  `full/delta` emits a minimal flat-array edit vs the cached prior array (10k-line files); `range` is
  parse-priced and served even at Hard memory pressure (full/delta are analysis-priced and shed). A
  `workspace/semanticTokens/refresh` is sent only with `workspace.semanticTokens.refreshSupport`.
- **`textDocument/inlayHint` (+ resolve, refresh)** (#73): inferred `var x := …` / `for`-loop types
  (`: Type`) and call-site parameter-name hints, each config-toggleable
  (`inlayHint.typeHints`/`parameterHints`). Type hints carry an accept-the-hint `textEdit` that
  neutralizes `:=` into `: Type =` without eating a parenthesized initializer's `(`; the edit is
  attached only for a source-valid annotation (an unnamed-script basename shows the label but carries
  no corrupting edit). The tooltip is DEFERRED to `inlayHint/resolve` (carried in `data`) for a
  client with `inlayHint.resolveSupport`, embedded eagerly otherwise; the textEdit is always eager. A
  `workspace/inlayHint/refresh` is sent only with `workspace.inlayHint.refreshSupport`.
- **`textDocument/documentColor` + `colorPresentation`** (#74): color swatches for `Color(r, g, b[,
  a])` constructors, `Color.CONSTANT` named constants, and `Color("#hex")`/`Color("name")` string
  forms, scanned token-primary (string-safe; no analyzer, no fan-out → a bare `Simple(true)` provider
  served even at Hard memory pressure). `colorPresentation` offers the lossless float `Color(…)`
  constructor (always) plus the `Color.NAME` form on an exact constant match.
- **`textDocument/codeAction` (+ resolve, `source.fixAll`)** (#75): quickfixes for the warning set —
  the `@warning_ignore("CODE")` suppression (landed above the enclosing statement, never
  mid-statement) plus mutating fixes (`_`-prefix an unused local, `@onready` for a
  `get_node`-without-onready, drop a redundant annotation), aggregated under `source.fixAll` for
  `editor.codeActionsOnSave`. `code_action_kinds` advertises EXACTLY the offered kinds (`quickfix` +
  `source.fixAll`); `executeCommandProvider` lists EXACTLY the one real command (`gdls.applyWarningIgnore`)
  — never an empty/broken list (anti-catalog W15). Generic-client-first: a `CodeAction` literal with a
  resolve-deferred edit for a rich client, a `Command` routed through
  `workspace/executeCommand` → `workspace/applyEdit` for a client without `codeActionLiteralSupport`,
  and an eager edit for one without `resolveSupport`; the additive `Diagnostic.data` tag is gated on
  `publishDiagnostics.dataSupport` (byte-identical pre-tag diagnostics otherwise). The mutating fixes
  carry their edit eagerly behind a fail-closed ERROR/shadow backstop (what the offer-time gate proved
  safe is exactly what the client applies — no re-derive-at-resolve onto a changed buffer).
- M10 §7.4 editor-profile walk extended over the semanticTokens (per-client legend allow-filter) /
  inlayHint (tooltip deferral) / documentColor / codeAction (deferred edit + `Diagnostic.data` gate +
  `source.fixAll` separation) gated projections (`tests/editor_profiles.rs`); the degraded
  `Command`/eager-edit codeAction paths (no vendored real client advertises them) are covered by
  `tests/code_action.rs`. Both conformance ratchets hold at 1.0000 (M10 is server-glue + additive —
  the parser/analyzer are untouched).

### Fixed
- **`textDocument/semanticTokens`** now emits server-legend indices (the client's advertised legend
  is an allow-filter, not a remap table) — fixes a wire-index mishighlight on clients whose legend
  differs from gdls's in membership or order (#121).

M11 — Phase 2 scenes & file operations (#76–#80), merged 2026-06-15. No release cut.

### Added
- **`.tscn` scene index + `$`/`%` typing** (#76): a panic-free `.tscn` **text** parser feeds a
  `SceneIndex` in `gd_project` (node tree / types / script attachments / `%`-unique names /
  instanced sub-scenes resolved by recursive TEXT, never engine instantiation — anti-catalog W16);
  the warm-start cache bumps (v5 → v6), the watcher tracks `**/*.tscn`, and a new `scene_parse`
  fuzz target joins the gate (now FIVE targets). **Premise correction (fusion- and
  real-4.6.3-binary-verified):** Godot's analyzer types `$`/`%` as a hard bare `NATIVE Node`
  (`gdscript_analyzer.cpp:3866-3886`), NOT scene-precise — feeding a precise scene type into the
  diagnostic path would manufacture false positives Godot never emits (sibling downcasts). So
  `reduce_get_node` types a valid `$`/`%` as bare `Node` (retiring the over-permissive `Variant`
  deviation, `docs/02 §11`): a member miss now fires `UNSAFE_PROPERTY_ACCESS`, while sibling
  downcasts/casts stay silent — exactly Godot. The precise scene-derived types are repurposed for
  **phase-3 navigation** (the resolution substrate ships dormant); exit criterion #4 is reframed to
  bare-Node parity.
- **Scene-aware node-path completion** (#77): `$`/`%`/`get_node("…")`/`load`/`preload` complete from
  the scene index — no scene ⇒ empty (anti-catalog W10), multiple scenes ⇒ the union with type
  ambiguity annotated. The `textEdit` is a mutating surface; five edit/UX corruption bugs were found,
  fixed, and guarded.
- **Autoload `uid://` → scene → root-script typing** (#78): a scene-backed autoload (a direct path or
  `uid://`) types as the scene root's attached script — matching Godot's analyzer, which dereferences
  the singleton (UNLIKE `$`/`%`); a scriptless root falls to the bare `Node` floor, and the
  `is_singleton` gate is honored. As a side benefit (oracle-verified on the acceptance project), an
  autoload name is itself a valid type annotation in Godot, so this also removed a class of
  `Could not find type "<autoload>"` false positives.
- **`willRenameFiles` + `did*`** (#79): the gated `workspace.fileOperations.willRename` returns a
  versioned `WorkspaceEdit` rewriting in-project `.gd` `preload`/`load` `res://` references (for `.gd`
  and `.tscn` moves); a fusion review caught and fixed a corruption-class over-capture — it now
  rewrites ONLY positively-identified `preload`/`load` arguments, never an arbitrary non-load `res://`
  value string. A scene-attached `.gd` move emits a `showMessage(Warning)` for the dangling
  `ext_resource`.
- **External-formatter bridge** (#80): `documentFormattingProvider` is advertised **only when** a
  `formatter.command` is configured (the rust-analyzer pattern — no formatter exists upstream to
  port). The subprocess path is hardened: no shell, a bounded timeout, concurrent stdin/stdout (no
  pipe deadlock), an output cap, and per-failure-class warning dedupe; edits are minimal-diff.

### Changed
- The warm-start cache format bumped (v5 → v6) for the `SceneIndex` — one cold re-index per project
  on first launch after upgrading, self-healing.

**Convergence (exit #4):** comparative `scan_diags.py` sweeps (prev-release v1.0.7 vs the M11 build)
on BOTH acceptance projects — Pixelorama (Linux) and 3-souls (Windows, the Windows binary) — held the
default-profile error baseline with **zero new false positives** (3-souls even shed a class of
autoload-as-type FPs); the `--strict` increase is the convergent bare-Node `UNSAFE_*`-on-node-access
family (Godot's analyzer emits the same). Both fidelity ratchets stay **1.0000**; the five-target
fuzz gate is clean.

**Deferred (filed):** #123 (`UNSAFE_METHOD_ACCESS` analyzer-wide under-emission), #124 (`$`/`%`
glyph), #125 (precise `$`/`%` navigation typing — the dormant substrate's consumer), #126 (`%`
mid-string completion span), #127 (`res://` asset completion), #129 (scriptless autoload as a type
annotation), #131 (`.tscn` `ext_resource` rewrite on rename), #132 (`willRenameFiles` write-set
misses), #135/#136 (formatter head-of-line / cancel). The M9 (#106/#107/#109) and M10
(#111/#113/#114/#115/#118/#119) carryovers remain tracked. (All of these were closed afterwards —
see the post-Phase-2 hardening entry below.)

**Phase 2 is now COMPLETE (M7–M11 shipped); a release will be cut from `main` as a separate step.**

Post-Phase-2 hardening (#99, #125, #132, #157, #161, #189, #193, #204, #246), merged 2026-08-28.
No release cut. Closes out every remaining tracker item — the issue tracker is empty.

### Added
- **Precise `$`/`%` navigation typing** (#125): `hover`, `definition` and `typeDefinition` on a
  `$Path` / `%Name` / `get_node("literal")` access answer with the scene-precise node type — the
  engine class of the node the access reaches, or the `class_name` of the script attached to it —
  instead of bare `Node` (and, for `definition`, instead of nothing at all). Built in
  `gd_server::scene_nav` from the scene-index fact and handed straight to the renderers, so it never
  enters an `AnalysisResult`: the diagnostic path keeps seeing bare `Node`, and the sibling downcasts
  Godot tolerates stay silent. Conservative — an absolute path, a scene-less script, or two attaching
  scenes disagreeing all fall back to bare `Node`.
- **Autoload singleton NAME rename** (#157): an autoload is the one project symbol whose declaration
  is not GDScript, so renaming it now rewrites the `project.godot` `[autoload]` key alongside the
  `.gd` uses — or refuses whole. The `.gd` edit set is collected BY IDENTITY (the autoload script's
  `FileId`), never by the raw name scan the read path uses, so an unrelated same-named local or
  attribute is excluded by construction; the config span covers the NAME only, leaving the `*`
  singleton marker, quoting and path intact. The new name is validated against the namespaces an
  autoload joins (engine symbols, project `class_name`s, other autoloads). A scriptless-scene
  autoload and a name that is also a `class_name` stay refused.
- **Lambda `.call` / `.call_deferred` signatures** (#193): signatureHelp on a lambda-valued name
  shows the LAMBDA's parameter list instead of the native `Callable.call(...)` vararg shape. The
  lambda is pinned in the server glue (Godot's analyzer types every lambda as a bare `Callable` with
  no method info, and the port is faithful to that), through the scope-correct binding resolver, and
  refuses on any rebind. `.bind` deliberately keeps the native signature — it binds a TRAILING slice
  whose length is unknowable mid-typing.

### Fixed
- **Half-applied class rename** (#246): a `class_name` consumer that used the class only in function
  BODIES was never scanned (the interface index records the `extends` head and member/param types
  only), so the class was renamed while that consumer kept calling the old name. The candidate set is
  now the project-wide textual prefilter; collection inside a candidate stays occurrence-positive.
- **Inner-class-scoped type rename** (#189): a type declared inside an inner class and colliding with
  a global `class_name` refused for lack of a precise target, and the global's own rename over-grabbed
  an unrelated in-file `extends`. Both now resolve positionally against the enclosing class.
- **`codeAction` delete-fix cross-file gate** (#204): the `UNUSED_PRIVATE_CLASS_VARIABLE` delete fix
  could not see body-only cross-file reads (invisible to the interface index) and offered a deletion
  that broke them. A one-sided textual refusal signal now suppresses the fix whenever any other file
  mentions the member.
- **`willRenameFiles` write-set misses** (#132): index-form and threaded `ResourceLoader` calls and
  `preload` through a const indirection were missed, while a bare `load` shadowed by a local was
  wrongly rewritten.

### Changed
- **completion / signatureHelp perf budget rows** (#99) replaced their estimates with measurements
  from a 2414-file project, with provenance recorded in `bench/budget.toml`.
- Test-only: the rename canonicalization class-decl backstop is pinned white-box (#161, unreachable
  from legal source by design), and the watcher coalescing bound is now measured only when the write
  burst actually fits inside one quiet window (#249, #252) — a stalled CI runner has nothing to
  coalesce, which was the long-standing flake.

Generic-client conformance wave (#255–#265), merged 2026-08-29. Findings from an end-to-end
verification of the release binary driven over stdio by a synthetic LSP client across four client
capability profiles, with disputed analyzer behaviour cross-checked against the Godot 4.6.3-stable
binary. No release cut.

### Fixed
- **`exit` without `shutdown` now exits 1** (#262): LSP 3.17 §exit reserves status 0 for the case
  where the shutdown handshake completed, so a supervising client can tell a clean stop from an
  abrupt one. gdls returned 0 either way. A transport close (stdin EOF, no `exit` at all) stays 0 —
  the client leaving is not a protocol violation.
- **Progress token no longer carries Debug quotes** (#265): `window/workDoneProgress/create` shipped
  `gdls/progress/"gdls-out-0"` — lsp-server renders a string `RequestId`'s `Display` through `Debug`
  on purpose, and interpolating the outgoing id inherited that into a wire value. Tokens are opaque
  so nothing broke, but the quotes read back verbatim in client protocol logs.
- **`textDocumentSync` advertised as options, not a bare kind** (#260): the number form said
  nothing about `openClose` or `save`, and every per-file surface here is keyed on an open buffer.
  Now `{ openClose: true, change: Incremental, save: { includeText: false } }` — `didSave` is
  routed and mutates nothing, so the text is never resent, and neither `willSave` hook is claimed.
- **Hover prose defaults to plaintext, not markdown** (#261): a client that advertised no
  `hover.contentFormat` had said nothing about what it can render, and got markdown anyway —
  fences and `**` into a popup that may not render them. `ProseFormat`'s default is now
  `PlainText`, which also puts hover on the floor completion and signatureHelp already took, and
  an empty format list takes it too. Every captured editor profile asks for markdown explicitly,
  so nothing real is downgraded.
- **A dependency edit now refreshes an open dependent** (#255): a file that named a project class
  ONLY inside a function body — `var d := Dep.new()`, `Global.setting` — had no dependency edge,
  because the eager-interface pass records `extends`, member annotations, parameter types and
  `preload`s and nothing else. Editing `Dep` never invalidated it, so an open buffer kept
  publishing diagnostics computed against the old `Dep` for the rest of the session. The interface
  now also carries the identifiers a file references anywhere; `recompute_edges` resolves them
  through the `class_name` registry, so only real project classes become edges and everything else
  is dropped. They stay out of the `references`/`rename` candidate index on purpose — filling that
  with every local's name would turn a cursor on an unresolvable identifier into a project-wide
  analysis. Costs about 1.2 KB of interface per file (8% of the warm-start cache on a 249-file
  project), and the cache format goes to v9 so a v8 cache is rebuilt rather than warm-loaded
  without the new edges.
- **`workspace/diagnostic/refresh` is now sent** (#255): gdls advertises
  `diagnosticProvider.interFileDependencies: true` — editing one file can change another's
  diagnostics — but never signalled it, and a pull-diagnostics client only re-pulls what it is
  editing, so its cached report for every dependent stayed stale. One refresh per reindex batch,
  gated on `workspace.diagnostics.refreshSupport`, and only when the batch reached past the file
  the client just edited: a plain body-only edit must not trigger a project-wide re-pull on every
  keystroke.
- **`func` reports `SymbolKind.Method`** (#263): a `.gd` file is a class, so every `func` in it —
  top-level, inner-class, or `static` — is a member of one, and GDScript has no free functions for
  `Function` to mean. `documentSymbol` (both shapes), `workspace/symbol` and call-hierarchy items
  all say `Method` now, matching the `CompletionItemKind::Method` completion already returned for
  the same symbols; the kind picks the glyph an outline draws, and the two surfaces disagreed.

- **A call to a function that does not exist reports it** (#256): `miss()`, `self.miss()` and a
  method miss on a hard builtin base (`Vector2(1, 2).bogus()`) were all completely silent — the
  most common GDScript typo produced no squiggle. Each was hushed on the theory that a trimmed
  native dump would make absence unprovable, but the utility lookup early-returns long before that
  branch is reached, and `ApiProvenance::Exact` is precisely the claim that the dump IS the engine
  surface — the same gate every neighbouring arm already used. Now they emit Godot's own text at
  Godot's own ranges, verified line-for-line against the 4.6.3-stable binary, and still stay silent
  wherever the claim would be unsound: a non-`Exact` dump, an `extends` that does not resolve, a
  soft-typed base, or a `Dictionary` (whose keys are its members, as upstream has it).
- **A member miss on a `class_name` instance warns** (#256): `UNSAFE_METHOD_ACCESS` /
  `UNSAFE_PROPERTY_ACCESS` fired on native bases and said nothing on script ones, because a script
  miss degrades to a permissive `Variant` before the arm that reports it. Both now fire for script
  bases too, when the chain was fully walkable, finishing #123's stated acceptance. Both codes stay
  ignore-by-default, exactly as in Godot. Script bases in these messages read as their `class_name`
  now, not the internal `<Script #3>` placeholder.

- **The watch registration no longer asks clients to watch everything** (#264): the glob set
  included a `**/*` catch-all so that a client with no server-side watcher would still report new
  assets and keep `load`/`preload` completion live (#226). It was sent unconditionally, which on a
  large project means the client watches `.git/`, `.import/`, `build/` and every exported binary —
  a great many inotify handles and a stream of notifications gdls discards. It is now registered
  only when gdls's own filesystem watcher failed to arm; when that watcher is live it already
  reports asset changes. The other six globs are unchanged, and both paths run the same
  server-side exclusion filter, so the semantics are identical either way. The trade-off is
  written down in `docs/09` §7.1.

### Added
- **The bundled engine API now carries documentation** (#259): the `extension_api.json` embedded in
  the binary as the last-resort fallback was dumped without docs, so a user who installs gdls with
  no Godot on `PATH` — the headless case this project exists for — got correct signatures and an
  empty hover on every engine class, silently. It is now built with
  `--dump-extension-api-with-docs` and stripped to only the fields gdls reads (the GDExtension ABI
  sections and binding hashes go), which costs 690 KB of gzipped asset — 396 KB → 1.09 MB, an 8%
  release-binary increase — and buys the whole engine's prose on first run.
  `scripts/regen-stock-api.py` regenerates it, and a CI test fails if a regeneration drops the
  docs. That session also sends one `window/showMessage` at startup naming the stock surface and
  how to replace it, since the prose being present does not make it the user's own engine build.
- **signatureHelp for builtin type constructors** (#257): `Vector2(`, `Color(`, `Callable(` and
  friends answer with one signature per overload, labelled `Type Type(args)` — the shape
  `Variant::get_constructor_list` implies — filtered by Godot's own arg-index rule so an overload
  the cursor overruns drops out. A deliberate divergence from Godot's language server, which
  returns null here and keeps constructor arghints on the completion surface (#194, untouched):
  under #30 a generic client reads parameter hints from `signatureHelp` and nowhere else, and
  these are among the most-typed calls in GDScript. Where Godot's filter would show nothing —
  typing past every overload's arity — the popup stays up with the widest overload first rather
  than vanishing mid-edit.
- **`##` docs now reach every surface they were written for** (#258): M7 wired the doc pipeline
  through the paths it needed at the time, which left most declaration kinds with prose nobody
  ever saw. Hovering a file's own `class_name` — or any cross-file use of that name — rendered a
  bare identifier instead of the head class's brief, long form and `@tutorial` links. Named enums
  and their values rendered a type label and no doc at all, on either side. And a documented
  `var` / `const` / `signal` / inner `class` showed its prose at the declaration but not where it
  was actually read from another file: the doc rode the CALL hover, so only `func` had both.
  All of them now render the same body wherever the symbol appears, read from the declaring file's
  interface.
- **`@deprecated` and `@experimental` are visible** (#258): both were parsed and then dropped.
  They now lead the hover body as a banner — above the prose, so a reader who stops at the first
  line still learns the symbol is on its way out — and `@deprecated` additionally sets
  `CompletionItem.tags: [Deprecated]`, downgraded to the pre-3.15 `deprecated: true` boolean for a
  client that never advertised `completionItem.tagSupport` (never both), and the standard
  `deprecated` semantic-token modifier on the declaration and on every resolved use — a member
  read, a call site, a class name — each resolved through the declaring file's interface rather
  than by name. The modifier was in the advertised legend since M10 with nothing behind it.
  Engine symbols never carry either signal: `extension_api.json` has no deprecation field in
  4.6.3, with or without docs, so there is nothing to claim one from.
- **`workspace/diagnostic/refresh` now reaches real clients** (#277): the refresh #255 added was
  gated on `workspace.diagnostic.refreshSupport`, which is what lsp-types 0.97 deserializes — but
  LSP 3.17 spells the key `workspace.diagnostics`, plural, and that is what VS Code, Neovim, Zed
  and Sublime all send. No editor had ever received it, so the pull half of the staleness fix was
  inert while the push half worked. The key is now read off the raw capabilities object, with the
  typed field kept as a fallback, and the test builds the capability as JSON so a typed round-trip
  can no longer hide the same class of bug.
- **An inner class hovers with its doc in a type position too** (#277): `func f(i: Outer.Inner)`
  showed a bare `Outer.Inner` while `Outer.Inner.new()` showed the doc, because the `Inner` segment
  has no `class_name` registry entry to route through. The analyzer already pins a script type
  there; following it reaches the same declaring interface.

## [1.0.7] — 2026-06-13

Follow-ups to the v1.0.6 utility-as-Callable hotfix, surfaced by its review (#92). Cut from
v1.0.6 plus these fixes only — the unreleased Phase 2 work on `main` stays unreleased, per the
Phase 2 release deferral.

### Fixed
- **Bare Variant utilities resolve to a constant `Callable` under any native-API state**, including
  an empty database (#92): the v1.0.6 arm gated Variant utilities on the ingested
  `extension_api.json` table, so when no dump was available (embedded fallback disabled, or a
  decompress failure) bare `print` / `floor` still fell through to
  `Identifier "X" not declared in the current scope.`. Resolution now mirrors Godot's compile-time
  `Variant::has_utility_function` through a database-independent registry of the 114 Variant
  utilities, so the fix holds regardless of how — or whether — the native API was loaded.
- **Duplicate same-utility dictionary keys are now reported** (#92): `{print: 1, print: 2}` emits
  Godot's `Key "@GlobalScope::print" was already used in this dictionary (at line N).` (and
  `@GDScript::len` for the GDScript-only family), matching the engine; previously the duplicate
  folded to an opaque value and went unreported. Distinct keys and non-utility constants are
  unaffected.

### Changed
- The constant-`Callable` type is now built by a single `make_callable_type` helper mirroring
  Godot's `gdscript_analyzer.cpp`, replacing eight inline copies (#92) — groundwork for the
  Phase 2 signatureHelp `MethodInfo` wiring. No behavior change.

Both conformance ratchets hold at 1.0; a new ingest guard pins the full 114-name Variant utility
set against the embedded dump, and the duplicate-key messages are oracle-verified against
godot 4.6.3-stable.

## [1.0.6] — 2026-06-13

Hotfix for a false-positive error reported from real-project use (#88). Cut from v1.0.5 plus
this fix only — the unreleased Phase 2 work on `main` stays unreleased, per the Phase 2
release deferral.

### Fixed
- **Bare utility-function references no longer error with `Identifier "X" not declared in the
  current scope.`** (#88): Godot 4.x exposes utility functions as first-class Callables, so
  `print.call_deferred(msg)`, `arr.map(floor)`, `var f := absi`, and `clamp.bind(0, 1)` are all
  legal — the `reduce_identifier` arm that resolves them (analyzer.cpp:4641-4652) was unported
  and every Variant utility referenced outside callee position false-positived. The arm now
  reduces both utility families (Variant + GDScript-only) to a constant `Callable`, matching
  Godot's `make_callable_type` shape.
- GDScript-only utilities (`len`, `range`, …) referenced bare previously escaped the error but
  carried **no type**; they now get the same constant `Callable` (hover/inference fixed).
- `const PRINTER = print` is accepted (the reference folds as a constant, mirroring Godot's
  `reduced_value = Callable(...)`), and `print = 5` now fails with Godot's exact
  `Cannot assign a new value to a constant.` instead of the bogus not-declared error.

Both conformance ratchets hold at 1.0; the differential-oracle harness gains a
utility-as-Callable fixture (19/19 at jaccard 1.0 against godot 4.6.3-stable).

## [1.0.5] — 2026-06-12

The LSP protocol-conventions release. A post-v1.0.4 audit compared every exposed capability
against the LSP 3.17 spec and the rust-analyzer/gopls/clangd conventions and filed twelve
issues (#43–#54); this release closes all of them — wrong range shapes, ignored client
capabilities, dead-end call-hierarchy items, fabricated locations, missing display metadata,
and the references raw-scan over-reporting (#54, pulled forward from Phase 2). Godot message
strings, spans, and severities are untouched throughout: both conformance ratchets hold at
parser 186/186 and analyzer 300/300.

### Changed — ranges anchor the symbol name token (#44, #46, #48)
- **Cross-file `definition` returns the member's name token**, not the whole declaration node
  editors would select: `MemberDecl` records `name_span` at interface-extraction time (index
  cache format **v4** — old caches rebuild cold once on upgrade), validated against live text
  with a re-locate-on-drift fallback. Native-stub jumps land on the member's name token too
  (`RenderedStub` records per-member column extents; on-disk stub text unchanged).
- **`workspace/symbol` results carry real name-token ranges** instead of zero-width points at
  column 0 — each winner file is read once post-cap for the encoding-correct mapping, falling
  back to the old point only when validation fails. No result carries `start == end`.
- **callHierarchy `fromRanges` cover the callee name token** instead of the whole call
  expression — multi-line calls no longer highlight entire blocks, and clicking an incoming
  call lands on the method name, not the receiver.

### Added — client capabilities honored (#43, #45, #47, #52)
- **`documentSymbol` downgrades to flat `SymbolInformation[]`** for clients without
  `hierarchicalDocumentSymbolSupport` (absent ⇒ flat, the rust-analyzer convention) — Helix
  explicitly declines the nested shape it was receiving.
- **`references` honors `includeDeclaration: false` as a filter**: declaration name tokens are
  removed at final assembly instead of leaking through the identifier scan.
- **The empty `workspace/symbol` query returns all symbols** (spec: "Clients may send an empty
  string here to request all symbols") — classes first, capped at 256 — so symbol pickers open
  populated.
- **UNUSED_*/UNREACHABLE_* diagnostics carry `DiagnosticTag.Unnecessary`** (editors fade dead
  code), gated on the client's `publishDiagnostics.tagSupport`; every warning-coded diagnostic
  links Godot's warning-system docs via `codeDescription`.

### Fixed — call hierarchy correctness (#49, #50, #51)
- **Outgoing `to` items are expandable**: they carry the same `{uri, name}` data blob
  prepare/incoming items do (expansion used to die with `null` at depth 2), and data-less items
  re-resolve from `uri` + `selectionRange` (the rust-analyzer/gopls shape).
- **Native callees anchor into their API stubs** at the member's name token — the fabricated
  `to` item claiming the callee was declared at (0,0) of the *caller's own file* is gone, and
  unresolvable callees are omitted entirely. Expanding a stub-anchored item answers with a
  clean empty list.
- **`detail` is populated everywhere**: documentSymbol outlines render member signatures
  through the same byte-stable formatters hover pins (classes carry their `extends` clause),
  and call-hierarchy items carry their `res://` script path (native items the declaring class)
  so same-named `_ready` callers stay distinguishable.

### Added — navigable diagnostics (#53)
- **SHADOWED_VARIABLE / SHADOWED_VARIABLE_BASE_CLASS publish `relatedInformation`** pointing at
  the shadowed declaration's name token — navigable even when the base class lives in another
  file, where the message's "at line N" was dead text. Message strings stay byte-identical.

### Changed — references precision (#54)
- **`Binding::Call` classifies its callee as a `CalleeTarget`** — `Script { file, class_path }`
  (the owning class within the file), `Native { class }`, or `Unresolved` — derived at one
  consolidated recording site from the resolution the dispatch actually used (bare calls in
  inner classes now attribute dispatch-accurately).
- **In-file attribute reads (`self.hp`) record `Binding::Use`**, closing the last
  attribute-read recording gap (cross-file reads already recorded precise kinds).
- **References on resolved member targets are binding-backed**: two unrelated `var speed`s in
  different classes no longer report each other's sites, typed cross-file accesses through
  body-local vars are now found (a recall fix), and local/parameter targets stay inside their
  function. The documented "over-approximate, never under-report" raw scan survives only where
  resolution genuinely can't decide (class/enum/type names, unanalyzable buffers).

## [1.0.4] — 2026-06-12

The native-surface completeness release. v1.0.3's real-project capability walks showed
script-side navigation had outpaced the native side: hover on a native member fell back to a
degraded type label, `definition` on a native class or member had nowhere to jump,
`workspace/symbol` anchored every global class at line 0, and the analyzer dropped to dynamic
exactly where upstream's class-resolution loop falls through to the native check — the same gap
that kept `UNSAFE_PROPERTY_ACCESS` deferred. Every v1.0.4 milestone issue clustered on that
surface (#32–#35); the adjacent gaps the work surfaced were fixed in the same pass (#37–#41).

### Added — native hover (#35, #40)
- **Native member and class hover render real declaration lines** via a shared `native_render`
  formatter pinned to Godot's `gdscript_workspace.cpp` detail formats, with native + builtin
  arms in both member paths (call callees and plain attribute access). Bare calls resolve
  implicit `self` through the extends chain, and `@GlobalScope` utility functions render too
  (#40). Class-name hover upgrades to `<Native> class X extends Y`.

### Added — definition into native API stubs (#34, #38, #39)
- **`definition` on a native class or member materializes a readable API stub** under the
  user-level cache (`stubs/v{N}-{hash}/Class.gd`; per-request keying, atomic write-if-absent)
  and returns a plain `file://` Location into it: the class header for class references, the
  declaring class's member line for members — implicit-`self` member calls included (#38). The
  native arms run only after every project resolution has missed, stub buffers publish empty
  diagnostics, the stub tree is GC'd once per session (#39), and a `stubCacheDir` seam keeps it
  testable (docs/05).

### Added — analyzer native fall-through + `UNSAFE_PROPERTY_ACCESS` (#32, #37)
- **The CLASS-branch native tail restores upstream's class-loop → native-check fall-through**,
  `type_from_type_ref` gains its enum/bitfield arms (`gdscript_analyzer.cpp:5744-5759`), and
  `reduce_call` probes the native surface on an interface miss (#37) — so int-typed native
  surfaces resolve faithfully instead of silently degrading to dynamic.
- **`UNSAFE_PROPERTY_ACCESS` now fires** (`gdscript_analyzer.cpp:4880-4886`) — until now the one
  deliberately-deferred warning code. Emission stays gated on negative-claim soundness per
  docs/02 §11b: unresolvable chain roots and cross-file shallow-interface misses are silent
  under any provenance, so the warning can't lie. Conformance holds **300/300 with the warning
  live**. On Pixelorama the default profile holds 0 errors (+3 `INTEGER_DIVISION` warnings on
  the newly int-typed surfaces) while the strict profile gains +677 genuinely subtype-dependent
  `UNSAFE_PROPERTY_ACCESS` findings — and sheds 50 `UNSAFE_CALL_ARGUMENT` + 29 `UNSAFE_CAST`
  false positives from the same typing improvements.

### Fixed — workspace/symbol anchors (#33)
- **`ClassEntry` records the `class_name` declaration line + identifier span** at index time
  (cache format v3): `workspace/symbol` anchors global classes at their declaration instead of
  a zero-width line-0 location, and `find_global_class_definition`'s closed-file arm drops its
  per-lookup re-parse.

### Changed
- **gd_types groundwork**: `NativeDb::lookup_member`/`lookup_builtin_member` chain-walk the
  class hierarchy and report the declaring class, `Param.default_value` survives ingestion, and
  `display_type` produces the hover labels.
- **`scan_diags.py` grows `--strict` and a per-code warning histogram**, so the release-gate
  sweep can gate the strict profile too.

### Fixed — hardening
- **Stub machinery**: per-session render cache, rename-race fallback, sentinel-mtime GC
  freshness, a stub-name guard, span content checks, and a native chain-walk depth cap.
- **The `analyze` fuzz target is revived and wired into CI's bounded fuzz job** (#41) — it had
  rotted out of the build (95,904 clean runs / 121 s locally).

## [1.0.3] — 2026-06-11

The warning-completeness release. The v1.0.2 plugin sessions exposed that the existing LSP test
suite cannot be the single source of truth: 22+ declared warning codes had no emission site
(#29), `@warning_ignore` missed multi-line targets (#28), and several navigation capabilities
quietly returned nothing on shapes every real project uses. Each fix below was verified the same
way the bugs were found — driving every exposed capability against the two acceptance projects
and reading what actually comes back, for native and script symbols alike.

### Fixed — analyzer warnings (#28, #29)
- **`@warning_ignore` covers multi-line targets**: the ignored-lines table now records Godot's
  annotation→target-header span (`warning_ignore_annotation`, gdscript_parser.cpp:5078-5151)
  per target kind — multi-line signatures, initializers, `for`/`if`/`while`/`match` headers,
  match-branch patterns — instead of a single line. The compensating node-attached annotation
  walk is removed: upstream filters purely by anchor line, and the spans subsume it (no
  over-suppression: body lines past the header still warn, exactly as upstream).
- **19 missing warning emission sites ported** function-for-function from
  `gdscript_analyzer.cpp`/`gdscript_parser.cpp` @ 4.6.3-stable: `EMPTY_FILE`,
  `STANDALONE_EXPRESSION`, `STANDALONE_TERNARY`, `UNREACHABLE_CODE`, `UNREACHABLE_PATTERN`,
  `ASSERT_ALWAYS_TRUE`, `ASSERT_ALWAYS_FALSE`, `INTEGER_DIVISION`, `INCOMPATIBLE_TERNARY`,
  `UNTYPED_DECLARATION`, `INFERRED_DECLARATION`, `REDUNDANT_STATIC_UNLOAD`,
  `UNASSIGNED_VARIABLE`, `UNASSIGNED_VARIABLE_OP_ASSIGN`, `UNUSED_VARIABLE`,
  `UNUSED_LOCAL_CONSTANT`, `RETURN_VALUE_DISCARDED`, `STATIC_CALLED_ON_INSTANCE`,
  `INT_AS_ENUM_WITHOUT_CAST`. (The issue's audit under-counted: `UNASSIGNED_VARIABLE` and
  `UNUSED_VARIABLE` were also silent — 24 total, of which `DEPRECATED_KEYWORD` and the 3
  deprecated `*_USED_AS_*` codes have no emission site in upstream either and stay silent by
  fidelity.) Per-code unit tests in `gd_analyze/tests/warning_emissions.rs`; the conformance
  ratchet holds 300/300.
- **`UNSAFE_PROPERTY_ACCESS` stays deferred, deliberately**: it is a negative claim and the
  attribute lookup is not yet complete enough to make it truthfully (native signals through
  attributes and some property shapes resolve in Godot but miss in gdls); under the strict
  profile it is promoted to an error, so a premature site would false-positive loudly. The
  would-be emission site documents the blockers.
- **Warning output order matches `apply_pending_warnings`**: warnings are stable-sorted by
  anchor line among themselves (errors keep emission order) — signature-pass warnings on late
  lines no longer render before body-pass warnings on earlier lines.
- **Untyped rest parameters no longer error**: `func f(...args):` false-positived
  `The rest parameter type must be "Array", but "Variant" is specified.` — upstream validates
  only when a type is specified; the untyped shape is an inferred `Array` plus
  UNTYPED_DECLARATION.

### Fixed — navigation (found by the real-project capability walk)
- **`definition` on a dotted method call through a typed var** (`cel.on_remove()`) jumped
  nowhere while hover resolved the signature; it now projects the call binding's declaring file
  and jumps to the member declaration.
- **`callHierarchy/incomingCalls` was structurally empty across files**: its candidates came
  from the interface-level reverse index, which never contains body-only method names. It now
  uses the same project-wide two-phase textual scan as `references` (Godot's workspace.cpp:472
  strategy).
- **`prepareCallHierarchy` on a call-site callee prepares the callee** (dotted and bare),
  not the function the cursor happens to sit inside; declaration clicks and non-identifier
  positions keep the enclosing-function behavior.
- **`<Script #N>` no longer leaks into cast errors**: `Invalid cast. Cannot convert from "Nil"
  to "<Script #3095>".` now renders the script's `class_name` (or file basename), matching
  Godot's `DataType::to_string()`.

### Fixed — logging & CI
- **Bridged `log::*` events render with their real target/file/line** (and `GDLS_LOG`
  per-target directives can match them): `tracing-subscriber`'s `tracing-log` feature was off,
  so every bridged event carried target `log` plus `log.target=…` field noise.
- **ETXTBSY spawn retry in the auto-dump**: exec can transiently fail with "Text file busy"
  while the Godot binary is open for write (mid-rebuild/copy — or, in CI, a concurrent test's
  fork briefly holding a fixture script's fd). A bounded 500 ms retry absorbs it; this was the
  ubuntu-latest flake in `chatty_child_does_not_deadlock`.
- **Integration tests read responses by skipping interleaved notifications**: on slow runners a
  `publishDiagnostics` could outlive the post-open drain and land where a response was expected
  (the windows-latest call-hierarchy failures); all request/response reads now match like a
  real LSP client.

### Changed
- **Background dump timeout raised 60 s → 5 min**: the deadline exists only to reap a wedged
  Godot child, and the dump left the critical path in v1.0.2 — a generous budget costs nothing,
  while 60 s could kill legitimate slow first boots (cold import caches, AV-scanned binaries,
  huge projects) mid-dump.
- **Acceptance warning baselines re-based** for the new emission sites (error baselines hold
  exactly on both acceptance projects, verified per-platform — the private project on Windows
  with the Windows binary): Pixelorama 43 → 116 warnings (0 errors before and after).

### Fixed
- **No background auto-dump when `extensionApiPath` is pinned**: `load_native` never consults
  the managed `.gdls/` dump while an explicit path is set, so the background Godot boot was
  pure waste (its result deduped to a no-op on adoption). `spawn_background_dump` now declines
  up front, like the other no-dump configurations.

## [1.0.2] — 2026-06-11

The first-run robustness release. The first real-world Claude Code plugin session on a fresh
Windows machine hit three failures at once: the synchronous `extension_api.json` auto-dump
wedged for the full 60 s timeout before the first request was answered, the timed-out dump left
the whole session on an **empty native DB** — so every native annotation in every file errored
`Could not find type "X" in the current scope.` — and hover on declarations rendered the opaque
`<Script #N>` placeholder. v1.0.2 makes the first run converge to the same behavior as every
later run, with no false positives at any point in between. (#24, #25, #26)

### Fixed — native API resolution (#24)
- **Embedded stock fallback**: a gzipped stock 4.6.3 no-docs `extension_api.json` (0.4 MB) ships
  inside the binary as the last resolution step — builtins always resolve on a fresh install
  with no Godot anywhere. Kill switch: `embeddedApiFallback` (default `true`); also covers an
  unreadable explicit `extensionApiPath`.
- **Provenance-gated negative claims**: `NativeDb` now carries `ApiProvenance`
  (`Exact` project-derived / `Generic` embedded / `Absent` empty). Native-rooted negative
  diagnostics — the terminal `Could not find type`, super-call misses, meta-base
  `Cannot find member` — fire only under `Exact`: a generic surface can prove what exists,
  never what doesn't (a custom engine build's class is indistinguishable from a typo).
  Documented as a deliberate deviation in `docs/02` §11b.

### Fixed — auto-dump lifecycle (#25)
- **The dump runs on a background thread** and is adopted mid-session through the event loop
  (reload native DB → republish open buffers → refresh the warm-start cache). The first request
  never queues behind a Godot boot; first-run diagnostics converge the moment the dump lands.
- **Child stdout/stderr are drained concurrently** — a chatty engine boot could fill the 64 KB
  pipe and ride the whole dump into the timeout.
- **"The artifact decides" now covers the timeout path**: a deadline-killed Godot that already
  wrote a complete dump is adopted (parse-validated; torn files quarantine and fall through).
- **Mid-session reload stability**: a reload that resolves strictly worse than the live DB (a
  torn read of a mid-write dump) keeps the live DB; identical content (the post-adoption watcher
  echo) skips the re-analyze entirely.
- didOpen/didChange/didClose now drain the dirty set they populate: open dependents get fresh
  diagnostics immediately instead of waiting for the next unrelated watcher batch.

### Fixed — hover (#26)
- **Declaration-site signatures**: hover on the name of a `func`/`var`/`const`/`signal`/inner
  `class` renders the member's signature through the same formatter as the call-site hover
  (statics now show `static` in both). Inner-class members resolve through their interface
  scope; body-level locals keep the analyzer's resolved-type hover.
- **Human type labels**: script-typed values render their `class_name` (or file basename) and
  in-file class metas their identifier — the `<Script #N>` / `<Class>` Display placeholders
  never reach hover output. Untyped members with an inferred initializer append the resolved
  type (`var made: ReproEntity`).

## [1.0.1] — 2026-06-10

The urgent diagnostics-correctness release. A post-v1.0.0 full-project sweep (didOpen every
`.gd`, tally `publishDiagnostics`) found error-level **false positives in ~45–55% of files** on
real layered projects (Pixelorama @ stock 4.6.3: 133/243 files, 1,223 errors; a 2,338-script
production project: 1,051 files, 6,167 errors) — all violations of the "never lie" rule, all
reproducing on vanilla Godot 4.6.3 fixtures. v1.0.1 fixes the four families behind that — and the
long tail the new sweep gate then exposed — closes both v1.0.0 follow-ups (#13, #14), and removes
the last manual setup step (`extension_api.json`). **Outcome: the Pixelorama sweep reports 0
files with errors** (from 133), with the conformance ratchets still at 186/186 + 300/300.

### Fixed — analyzer false positives
- **Cross-file `extends ClassName` lost native lineage and inherited members** (#15): a new
  shared script-chain resolver (`gd_analyze::script_chain`, memoized, cycle-guarded) walks
  `Interface::extends` links to the native root, mirroring Godot's `native_type` propagation
  (analyzer.cpp:617-619). Fixes the false `Cannot use "$" on a class that isn't a node.`,
  `"@onready" can only be used in classes that inherit "Node".`,
  `Identifier "…" not declared in the current scope.` (inherited members), and
  `Invalid argument … but is "<Class>".` (self-compat — `is_type_compatible` now ports Godot's
  source decomposition, analyzer.cpp:6210-6296). Unknown/unresolvable chains stay permissive.
- **Cross-file `Class.CONST.member` chains errored `Cannot get property from enum value.`**
  (#18): regular consts now take Godot's CONSTANT member arm (typed from their declaration via
  the new interface-TypeExpr resolver); only genuine `enum { … }` hoists type as anonymous-enum
  values (the interface now records them explicitly).
- **Builtin named constants poisoned constant arithmetic** (#16): `Vector3.UP * 3.0` reported
  `Invalid operands to operator *, Nil and float.` because the constant folded as a placeholder
  Nil. `FoldedValue::Opaque(kind)` keeps constancy without a fabricated value; binary ops
  validate by type, dict dup-key treats unknown values as never-equal, and the dump's
  per-constant declared types are now ingested (`Vector3.AXIS_X` is `int`).
- **`Packed*Array` was not iterable** (#17): `for p in paths:` errored
  `Unable to iterate on value of type "PackedStringArray".`; the typed-container element table
  (gdscript_parser.cpp:5508-5530) is now ported into `resolve_for`.
- Cross-file named enums carry their **declared** integer values (literal chains; unknown values
  suppress `INT_AS_ENUM_WITHOUT_MATCH` / `ENUM_VARIABLE_WITHOUT_DEFAULT` instead of judging
  against sequential placeholders).
- The long tail the sweep gate exposed, all vanilla: relative `preload("sibling.gd")` /
  `extends "../x.gd"` paths resolve against the referring script's directory; autoloads work as
  TYPE annotations (`-> Global`) and nested types under Script heads resolve
  (`-> BaseLayer.BlendModes`); builtin INSTANCE members type (`pos.x` → `int`); `Object`-returning
  utilities are Native-kind (fixes `instance_from_id(...) is Node3D`); the transform/xform
  operator registrations are ported (`Transform2D * Vector2`, Basis/Quaternion/Projection
  families); implicit conversion accepts Object-derived arguments for `RID` parameters; native
  SIGNALS and bare native members resolve through the implicit class base (`position`,
  `changed.connect(…)`); bare inherited calls walk the chain and call arity honors parameter
  DEFAULTS; interfaces capture obvious `:=` member initializer types (literals, `Color.PURPLE`
  via the dump, `X.new()`); builtin constructor calls over constant args are constant
  (`match v:` `Vector2(-1, -1):`); ternaries of two hard same-shaped branches infer; assigning
  through singleton metas is legal (`Engine.max_fps = x`); `Variant.Type` annotations resolve;
  non-script preloads type by extension (`.tscn` → PackedScene, `.gdshader` → Shader); and a
  same-file Class↔Script identity bridge fixes `self is OwnSubclass` / `node = node.parent`
  chains.

### Added
- **Cross-file member navigation slice** (#13): member access through typed bases and bare
  inherited identifiers now records `Binding::Use` against the *declaring* file —
  `references` on a signal declaration finds cross-file `obj.sig.emit(…)` sites, `definition`
  on a member-access jumps to the declaration, and hover renders `var`/`const` member shapes.
- **Zero-config native types** (#20): gdls now auto-dumps `extension_api.json` into `.gdls/`
  by running the user's Godot (`godotBinaryPath` → `GDLS_GODOT` → `godot4`/`godot` on PATH)
  with project context — which is what captures GDExtension classes — and keeps staleness
  metadata (binary identity + `.gdextension` set). Opt out with `autoDumpExtensionApi: false`.
  Resolution order: explicit `extensionApiPath` → fresh `.gdls` dump → auto-dump → stale dump →
  unmanaged `<root>/extension_api.json` → dynamic.
- `uid://` autoload script targets resolve through the sidecar map (#19) — definition,
  references, and singleton typing now work for `Name="*uid://…"` autoloads.
- The full-project diagnostics sweep is committed as a release gate
  (`scripts/m6-acceptance/scan_diags.py`) — a nav-row walk is not a diagnostics gate.

### Performance
- **Windows/NTFS startup** (#14): the 7–9 s window blamed on the reconcile walk was mostly the
  file watcher's `FileIdMap` arming scan (a full-tree, handle-per-file walk that exists only to
  pair rename events — which gdls already handles unpaired). The debouncer now runs `NoCache` on
  every platform; reconcile and the warm-load walk reuse the directory enumeration's own
  metadata (zero extra stats on Windows); and with the watcher armed *before* the workspace
  loads, the startup backstop runs in `DiscoverOnly` mode (enumeration-only for known files).

### Cache
- `CACHE_FORMAT_VERSION` 1 → 2 (the interface now carries enum values + unnamed-enum hoists).
  Old caches are ignored; expect one cold start after upgrading.

## [1.0.0] — 2026-06-10

The first release. **M6 has landed** — the ship bar raised at the close of M5 (exposed-capability
parity vs Godot's own LSP + a persistent warm-start cache) is met, and both final acceptance walks
(an OSS Godot 4.6.3 project and a Windows-native walk on a 2,338-script production project) came
back clean. Everything below is what v1.0.0 contains (M0–M6). Full M6 scope:
[`docs/08-m6-v1-ship.md`](docs/08-m6-v1-ship.md).

### Diagnostics
- Parser fidelity 1.0000 (186/186) on the vendored Godot 4.6.3-stable corpus.
- Analyzer fidelity 1.0000 (300/300).
- Full strict mode (45 active + 3 deprecated-gated warnings; godot/strict/off profiles).
- `NATIVE_METHOD_OVERRIDE` correctly exempts engine virtuals (`_ready`, `_process`, …): Godot
  only warns when a real `MethodBind` exists, so overriding a virtual is silent (Phase H fix —
  caught against the full `extension_api.json`, which the trimmed conformance fixture didn't cover).

### LSP surface (per docs/05 §1)
- publishDiagnostics (push, per-file)
- documentSymbol / workspaceSymbol
- definition / references / implementation
- prepareCallHierarchy / incomingCalls / outgoingCalls
- hover (with engine + GDExtension doc prose)

### CLI
- `gdls --version` / `-V` and `gdls --help` / `-h` (terminal probes that print and exit before any
  LSP traffic); `gdls diagnose --reconcile|--path-audit` and `gdls bench --record|--replay` as before.

### Hardening (M5)
- Soft / hard RSS budget enforcer (bench/budget.toml reference caps) with LRU cache eviction.
- $/cancelRequest with cooperative checkpoints.
- Fixpoint loop governor (analyzer_runaway diagnostic).
- tracing-based observability (GDLS_LOG / GDLS_TRACE env vars).
- Local-only differential check tool against godot on PATH (WP-D1; no CI).
- gdls bench --record / --replay local reproducer.
- Manual pre-release walk on a large real-world GDScript project with committed verification report (Phase H).

### Closed milestones
- M0 LSP skeleton
- M1 tokenizer + parser port
- M2 environment + indexing (native DB, project.godot, class_name registry)
- M3 analyzer + warning set + per-file diagnostics + hover + definition + strict mode
- M4 freshness watcher + 4 nav handlers + cross-file cycle detection
- M5 hardening, observability, parity gap closure

### M6 — landed (ships v1.0.0)

The raised ship bar is met (full design: [`docs/08-m6-v1-ship.md`](docs/08-m6-v1-ship.md)):
- **Exposed-capability parity vs Godot's own LSP** — `hover` renders member/call/`preload` signatures
  with parameter names (M6-F); `definition` resolves `class_name`-in-expression (M6-B), `preload`/`load`
  `res://` strings (M6-C1), and autoload names (M6-D); `documentLink` on resolving `res://` literals
  (M6-C2); project-wide `references` through typed vars (M6-E); hierarchical `documentSymbol` with a root
  `Class` (M6-A); `implementation` for method overrides across direct + transitive subclasses (M6-G).
  All res://-resolving navigation gates on index membership — never a link/Location to a file that isn't
  on disk ("never lie").
- **Autoload-singleton typing** — autoload names resolve to their script's instance type, so member
  access through a singleton (`Global.popup_error()`) gives the full hover signature and project-wide
  references, matching Godot (closes a parity gap the OSS acceptance walk surfaced; analyzer-level,
  ratchets unaffected).
- **Persistent warm-start index cache** — serde-serialized `Index` keyed by
  `(format_version, gdls_version, NativeDb::content_hash, project.godot fingerprint)` + a per-file
  `(size, mtime_ns)` table; atomic, multi-instance-safe (temp + rename, last-writer-wins, tolerant
  reads → cold fallback, never crashes) (M6-H / M6-I); `reconcile` re-parses only stat-changed files.
- **Attribute-fallback hover** — `func`/`signal` member signatures also render when the member
  identifier is *not* a call's callee: the signal in `Singleton.sig.emit(…)`/`obj.sig.connect(…)`
  and uncalled references like `var f = obj.method` (caught by the final acceptance walks; the
  Call-gated path alone left these on the degraded type label).
- **Verified (final acceptance):** the parameterized runner (`scripts/m6-acceptance/`) passes every
  capability row on a real Godot 4.6.3 OSS project (Pixelorama, committed session — including
  autoload-singleton member hover/references); a Windows-native walk on a 2,338-script production
  project returns correct data on every row with graceful degradation of doc-less GDExtensions and
  the cache stat-diffing `2338 unchanged, 0 reparsed` over NTFS; warm start **14.7×** faster than
  cold on the 3,000-file synthetic gate (>5× exit criterion); ratchets hold **1.0000 / 1.0000**
  (parser 186/186, analyzer 300/300).
- **Deferred to Phase 2** (documented, not regressions): cross-file instance-member typing for
  signal/var members through typed bases — references/definition on those *member-access* sites
  (#13; method calls and in-file signal uses work, and hover covers func/signal members via the
  attribute fallback); go-to-`definition` on a cross-file method *member* (#13); Windows/NTFS
  startup wall-clock dominated by the reconcile backstop walk, masking the warm-cache win there
  (#14); precise typing of `$`/`%` nodes; `completion`, `signatureHelp`, `rename`,
  `documentHighlight`.

---

**Tag conventions.** `v1.0.0` is tagged with M6 landed (the persistent warm-start cache ships *in* v1,
not later). Subsequent Godot-tracked re-ports become `v1.x.0` minor releases (Godot 4.8, 4.9, …). `v2.0.0`
is reserved for Phase 2 features (`.tscn` node typing for `$`/`%`, `signatureHelp`, `completion`).
