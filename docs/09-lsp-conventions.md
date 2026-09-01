# 09. The generic-LSP contract

Godot's own language server is exotic. Full functionality requires Godot-aware clients, custom notifications, and a running editor. gdls works the way rust-analyzer works for Rust instead: a complete, spec-conventional language server that any editor (Helix, VS Code, Neovim, Zed, Emacs eglot, Sublime LSP) or agent uses fully, with zero server-specific client code.

This document is the contract that keeps it that way.

## 1. Governing rules

1. **Generic first, complete on its own.** Every capability serves complete, spec-standard LSP to a client that knows nothing about Godot. Godot-specific data may ride *in addition to* generic output (standard fields, standard modifiers, `data` payloads), never *instead of* it.
2. **No custom protocol.** No custom method or notification is required for full functionality. Godot's `gdscript/*` and `gdscript_client/*` extensions are a permanent non-goal (§3).
3. **Gate on client capabilities, degrade gracefully.** Every feature checks the exact `ClientCapabilities` path that guards it, and omits or downgrades when it is absent. This discipline, not any single feature, is what lets six different editors work unmodified.

Faithful-port discipline governs the frontend; it does not govern this layer. The tokenizer, parser, and analyzer hold their fidelity ratchets at 1.0000, and everything in this document is server glue outside the ratchet. Godot's `gdscript_editor.cpp` (`complete_code`, `lookup_code`) is the semantic reference for what a suggestion or an answer *means*, but the wire shape follows LSP conventions and the reference servers (rust-analyzer, gopls, clangd).

## 2. Godot's own LSP, as reference

Audited in `modules/gdscript/language_server/` at `4.6.3-stable`. This is the baseline gdls measured itself against, and it explains why several gdls capabilities have no parity bar at all: Godot does not advertise them.

4.7 reworked the module (~670 lines added, ~475 removed) without moving the parity bar in any row below. Two changes are worth naming: `documentHighlightProvider` flipped from `false` to `true`, and `initialize` now reads exactly one entry out of `ClientCapabilities`, `completion.completionItem.snippetSupport`, to decide whether brace completion emits a snippet. The rest is a new scene cache and internal restructuring. Neither change touches the anti-catalog in §3: reading one capability path is not gating on capabilities (W7), and `documentHighlight` was already a gdls capability. Nothing here has bearing on gdls's own dialect support, which is a frontend concern; this module is not ported.

| Capability | Godot's own LSP | gdls |
|---|---|---|
| `hover` | signature, doc, and a "Defined in" link, including member and method signatures (`text_document.cpp:347`, `godot_lsp.h:1284`) | full signatures for members, calls, and `preload`, plus doc prose |
| `definition` | `Location` at the symbol; resolves `class_name` in expression position via `is_global_class` (`workspace.cpp:650`) | in-file members, cross-file classes, `class_name` in expression position, `preload`/`load` strings, autoloads, and native symbols — engine classes, Variant types, and global utilities alike — into materialized stubs |
| preload `res://` navigation | via `documentLink` on any file-resolving string literal (`extend_parser.cpp:186-215`), not `definition` | both: `documentLink` on `res://` literals, and `definition` inside the string |
| autoload navigation | no dedicated handling | resolves the singleton name to its script |
| `references` | project-wide textual name scan plus per-hit re-resolve (`workspace.cpp:472`) | index-backed and binding-correct, including cross-file member and signal call sites through typed vars |
| `documentSymbol` | hierarchical: root Class then members, inner classes nested (`text_document.cpp:139`) | hierarchical, with a flat fallback for clients that do not advertise hierarchy support |
| `implementation` | not supported (`implementationProvider=false`, `godot_lsp.h:1768`) | subtypes and method overrides |
| `callHierarchy` | not supported | full prepare, incoming, outgoing |
| `workspace/symbol` | not supported (`workspaceSymbolProvider=false`) | fuzzy-ranked, with lazy `workspaceSymbol/resolve` |
| `completion` / `signatureHelp` | supported | supported, per §6.1 and §6.2 |
| `rename`, `documentHighlight`, `declaration`, `onTypeFormatting` | supported; `documentHighlight` was advertised only from 4.7 | all but `onTypeFormatting`, which is meaningless without a built-in formatter |

