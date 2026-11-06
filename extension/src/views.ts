import * as vscode from "vscode";
import type { LanguageClient } from "vscode-languageclient/node";

/// URI scheme for virtual documents used by view commands.
/// Documents using this scheme are in-memory only — no file on disk,
/// no "save changes?" prompt on close.
const VIRTUAL_SCHEME = "solrepr";

const virtualDocChanges = new vscode.EventEmitter<vscode.Uri>();

/// In-memory store of virtual document contents, keyed by URI.
const virtualDocs = new Map<string, string>();

export function registerViews(
    context: vscode.ExtensionContext,
    client: LanguageClient,
): void {
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
        vscode.workspace.registerTextDocumentContentProvider(VIRTUAL_SCHEME, provider),
    );

    // Sends a custom `solidity/viewHir` request with the user's selection
    // range and displays the lowered HIR expression tree in a virtual
    // read-only file.
    context.subscriptions.push(
        vscode.commands.registerCommand("solrepr.viewHir", async () => {
            await sendViewRequest(client, "solidity/viewHir", "viewHir", "hir");
        }),
    );

    // Sends a custom `solidity/viewAst` request with the user's selection
    // range and displays the raw tree-sitter named AST tree in a virtual
    // read-only file.
    context.subscriptions.push(
        vscode.commands.registerCommand("solrepr.viewAst", async () => {
            await sendViewRequest(client, "solidity/viewAst", "viewAst", "ast");
        }),
    );
}

/// Shared helper: reads the active editor's selection, sends a custom
/// LSP request with the range, and displays the returned text in a
/// virtual read-only document beside the editor.
async function sendViewRequest(
    client: LanguageClient,
    method: string,
    label: string,
    kind: string,
): Promise<void> {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
        vscode.window.showWarningMessage("No active editor.");
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
        const uri = vscode.Uri.parse(
            `${VIRTUAL_SCHEME}:${kind}.${kind === "hir" ? "rs" : "txt"}`,
        );
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
