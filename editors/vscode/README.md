# gdls for VS Code and Cursor

This extension connects VS Code and Cursor to [gdls](https://github.com/kurushimee/gdls), a fast GDScript language server for Godot 4.6 and 4.7. You get type-aware errors and warnings, hover, go to definition, find references, completion, rename, and the rest of the language features for `.gd` files, all without the Godot editor running.

## Requirements

The extension does not ship or download gdls. You need the `gdls` binary on your `PATH`, or you can set `gdls.path` to point at it.

To build the latest `main` and put it in `~/.cargo/bin`:

```sh
cargo install --git https://github.com/kurushimee/gdls gd_server
```

You can also download a release binary from [GitHub Releases](https://github.com/kurushimee/gdls/releases). If VS Code was already open when you installed gdls, restart it so it picks up the new `PATH`.

## Install

From the Extensions view, search for **gdls**. To install a downloaded `.vsix` instead, run **Extensions: Install from VSIX...** from the command palette, or use the command line:

```sh
code --install-extension gdls-1.0.0.vsix
cursor --install-extension gdls-1.0.0.vsix
```

## Features

- Errors and warnings that match Godot's own messages, updated as you type
- Hover with types and docs, including `##` doc comments and engine class docs
- Go to definition, declaration, type definition, and implementation, plus find references
- Call and type hierarchies
- Completion with lazy docs, and signature help
- Rename across files, which refuses rather than making a partial edit it can't prove safe
- Semantic highlighting, inlay type and parameter hints, and color pickers for `Color(...)`
- Code actions, such as adding `@warning_ignore`
- Document and workspace symbols, folding, selection ranges, and document links
- Format Document through an external formatter such as `gdformat`

The extension also adds GDScript syntax highlighting, comment toggling, bracket matching, and indentation after a `:`.

## Settings

The extension starts gdls in the first folder that has a `project.godot`, skipping `addons/`. Set `gdls.projectRoot` if your workspace holds more than one project.

| Setting | Default | What it does |
|---|---|---|
| `gdls.path` | empty | Path to the `gdls` binary. When empty, the extension looks on `PATH`. |
| `gdls.projectRoot` | empty | The Godot project folder. It can be relative to the workspace folder. |
| `gdls.dialect` | unset | Pins the Godot release (`4.6` or `4.7`) instead of reading it from `project.godot`. |
| `gdls.godotBinaryPath` | empty | The Godot binary that gdls runs to dump your project's native API, including GDExtension classes. |
| `gdls.autoDumpExtensionApi` | `true` | Lets gdls run that dump in the background. |
| `gdls.extensionApiPath` | empty | Uses a hand-made `extension_api.json` instead. |
| `gdls.embeddedApiFallback` | `true` | Uses the stock engine classes for your release when no dump is available. |
| `gdls.strict.profile` | `godot` | `godot` follows your project settings, `strict` turns on every typing warning as an error, and `off` shows errors only. |
| `gdls.strict.enableWarnings`, `disableWarnings`, `errorWarnings` | `[]` | Turns single warnings on or off, or reports them as errors. |
| `gdls.completion.snippets`, `callArgumentStyle` | `true`, `parensWithCursor` | Controls how function completions insert parentheses. |
| `gdls.inlayHint.typeHints`, `parameterHints` | `true` | Shows inferred types and parameter names inline. |
| `gdls.formatter.command`, `args` | empty | The formatter for Format Document. Formatting is off while the command is empty. |
| `gdls.memory.*`, `gdls.analyzer.iterLimit`, `gdls.stubCacheDir` | unset | Tuning knobs that most projects never need. |
| `gdls.trace.server` | `off` | Logs the traffic between the editor and gdls to the **gdls** output channel. |

Most settings apply right away. Changing the path, project root, dialect, Godot binary, API dump, or formatter restarts gdls for you.

Path settings accept `${workspaceFolder}` and a leading `~`. In an untrusted workspace, gdls stays stopped until you trust it, since it can run the Godot binary and a formatter that the workspace's settings name.

## Commands

- **gdls: Restart Server**
- **gdls: Show Output**, which you can also open by clicking **gdls** in the status bar

## Using it next to godot-tools

The godot-tools extension also handles `.gd` files and connects to the Godot editor's own language server. With both running, you will see duplicate hovers, completions, and diagnostics. Disable godot-tools for the workspace, or turn off its language server, when you use gdls.

## Development

```sh
npm ci
npm run typecheck
npm test
npm run package
```

`npm test` downloads a stock VS Code, loads the extension, and checks diagnostics, hover, navigation, completion, rename, live edits, and settings changes against the project in `test/fixture`. It needs `gdls` on `PATH`. On a bare Linux box, VS Code also needs the `libnss3` and `libasound2` system libraries.
