import * as fs from "node:fs";
import * as path from "node:path";
import * as readline from "node:readline";
import type { Readable } from "node:stream";
import * as vscode from "vscode";
import {
  DidChangeConfigurationNotification,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  State,
  TransportKind,
} from "vscode-languageclient/node";

const SECTION = "gdls";

// Settings that gdls reads only once at startup, so a change needs a fresh server. `path` and
// `dialect` belong to the extension's own startup and the rest mirror the server's
// session-structural list in `apply_runtime_config`.
const RESTART_KEYS = [
  "path",
  "projectRoot",
  "dialect",
  "godotBinaryPath",
  "extensionApiPath",
  "autoDumpExtensionApi",
  "embeddedApiFallback",
  "stubCacheDir",
  "formatter",
];

let client: LanguageClient | undefined;
let output: vscode.LogOutputChannel;
let status: vscode.StatusBarItem;
let projectRoot: string | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  output = vscode.window.createOutputChannel("gdls", { log: true });
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 0);
  status.command = "gdls.showOutput";
  context.subscriptions.push(output, status);

  context.subscriptions.push(
    vscode.commands.registerCommand("gdls.restart", () => restart()),
    vscode.commands.registerCommand("gdls.showOutput", () => output.show(true)),
    vscode.workspace.onDidChangeConfiguration((e) => onConfigChange(e)),
  );

  if (vscode.workspace.isTrusted) {
    await start();
  } else {
    setStatus("stopped", "gdls waits for this workspace to be trusted.");
    context.subscriptions.push(vscode.workspace.onDidGrantWorkspaceTrust(() => start()));
  }
}

export async function deactivate(): Promise<void> {
  await stop();
}

async function start(): Promise<void> {
  if (client) {
    return;
  }
  const command = resolveServer();
  if (!command) {
    setStatus("error", "gdls is not on PATH.");
    const pick = await vscode.window.showErrorMessage(
      "gdls: the `gdls` binary was not found on PATH. Install it, or point the `gdls.path` setting at it.",
      "How to Install",
      "Set Path",
    );
    if (pick === "How to Install") {
      void vscode.env.openExternal(vscode.Uri.parse("https://github.com/kurushimee/gdls#install"));
    } else if (pick === "Set Path") {
      void vscode.commands.executeCommand("workbench.action.openSettings", "gdls.path");
    }
    return;
  }

  projectRoot = await findProjectRoot();
  output.appendLine(`Starting ${command}${projectRoot ? ` for ${projectRoot}` : ""}`);

  const serverOptions: ServerOptions = {
    command,
    transport: TransportKind.stdio,
    options: { cwd: projectRoot, env: { ...process.env, GDLS_LOG: serverLogFilter() } },
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "gdscript" }],
    initializationOptions: settings(),
    outputChannel: output,
    // The client's default logs every stderr line as an error. gdls prints its own level on each
    // line, so route by that instead.
    stdioOptions: {
      stdout: (input) => forEachLine(input, (line) => output.info(line)),
      stderr: (input) => forEachLine(input, logServerLine),
    },
    middleware: {
      workspace: {
        // gdls pulls its whole "gdls" section on every configuration change. Answer it from the
        // same builder that made the initializationOptions, so both always agree.
        configuration: async (params, token, next) => {
          const result = await next(params, token);
          if (!Array.isArray(result)) {
            return result;
          }
          return params.items.map((item, i) => (item.section === SECTION ? settings() : result[i]));
        },
      },
    },
  };

  client = new LanguageClient(SECTION, "gdls", serverOptions, clientOptions);
  client.onDidChangeState((e) => {
    if (e.newState === State.Running) {
      const info = client?.initializeResult?.serverInfo;
      setStatus("running", `${info?.name ?? "gdls"} ${info?.version ?? ""}`.trim());
    } else if (e.newState === State.Starting) {
      setStatus("starting", "gdls is starting.");
    } else {
      setStatus("stopped", "gdls is not running.");
    }
  });

  try {
    await client.start();
  } catch (err) {
    setStatus("error", `gdls failed to start: ${String(err)}`);
    output.appendLine(`Failed to start gdls: ${String(err)}`);
    void vscode.window.showErrorMessage(`gdls failed to start: ${String(err)}`, "Show Output").then((pick) => {
      if (pick) {
        output.show(true);
      }
    });
    client = undefined;
  }
}

