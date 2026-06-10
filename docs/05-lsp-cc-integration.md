# 05 — LSP surface & Claude Code integration

## 1. Target LSP surface (driven by what Claude Code consumes)

Per Claude Code's docs, its LSP client uses the following. v1 exposes all of them; the rest are Phase 2.
(Several exposed handlers are wired but still have Godot-parity gaps that **M6** closes before v1 ships —
see the parity note below the table.)

| Capability | LSP method(s) | v1? | Notes |
|---|---|---|---|
| Diagnostics (push, after edit) | `textDocument/publishDiagnostics` | ✅ M3 | Per-file (see `04-diagnostics-strict-mode.md`). CC uses **push**, not pull. |
| List symbols in a file | `textDocument/documentSymbol` | ✅ M1 | From per-script symbol tables. |
| Workspace symbol search | `workspace/symbol` | ✅ M4 | From the `class_name` registry + interface tables. Wired in M4 (`crates/gd_server/src/server.rs`). |
| Go to definition | `textDocument/definition` | ✅ M3 | From analyzer-recorded bindings. |
| Find references | `textDocument/references` | ✅ M4 | Needs the reverse-reference index. Wired in M4. |
| Hover / type info | `textDocument/hover` | ✅ M3 | Resolved type + signature + doc prose (see "Hover doc-string source" below). |
| Find implementations | `textDocument/implementation` | ✅ M4 | Subtypes / overrides via the class graph. Wired in M4. |
| Call hierarchy | `textDocument/prepareCallHierarchy` (+ incoming/outgoing) | ✅ M4 | From the call graph built during analysis. Wired in M4. |
| Signature help | `textDocument/signatureHelp` | ❌ Phase 2 | Not documented as consumed by CC. |
| Completion | `textDocument/completion` | ❌ Phase 2 | Not documented as consumed by CC. |
| Rename / formatting / code actions / semantic tokens | — | ❌ | Out of scope. |

> **Parity note (M6).** A ✅ above means the capability is *exposed and wired*, not that it already
> matches Godot's own LSP on every input. Five have parity gaps that **M6** closes before v1 is tagged:
> hierarchical `documentSymbol` (M6-A); `definition` on `class_name`-in-expression / `preload` strings /
> autoloads (M6-B/C/D); project-wide `references` through typed vars (M6-E); `hover` member/call/`preload`
> signatures (M6-F); and `implementation` for method overrides (M6-G). Until then they are safe but
> sometimes incomplete — never wrong. See [`08-m6-v1-ship.md`](08-m6-v1-ship.md).

### Hover doc-string source

Doc prose for `textDocument/hover` resolves through this priority order:

1. **`--dump-extension-api-with-docs` description fields** — engine classes pulled from the
   in-project API dump (`description` and `brief_description` per class / method / property /
   signal). This is the primary source.
2. **`doc_classes` XML** — fallback for GDExtension classes whose addons ship XML but were absent
   from the dump (and for older dumps without docs); merged into the same native DB. Same XML
   format Godot's class reference uses. See `03-indexing-freshness.md` §1–§2 for the multi-source
   capture story.
3. **Absent** — if neither tier carries prose, hover shows the type signature alone.

**Project-class (user-defined) doc prose is deferred to Phase 2.** GDScript's `##` doc-comment
syntax is discarded by the M1 lexer (no `##`-token retention); restoring it requires either lexer
work or a sidecar doc extractor, and that work is naturally bundled with Phase 2's `.tscn` /
`$Node` typing. Until then, hover on a project class shows the resolved type only.

### M4 nav handler semantics

Specifies each M4 handler's parameter and result shape (per the LSP 3.17 spec —
<https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/>) and
where each derives its data from. The full index designs live in
`03-indexing-freshness.md §7`; this is the protocol contract.

