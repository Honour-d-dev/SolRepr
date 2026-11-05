import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  Trace,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

///@notice This function tries to get the path to the compiled lsp server
function resolveServerPath(context: vscode.ExtensionContext): string {
  //First try to read it from the config file(package.json)
  const config = vscode.workspace.getConfiguration("solrepr");
  const configuredPath = config.get<string>("serverPath");
  if (configuredPath && configuredPath.trim().length > 0) {
    return configuredPath;
  }

  return path.resolve(//else default  to this
    context.extensionPath,
    "..",
    "solrepr",
    "target",
    "debug",
    "solrepr"
  );
}

export async function activate(context: vscode.ExtensionContext) {
  const serverPath = resolveServerPath(context);
  const config = vscode.workspace.getConfiguration("solrepr");
  const traceSetting = config.get<"off" | "messages" | "verbose">(
    "trace.server",
    "verbose"
  );
  const outputChannel = vscode.window.createOutputChannel("SolRepr");
  const traceOutputChannel = vscode.window.createOutputChannel("SolRepr Trace");
  context.subscriptions.push(outputChannel, traceOutputChannel);
  outputChannel.appendLine("Activating SolRepr extension...");
  outputChannel.appendLine(`Resolved server path: ${serverPath}`);
  outputChannel.appendLine(`Configured trace.server: ${traceSetting}`);
  outputChannel.show(true);

  if (!fs.existsSync(serverPath)) {
    const msg = `SolRepr server not found at ${serverPath}. Build it first (cargo build).`;
    outputChannel.appendLine(msg);
    vscode.window.showErrorMessage(msg);
    return;
  }

  const env = { ...process.env };
  env.RUST_LOG = "solrepr=debug";

  const serverOptions: ServerOptions = {
    command: serverPath,
    args: [],
    options: { env },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "solidity" },
      { scheme: "file", pattern: "**/*.sol" },
    ],
    outputChannel,
    traceOutputChannel,
  };

  client = new LanguageClient(
    "solrepr",
    "SolRepr",
    serverOptions,
    clientOptions
  );

  try {
    await client.start();
    const traceLevel =
      traceSetting === "verbose"
        ? Trace.Verbose
        : traceSetting === "messages"
        ? Trace.Messages
        : Trace.Off;
    await client.setTrace(traceLevel);
    outputChannel.appendLine(`SolRepr started: ${serverPath}`);
    outputChannel.appendLine(`Server env RUST_LOG=${env.RUST_LOG}`);
  } catch (error) {
    const msg = `Failed to start SolRepr: ${String(error)}`;
    outputChannel.appendLine(msg);
    vscode.window.showErrorMessage(msg);
  }

  // ── Virtual document provider ─────────────────────────────────────
  // Registers a custom URI scheme so that view commands can display
  // output in read-only virtual files.
  const provider: vscode.TextDocumentContentProvider = {
    onDidChange: virtualDocChanges.event,

    provideTextDocumentContent(uri: vscode.Uri): string {
      return virtualDocs.get(uri.toString()) ?? "";
    },
  };
  context.subscriptions.push(
    virtualDocChanges,
    vscode.workspace.registerTextDocumentContentProvider(VIRTUAL_SCHEME, provider)
  );

  // ── Ignored library dimming ───────────────────────────────────────
  // The server reports library roots the package graph deduplicated out
  // of the compilation unit in `initializeResult.capabilities.experimental
  // .ignoredRoots`. A file decoration provider dims them in the explorer
  const experimental = client.initializeResult?.capabilities.experimental as | { ignoredRoots?: string[] } | undefined;
  const ignoredRoots = (experimental?.ignoredRoots ?? []).map(
    (uri) => vscode.Uri.parse(uri).fsPath
  );

  const isIgnored = (fsPath: string | undefined): boolean =>
    fsPath !== undefined &&
    ignoredRoots.some(
      (root) => fsPath === root || fsPath.startsWith(root + path.sep)
    );

  context.subscriptions.push(
    vscode.window.registerFileDecorationProvider({
      provideFileDecoration(uri) {
        if (!isIgnored(uri.fsPath)) {
          return undefined;
        }
        return {
          color: new vscode.ThemeColor("list.deemphasizedForeground"),
          tooltip: "Deduplicated dependency copy — outside the compilation unit",
          propagate: true,
        };
      },
    })
  );

  // Explorer dimming only covers the tree — an opened ignored file gets a
  // status bar marker instead, the same signal rust-analyzer uses for files
  // outside the project model.
  const ignoredStatus = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Left
  );
  ignoredStatus.text = "$(exclude) Outside compilation unit";
  ignoredStatus.tooltip =
    "This file is under a deduplicated dependency copy — the server provides no analysis for it.";
  context.subscriptions.push(ignoredStatus);

  const refreshIgnoredStatus = () => {
    if (isIgnored(vscode.window.activeTextEditor?.document.uri.fsPath)) {
      ignoredStatus.show();
    } else {
      ignoredStatus.hide();
    }
  };
  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor(refreshIgnoredStatus)
  );
  refreshIgnoredStatus();

  // ── View HIR command ──────────────────────────────────────────────
  // Sends a custom `solidity/viewHir` request with the user's selection
  // range and displays the lowered HIR expression tree in a virtual
  // read-only file.
  context.subscriptions.push(
    vscode.commands.registerCommand("solrepr.viewHir", async () => {
      await sendViewRequest(client, "solidity/viewHir", "viewHir", "hir");
    })
  );

  // ── View AST command ──────────────────────────────────────────────
  // Sends a custom `solidity/viewAst` request with the user's selection
  // range and displays the raw tree-sitter named AST tree in a virtual
  // read-only file.
  context.subscriptions.push(
    vscode.commands.registerCommand("solrepr.viewAst", async () => {
      await sendViewRequest(client, "solidity/viewAst", "viewAst", "ast");
    })
  );
}

