"use strict";
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));
var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, "default", { enumerable: true, value: v });
}) : function(o, v) {
    o["default"] = v;
});
var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== "default") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();
Object.defineProperty(exports, "__esModule", { value: true });
exports.activate = activate;
exports.deactivate = deactivate;
const fs = __importStar(require("fs"));
const path = __importStar(require("path"));
const vscode = __importStar(require("vscode"));
const node_1 = require("vscode-languageclient/node");
let client;
///@notice This function tries to get the path to the compiled lsp server
function resolveServerPath(context) {
    //First try to read it from the config file(package.json)
    const config = vscode.workspace.getConfiguration("solidityLsp");
    const configuredPath = config.get("serverPath");
    if (configuredPath && configuredPath.trim().length > 0) {
        return configuredPath;
    }
    return path.resolve(//else default  to this
    context.extensionPath, "..", "lsp_server", "target", "debug", "lsp_server");
}
async function activate(context) {
    const serverPath = resolveServerPath(context);
    const config = vscode.workspace.getConfiguration("solidityLsp");
    const traceSetting = config.get("trace.server", "verbose");
    const outputChannel = vscode.window.createOutputChannel("Solidity LSP");
    const traceOutputChannel = vscode.window.createOutputChannel("Solidity LSP Trace");
    context.subscriptions.push(outputChannel, traceOutputChannel);
    outputChannel.appendLine("Activating Solidity LSP extension...");
    outputChannel.appendLine(`Resolved server path: ${serverPath}`);
    outputChannel.appendLine(`Configured trace.server: ${traceSetting}`);
    outputChannel.show(true);
    if (!fs.existsSync(serverPath)) {
        const msg = `Solidity LSP server not found at ${serverPath}. Build it first (cargo build).`;
        outputChannel.appendLine(msg);
        vscode.window.showErrorMessage(msg);
        return;
    }
    const env = { ...process.env };
    env.RUST_LOG = "lsp_server=debug";
    const serverOptions = {
        command: serverPath,
        args: [],
        options: { env },
    };
    const clientOptions = {
        documentSelector: [
            { scheme: "file", language: "solidity" },
            { scheme: "file", pattern: "**/*.sol" },
        ],
        outputChannel,
        traceOutputChannel,
    };
    client = new node_1.LanguageClient("solidityLsp", "Solidity LSP", serverOptions, clientOptions);
    try {
        await client.start();
        const traceLevel = traceSetting === "verbose"
            ? node_1.Trace.Verbose
            : traceSetting === "messages"
                ? node_1.Trace.Messages
                : node_1.Trace.Off;
        await client.setTrace(traceLevel);
        outputChannel.appendLine(`Solidity LSP started: ${serverPath}`);
        outputChannel.appendLine(`Server env RUST_LOG=${env.RUST_LOG}`);
    }
    catch (error) {
        const msg = `Failed to start Solidity LSP: ${String(error)}`;
        outputChannel.appendLine(msg);
        vscode.window.showErrorMessage(msg);
    }
    // ── Virtual document provider ─────────────────────────────────────
    // Registers a custom URI scheme so that view commands can display
    // output in read-only virtual files.
    const provider = {
        onDidChange: virtualDocChanges.event,
        provideTextDocumentContent(uri) {
            return virtualDocs.get(uri.toString()) ?? "";
        },
    };
    context.subscriptions.push(virtualDocChanges, vscode.workspace.registerTextDocumentContentProvider(VIRTUAL_SCHEME, provider));
    // ── Ignored library dimming ───────────────────────────────────────
    // The server reports library roots the package graph deduplicated out
    // of the compilation unit in `initializeResult.capabilities.experimental
    // .ignoredRoots`. A file decoration provider dims them in the explorer
    const experimental = client.initializeResult?.capabilities.experimental;
    const ignoredRoots = (experimental?.ignoredRoots ?? []).map((uri) => vscode.Uri.parse(uri).fsPath);
    const isIgnored = (fsPath) => fsPath !== undefined &&
        ignoredRoots.some((root) => fsPath === root || fsPath.startsWith(root + path.sep));
    context.subscriptions.push(vscode.window.registerFileDecorationProvider({
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
    }));
    // Explorer dimming only covers the tree — an opened ignored file gets a
    // status bar marker instead, the same signal rust-analyzer uses for files
    // outside the project model.
    const ignoredStatus = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left);
    ignoredStatus.text = "$(exclude) Outside compilation unit";
    ignoredStatus.tooltip =
        "This file is under a deduplicated dependency copy — the server provides no analysis for it.";
    context.subscriptions.push(ignoredStatus);
    const refreshIgnoredStatus = () => {
        if (isIgnored(vscode.window.activeTextEditor?.document.uri.fsPath)) {
            ignoredStatus.show();
        }
        else {
            ignoredStatus.hide();
        }
    };
    context.subscriptions.push(vscode.window.onDidChangeActiveTextEditor(refreshIgnoredStatus));
    refreshIgnoredStatus();
    // ── View HIR command ──────────────────────────────────────────────
    // Sends a custom `solidity/viewHir` request with the user's selection
    // range and displays the lowered HIR expression tree in a virtual
    // read-only file.
    context.subscriptions.push(vscode.commands.registerCommand("solidityLsp.viewHir", async () => {
        await sendViewRequest(client, "solidity/viewHir", "viewHir", "hir");
    }));
    // ── View AST command ──────────────────────────────────────────────
    // Sends a custom `solidity/viewAst` request with the user's selection
    // range and displays the raw tree-sitter named AST tree in a virtual
    // read-only file.
    context.subscriptions.push(vscode.commands.registerCommand("solidityLsp.viewAst", async () => {
        await sendViewRequest(client, "solidity/viewAst", "viewAst", "ast");
    }));
}
/// URI scheme for virtual documents used by view commands.
/// Documents using this scheme are in-memory only — no file on disk,
/// no "save changes?" prompt on close.
const VIRTUAL_SCHEME = "solidity-lsp";
const virtualDocChanges = new vscode.EventEmitter();
/// In-memory store of virtual document contents, keyed by URI.
const virtualDocs = new Map();
/// Shared helper: reads the active editor's selection, sends a custom
/// LSP request with the range, and displays the returned text in a
/// virtual read-only document beside the editor.
async function sendViewRequest(client, method, label, kind) {
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
        const result = await client.sendRequest(method, {
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
    }
    catch (err) {
        vscode.window.showErrorMessage(`${label} failed: ${String(err)}`);
    }
}
async function deactivate() {
    if (!client) {
        return;
    }
    await client.stop();
}
//# sourceMappingURL=extension.js.map