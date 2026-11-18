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
    const mode = process.platform === "win32" ? fs.constants.F_OK : fs.constants.X_OK;
    for (const dir of (process.env.PATH ?? "").split(path.delimiter)) {
        if (!dir) {
            continue;
        }
        const candidate = path.join(dir, name);
        try {
            fs.accessSync(candidate, mode);
            return candidate;
        } catch {
            // Not executable or not present; keep looking.
        }
    }
    return undefined;
}

/// The workspace build a development host runs, debug first — the profile
/// `cargo build` writes there.
function findDevBuild(context: vscode.ExtensionContext): string | undefined {
    const root = path.resolve(context.extensionPath, "..", "..");
    const candidates = [
        path.join(root, "target", "debug", executableName()),
        path.join(root, "target", "release", executableName()),
        path.join(root, "solrepr", "target", "debug", executableName()),
    ];
    return candidates.find((candidate) => fs.existsSync(candidate));
}

///@notice Resolves the server binary: explicit setting, a development build in a
/// development host, bundled copy, then PATH. Throws with instructions when none is found.
export function resolveServerPath(context: vscode.ExtensionContext): ResolvedServer {
    const configuredPath = vscode.workspace
        .getConfiguration("solrepr")
        .get<string>("serverPath");
    if (configuredPath && configuredPath.trim().length > 0) {
        return { path: configuredPath, source: "setting" };
    }

    const isDev = context.extensionMode === vscode.ExtensionMode.Development;
    // The workspace build is the one being edited, so it outranks an installed
    // copy on PATH or a bundled binary.
    const devBuild = isDev ? findDevBuild(context) : undefined;
    if (devBuild) {
        return { path: devBuild, source: "dev" };
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

    throw new Error(
        isDev
            ? `solrepr binary not found. Run \`cargo build\` at the repo root, set "solrepr.serverPath", or put "${executableName()}" on PATH.`
            : `solrepr binary not found. Set "solrepr.serverPath", put "${executableName()}" on PATH, or install a platform-specific build.`,
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
    // The channel stays in the Output dropdown; activating the extension must
    // not steal the panel from whatever the user was reading.

    if (!fs.existsSync(serverPath)) {
        const msg = `SolRepr server not found at ${serverPath}. Build it first (cargo build).`;
        channels.output.appendLine(msg);
        vscode.window.showErrorMessage(msg);
        return undefined;
    }

    const isDev = context.extensionMode === vscode.ExtensionMode.Development;
    // The level is the only cache-logging knob: `debug` shows misses, `trace`
    // adds hits and blocks. A release binary installs no subscriber at all, so
    // this only has an effect on a development build.
    const env = { ...process.env };
    env.RUST_LOG = isDev ? "solrepr=debug" : "solrepr=info";

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
