import * as vscode from "vscode";
import { startClient, stopClient } from "./client";
import { registerIgnoredRoots } from "./ignoredRoots";
import { registerViews } from "./views";

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    const output = vscode.window.createOutputChannel("SolRepr");
    const trace = vscode.window.createOutputChannel("SolRepr Trace", { log: true });
    context.subscriptions.push(output, trace);

    const client = await startClient(context, { output, trace });
    if (!client) {
        return;
    }

    registerViews(context, client);
    registerIgnoredRoots(context, client);
}

export async function deactivate(): Promise<void> {
    await stopClient();
}