| Handler | LSP request → result | gdls semantics |
|---|---|---|
| `textDocument/references` | `ReferenceParams` (`TextDocumentPositionParams + context.includeDeclaration: boolean`) → `Location[] \| null` | Resolves identifier under cursor via the analyzer's smallest-typed-containing walk, queries `Index.name_referencers[name]` for candidate files, lazy-analyzes each and filters bindings to those matching the resolved `(kind, qualified_name)`. Includes the declaration when `includeDeclaration: true`. Cross-file walks share the analysis cache with `definition`. |
| `textDocument/implementation` | `ImplementationParams` (`TextDocumentPositionParams`) → `Location \| Location[] \| LocationLink[] \| null` | For a class C, returns the declaration sites of all direct + transitive subclasses (linear walk of `Index.interfaces` checking `extends.target`). For a virtual/abstract method M on class C, also requires each candidate subclass to declare a member with the same name (same scan, plus a per-candidate `MemberDecl` lookup). |
| `textDocument/prepareCallHierarchy` | `CallHierarchyPrepareParams` (`TextDocumentPositionParams`) → `CallHierarchyItem[] \| null` | Resolves the symbol under cursor (function, method, or constructor) to a `CallHierarchyItem` with the declaration's name, kind, uri, full range, and selectionRange (identifier-only span). Returns one item per overload when the cursor is on a multiply-defined name. |
| `callHierarchy/incomingCalls` | `CallHierarchyIncomingCallsParams` (`{ item: CallHierarchyItem }`) → `CallHierarchyIncomingCall[] \| null` | Queries `Index.name_referencers[item.name]` for caller candidates, lazy-analyzes each, filters `AnalysisResult.bindings` for `Binding::Call` variants targeting `item`, returns one `CallHierarchyIncomingCall { from: CallHierarchyItem (the caller), fromRanges: Range[] (the call sites within the caller) }` per unique caller. |
| `callHierarchy/outgoingCalls` | `CallHierarchyOutgoingCallsParams` (`{ item: CallHierarchyItem }`) → `CallHierarchyOutgoingCall[] \| null` | Filters `item`'s own `AnalysisResult.bindings` for `Binding::Call` variants, groups by callee, returns one `CallHierarchyOutgoingCall { to: CallHierarchyItem (the callee), fromRanges: Range[] (the call sites within `item`) }` per unique callee. |
| `workspace/symbol` | `WorkspaceSymbolParams` (`{ query: string }`) → `SymbolInformation[] \| WorkspaceSymbol[] \| null` | Fuzzy match against the flat union of `ClassNameRegistry` entries + every `MemberDecl` across `Index.interfaces`. Ordering: (1) prefix match on class name, (2) prefix match on member name, (3) fuzzy score. Capped at 256 results (configurable via `initializationOptions.workspaceSymbolMaxResults`). If the client advertises `workspace.symbol.resolveSupport` (3.17), gdls returns `WorkspaceSymbol[]` without `range` and resolves on demand via `workspaceSymbol/resolve`; otherwise returns full `SymbolInformation[]`. |

**Call-graph limitations** (intentional, matching Godot's editor LSP and rust-analyzer's
approach): dynamic dispatch through `Variant` or `Callable`, signal connections via dynamic name
strings, and lambda invocations through opaque callables are **not** captured. Static
method-resolution and direct call expressions are.

**Cancellation.** Per LSP 3.17 `$/cancelRequest`, a cancelled request still returns a response —
gdls returns an error with `ErrorCodes.RequestCancelled`. M5 plumbs a cancellation token through
the analyzer with cooperative checkpoints (see `06-testing-fidelity.md §8`).

## 2. Lifecycle & capabilities

- Implement `initialize` / `initialized` / `shutdown` / `exit`. Advertise exactly the capabilities in §1.
- **Text sync:** advertise **incremental** sync (`TextDocumentSyncKind.Incremental`); apply edits to the
  `ropey` buffer. Handle `didOpen` / `didChange` / `didSave` / `didClose`.
