import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
    Trace,
} from "vscode-languageclient/node";

type ServerSource = "setting" | "bundled" | "path" | "dev";

export interface ResolvedServer {
    path: string;
    source: ServerSource;
}

export interface ClientChannels {
    output: vscode.OutputChannel;
    trace: vscode.LogOutputChannel;
}

let client: LanguageClient | undefined;

function executableName(): string {
    return process.platform === "win32" ? "solrepr.exe" : "solrepr";
}

function platformKey(): string | undefined {
    const arch =
        process.arch === "x64" ? "x64" : process.arch === "arm64" ? "arm64" : undefined;
    if (!arch) {
        return undefined;
    }
    switch (process.platform) {
        case "linux":
            return `linux-${arch}`;
        case "darwin":
            return `darwin-${arch}`;
        case "win32":
            return `win32-${arch}`;
        default:
            return undefined;
    }
}

function findOnPath(name: string): string | undefined {
    for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
        if (!dir) {
            continue;
        }
        const candidate = path.join(dir, name);
        try {
            fs.accessSync(candidate, fs.constants.X_OK);
            return candidate;
        } catch {
            // Not executable or not present; keep looking.
        }
    }
    return undefined;
}

///@notice Resolves the server binary: explicit setting, bundled copy, PATH, then a
/// development build. Throws with instructions when none is found.
export function resolveServerPath(context: vscode.ExtensionContext): ResolvedServer {
    const configuredPath = vscode.workspace
        .getConfiguration("solrepr")
        .get<string>("serverPath");
    if (configuredPath && configuredPath.trim().length > 0) {
        return { path: configuredPath, source: "setting" };
    }

    const key = platformKey();
    if (key) {
        const bundled = context.asAbsolutePath(
            path.join("bin", key, executableName()),
        );
        if (fs.existsSync(bundled)) {
            return { path: bundled, source: "bundled" };
        }
    }

    const onPath = findOnPath(executableName());
    if (onPath) {
        return { path: onPath, source: "path" };
    }

    if (context.extensionMode === vscode.ExtensionMode.Development) {
        const devCandidates = [
            path.resolve(context.extensionPath, "..", "target", "debug", executableName()),
            path.resolve(
                context.extensionPath,
                "..",
                "solrepr",
                "target",
                "debug",
                executableName(),
            ),
        ];
        for (const candidate of devCandidates) {
            if (fs.existsSync(candidate)) {
                return { path: candidate, source: "dev" };
            }
        }
    }

    throw new Error(
        `solrepr binary not found. Set "solrepr.serverPath", put "${executableName()}" on PATH, or install a platform-specific build.`,
    );
}

/// The language client writes server stderr through `outputChannel.error`, and
/// a `LogOutputChannel` decorates every line with a timestamp and a severity.
/// Routing each level through `appendLine` keeps the server's own formatting.
function asLogChannel(channel: vscode.OutputChannel): vscode.LogOutputChannel {
    const line = (message: string) => channel.appendLine(message);
    return Object.assign(channel, {
        logLevel: vscode.LogLevel.Trace,
        onDidChangeLogLevel: new vscode.EventEmitter<vscode.LogLevel>().event,
        trace: line,
        debug: line,
        info: line,
        warn: line,
        error: line,
    });
}

export async function startClient(
    context: vscode.ExtensionContext,
    channels: ClientChannels,
): Promise<LanguageClient | undefined> {
    let resolved: ResolvedServer;
    try {
        resolved = resolveServerPath(context);
    } catch (error) {
        const msg = String(error);
        channels.output.appendLine(msg);
        vscode.window.showErrorMessage(msg);
        return undefined;
    }

    const serverPath = resolved.path;
    const config = vscode.workspace.getConfiguration("solrepr");
    const traceSetting = config.get<"off" | "messages" | "verbose">(
        "trace.server",
        "verbose",
    );
    channels.output.appendLine("Activating SolRepr extension...");
    channels.output.appendLine(`Resolved server path: ${serverPath} (${resolved.source})`);
    channels.output.appendLine(`Configured trace.server: ${traceSetting}`);
    channels.output.show(true);

    if (!fs.existsSync(serverPath)) {
        const msg = `SolRepr server not found at ${serverPath}. Build it first (cargo build).`;
        channels.output.appendLine(msg);
        vscode.window.showErrorMessage(msg);
        return undefined;
    }

    const isDev = context.extensionMode === vscode.ExtensionMode.Development;
    const env = { ...process.env };
    env.RUST_LOG = isDev ? "solrepr=debug" : "solrepr=info";
    if (isDev) {
        // The dev binary installs the salsa cache observer; this also opts a
        // release binary in when it is used from a development host.
        env.SOLREPR_SALSA_EVENTS = "1";
        // env.SOLREPR_SALSA_HITS = "1"; // also log cache hits (noisy)
    }

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
        outputChannel: asLogChannel(channels.output),
        traceOutputChannel: channels.trace,
    };

    client = new LanguageClient(
        "solrepr",
        "SolRepr",
        serverOptions,
        clientOptions,
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
        channels.output.appendLine(`SolRepr started: ${serverPath}`);
        channels.output.appendLine(`Server env RUST_LOG=${env.RUST_LOG}`);
        channels.output.appendLine(
            `Server env SOLREPR_SALSA_EVENTS=${env.SOLREPR_SALSA_EVENTS ?? "0"}`,
        );
        return client;
    } catch (error) {
        const msg = `Failed to start SolRepr: ${String(error)}`;
        channels.output.appendLine(msg);
        vscode.window.showErrorMessage(msg);
        client = undefined;
        return undefined;
    }
}

export async function stopClient(): Promise<void> {
    if (!client) {
        return;
    }
    await client.stop();
    client = undefined;
}
