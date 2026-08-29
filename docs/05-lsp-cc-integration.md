# 05. The LSP surface, configuration, and deployment

## 1. What gdls serves

gdls speaks LSP 3.17 over stdio. Every capability below is advertised only when it is wired to real work, and every one that a client capability guards is gated on that exact capability path. The wire conventions for each family, and the reasoning behind them, are in `09-lsp-conventions.md`.

**Lifecycle and sync.** `initialize`, `initialized`, `shutdown`, `exit`, `serverInfo`. Incremental `textDocumentSync` (`didOpen`, `didChange`, `didSave`, `didClose`). `positionEncoding` negotiated across UTF-8, UTF-16, and UTF-32. `$/cancelRequest` with true preemption: a cancelled request returns `RequestCancelled`, and one made stale by an edit returns `ContentModified`. `workDoneProgress` on the cold index, warm start, reconcile, and re-index, plus client tokens on `references` and `workspace/symbol`.

**Diagnostics.** Push `textDocument/publishDiagnostics` and pull `textDocument/diagnostic`, computing the same items either way. `workspace/diagnostic` is deliberately absent (`04-diagnostics-strict-mode.md` §2).

**Configuration and files.** `workspace/didChangeConfiguration` plus `workspace/configuration` for runtime re-config. `workspace/didChangeWatchedFiles`, dynamically registered when the client offers it, merged with the native watcher and de-duplicated by content fingerprint. `workspace/willRenameFiles` plus the `did*` notifications.

| Family | Methods |
|---|---|
| Symbols | `textDocument/documentSymbol` (hierarchical, with a flat fallback), `workspace/symbol`, `workspaceSymbol/resolve` |
| Navigation | `textDocument/definition`, `declaration`, `typeDefinition`, `references`, `implementation`, `documentLink` |
| Hierarchies | `textDocument/prepareCallHierarchy` with `callHierarchy/incomingCalls` and `outgoingCalls`; `textDocument/prepareTypeHierarchy` with `typeHierarchy/supertypes` and `subtypes` |
| Editing | `textDocument/completion` plus `completionItem/resolve`; `textDocument/signatureHelp`; `textDocument/rename` plus `prepareRename` |
| Reading | `textDocument/hover`, `documentHighlight`, `foldingRange`, `selectionRange` |
| Presentation | `textDocument/semanticTokens` (full, delta, range), `inlayHint` plus `inlayHint/resolve`, `documentColor` plus `colorPresentation` |
| Actions | `textDocument/codeAction` plus `codeAction/resolve`, `workspace/executeCommand` (exactly one command, `gdls.applyWarningIgnore`), `workspace/applyEdit` |
| Formatting | `textDocument/formatting`, advertised only when a formatter command is configured |

The index designs behind the navigation handlers are in `03-indexing-freshness.md` §7.

### Trigger characters and defaults

`completion` triggers on `.`, `$`, `%`, `"`, and `@`, all non-identifier characters. `signatureHelp` triggers on `(` and `,`, and retriggers on `)`. Completion documentation and detail are resolved lazily through `completionItem/resolve`, and `data` carries a compact file-plus-symbol-path key rather than the request params.

`semanticTokens` advertises a fixed legend of 10 standard token types and 6 standard modifiers, with zero custom names, at full width always so delta correlation keeps stable wire indices. Per-client legend intersection happens at emit time, never by shrinking the advertised legend.

### Cross-file navigation limits

Dynamic dispatch through `Variant` or `Callable`, signal connections made with dynamic name strings, and lambda invocations through opaque callables are not captured by `references` or the call hierarchy. Static method resolution and direct call expressions are. This matches both Godot's editor LSP and rust-analyzer.

### Hover doc-string source

Doc prose for `textDocument/hover` resolves in this order:

1. **`--dump-extension-api-with-docs` description fields.** Engine classes pulled from the in-project API dump (`description` and `brief_description` per class, method, property, and signal). The primary source.
2. **`doc_classes` XML.** The fallback for GDExtension classes whose addons ship XML but were absent from the dump, and for older dumps without docs. Merged into the same native DB. See `03-indexing-freshness.md` §1 and §2.
3. **GDScript `##` doc comments.** Project class and member prose. The lexer records comments into a side channel, leaving the token stream untouched so the fidelity ratchets are unaffected, and a post-parse pass applies Godot's association rules (`gd_syntax::doc_comments`). Docs ride the `Interface` outside the signature hash, so a doc-only edit never invalidates dependents.
4. **Absent.** If no tier carries prose, hover shows the type signature alone.

All outgoing prose flows through the single converter in `gd_server::docs`, since dump descriptions and `##` docs are both BBCode-flavored. Output is either GitHub-Flavored Markdown (a fenced signature, a `---` rule, then prose, the rust-analyzer hover shape) or stripped plaintext when the client's `hover.contentFormat` prefers it. Raw BBCode never reaches the wire. Completion and signature help documentation go through the same converter. Details: `09-lsp-conventions.md` §7.2.

## 2. Lifecycle and capabilities