- **Position encoding:** LSP defaults to **UTF-16** offsets. Internally gdls uses byte offsets; convert at
  the boundary. (If the client negotiates `utf-8` via `general.positionEncodings`, prefer it.)
- **One server instance per workspace root** for the session (standard CC behavior). The `res://` root is
  taken from `initializationOptions.projectRoot`, else the workspace folder, else the directory containing
  the nearest `project.godot`.
- **Streams:** JSON-RPC on **stdout**; **all logs on stderr** (writing logs to stdout corrupts the protocol).

## 3. Claude Code configuration

Claude Code launches the server over stdio; the **binary must be discoverable on `PATH`**, and CC does
**not** ship the server (you build/install it). Register via a plugin `.lsp.json` (or the `lspServers` key
in `plugin.json`):

```jsonc
// .lsp.json
{
  "gdscript": {
    "command": "gdls",
    "extensionToLanguage": { ".gd": "gdscript" },
    "initializationOptions": {
      "projectRoot": "/home/me/MyGame",
      "strict": { "profile": "strict" }
    },
    "restartOnCrash": true,
    "maxRestarts": 5
  }
}
```

### `initializationOptions` schema

| Key | Type | Meaning |
|---|---|---|
| `projectRoot` | string (path) | `res://` root. Optional; falls back to workspace folder / nearest `project.godot`. |
| `extensionApiPath` | string (path) | Pin a hand-made `extension_api.json`. Optional — when absent, the auto-dump resolution applies (`03-indexing-freshness.md` §1); when no project-derived source resolves, the embedded stock fallback serves builtins. |
| `godotBinaryPath` | string (path) | Godot 4.x executable for the auto-dump. Optional; discovery falls back to `GDLS_GODOT`, then `godot4`/`godot` on PATH. |
| `autoDumpExtensionApi` | bool | Allow gdls to spawn Godot to (re)generate the managed dump under `.gdls/`. Since v1.0.2 the dump runs on a background thread and is adopted mid-session (reload + republish) — it never blocks a request. Default `true`; `false` forbids spawning entirely. |
| `embeddedApiFallback` | bool | v1.0.2: when every native-API source misses, fall back to a bundled stock 4.6.3 class surface instead of an empty DB, so builtins always resolve on a fresh install. Under this fallback (`Generic` provenance) unknown-type/member negatives are suppressed — only a project-derived (`Exact`) dump may claim a name doesn't exist. Default `true`. |
| `strict.profile` | `"godot"` \| `"strict"` \| `"off"` | Diagnostics profile (see `04-diagnostics-strict-mode.md`). Default `"godot"`. |
| `strict.enableWarnings` / `disableWarnings` / `errorWarnings` | string[] | Fine-grained overrides. |

Other config fields supported by CC and usable here: `transport` (`"stdio"` default; `"socket"` exists but
is undocumented — stay on stdio), `env`, `startupTimeout`, `shutdownTimeout`.

## 4. Deployment

- Ship a single static `gdls` (Rust release build) and place it on `PATH` (or reference it from a plugin
  that ensures it is installed).
- `extension_api.json` is auto-managed since v1.0.1 (gdls dumps it via the discovered Godot binary and
  regenerates on binary / `.gdextension` changes — `03-indexing-freshness.md` §1). Manual generation is
  only needed when `autoDumpExtensionApi` is disabled or no Godot binary is discoverable.
- No other runtime files required; the server is stateless across restarts except for the
  `.gdls/` warm-start cache + managed dump.

## 5. Sources

- LSP servers in plugins (`.lsp.json`, `command`/`args`/`extensionToLanguage`, transport, `restartOnCrash`,
  binary must be on PATH) — https://code.claude.com/docs/en/plugins-reference.md
- LSP tool behavior (diagnostics after edit; definition/references/hover/symbols/implementation/call
  hierarchy) — https://code.claude.com/docs/en/tools-reference.md
- LSP position encoding (UTF-16 default) — https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#positionEncodingKind