## 3. The anti-catalog: Godot-LSP behavior gdls must never replicate

Catalogued from `modules/gdscript/language_server/` at `4.6.3-stable`, where the file and line references point. This is a conformance checklist in reverse: each row is something a Godot-aware client must specially handle, and that a generic gdls client must never need to.

| # | Godot's behavior (4.6.3-stable) | gdls rule |
|---|---|---|
| W1 | TCP-only (`127.0.0.1:6005`), running inside the editor process; up to 8 clients, with server-pushed traffic routed to the "latest" client (`gdscript_language_protocol.cpp:138-147,285`) | stdio, standalone binary, one client per session. |
| W2 | `Content-Length` must be the first header; a 4 MiB hard message cap; oversize drops the TCP connection (`gdscript_language_protocol.cpp:55-116`) | Robust framing in any header order, and no arbitrary message cap. |
| W3 | No `shutdown` or `exit` (they return `-32601`), and no cancellation despite declaring `RequestCancelled`; responses to server-initiated requests are bounced back as "Method not found" errors (`jsonrpc.cpp:98-162`) | Full lifecycle, request and response correlation, and real preempting cancellation. |
| W4 | Custom server-to-client notifications required for core function: `gdscript/show_native_symbol` (native go-to-definition routes out of band and the LSP result is `[]`), `gdscript/capabilities`, and `gdscript_client/changeWorkspace`, the last sent *before* the `initialize` response (`gdscript_language_protocol.cpp:219-264`) | None of these, ever. Native navigation returns real `Location`s into materialized stubs. If a Godot-editor bridge is ever wanted, it is additive, opt-in, advertised under `capabilities.experimental`, and nothing may depend on it. |
| W5 | `textDocument/nativeSymbol`, a custom request hiding under the standard namespace (`gdscript_language_protocol.cpp:574`) | No fake-namespaced methods. |
| W6 | Positions are UTF-32 columns, tab-expanded using the editor's indent-size setting, with no `positionEncoding` negotiation (`gdscript_extend_parser.cpp:39-127`) | Byte offsets internally, exact negotiated UTF-8/16/32 at the boundary. Column math never depends on user settings. |
| W7 | `ClientCapabilities` all but ignored: markdown assumed, hierarchical `documentSymbol` assumed, snippets unannounced (`gdscript_language_protocol.cpp:185-247`; 4.7 reads exactly one path, `completion.completionItem.snippetSupport`) | Rule 3: gate everything. `MarkupContent` kind is chosen from `contentFormat`/`documentationFormat`, with a plaintext fallback. |
| W8 | Raw BBCode leaks in `documentSymbol.documentation` and native-symbol payloads, converted only on some paths; docs localized via editor `DTR()` (`godot_lsp.h:1248-1273,1927-2125`) | One BBCode-to-Markdown pipeline for all outgoing prose (§7.2), spec fields only, never localized engine-side. |
| W9 | Non-spec fields injected into standard responses (`native_class`, `documentation` on every symbol node) | Extensions ride only in `data` fields or documented `initializationOptions`, so standard consumers see pure spec shapes. |
| W10 | "Smart resolve", on by default: failed resolution returns every same-named symbol project-wide as definitions and hovers, in the deprecated `MarkedString[]` shape (`gdscript_language_protocol.cpp:363-365,498-532`) | Resolution is semantic and binding-backed, or absent. No name-match guessing in standard methods. |
| W11 | `declaration` on a native symbol can open the doc page inside the Godot editor and steal OS window focus (`gdscript_text_document.cpp:463-467`) | Responses never have engine or editor side effects. |
| W12 | `didSave` hot-reloads the script into a running game; `willSaveWaitUntil` clears editor caches instead of returning edits (`gdscript_text_document.cpp:77-118`) | Document sync mutates server state only. |
| W13 | Advertises Full sync but applies only the *last* content change; unknown `languageId`s are silently dropped; stale diagnostics are never cleared on close (`gdscript_language_protocol.cpp:433-496`) | Incremental sync applied exactly; diagnostics cleared on `didClose`. |
| W14 | Diagnostic ranges are whole trimmed lines; `code: -1`; warning names prefixed into the message text (`gdscript_workspace.cpp:578-593`) | Exact source ranges, a string `code` equal to the warning name, and `tags`, `relatedInformation`, and `codeDescription` as metadata. |
| W15 | Advertised-but-broken capabilities (`documentOnTypeFormattingProvider`, `executeCommandProvider: {commands: []}` giving `-32601`) and implemented-but-unadvertised stubs (`foldingRange`, `codeLens`, `colorPresentation` return `[]` while advertised `false`) (`godot_lsp.h:1720-1880`) | Advertise exactly what is implemented, nothing else. `executeCommandProvider` lists the one real command, `gdls.applyWarningIgnore`. `documentFormattingProvider` appears only when a formatter is configured. |
| W16 | Per-request whole-project text grep plus reparse for `references` and `rename`; completion `instantiate()`s the owning PackedScene per request (`gdscript_workspace.cpp:433-487,621-677`) | Index-backed queries. Scene knowledge comes from parsing `.tscn` *text*, never engine instantiation. |
| W17 | Behavior silently varies with editor settings: `use_single_quotes` rewrites completion text, plus `add_type_hints`, indent size, and smart-resolve | All behavior comes from `initializationOptions` or `workspace/configuration`, with documented defaults. gdls owns the canonical style rather than reading the editor's settings. |
| W18 | Completion `data` is the whole request params copied into every item; no `textEdit`, `sortText`, `filterText`, or `insertTextFormat`; a bare `CompletionItem[]`, never a `CompletionList` (`gdscript_text_document.cpp:168-228`) | The completion conventions in §6.1. |
| W19 | Workspace root hardwired to the editor's open project; client roots case-compared and "corrected" via a custom notification | Root comes from `initializationOptions.projectRoot`, then `workspaceFolders`/`rootUri`, then the nearest `project.godot`. `res://` is internal vocabulary only, and the wire carries `file://` URIs. |
| W20 | End-inclusive `Range` semantics, off by one against the spec (`godot_lsp.h:126-137`) | Spec ranges, end-exclusive, everywhere. |

