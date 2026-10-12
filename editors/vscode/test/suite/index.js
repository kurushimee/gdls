// Runs inside the extension host. Each check drives a real editor feature through gdls.
const assert = require("node:assert");
const path = require("node:path");
const vscode = require("vscode");

const workspace = process.env.GDLS_TEST_WORKSPACE;
const uri = (rel) => vscode.Uri.file(path.join(workspace, rel));

async function waitFor(what, fn, timeoutMs = 60000) {
  const end = Date.now() + timeoutMs;
  let last;
  while (Date.now() < end) {
    try {
      last = await fn();
      if (last) return last;
    } catch (err) {
      last = err;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`timed out waiting for ${what} (last: ${last})`);
}

const checks = {
  async extensionIsActive() {
    const ext = vscode.extensions.getExtension("kurushimee.gdls");
    assert.ok(ext, "extension not found");
    await ext.activate();
    assert.ok(ext.isActive);
  },

  async languageIsGdscript() {
    const doc = await vscode.workspace.openTextDocument(uri("scripts/main.gd"));
    assert.strictEqual(doc.languageId, "gdscript");
    await vscode.window.showTextDocument(doc);
  },

  async diagnostics() {
    const diags = await waitFor("diagnostics on main.gd", () => {
      const d = vscode.languages.getDiagnostics(uri("scripts/main.gd"));
      return d.length > 0 ? d : undefined;
    });
    const err = diags.find((d) => d.severity === vscode.DiagnosticSeverity.Error);
    assert.ok(err, `no error among ${JSON.stringify(diags.map((d) => d.message))}`);
    assert.match(err.message, /String/);
    assert.strictEqual(err.range.start.line, 5);
  },

  async hover() {
    const hovers = await waitFor("hover on speed", async () => {
      const h = await vscode.commands.executeCommand("vscode.executeHoverProvider", uri("scripts/player.gd"), new vscode.Position(4, 6));
      return h && h.length ? h : undefined;
    });
    const text = hovers.flatMap((h) => h.contents.map((c) => (typeof c === "string" ? c : c.value))).join("\n");
    assert.match(text, /float/);
    assert.match(text, /How fast the player moves/);
  },

  async definitionAcrossFiles() {
    const locs = await vscode.commands.executeCommand("vscode.executeDefinitionProvider", uri("scripts/main.gd"), new vscode.Position(4, 16));
    const targets = locs.map((l) => (l.targetUri ?? l.uri).fsPath);
    assert.ok(targets.some((t) => t.endsWith("player.gd")), `definition went to ${targets}`);
  },

  async references() {
    const refs = await vscode.commands.executeCommand("vscode.executeReferenceProvider", uri("scripts/player.gd"), new vscode.Position(7, 6));
    assert.ok(refs.some((r) => r.uri.fsPath.endsWith("main.gd")), `references: ${refs.map((r) => r.uri.fsPath)}`);
  },

  async completion() {
    const list = await vscode.commands.executeCommand("vscode.executeCompletionItemProvider", uri("scripts/main.gd"), new vscode.Position(6, 8), ".");
    const labels = list.items.map((i) => (typeof i.label === "string" ? i.label : i.label.label));
    assert.ok(labels.includes("move"), "move missing from completion");
    assert.ok(labels.includes("speed"), "speed missing from completion");
    assert.ok(labels.includes("add_child"), "inherited Node member missing from completion");
  },

  async signatureHelp() {
    const help = await vscode.commands.executeCommand("vscode.executeSignatureHelpProvider", uri("scripts/main.gd"), new vscode.Position(6, 13), "(");
    assert.ok(help && help.signatures.length > 0, "no signature help");
    assert.match(help.signatures[0].label, /delta/);
  },

  async documentSymbols() {
    const syms = await vscode.commands.executeCommand("vscode.executeDocumentSymbolProvider", uri("scripts/player.gd"));
    const names = JSON.stringify(syms.map((s) => [s.name, (s.children ?? []).map((c) => c.name)]));
    assert.match(names, /speed/);
    assert.match(names, /move/);
  },

  async workspaceSymbols() {
    const syms = await vscode.commands.executeCommand("vscode.executeWorkspaceSymbolProvider", "Player");
    assert.ok(syms.some((s) => s.name === "Player"), `workspace symbols: ${syms.map((s) => s.name)}`);
  },

  async semanticTokens() {
    const tokens = await vscode.commands.executeCommand("vscode.provideDocumentSemanticTokens", uri("scripts/main.gd"));
    assert.ok(tokens && tokens.data.length > 0, "no semantic tokens");
  },

  async inlayHints() {
    const hints = await vscode.commands.executeCommand("vscode.executeInlayHintProvider", uri("scripts/main.gd"), new vscode.Range(0, 0, 8, 0));
    const labels = hints.map((h) => (typeof h.label === "string" ? h.label : h.label.map((p) => p.value).join("")));
    assert.ok(labels.some((l) => l.includes("Player")), `inlay hints: ${labels}`);
  },

  async rename() {
    // VS Code mangles the new name when the target file has no open model, so open it first.
    await vscode.workspace.openTextDocument(uri("scripts/player.gd"));
    const edit = await vscode.commands.executeCommand("vscode.executeDocumentRenameProvider", uri("scripts/player.gd"), new vscode.Position(4, 6), "velocity");
    const lines = edit.get(uri("scripts/player.gd")).map((e) => [e.range.start.line, e.newText]);
    assert.deepStrictEqual(lines, [[4, "velocity"], [8, "velocity"]]);
  },

  async liveEditUpdatesDiagnostics() {
    const doc = await vscode.workspace.openTextDocument(uri("scripts/main.gd"));
    const editor = await vscode.window.showTextDocument(doc);
    const line = doc.lineAt(5).text;
    const start = line.indexOf('"');
    await editor.edit((e) => e.replace(new vscode.Range(5, start, 5, line.length), "42"));
    await waitFor("error to clear after edit", () =>
      vscode.languages.getDiagnostics(doc.uri).every((d) => d.severity !== vscode.DiagnosticSeverity.Error) || undefined,
    );
  },

  async settingsReachTheServer() {
    // RETURN_VALUE_DISCARDED is off by default; turning it on must flow through workspace/configuration.
    const cfg = vscode.workspace.getConfiguration("gdls");
    await cfg.update("strict.enableWarnings", ["RETURN_VALUE_DISCARDED"], vscode.ConfigurationTarget.Workspace);
    try {
      await waitFor("RETURN_VALUE_DISCARDED on main.gd", () =>
        vscode.languages.getDiagnostics(uri("scripts/main.gd")).find((d) => d.code === "RETURN_VALUE_DISCARDED" || /discarded/i.test(d.message)),
      );
    } finally {
      await cfg.update("strict.enableWarnings", undefined, vscode.ConfigurationTarget.Workspace);
    }
  },

  async restartCommand() {
    await vscode.commands.executeCommand("gdls.restart");
    const h = await waitFor("hover after restart", async () => {
      const r = await vscode.commands.executeCommand("vscode.executeHoverProvider", uri("scripts/player.gd"), new vscode.Position(4, 6));
      return r && r.length ? r : undefined;
    });
    assert.ok(h.length > 0);
  },
};

exports.run = async function run() {
  const failures = [];
  for (const [name, check] of Object.entries(checks)) {
    try {
      await check();
      console.log(`  ok   ${name}`);
    } catch (err) {
      console.log(`  FAIL ${name}: ${err && err.message}`);
      failures.push(name);
    }
  }
  if (failures.length) {
    throw new Error(`${failures.length} check(s) failed: ${failures.join(", ")}`);
  }
};