- **Text sync.** Incremental (`TextDocumentSyncKind.Incremental`), applied to the `ropey` buffer.
- **Position encoding.** Internally gdls uses byte offsets and converts at the boundary. LSP defaults to UTF-16; if the client negotiates `utf-8` or `utf-32` via `general.positionEncodings`, gdls honors it.
- **One server instance per workspace root** for the session. The `res://` root comes from `initializationOptions.projectRoot`, else `workspaceFolders`/`rootUri`, else the directory containing the nearest `project.godot`. `res://` is internal vocabulary only; the wire carries `file://` URIs.
- **Streams.** JSON-RPC on stdout, all logs on stderr. Writing logs to stdout corrupts the protocol.
- **Multi-root sessions are a non-goal.** One Godot project is one gdls, and editors spawn one instance per workspace.

## 3. Client configuration

The binary must be discoverable on `PATH`. Claude Code launches the server over stdio; register it via a plugin `.lsp.json`, or the `lspServers` key in `plugin.json`:

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

Other Claude Code config fields that work here: `transport` (`"stdio"` is the default; `"socket"` exists but is undocumented, so stay on stdio), `env`, `startupTimeout`, `shutdownTimeout`.

### `initializationOptions` schema

| Key | Type | Meaning |
|---|---|---|
| `projectRoot` | string (path) | The `res://` root. Optional; falls back to the workspace folder, then the nearest `project.godot`. |
| `extensionApiPath` | string (path) | Pin a hand-made `extension_api.json`. Optional. When absent, the auto-dump resolution applies (`03-indexing-freshness.md` §1), and when no project-derived source resolves, the embedded stock fallback serves builtins. Pinning also skips the background auto-dump entirely, so the managed dump can never be served while a path is pinned. |
| `godotBinaryPath` | string (path) | Godot 4.x executable for the auto-dump. Optional; discovery falls back to `GDLS_GODOT`, then `godot4`/`godot` on `PATH`. |
| `autoDumpExtensionApi` | bool | Allow gdls to spawn Godot to regenerate the managed dump under `.gdls/`. The dump runs on a background thread and is adopted mid-session (reload plus republish), so it never blocks a request. Default `true`; `false` forbids spawning entirely. |
| `embeddedApiFallback` | bool | When every native-API source misses, fall back to a bundled stock class surface for the project's own release (one asset per supported release, picked by dialect) instead of an empty DB, so builtins always resolve on a fresh install. Under this fallback (`Generic` provenance) unknown-type and unknown-member negatives are suppressed: only a project-derived (`Exact`) dump may claim a name does not exist. Default `true`. See `02-frontend-port.md` §11b. |
| `strict.profile` | `"godot"` \| `"strict"` \| `"off"` | Diagnostics profile (see `04-diagnostics-strict-mode.md`). Default `"godot"`. |
| `strict.enableWarnings` / `disableWarnings` / `errorWarnings` | string[] | Fine-grained warning overrides. |
| `completion.snippets` | bool | Emit snippet placeholders for callable completions. Gated a second time by the client's `completionItem.snippetSupport`. Default `true`. |
| `completion.callArgumentStyle` | enum | How a callable's call parentheses render when snippets are on. Default `parensWithCursor`, the gopls style: accepting `foo` yields `foo()` with the cursor between the parens. |
| `inlayHint.typeHints` | bool | Inferred type on `var x := …` and on an inferred `for` variable. Default `true`. |
| `inlayHint.parameterHints` | bool | Parameter-name labels at resolved call sites. Default `true`. Single-argument calls never get one regardless. |
| `formatter.command` / `formatter.args` | string / string[] | The external formatter executable and its argument vector. No shell is involved, so neither value can be interpreted as a shell expression. `documentFormattingProvider` is advertised only when `command` is set. |
| `memory.cacheCapacity` / `softCapMb` / `hardCapMb` | number | Parse and analysis cache size, and the memory-pressure ladder thresholds (`06-testing-fidelity.md` §7.3). |
| `analyzer.iterLimit` / `checkpointDelayUs` | number | The per-file fixpoint iteration cap and the cancellation-checkpoint interval. |
| `stubCacheDir` | string | Override the root under which native-class API stubs, the `definition` targets for native symbols, are materialized. Default: the user-level gdls cache (`%LOCALAPPDATA%\gdls` or `~/.cache/gdls`), deliberately outside any workspace root. Normally left unset. |

Malformed options fall back to documented defaults with a `window/showMessage(Warning)`; `initialize` never fails on them.

## 4. Deployment

- Ship a single static `gdls` (Rust release build) and put it on `PATH`, or reference it from a plugin that makes sure it is installed.
- `extension_api.json` is auto-managed: gdls dumps it via the discovered Godot binary and regenerates on binary or `.gdextension` changes (`03-indexing-freshness.md` §1). Manual generation is only needed when `autoDumpExtensionApi` is disabled or no Godot binary is discoverable.
- No other runtime files are required. The server is stateless across restarts apart from the `.gdls/` warm-start cache and managed dump.

## 5. Sources

- [LSP 3.17 spec](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
- [LSP servers in plugins: `.lsp.json`, `command`/`args`/`extensionToLanguage`, transport, `restartOnCrash`, and the binary having to be on PATH](https://code.claude.com/docs/en/plugins-reference.md)
- [LSP tool behavior: diagnostics after edit, definition, references, hover, symbols, implementation, call hierarchy](https://code.claude.com/docs/en/tools-reference.md)
- [LSP position encoding, UTF-16 default](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#positionEncodingKind)
