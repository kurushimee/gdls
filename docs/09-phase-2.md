# 09 — Phase 2: the generic GDScript language server (M7–M11)

| | |
|---|---|
| **Spec date** | 2026-06-12 |
| **Status** | Phase 2 definition — authoritative scope for everything post-v1.0.5. Supersedes the one-row "Phase 2" sketches in `docs/00 §3`, `docs/05 §1`, `docs/07 §1`, and `docs/08 §4`. **Progress (2026-06-15): M7–M11 shipped and closed (#57–#80); Phase 2 COMPLETE.** |
| **Baseline** | v1.0.5 (2026-06-12): both fidelity ratchets 1.0000, exposed-capability parity + LSP-convention audit complete. |
| **Tracking** | Umbrella issue [#30](https://github.com/kurushimee/gdls/issues/30); milestones **M7–M11** on GitHub; project board *gdls roadmap*. |

---

## 1. Goal and governing principle

Phase 1 made gdls a faithful diagnostics-and-navigation oracle for Claude Code, with
"exposed-capability parity vs Godot's own LSP" as the ship bar. **Phase 2 reframes that parity as
the problem** (issue #30): Godot's LSP is exotic — full functionality requires Godot-aware clients,
custom notifications, and a running editor. Phase 2 makes gdls function the way rust-analyzer
functions for Rust: a **complete, spec-conventional, generic language server** that any editor
(Helix, VS Code, Neovim, Zed, Emacs eglot, Sublime LSP) or agent uses fully **with zero
server-specific client code**.

Three design rules govern every Phase 2 feature:

1. **Generic first, complete on its own.** Every capability serves complete, spec-standard LSP to
   a client that knows nothing about Godot. Godot-specific data may ride **in addition to** generic
   output (standard fields, standard modifiers, `data` payloads) — never **instead of** it.
2. **No custom protocol.** No custom methods or notifications are required for full functionality.
   Godot's `gdscript/*` / `gdscript_client/*` extensions are a **permanent non-goal** (§3).
3. **Gate on client capabilities, degrade gracefully.** Every feature checks the exact
   `ClientCapabilities` path that guards it and omits or downgrades when absent (the v1.0.5
   hierarchical-`documentSymbol` and diagnostic-tag fixes are the template). This discipline — not
   any single feature — is what lets six different editors work unmodified.

Faithful-port discipline still applies to the **frontend** (tokenizer/parser/analyzer: fidelity
ratchets stay 1.0000 throughout Phase 2). The new Phase 2 surface (completion, semantic tokens,
code actions, …) is **server glue, not ported frontend**: Godot's `gdscript_editor.cpp`
(`complete_code`, `lookup_code`) serves as the *semantic reference* for what suggestions/answers
mean, but the wire shape follows LSP conventions and reference servers (rust-analyzer, gopls,
clangd), and none of it is under the conformance ratchet.

## 2. Baseline: what v1.0.5 already exposes

Advertised and implemented (see `crates/gd_server/src/server.rs` `capabilities()`):
incremental `textDocumentSync` · push `publishDiagnostics` (tags + `relatedInformation`,
capability-gated) · `hover` (native + project signatures) · `definition` (incl. native classes and
members → materialized API stubs; `preload` strings; autoloads) · `references`
(`includeDeclaration` honored) · `implementation` (subtypes + overrides) · `documentSymbol`
(hierarchical, flat fallback) · `workspace/symbol` (empty query = all, capped) · call hierarchy
(prepare/incoming/outgoing) · `documentLink` (`res://` strings) · `positionEncoding` negotiation
(UTF-8/16/32) · `$/cancelRequest` (responds `RequestCancelled`; no mid-handler preemption yet) ·
`shutdown`/`exit` lifecycle, stdio transport, `serverInfo`.

Everything in §5's matrix that is not in this list is Phase 2 scope or an explicit skip.

## 3. The anti-catalog: Godot-LSP weirdness gdls must never replicate

Catalogued from `modules/gdscript/language_server/` at `4.6.3-stable` (file:line refs are into
that tree). This is a **conformance checklist in reverse** — each row is something a Godot-aware
client must specially handle today, and that a generic gdls client must never need to.

| # | Godot's behavior (4.6.3-stable) | gdls rule |
|---|---|---|
| W1 | TCP-only (`127.0.0.1:6005`), runs inside the editor process; up to 8 clients with server-pushed traffic routed to the "latest" client (`gdscript_language_protocol.cpp:138-147,285`) | stdio, standalone binary, one client per session. Already the case; stays locked. |
| W2 | `Content-Length` must be the first header; 4 MiB hard message cap; oversize drops the TCP connection (`gdscript_language_protocol.cpp:55-116`) | Robust framing (any header order), no arbitrary message cap. |
| W3 | No `shutdown`/`exit` (→ `-32601`), no cancellation despite declaring `RequestCancelled`; responses to server-initiated requests are bounced as "Method not found" errors (`jsonrpc.cpp:98-162`) | Full lifecycle; request/response correlation; real cancellation (M7). |
| W4 | Custom server→client notifications required for core function: `gdscript/show_native_symbol` (native go-to-definition routes **out-of-band**; the LSP result is `[]`), `gdscript/capabilities`, `gdscript_client/changeWorkspace` — the last sent **before** the `initialize` response (`gdscript_language_protocol.cpp:219-264`, `gdscript_text_document.cpp:120-123`) | **None of these, ever.** Native navigation returns real `Location`s into materialized stubs (shipped v1.0.4). If a Godot-editor bridge is ever wanted, it is additive, opt-in, and advertised under `capabilities.experimental` — and nothing may depend on it. |
| W5 | `textDocument/nativeSymbol` — a custom request hiding under the standard namespace (`gdscript_language_protocol.cpp:574`) | No fake-namespaced methods. |
| W6 | Positions are UTF-32 columns, tab-expanded using the **editor's indent-size setting**; no `positionEncoding` negotiation (`gdscript_extend_parser.cpp:39-127`) | Byte offsets internally; exact negotiated UTF-8/16/32 at the boundary (shipped); column math never depends on user settings. |
| W7 | `ClientCapabilities` ignored entirely: markdown assumed, hierarchical `documentSymbol` assumed, snippets unannounced (`gdscript_language_protocol.cpp:185-247`) | Rule 3 above: gate everything. `MarkupContent` kind chosen from `contentFormat`/`documentationFormat`; plaintext fallback. |
| W8 | Raw **BBCode** leaks in `documentSymbol.documentation` and native-symbol payloads; converted only on some paths; docs localized via editor `DTR()` (`godot_lsp.h:1248-1273,1927-2125`, `gdscript_workspace.cpp:230`) | One BBCode→Markdown pipeline for **all** outgoing prose (§7.2); spec fields only; never localized engine-side. |
| W9 | Non-spec fields injected into standard responses (`native_class`, `documentation` on every symbol node) | Extensions ride only in `data` fields or documented `initializationOptions` — standard consumers see pure spec shapes. |
| W10 | "Smart resolve" (default **on**): failed resolution returns every same-named symbol project-wide as definitions/hovers, in the deprecated `MarkedString[]` shape (`gdscript_language_protocol.cpp:363-365,498-532`) | Resolution is semantic (binding-backed) or absent. No name-match guessing in standard methods (v1.0.5 #54 already enforced this for `references`). |
| W11 | `declaration` on a native symbol can open the doc page **inside the Godot editor and steal OS window focus** (`gdscript_text_document.cpp:463-467`) | Responses never have engine/editor side effects. |
| W12 | `didSave` hot-reloads the script into a running game; `willSaveWaitUntil` clears editor caches instead of returning edits (`gdscript_text_document.cpp:77-118`) | Document sync mutates server state only. |
| W13 | Advertises Full sync but applies only the **last** content change; unknown `languageId`s are silently dropped; stale diagnostics never cleared on close (`gdscript_language_protocol.cpp:433-496`) | Incremental sync applied exactly (shipped); diagnostics cleared on `didClose` (shipped). |
| W14 | Diagnostic ranges are whole trimmed lines; `code: -1`; warning names prefixed into the message text (`gdscript_workspace.cpp:578-593`) | Exact source ranges, string `code` = warning name, `tags`/`relatedInformation`/`codeDescription` as metadata (all shipped; per-code `codeDescription` anchors landed in M7 #63). |
| W15 | Advertised-but-broken capabilities (`documentOnTypeFormattingProvider`, `executeCommandProvider: {commands: []}` → `-32601`) and implemented-but-unadvertised stubs (`foldingRange`/`codeLens`/`colorPresentation` return `[]` while advertised `false`) (`godot_lsp.h:1720-1880`) | Advertise **exactly** what is implemented, nothing else. |
| W16 | Per-request whole-project text grep + reparse for `references`/`rename`; completion `instantiate()`s the owning PackedScene per request (`gdscript_workspace.cpp:433-487,621-677`) | Index-backed queries; scene knowledge comes from parsing `.tscn` **text** (M11), never engine instantiation. |
| W17 | Behavior silently varies with editor settings (`use_single_quotes` rewrites completion text, `add_type_hints`, indent size, smart-resolve) | All behavior from `initializationOptions`/`workspace/configuration` with documented defaults. |
| W18 | Completion `data` = the whole request params copied into every item; no `textEdit`/`sortText`/`filterText`/`insertTextFormat`; bare `CompletionItem[]`, never `CompletionList` (`gdscript_text_document.cpp:168-228`) | Completion conventions per §6-M8. |
| W19 | Workspace root hardwired to the editor's open project; client roots case-compared and "corrected" via custom notification | Root from `initializationOptions.projectRoot` → `workspaceFolders`/`rootUri` → nearest `project.godot` (shipped). `res://` is internal vocabulary only; the wire carries `file://` URIs (shipped). |
| W20 | End-inclusive `Range` semantics off-by-one vs spec (`godot_lsp.h:126-137`) | Spec ranges, end-exclusive, everywhere (shipped). |

## 4. Spec target and the highlighting reality

- **Conformance target: LSP 3.17 semantics.** 3.18 was finalized 2026-06-04, but no surveyed
  editor requires 3.18-only features; 3.17 is what all six clients speak today. gdls **parses**
  3.18 client capabilities tolerantly (unknown fields ignored — already guaranteed by serde
  defaults) and adopts 3.18 items only when listed in §5.
- **3.18 watch list (not scope):** `workspace/textDocumentContent` (virtual read-only docs — gdls'
  materialized stubs are strictly more generic, since they work in every client today),
  `textDocument/inlineCompletion`, `SnippetTextEdit` in workspace edits, `Diagnostic.message` as
  `MarkupContent`, `textDocument/rangesFormatting`.
- **Syntax highlighting is two layers, and a server only owns one.** Base highlighting in
  Helix/Zed comes from client-side tree-sitter grammars (`tree-sitter-gdscript`); semantic tokens
  are an *enhancement layer* that VS Code, Neovim (≥0.9, default-on), Zed (≥0.224, opt-in), eglot
  (≥1.20, default-on), and Sublime LSP consume — Helix currently does not (tree-sitter only;
  upstream discussion #5589). The user-facing promise "generic color schemes display GDScript
  correctly" is delivered by **emitting only the 23 standard token types and 10 standard
  modifiers** (§6-M10): every theme already maps those. Custom token names a theme must learn are
  exactly the Godot-weirdness this phase removes; if a refinement ever needs a custom type, it
  declares a standard fallback rust-analyzer-style and is emitted only when the client's legend
  capability lists it.

## 5. Capability matrix — complete LSP surface, disposition of every feature

Status: **✅ shipped** (v1.0.5 baseline) · **M7–M11** (Phase 2 milestone) · **skip** (explicit
non-goal with rationale). Every `textDocument/*`, `workspace/*`, `window/*` feature of LSP 3.17 (+
relevant 3.18) appears exactly once.

| Capability | Disposition | Notes |
|---|---|---|
| didOpen/didChange/didClose/didSave (incremental) | ✅ | |
| publishDiagnostics (push) | ✅ | Tags + relatedInformation + `codeDescription` gated (M7 #63: per-code ProjectSettings anchors) |
| hover · definition · references · implementation · documentSymbol · workspace/symbol · callHierarchy · documentLink · positionEncoding | ✅ | v1 surface, convention-audited in v1.0.5 |
| `$/cancelRequest` → true preemption | ✅ M7 | Router thread + cooperative checkpoints (#57); stale-by-edit → `ContentModified` |
| workDoneProgress (`window/workDoneProgress/create` + `$/progress`) | ✅ M7 | Cold index / warm-start / reconcile / re-index progress + client tokens on references/workspace-symbol (#58) |
| workspace/didChangeConfiguration + workspace/configuration | ✅ M7 | Runtime re-config; sparse payloads keep absent groups; structural fields warn + retain (#59) |
| workspace/didChangeWatchedFiles (dynamic registration) | ✅ M7 | The only dynamic registration gdls performs; Helix's only watch path. Client events merge with the native watcher; duplicate delivery deduped by content fingerprint (#60) |
| Pull diagnostics: `textDocument/diagnostic` | ✅ M7 | Same per-file computation as push (items byte-identical); unchanged via `version:hash:epoch:generation` resultId (#61) |
| `##` doc-comments + BBCode→Markdown pipeline | ✅ M7 | §7.2; feeds hover now, completion/signatureHelp in M8 (#62; ratchets untouched) |
| Diagnostic `codeDescription.href` | ✅ M7 | Capability-gated; per-code ProjectSettings anchors, deprecated trio → overview page (#63) |
| `window/showMessage` / `logMessage` conventions | ✅ M7 | Malformed runtime config surfaces as `showMessage(Warning)` (#59); startup diagnostics stay stderr logs by design (never log-spam) |
| textDocument/completion + completionItem/resolve | **M8** | The single biggest gap; conventions in §6-M8 |
| textDocument/signatureHelp | **M8** | Triggers `(` `,`; retrigger `)`; labelOffset-gated |
| textDocument/rename + prepareRename | **M9** | `documentChanges` + versioned edits; `{range, placeholder}` prepare |
| textDocument/documentHighlight | **M9** | Read/Write kinds from the binding graph |
| textDocument/declaration | **M9** | = definition target for GDScript (documented equivalence) |
| textDocument/typeDefinition | **M9** | Declared/inferred type → its declaring script / native stub |
| typeHierarchy prepare/supertypes/subtypes | **M9** | The `class_name` registry + extends graph *is* this data |
| textDocument/foldingRange | **M9** | AST blocks + `#region`/`#endregion` + comment runs, with kinds |
| textDocument/selectionRange | **M9** | AST ancestor chain |
| workspaceSymbol/resolve | **M9** | Lazy ranges per `docs/05 §1`; finish + advertise `resolveProvider` |
| textDocument/semanticTokens full/delta/range (+ refresh) | **M10** | Standard legend only; §6-M10 mapping table |
| textDocument/inlayHint (+ resolve, refresh) | **M10** | Inferred `:=` types, parameter names; config-toggleable |
| textDocument/documentColor + colorPresentation | **M10** | `Color(...)`, `Color.CONSTANT`, `Color("#hex")` literals |
| textDocument/codeAction (+ resolve, `source.fixAll`) | **M10** | Quickfixes for the warning set; standard kind strings; honors `context.only`; `workspace/applyEdit` + `executeCommand` plumbing |
| `.tscn` scene index → `$`/`%` typing | ✅ M11 | Valid `$`/`%` types as bare `NATIVE Node` — diagnostics converge with Godot (`docs/02 §11`) (#76). Precise scene-derived node facts (the scene index + resolution seam) drive NAVIGATION only — hover/definition/typeDefinition (#125) and completion — never the diagnostic path (they'd turn Godot-tolerated downcasts into false positives) |
| Scene-aware completion (node paths in `$`/`%`/`get_node`) | ✅ M11 | Extends M8 completion with the M11 scene index; no scene ⇒ empty, multi-scene ⇒ annotated union (#77) |
| Autoload scene typing (`uid://` → scene → root script) | ✅ M11 | The `gd_project` deferral (`model.rs` uid→scene lookup); scriptless root ⇒ bare `Node` floor (#78) |
| fileOperations: willRenameFiles (+ did\*) | ✅ M11 | Moving a `.gd`/`.tscn` returns `WorkspaceEdit` fixing `preload`/`load` paths (positively-identified args only); `did*` nudges the index (#79) |
| Formatting: external-command bridge | ✅ M11 | rust-analyzer pattern: no built-in formatter (none exists upstream to port); optional user-configured command (gdformat …) over stdin/stdout; `documentFormattingProvider` advertised **only when configured** (#80) |
| `workspace/workspaceFolders` (first folder = root) | ✅/skip | Single-root per session stays (one Godot project = one gdls); multi-root sessions are a documented non-goal — editors spawn one instance per workspace |
| workspace/diagnostic (project-wide pull) | skip | Conflicts with the per-file-diagnostics principle (`docs/00 §4`); clients fall back to per-file pull/push cleanly |
| textDocument/codeLens (+ refresh) | skip (revisit post-M10) | Low value for GDScript; not consumed by Helix/eglot; reference-count lenses can be added later without protocol risk |
| textDocument/linkedEditingRange | skip | HTML-tag-shaped; no GDScript construct benefits |
| textDocument/onTypeFormatting | skip | Only meaningful with a built-in formatter |
| textDocument/moniker | skip | LSIF plumbing; no editor consumes it |
| textDocument/inlineValue | skip | Debugger-coupled; gdls has no runtime by design |
| notebookDocument/* | skip | No GDScript notebook embedding exists |
| textDocument/inlineCompletion (3.18) | skip | AI-completion shaped; client/tooling territory |
| `source.organizeImports` | skip | GDScript has no imports |
| telemetry/event | skip | Conventionally unused by generic servers |
| Godot custom protocol (`gdscript/*`, `gdscript_client/*`, `textDocument/nativeSymbol`) | **never** | §3 W4/W5 |

## 6. Milestones

Phase 2 = **M7–M11**. Each milestone is independently shippable and independently useful; order is
by dependency and editor impact. Effort calibration: assume the M3 pattern (fidelity-bench-driven
follow-on WPs, ~3× the initial WP list — `docs/07 §2`) for M8 and M11; M7/M9/M10 are
glue/projection over existing structures, M6-sized.

Every milestone's exit additionally includes the §7.4 editor-matrix capability walk — all six
vendored client-capability profiles (Helix, VS Code, Neovim, Zed, eglot, Sublime) — over the
features that milestone ships. Editors named inside an exit criterion below are the *interactive*
smoke-checks for that milestone, not the full bar; the six-profile walk is.

### M7 — protocol foundations (de-weirding the session layer)

Concurrent request dispatch so `$/cancelRequest` actually preempts (cancelled → `RequestCancelled`,
stale-by-edit → `ContentModified`); `workDoneProgress` for index build/warm start;
`workspace/didChangeConfiguration` + `workspace/configuration`; dynamic `didChangeWatchedFiles`
registration when offered (client events merged with the native watcher — belt and suspenders);
pull diagnostics (`textDocument/diagnostic`, keep push); the documentation pipeline (§7.2: `##`
doc-comment extraction + BBCode→GFM, markupKind-gated); diagnostic `codeDescription.href`.
**Exit:** a Helix or Neovim session exercises cancellation under load without a stuck UI; progress
is visible during cold index; doc-comment prose renders in hover on all six clients; pull and push
diagnostics agree byte-for-byte.

### M8 — editing core: completion + signatureHelp

The long pole. Semantic reference: `gdscript_editor.cpp::complete_code`/`lookup_code` (what Godot
suggests where); wire conventions: rust-analyzer/gopls/clangd. Non-negotiables:

- **Response shape:** `CompletionList` (never bare array); `isIncomplete: true` only when
  truncating/re-ranking server-side; `itemDefaults` when the client's
  `completionList.itemDefaults` property list allows.
- **Insertion:** `textEdit` (single-line, containing the request position) preferred over
  `insertText`; `InsertReplaceEdit` only behind `insertReplaceSupport`; snippets
  (`InsertTextFormat.Snippet`, `${1:arg}`/`$0`) only behind `snippetSupport`, with call-argument
  placeholders configurable (default: parens + `$0`, gopls-style).
- **Ranking:** `sortText` = fixed-width lexicographic encoding of the rank (gopls `%05d` pattern);
  `filterText` aligned with the typed prefix.
- **Laziness:** `documentation`/`detail` resolved in `completionItem/resolve`; `data` is a
  compact self-sufficient key (file + symbol path), never the request params (§3 W18);
  `additionalTextEdits` only when listed in `resolveSupport.properties`.
- **Triggers:** `.` `$` `%` `"` `@` (non-identifier chars only). `commitCharacters` only behind
  `commitCharactersSupport`, suppressed in string/new-identifier contexts.
- **Kinds:** clamp `CompletionItemKind` to the client's `valueSet`.
- **signatureHelp:** triggers `(` `,`, retrigger `)`; `activeParameter` per spec;
  `[start,end)` label offsets behind `labelOffsetSupport`, substring labels otherwise; stable
  overload selection via `context.activeSignatureHelp`; `null` when none.

**Exit:** completion and signature help work unmodified in VS Code, Helix, Neovim, Zed, eglot,
Sublime; node-path items defer to M11 (no scene guessing); latency budget per `bench/budget.toml`
extended with completion p50/p99.

### M9 — navigation & refactoring completeness

`rename` + `prepareRename` (reuses the M6-E reference graph; `WorkspaceEdit.documentChanges` with
versioned `TextDocumentEdit`s; natives and stub files politely refused in `prepareRename`);
`documentHighlight` (Read/Write kinds); `declaration` (= definition, documented); `typeDefinition`;
`typeHierarchy` (supertypes/subtypes from the extends graph, natives via stubs); `foldingRange`;
`selectionRange`; `workspaceSymbol/resolve`. All are projections over existing index/binding
structures — M4/M6-shaped work. **Exit:** every navigation keybinding in Helix's space-mode and
Neovim's `gr*` family does something correct; rename round-trips on the acceptance projects with
zero stale-version edits.

### M10 — presentation: semantic tokens, hints, colors, actions

- **semanticTokens** (full + delta + range): standard legend only. Mapping:
  `class_name`/types → `class` (natives + `defaultLibrary`), enums → `enum`/`enumMember`,
  functions → `function`/`method` (+`static`), signals → `event`, annotations (`@export`, `@onready`,
  `@rpc`…) → `decorator`, `const` → `variable`+`readonly`, parameters → `parameter`, members →
  `property`, locals → `variable`, keywords/operators/literals only where the grammar layer is
  known-absent (`augmentsSyntaxTokens` respected). Modifiers: `declaration`, `definition`,
  `readonly`, `static`, `defaultLibrary`, `deprecated`.
- **inlayHint** (+resolve, refresh): inferred types on `:=` declarations, parameter names at call
  sites; both individually toggleable via configuration; off-by-default param-name hints for
  single-argument calls.
- **documentColor/colorPresentation** on `Color` literals — unusually high payoff for a game-dev
  language.
- **codeAction**: quickfixes paired to diagnostics via `Diagnostic.data` (e.g. UNUSED_\* →
  `_`-prefix; GET_NODE_DEFAULT_WITHOUT_ONREADY → add `@onready`; mechanical `@warning_ignore`
  insertion as an explicit, clearly-labeled action), `source.fixAll` aggregating safe fixes; exact
  standard kind strings; honor `context.only`; `codeAction/resolve` for lazy edits; requires
  `workspace/applyEdit` + minimal `executeCommand`.

**Exit:** generic themes color GDScript correctly via standard tokens on VS Code/Neovim/Zed/eglot;
zero custom token names on the wire; fix-all on save works in VS Code via standard
`editor.codeActionsOnSave`.

### M11 — scenes & file operations (the original "Phase 2" payload) — ✅ shipped 2026-06-15 (#76–#80)

`.tscn` text parsing (`tree-sitter-godot-resource` as format reference; no engine instantiation —
§3 W16) into a scene index keyed alongside the script index; valid `$Node`/`%Unique` type as bare
`NATIVE Node` (faithful to Godot's analyzer, `docs/02 §11`), so diagnostics converge — precise
scene-derived node types are deferred to a phase-3 navigation feature (precise hover/completion),
explicitly OUT of the diagnostic path because a precise type fed into the symmetric compatibility
checks would turn Godot-tolerated sibling downcasts into false positives; the resolution substrate
(scene index + `scene_node_facts` seam) is what those navigation surfaces read (#125 landed the
precise hover/definition/typeDefinition half in `gd_server::scene_nav`). Scene-aware node-path
completion; autoload `uid://` scene → root-script typing; `willRenameFiles` → `preload`/`load` path
edits (+`did*` index nudges); the external-formatter bridge. **Exit:** `$`/`%` diagnostics converge
with Godot on the acceptance projects (sweep gate per `scripts/m6-acceptance/scan_diags.py`,
comparative, both projects, Windows binary for the private one); renaming a script in VS Code's
explorer fixes every `preload` in-project.

## 7. Cross-cutting contracts

### 7.1 Capability gating table (the generic-client mechanism)

Every Phase 2 feature names its gate up front; absent gate ⇒ omit or downgrade, never assume.
Already shipped: `hierarchicalDocumentSymbolSupport`, `publishDiagnostics.tagSupport`,
`general.positionEncodings`. Phase 2 adds (non-exhaustive, per feature specs above):
`completionItem.{snippetSupport, insertReplaceSupport, resolveSupport.properties,
commitCharactersSupport, documentationFormat}`, `completionList.itemDefaults`,
`signatureHelp.signatureInformation.{documentationFormat, parameterInformation.labelOffsetSupport,
activeParameterSupport}`, `hover.contentFormat`, `codeAction.{codeActionLiteralSupport,
resolveSupport, dataSupport}`, `rename.prepareSupport`, `semanticTokens.{requests, tokenTypes,
tokenModifiers, augmentsSyntaxTokens}`, `inlayHint.resolveSupport`, `workspace.{configuration,
didChangeWatchedFiles.dynamicRegistration, applyEdit, workspaceEdit.documentChanges}`,
`window.workDoneProgress`, `textDocument.diagnostic`. Registration is **static** in
`InitializeResult` for everything except `workspace/didChangeWatchedFiles` (dynamic when offered —
the spec forbids double registration, and this is the one capability Helix only honors
dynamically).

### 7.2 Documentation pipeline

One converter, used by every prose-emitting feature (hover, completion docs, signatureHelp docs):
GDScript `##` doc comments and `extension_api.json` description fields (BBCode-flavored: `[b]`,
`[code]`, `[codeblock]`, `[method X]`, `[member X]`, `[param x]`, `[url]`, …) → **GitHub-Flavored
Markdown**, with class/member cross-references rendered as code spans (links where a stable target
exists — e.g. into materialized stubs). Output kind selected per the client's
`contentFormat`/`documentationFormat` order; plaintext fallback strips markup. Raw BBCode never
appears on the wire (§3 W8). Hover shape follows the rust-analyzer convention: fenced
` ```gdscript ` signature block, `---`, prose.

### 7.3 Performance and scale

Phase 2 features inherit the M5 discipline: per-feature latency rows added to `bench/budget.toml`
(completion is the critical one), memory-pressure ladder semantics extended (Hard pressure refuses
analysis-priced requests with `ContentModified`; parse/index-priced features — foldingRange,
selectionRange, semanticTokens/range — stay served), and `partialResultToken` streaming considered
for `references`/`workspace/symbol` at 10k-file scale (entire result via `$/progress`, empty final
response) — adopt only if acceptance-project latency demands it.

### 7.4 Testing

The fidelity ratchets cover the frontend and are untouched. Phase 2 adds: per-capability
protocol-shape tests over the in-memory `Connection` (the `lifecycle.rs` pattern) asserting both
the gated and ungated projections of every response; an editor-matrix smoke checklist (Helix,
VS Code, Neovim, Zed, eglot, Sublime — scripted via `scripts/lsp-poke.py` walks with each client's
real capability JSON, captured once and vendored); completion-semantics spot checks against the
Godot editor as differential oracle (manual, not CI — the editor's completion is not headless);
the scene-typing convergence sweep (M11 exit). Fuzzing extends to the `.tscn` parser (M11) under
the same any-panic-blocks-release rule.

**Process note (2026-06-13):** the per-milestone *interactive* smoke checks are batched into
one end-of-Phase-2 trial — the user observes the finished tool in real work; they perform no
per-milestone captures or test-rig steps. Editor capability captures are done headlessly as
each becomes feasible (`crates/gd_server/tests/fixtures/client_caps/README.md` tracks the
inventory and this machine's gaps). Deferred interactive items accumulate here per milestone:

- **M7:** cancellation under editor load with no stuck UI; cold-index progress spinner
  visibility; `##` doc prose rendering in hover across editors; pull-vs-push diagnostics
  agreement observed live.
- **M8:** completion popup feel (trigger timing, ranking, snippet placeholder navigation) and
  `signatureHelp` popup behavior (active-parameter tracking, retrigger) in real editing across
  the six editors. Three capability profiles (helix/neovim/zed) are walked headlessly in
  `tests/editor_profiles.rs`; **vscode/eglot/sublime remain machine-capture gaps** (not
  installed — see `client_caps/README.md`), so their completion/signatureHelp projections are
  the deferred items here (tracked: #98). The Godot-editor completion differential (member/
  identifier/type contexts) was run headlessly and agreed 99.6% on member access — but a *human*
  feel-check of ranking/snippet ergonomics in real editing is still the end-of-phase trial.
  Post-M8 polish/completeness follow-ups are filed as #96 (completion contexts — incl. the
  native-only `OVERRIDE_METHOD` stubs, renderable from `Interface.param_names`), #97 (signatureHelp
  — lambdas, script-method docs, constructor overload args), and #99 (perf-budget recalibration).

## 8. Phase 2 exit criteria

**Status (2026-06-15): all met — Phase 2 COMPLETE.** Exit #4 is bare-Node `$`/`%` parity (the
premise correction in M11 #76), verified by the comparative `--strict` `scan_diags.py` sweeps on
both acceptance projects (zero new default-profile false positives; the `--strict` increase is the
convergent bare-Node `UNSAFE_*`-on-node-access family Godot's analyzer also emits); both ratchets
hold 1.0000 and the five-target fuzz gate (now including `scene_parse`) is clean.

1. Every capability in §5 marked M7–M11 is shipped, advertised exactly, and capability-gated.
2. A stock Helix, VS Code (generic LSP client), Neovim, Zed, eglot, and Sublime LSP session gets
   the full feature set with **zero gdls-specific client configuration** beyond launching the
   binary (and a tree-sitter grammar for base highlighting where the editor requires one).
3. Zero custom methods/notifications on the wire; zero non-spec fields outside `data`; the §3
   anti-catalog holds row by row.
4. `$`/`%` diagnostics converge with Godot: a valid `$`/`%` types as bare `NATIVE Node`
   (`docs/02 §11`), with no false positives on the sibling/subtype downcasts Godot tolerates.
   Precise scene-derived node types are navigation-only (phase 3), explicitly out of the diagnostic
   path. (Member-access convergence is property-access complete; `UNSAFE_METHOD_ACCESS` on a
   method miss is a pre-existing analyzer-wide under-emission tracked in #123, not `$`-specific.)
   Full convergence is adjudicated by the comparative `--strict` acceptance sweep on both projects
   (the standing release gate), which may surface further follow-ups.
5. Both fidelity ratchets still 1.0000; both acceptance-project sweeps clean under the standing
   comparative gate; fuzz gate green including the `.tscn` target.

## 9. Sources

- LSP 3.17 spec — https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/
- LSP 3.18 spec (finalized 2026-06-04) — https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/
- Godot LSP source audited at `4.6.3-stable` — `modules/gdscript/language_server/` (local checkout)
- rust-analyzer token fallbacks — https://github.com/rust-lang/rust-analyzer/blob/master/crates/rust-analyzer/src/lsp/semantic_tokens.rs
- gopls completion/sortText — https://github.com/golang/tools/blob/master/gopls/internal/server/completion.go
- clangd features — https://clangd.llvm.org/features
- Helix LSP client surface — https://github.com/helix-editor/helix/blob/master/helix-lsp/src/client.rs · semantic-tokens stance: https://github.com/helix-editor/helix/discussions/5589
- Neovim LSP — https://neovim.io/doc/user/lsp.html (news-0.11/0.12)
- Zed semantic tokens — https://zed.dev/docs/semantic-tokens · eglot — https://github.com/emacs-mirror/emacs/blob/master/etc/EGLOT-NEWS · Sublime LSP — https://lsp.sublimetext.io/features/
- GDScript formatter landscape — https://github.com/Scony/godot-gdscript-toolkit · https://github.com/GDQuest/GDScript-formatter · proposal https://github.com/godotengine/godot-proposals/issues/3630
- tree-sitter grammars — https://github.com/PrestonKnopp/tree-sitter-gdscript · https://github.com/PrestonKnopp/tree-sitter-godot-resource