function forEachLine(input: Readable, fn: (line: string) => void): void {
  readline.createInterface({ input, crlfDelay: Infinity, terminal: false }).on("line", fn);
}

// A gdls stderr line looks like `   9.549566900s  INFO rss: rss sample phase="tick" …`.
const SERVER_LINE = /^\s*\d+(?:\.\d+)?s\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+(.*)$/;
// Lines without a level, such as a panic message or its backtrace, keep the last level seen.
let lastServerLevel = "ERROR";

function logServerLine(line: string): void {
  const m = SERVER_LINE.exec(line);
  if (m) {
    lastServerLevel = m[1];
  }
  const text = m ? m[2] : line;
  switch (lastServerLevel) {
    case "TRACE":
      return output.trace(text);
    case "DEBUG":
      return output.debug(text);
    case "INFO":
      return output.info(text);
    case "WARN":
      return output.warn(text);
    default:
      return output.error(text);
  }
}

/**
 * The `GDLS_LOG` filter for a new server: the user's own `GDLS_LOG` if set, else the output
 * channel's log level, so gdls never formats lines the channel would hide anyway.
 */
function serverLogFilter(): string {
  if (process.env.GDLS_LOG) {
    return process.env.GDLS_LOG;
  }
  switch (output.logLevel) {
    case vscode.LogLevel.Off:
      return "off";
    case vscode.LogLevel.Trace:
      return "trace";
    case vscode.LogLevel.Debug:
      return "debug";
    case vscode.LogLevel.Warning:
      return "warn";
    case vscode.LogLevel.Error:
      return "error";
    default:
      return "info";
  }
}

async function stop(): Promise<void> {
  const c = client;
  client = undefined;
  if (c) {
    try {
      await c.stop();
    } catch (err) {
      output.appendLine(`Error while stopping gdls: ${String(err)}`);
    }
  }
}

async function restart(): Promise<void> {
  await stop();
  await start();
}

async function onConfigChange(e: vscode.ConfigurationChangeEvent) {
  if (!e.affectsConfiguration(SECTION) || !vscode.workspace.isTrusted) {
    return;
  }
  if (RESTART_KEYS.some((key) => e.affectsConfiguration(`${SECTION}.${key}`))) {
    output.appendLine("A startup setting changed, so gdls is restarting.");
    await restart();
  } else if (client?.isRunning()) {
    // gdls ignores the payload and pulls the section through workspace/configuration.
    await client.sendNotification(DidChangeConfigurationNotification.type, { settings: null });
  }
}