/// URI scheme for virtual documents used by view commands.
/// Documents using this scheme are in-memory only — no file on disk,
/// no "save changes?" prompt on close.
const VIRTUAL_SCHEME = "solrepr";

const virtualDocChanges = new vscode.EventEmitter<vscode.Uri>();

/// In-memory store of virtual document contents, keyed by URI.
const virtualDocs = new Map<string, string>();

/// Shared helper: reads the active editor's selection, sends a custom
/// LSP request with the range, and displays the returned text in a
/// virtual read-only document beside the editor.
async function sendViewRequest(
  client: LanguageClient | undefined,
  method: string,
  label: string,
  kind: string,
): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    vscode.window.showWarningMessage("No active editor.");
    return;
  }
  if (!client) {
    vscode.window.showErrorMessage("LSP client is not running.");
    return;
  }

  const textDocument = editor.document.uri;
  const start = editor.selection.start;
  const end = editor.selection.end;

  try {
    const result = await client.sendRequest<{ content: string }>(method, {
      textDocument: { uri: textDocument.toString() },
      start: { line: start.line, character: start.character },
      end: { line: end.line, character: end.character },
    });

    const content = result?.content ?? `(no output from ${label})`;

    // Store content and open via the virtual document provider.
    // Using a unique URI per kind so repeated invocations replace
    // the same tab rather than spawning new ones.
    const uri = vscode.Uri.parse(`${VIRTUAL_SCHEME}:${kind}.${kind === "hir" ? "rs" : "txt"}`);
    virtualDocs.set(uri.toString(), content);

    const doc = await vscode.workspace.openTextDocument(uri);
    virtualDocChanges.fire(uri);
    await vscode.window.showTextDocument(doc, {
      viewColumn: vscode.ViewColumn.Beside,
      preview: false,
    });
  } catch (err) {
    vscode.window.showErrorMessage(`${label} failed: ${String(err)}`);
  }
}

export async function deactivate() {
  if (!client) {
    return;
  }
  await client.stop();
}
