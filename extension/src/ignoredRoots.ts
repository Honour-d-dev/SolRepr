import * as path from "path";
import * as vscode from "vscode";
import type { LanguageClient } from "vscode-languageclient/node";

export function registerIgnoredRoots(
    context: vscode.ExtensionContext,
    client: LanguageClient,
): void {
    // The server reports library roots the package graph deduplicated out
    // of the compilation unit in `initializeResult.capabilities.experimental
    // .ignoredRoots`. A file decoration provider dims them in the explorer.
    const experimental = client.initializeResult?.capabilities.experimental as
        | { ignoredRoots?: string[] }
        | undefined;
    const ignoredRoots = (experimental?.ignoredRoots ?? []).map(
        (uri) => vscode.Uri.parse(uri).fsPath,
    );

    const isIgnored = (fsPath: string | undefined): boolean =>
        fsPath !== undefined &&
        ignoredRoots.some(
            (root) => fsPath === root || fsPath.startsWith(root + path.sep),
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
        }),
    );

    // Explorer dimming only covers the tree — an opened ignored file gets a
    // status bar marker instead, the same signal rust-analyzer uses for files
    // outside the project model.
    const ignoredStatus = vscode.window.createStatusBarItem(
        vscode.StatusBarAlignment.Left,
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
        vscode.window.onDidChangeActiveTextEditor(refreshIgnoredStatus),
    );
    refreshIgnoredStatus();
}