/** Builds the gdls settings object, leaving out anything unset so gdls keeps its own defaults. */
function settings(): Record<string, unknown> {
  const cfg = vscode.workspace.getConfiguration(SECTION);
  const str = (key: string) => {
    const v = cfg.get<string | null>(key);
    return typeof v === "string" && v.trim() !== "" ? expand(v.trim()) : undefined;
  };
  const num = (key: string) => {
    const v = cfg.get<number | null>(key);
    return typeof v === "number" ? v : undefined;
  };
  const prune = (o: Record<string, unknown>) =>
    Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined));
  const nonEmpty = (o: Record<string, unknown>) => {
    const p = prune(o);
    return Object.keys(p).length > 0 ? p : undefined;
  };

  return prune({
    projectRoot,
    dialect: str("dialect"),
    godotBinaryPath: str("godotBinaryPath"),
    extensionApiPath: str("extensionApiPath"),
    autoDumpExtensionApi: cfg.get<boolean>("autoDumpExtensionApi", true),
    embeddedApiFallback: cfg.get<boolean>("embeddedApiFallback", true),
    stubCacheDir: str("stubCacheDir"),
    strict: {
      profile: cfg.get<string>("strict.profile", "godot"),
      enableWarnings: cfg.get<string[]>("strict.enableWarnings", []),
      disableWarnings: cfg.get<string[]>("strict.disableWarnings", []),
      errorWarnings: cfg.get<string[]>("strict.errorWarnings", []),
    },
    completion: {
      snippets: cfg.get<boolean>("completion.snippets", true),
      callArgumentStyle: cfg.get<string>("completion.callArgumentStyle", "parensWithCursor"),
    },
    inlayHint: {
      typeHints: cfg.get<boolean>("inlayHint.typeHints", true),
      parameterHints: cfg.get<boolean>("inlayHint.parameterHints", true),
    },
    formatter: prune({
      command: str("formatter.command"),
      args: cfg.get<string[]>("formatter.args", []),
    }),
    memory: nonEmpty({
      cacheCapacity: num("memory.cacheCapacity"),
      softCapMb: num("memory.softCapMb"),
      hardCapMb: num("memory.hardCapMb"),
    }),
    analyzer: nonEmpty({ iterLimit: num("analyzer.iterLimit") }),
  });
}

/** Expands `${workspaceFolder}` and a leading `~` in a path setting. */
function expand(value: string): string {
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? "";
  let v = value.replace(/\$\{workspaceFolder\}/g, folder);
  if (v === "~" || v.startsWith("~/") || v.startsWith("~\\")) {
    v = path.join(process.env.HOME ?? process.env.USERPROFILE ?? "", v.slice(1));
  }
  return v;
}

/** The `gdls.path` setting, else `gdls` on PATH. */
function resolveServer(): string | undefined {
  const exe = process.platform === "win32" ? "gdls.exe" : "gdls";
  const configured = vscode.workspace.getConfiguration(SECTION).get<string | null>("path");
  if (typeof configured === "string" && configured.trim() !== "") {
    return expand(configured.trim());
  }
  return onPath(exe);
}

function onPath(exe: string): string | undefined {
  for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
    if (dir === "") {
      continue;
    }
    const candidate = path.join(dir, exe);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

/**
 * The `gdls.projectRoot` setting, else the shallowest `project.godot` in the workspace. gdls
 * falls back to the workspace folder itself when this returns nothing.
 */
async function findProjectRoot(): Promise<string | undefined> {
  const configured = vscode.workspace.getConfiguration(SECTION).get<string | null>("projectRoot");
  if (typeof configured === "string" && configured.trim() !== "") {
    const p = expand(configured.trim());
    const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    return path.isAbsolute(p) || !folder ? p : path.join(folder, p);
  }
  const found = await vscode.workspace.findFiles(
    "**/project.godot",
    "**/{.git,.godot,.gdls,node_modules,addons}/**",
    50,
  );
  if (found.length === 0) {
    return undefined;
  }
  const depth = (u: vscode.Uri) => u.fsPath.split(/[\\/]/).length;
  found.sort((a, b) => depth(a) - depth(b) || a.fsPath.localeCompare(b.fsPath));
  if (found.length > 1) {
    output.appendLine(
      `Found ${found.length} project.godot files; using ${found[0].fsPath}. Set gdls.projectRoot to pick another.`,
    );
  }
  return path.dirname(found[0].fsPath);
}

function setStatus(state: "starting" | "running" | "stopped" | "error", tooltip: string): void {
  const icon = { starting: "$(sync~spin)", running: "$(check)", stopped: "$(circle-slash)", error: "$(error)" }[state];
  status.text = `${icon} gdls`;
  status.tooltip = `${tooltip}\nClick to show the gdls output.`;
  status.backgroundColor =
    state === "error" ? new vscode.ThemeColor("statusBarItem.errorBackground") : undefined;
  status.show();
}
