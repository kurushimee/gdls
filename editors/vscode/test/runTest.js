// Downloads a stock VS Code, loads this extension into it, and runs test/suite against the fixture project.
const path = require("node:path");
const fs = require("node:fs");
const os = require("node:os");
const { runTests } = require("@vscode/test-electron");

async function main() {
  const root = path.resolve(__dirname, "..");
  // Work on a copy so the server's .gdls/ cache never lands in the repo.
  const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "gdls-vscode-"));
  // The extension host's console does not always reach this process, so the suite writes here.
  const log = path.join(root, ".vscode-test", "results.log");
  fs.mkdirSync(path.dirname(log), { recursive: true });
  fs.writeFileSync(log, "");
  fs.cpSync(path.join(__dirname, "fixture"), workspace, { recursive: true });
  try {
    await runTests({
      version: process.env.VSCODE_VERSION || "stable",
      extensionDevelopmentPath: root,
      extensionTestsPath: path.join(__dirname, "suite", "index.js"),
      launchArgs: [workspace, "--disable-extensions", "--disable-workspace-trust", "--disable-gpu", "--no-sandbox"],
      extensionTestsEnv: { GDLS_TEST_WORKSPACE: workspace, GDLS_TEST_LOG: log },
    });
  } finally {
    process.stdout.write(fs.readFileSync(log, "utf8"));
    fs.rmSync(workspace, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