## 4. Spec target, and the highlighting reality

**The conformance target is LSP 3.17 semantics.** 3.18 was finalized 2026-06-04, but no surveyed editor requires 3.18-only features, and 3.17 is what all six clients speak. gdls parses 3.18 client capabilities tolerantly, ignoring unknown fields, which serde defaults already guarantee, and adopts a 3.18 item only when §5 lists it.

**3.18 watch list, not scope:** `workspace/textDocumentContent` (virtual read-only docs, where gdls's materialized stubs are strictly more generic since they work in every client today), `textDocument/inlineCompletion`, `SnippetTextEdit` in workspace edits, `Diagnostic.message` as `MarkupContent`, and `textDocument/rangesFormatting`.

**Syntax highlighting is two layers, and a server only owns one.** Base highlighting in Helix and Zed comes from client-side tree-sitter grammars (`tree-sitter-gdscript`). Semantic tokens are an enhancement layer that VS Code, Neovim (0.9+, default on), Zed (0.224+, opt in), eglot (1.20+, default on), and Sublime LSP consume; Helix does not, using tree-sitter only (upstream discussion #5589). "Generic color schemes display GDScript correctly" is delivered by emitting only standard token types and modifiers (§6.5), since every theme already maps those. A custom token name a theme has to learn is exactly the weirdness this contract removes. If a refinement ever needs one, it declares a standard fallback rust-analyzer-style and is emitted only when the client's legend capability lists it.

## 5. Capability matrix

Every `textDocument/*`, `workspace/*`, and `window/*` feature of LSP 3.17, plus the relevant 3.18 ones, appears exactly once. A **skip** row is an explicit non-goal with its rationale, not an omission.

| Capability | Disposition | Notes |
|---|---|---|
| didOpen/didChange/didClose/didSave (incremental) | served | |
| publishDiagnostics (push) | served | Tags, `relatedInformation`, and `codeDescription` capability-gated |
| Pull diagnostics: `textDocument/diagnostic` | served | The same per-file computation as push, with byte-identical items; unchanged via a `version:hash:epoch:generation` resultId |
| hover · definition · declaration · typeDefinition · references · implementation · documentHighlight · documentSymbol · workspace/symbol · workspaceSymbol/resolve · documentLink | served | |
| callHierarchy (prepare, incoming, outgoing) · typeHierarchy (prepare, supertypes, subtypes) | served | The `class_name` registry plus extends graph *is* the type-hierarchy data |
| completion plus completionItem/resolve · signatureHelp | served | §6.1, §6.2 |
| rename plus prepareRename | served | §6.3 |
| foldingRange · selectionRange | served | AST blocks, `#region`/`#endregion`, and comment runs, with kinds; the AST ancestor chain |
| semanticTokens full/delta/range (plus refresh) | served | Standard legend only; §6.5 |
| inlayHint (plus resolve, refresh) | served | Inferred `:=` types and parameter names; config-toggleable |
| documentColor plus colorPresentation | served | `Color(...)`, `Color.CONSTANT`, and `Color("#hex")` literals |
| codeAction (plus resolve, `source.fixAll`) | served | Quickfixes for the warning set; standard kind strings; honors `context.only` |
| `$/cancelRequest` with true preemption | served | Router thread plus cooperative checkpoints; stale-by-edit gives `ContentModified` |
| workDoneProgress (`window/workDoneProgress/create` plus `$/progress`) | served | Cold index, warm start, reconcile, re-index, plus client tokens on references and workspace-symbol |
| workspace/didChangeConfiguration plus workspace/configuration | served | Runtime re-config; sparse payloads keep absent groups; structural fields warn and retain |
| workspace/didChangeWatchedFiles (dynamic registration) | served | The only dynamic registration gdls performs, and Helix's only watch path. Client events merge with the native watcher, deduped by content fingerprint. §7.1 |
| fileOperations: willRenameFiles (plus did\*) | served | §6.7 |
| Formatting: external-command bridge | served | §6.6 |
| `window/showMessage` and `logMessage` | served | Malformed runtime config surfaces as `showMessage(Warning)`; startup diagnostics stay stderr logs by design, so no log spam |
| `workspace/workspaceFolders` | first folder only | Single-root per session: one Godot project is one gdls. Multi-root sessions are a documented non-goal, and editors spawn one instance per workspace |
| workspace/diagnostic (project-wide pull) | skip | Conflicts with the per-file-diagnostics principle (`00-overview.md` §4); clients fall back to per-file pull or push cleanly |
| textDocument/codeLens (plus refresh) | skip | Low value for GDScript, and not consumed by Helix or eglot. Reference-count lenses could be added later without protocol risk |
| textDocument/linkedEditingRange | skip | HTML-tag-shaped; no GDScript construct benefits |
| textDocument/onTypeFormatting | skip | Only meaningful with a built-in formatter, and there is none to port |
| textDocument/moniker | skip | LSIF plumbing; no editor consumes it |
| textDocument/inlineValue | skip | Debugger-coupled, and gdls has no runtime by design |
| notebookDocument/* | skip | No GDScript notebook embedding exists |
| textDocument/inlineCompletion (3.18) | skip | AI-completion shaped; client and tooling territory |
| `source.organizeImports` | skip | GDScript has no imports |
| telemetry/event | skip | Conventionally unused by generic servers |
| Godot custom protocol (`gdscript/*`, `gdscript_client/*`, `textDocument/nativeSymbol`) | never | §3 W4 and W5 |

## 6. Feature conventions

### 6.1 Completion

- **Response shape.** A `CompletionList`, never a bare array. `isIncomplete: true` only when truncating or re-ranking server-side. `itemDefaults` when the client's `completionList.itemDefaults` property list allows it.
- **Insertion.** `textEdit` (single-line, containing the request position) preferred over `insertText`. `InsertReplaceEdit` only behind `insertReplaceSupport`. Snippets (`InsertTextFormat.Snippet`, `${1:arg}`, `$0`) only behind `snippetSupport`, with call-argument placeholders configurable and defaulting to parens plus `$0`, the gopls style.
- **Ranking.** `sortText` is a fixed-width lexicographic encoding of the rank, the gopls `%05d` pattern. `filterText` aligns with the typed prefix.
- **Laziness.** `documentation` and `detail` resolve in `completionItem/resolve`. `data` is a compact self-sufficient key (file plus symbol path), never the request params (§3 W18). `additionalTextEdits` only when listed in `resolveSupport.properties`.
- **Triggers.** `.` `$` `%` `"` `@`, non-identifier characters only. `commitCharacters` only behind `commitCharactersSupport`, suppressed in string and new-identifier contexts.
- **Kinds.** `CompletionItemKind` is clamped to the client's `valueSet`.

Node-path completion inside `$`, `%`, and `get_node` reads the scene index. No scene gives an empty result rather than a guess; multiple attaching scenes give an annotated union.

### 6.2 Signature help

Triggers on `(` and `,`, retriggers on `)`. `activeParameter` per spec. `[start,end)` label offsets behind `labelOffsetSupport`, substring labels otherwise. Stable overload selection via `context.activeSignatureHelp`. `null` when there is no signature, never an empty shell.

### 6.3 Rename and mutating edits

`rename` returns `WorkspaceEdit.documentChanges` with versioned `TextDocumentEdit`s. `prepareRename` returns `{range, placeholder}`, and politely refuses natives and stub files rather than offering an edit that cannot apply.

**Mutating consumers need their own firewall.** `references` and `definition` are read-tuned, and their inaccuracies become silent source corruption under `rename`, `codeAction` edits, `willRenameFiles`, and autoload renames. The pattern that holds: a fail-closed positive-project-resolution gate, binding-correct collection (never name-only), and refusing outright rather than half-applying. Widening a *candidate set* is safe; widening what is *collected inside* one is not.

`super.X()` and bare `super()` name the PARENT's `X`, and the only record of that is the call binding `reduce_call`'s super branch resolved against the parent chain — the analyzer never resolves a super callee in the current scope, and neither may the server. Projecting anything else answers an override's `super.describe()` with the override itself, which under `rename` rewrites the call into one the parent cannot answer. The invariant: a `super.X()` site is edited exactly when the declaration its call binding resolves to is edited.

A mutating *recipe* — a `codeAction` payload whose edit is rebuilt later, at `codeAction/resolve` or through `workspace/executeCommand` — carries the buffer version its refuse-gate ran against, and a version mismatch refuses with `ContentModified`. A line number is a coordinate valid in one snapshot only, so the version is half the recipe's identity; without it the splice lands wherever that line drifted to, which for the warning-ignore quickfix meant inside a multi-line call. Re-deriving the target inside `resolve` is not the fix: it can land on a different, valid target the offer-time gate never saw. The client's remedy is to re-request `codeAction`, which re-runs the whole gate against the current text.

A method override chain is ONE symbol, and `rename` edits the whole group: the base, every subclass override, and every resolved call site of every one of them. GDScript overriding is purely name-based, so the name IS the binding — renaming a single link corrupts in both directions, silently un-overriding the subclass (its calls start dispatching to the base with no diagnostic) or orphaning the override and dangling its `super.X()`. The group is collected binding-correctly: up-walk the base chain to the last ancestor declaring the name, then take every class whose OWN chain reaches that root and declares it. A name-seeded sweep is the widened candidate set, which is safe; the per-candidate chain walk is what keeps the collection correct. A chain whose native root declares the name refuses at every cursor in it — the rename-side twin of `NATIVE_METHOD_OVERRIDE` — and so does a chain that cannot be walked to a definite root, since then the group's membership is genuinely unknowable.

A refusal is the right answer only when the identity itself is in doubt, never as a stand-in for a collector that cannot reach across files. An enum value is the case that made the difference: its identity is the triple `(declaring file, class path, "Enum.VALUE")`, which is file-independent, so a click on `Lib.Dir.NORTH` in another file names exactly the same thing as a click on the declaration and renames the same two sites — even where a `const NORTH` or a `class_name Idle` shares the bare name, since neither records that binding. Refusing those clicks, as gdls did before, only hid a collector that was keyed on an in-tree `NodeId`.

### 6.4 Inlay hints and colors

`inlayHint` covers inferred types on `:=` declarations and parameter names at resolved call sites, each independently toggleable, with parameter hints suppressed on single-argument calls regardless. With `inlayHint.resolveSupport` the tooltip is pulled lazily via a `data` blob; without it hints arrive complete. The `textEdit` is always eager, so applying a hint never needs a resolve round-trip.

`documentColor` and `colorPresentation` cover `Color(...)`, `Color.CONSTANT`, and `Color("#hex")` literals. Both are parse-priced, so they stay served under Hard memory pressure.

### 6.5 Semantic tokens

Standard legend only, zero custom names. The legend is advertised at full width always, so delta correlation keeps stable wire indices; per-client intersection happens at emit time, never by shrinking the advertised legend.

Mapping: `class_name` and types to `class` (natives get `defaultLibrary`), enums to `enum` and `enumMember`, functions to `function` and `method` (plus `static`), signals to `event`, annotations (`@export`, `@onready`, `@rpc`, and so on) to `decorator`, `const` to `variable` plus `readonly`, parameters to `parameter`, members to `property`, locals to `variable`. Keywords, operators, and literals are emitted only where the grammar layer is known-absent, respecting `augmentsSyntaxTokens`. Modifiers: `declaration`, `definition`, `readonly`, `static`, `defaultLibrary`, and `deprecated`, the last read from `## @deprecated` (§7.2).

`full` and `full/delta` are analysis-priced and shed at Hard memory pressure; `range` is parse-priced and stays served.

### 6.6 Formatting

The rust-analyzer pattern: no built-in formatter, since there is none upstream to port. An optional user-configured command (gdformat, say) receives the document on stdin and must write the formatted document to stdout and exit 0. Any other outcome yields no edits, so the buffer is never touched by a failed format.

There is no shell in the path. `command` is an executable name or path and `args` is its argument vector, passed straight to `std::process::Command`, so neither can be interpreted as a shell expression. A user who wants a pipeline configures a wrapper script as `command`. `documentFormattingProvider` is advertised only when a command is configured (§3 W15).

### 6.7 File operations

Moving a `.gd` or `.tscn` returns a `WorkspaceEdit` fixing `preload` and `load` paths, for positively-identified argument literals only, never every string that happens to look like a path. The `did*` notifications nudge the index.

### 6.8 Scenes

`.tscn` files are parsed as text, never instantiated (§3 W16), into a scene index keyed alongside the script index. It feeds node-path completion and the precise `$`/`%` navigation types, and it never enters the diagnostic path. The full reasoning is in `02-frontend-port.md` §11.

## 7. Cross-cutting contracts

### 7.1 Capability gating

Every feature names its gate. An absent gate means omit or downgrade, never assume.

The gates in use, non-exhaustively: `textDocument.documentSymbol.hierarchicalDocumentSymbolSupport`, `publishDiagnostics.tagSupport`, `general.positionEncodings`, `completionItem.{snippetSupport, insertReplaceSupport, resolveSupport.properties, commitCharactersSupport, documentationFormat, tagSupport}`, `completionList.itemDefaults`, `signatureHelp.signatureInformation.{documentationFormat, parameterInformation.labelOffsetSupport, activeParameterSupport}`, `hover.contentFormat`, `codeAction.{codeActionLiteralSupport, resolveSupport, dataSupport}`, `rename.prepareSupport`, `semanticTokens.{requests, tokenTypes, tokenModifiers, augmentsSyntaxTokens}`, `inlayHint.resolveSupport`, `workspace.{configuration, didChangeWatchedFiles.dynamicRegistration, applyEdit, workspaceEdit.documentChanges, symbol.resolveSupport}`, `window.workDoneProgress`, and `textDocument.diagnostic`.

Registration is static in `InitializeResult` for everything except `workspace/didChangeWatchedFiles`, which is dynamic when offered. The spec forbids double registration, and this is the one capability Helix honors only dynamically.

**Watch breadth is a trade, decided per session.** The registered globs always include the script, scene, and engine-managed files (`**/*.gd`, `**/*.tscn`, `**/project.godot`, `**/*.gdextension`, `**/extension_api.json`, `**/doc_classes/*.xml`). Those are few files, and a duplicate delivery costs one content-fingerprint comparison.

A seventh glob, the `**/*` catch-all, is conditional. It exists because arbitrary project assets are defined by exclusion, meaning everything that is not a script, scene, or engine-managed file, so no positive extension allowlist can express the set the asset index covers. Without it, a client whose only freshness channel is `didChangeWatchedFiles` never learns about a new `icon.png`, leaving `load` and `preload` completion stale until a restart.

But it asks the client to watch the whole workspace: `.git/`, `.import/`, `build/`, exported binaries. On a large project that means a great many inotify handles and a steady stream of notifications the server discards, and some clients cap or warn on watcher breadth. So gdls registers it only when it buys something, which is when gdls's own filesystem watcher failed to arm. When that watcher is live it already reports asset create and delete, and the catch-all is pure client-side cost. `classify_client_event` re-applies the same server-side `is_excluded` filter on both paths, so the two converge to identical semantics; what differs is only who pays for the delivery.

### 7.2 Documentation pipeline

One converter, used by every prose-emitting feature (hover, completion docs, signature-help docs). GDScript `##` doc comments and `extension_api.json` description fields are BBCode-flavored (`[b]`, `[code]`, `[codeblock]`, `[method X]`, `[member X]`, `[param x]`, `[url]`, and so on) and convert to GitHub-Flavored Markdown, with class and member cross-references rendered as code spans, or as links where a stable target exists, such as into materialized stubs. Output kind is selected per the client's `contentFormat`/`documentationFormat` order, and the plaintext fallback strips markup. Raw BBCode never appears on the wire (§3 W8). The hover shape follows the rust-analyzer convention: a fenced ` ```gdscript ` signature block, `---`, then prose.

Every declaration that can carry a `##` block reaches every prose surface, not only `func`: the file's own head class (brief, long form, and `@tutorial` links) on its `class_name` and on every cross-file use of that name; named enums and their values; and `var`, `const`, `signal`, and inner `class` at a cross-file *use* site, not only at the declaration.

A signature is spelled one way. The parameter list between the parentheses comes from a single builder, so a declaration reads identically on hover and in `signatureHelp`, defaults included — the interface seam carries no default expressions, so both surfaces read the declaring file's own AST for them. The frames differ on purpose: hover renders GDScript declaration syntax, `func name(params) -> ret`, while `signatureHelp` renders Godot's own call hint, `<return type> name(params)`, which is what `_make_arguments_hint` builds (`gdscript_editor.cpp:750`). Godot's editor server answers `null` to `textDocument/signatureHelp`, so the hint popup is the only thing to be faithful to there. A `const` carries its folded value where one exists — at its declaration and at a read in the same file — and stays type-only where none does, across a file boundary or for an initializer that cannot fold. A value is never guessed to fill the slot.

`@deprecated` and `@experimental` render as a banner above the prose, so a reader who stops at the first line still learns the symbol is on its way out. `@deprecated` additionally drives two non-prose signals: `CompletionItem.tags: [Deprecated]`, downgraded to the pre-3.15 `deprecated: true` boolean for a client that never advertised `completionItem.tagSupport`, and never both; and the standard `deprecated` semantic-token modifier on the declaration and on every resolved use (member reads, call sites, class names), each read from the declaring file's `Interface`. Native symbols never carry either signal, since `extension_api.json` at 4.6.3 has no deprecation field, with or without docs, so there is nothing to claim one from.

### 7.3 Materialized API pages

A native symbol's `definition` is a real `Location` in a real file, because LSP through 3.17 has no virtual-document mechanism and a custom URI scheme is ruled out (§3 W4). Pages live in the user-level cache, keyed by renderer version and the dump's content hash, and never inside a workspace root, so the project indexer cannot ingest one as a script. They are read-only by convention and never self-diagnose; a page need only read as GDScript, not parse as it.

Every symbol Godot documents has a page to land on, which means one per engine class, one per Variant type, and two more for the things that belong to no class: `@GlobalScope.gd` for the Variant utilities the dump carries, and `@GDScript.gd` for the ones the language compiles in. The split mirrors Godot's own documentation. The second page is the reason `len` and `range` resolve in a project whose dump is stale, trimmed, or missing entirely — those functions live in the engine binary, not in `extension_api.json`, so their declarations are transcribed from the `REGISTER_FUNC` table rather than read from data.

A declaration line on a page is produced by the same builder as the hover for the same symbol, so the two can never drift apart.

### 7.4 Performance and scale

Per-feature latency rows live in `bench/budget.toml` (`06-testing-fidelity.md` §7.2), with completion the critical one. The memory-pressure ladder splits requests by price: at Hard pressure, analysis-priced requests are refused with `ContentModified` while parse- and index-priced features stay served.

`partialResultToken` streaming for `references` and `workspace/symbol` at 10k-file scale (sending the entire result via `$/progress` with an empty final response) is available in the spec and not currently used; adopt it only if acceptance-project latency demands it.

### 7.5 Testing

The fidelity ratchets cover the frontend and do not touch this layer. What covers it instead: per-capability protocol-shape tests over the in-memory `Connection`, asserting both the gated and ungated projection of every response; the vendored editor-capability profiles in `crates/gd_server/tests/fixtures/client_caps/`, replayed by `tests/editor_profiles.rs`; completion-semantics spot checks against the Godot editor as a manual differential oracle, since the editor's completion is not headless; and the `.tscn` fuzz target under the same any-panic-blocks-release rule. See `06-testing-fidelity.md`.

## 8. Sources

- [LSP 3.17 spec](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
- [LSP 3.18 spec, finalized 2026-06-04](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/)
- Godot LSP source audited at `4.6.3-stable`: `modules/gdscript/language_server/` in a local checkout
- [rust-analyzer token fallbacks](https://github.com/rust-lang/rust-analyzer/blob/master/crates/rust-analyzer/src/lsp/semantic_tokens.rs)
- [gopls completion and sortText](https://github.com/golang/tools/blob/master/gopls/internal/server/completion.go)
- [clangd features](https://clangd.llvm.org/features)
- Helix: [LSP client surface](https://github.com/helix-editor/helix/blob/master/helix-lsp/src/client.rs) and its [semantic-tokens stance](https://github.com/helix-editor/helix/discussions/5589)
- [Neovim LSP](https://neovim.io/doc/user/lsp.html)
- [Zed semantic tokens](https://zed.dev/docs/semantic-tokens), [eglot](https://github.com/emacs-mirror/emacs/blob/master/etc/EGLOT-NEWS), [Sublime LSP](https://lsp.sublimetext.io/features/)
- GDScript formatter landscape: [gdtoolkit](https://github.com/Scony/godot-gdscript-toolkit), [GDScript-formatter](https://github.com/GDQuest/GDScript-formatter), [proposal #3630](https://github.com/godotengine/godot-proposals/issues/3630)
- tree-sitter grammars: [tree-sitter-gdscript](https://github.com/PrestonKnopp/tree-sitter-gdscript) and [tree-sitter-godot-resource](https://github.com/PrestonKnopp/tree-sitter-godot-resource)
