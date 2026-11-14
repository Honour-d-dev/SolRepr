#!/usr/bin/env node
import { copyFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { resolve } from "node:path";

const extensionRoot = resolve(import.meta.dirname, "..");
const repoRoot = resolve(extensionRoot, "..", "..");
const npm = process.platform === "win32" ? "npm.cmd" : "npm";
const npx = process.platform === "win32" ? "npx.cmd" : "npx";

function platformKey() {
    if (process.platform === "linux" && process.arch === "x64") {
        return "linux-x64";
    }
    if (process.platform === "darwin" && process.arch === "arm64") {
        return "darwin-arm64";
    }
    if (process.platform === "win32" && process.arch === "x64") {
        return "win32-x64";
    }
    return undefined;
}

const key = platformKey();
if (!key) {
    console.error(
        `unsupported platform: ${process.platform}-${process.arch}; use \`npm run package\` for the universal package`,
    );
    process.exit(1);
}

const executable = process.platform === "win32" ? "solrepr.exe" : "solrepr";
const source = resolve(repoRoot, "target", "release", executable);
if (!existsSync(source)) {
    console.error(
        `missing ${source}; run \`cargo build --release\` from the repo root first`,
    );
    process.exit(1);
}

execFileSync(npm, ["run", "compile"], { cwd: extensionRoot, stdio: "inherit" });

const destination = resolve(extensionRoot, "bin", key, executable);
mkdirSync(resolve(extensionRoot, "bin", key), { recursive: true });
copyFileSync(source, destination);

const version = JSON.parse(
    readFileSync(resolve(extensionRoot, "package.json"), "utf8"),
).version;
const out = resolve(extensionRoot, `solrepr-${version}-${key}.vsix`);
execFileSync(
    npx,
    ["vsce", "package", "--target", key, "--no-dependencies", "--out", out],
    { cwd: extensionRoot, stdio: "inherit" },
);

console.log(`packaged ${out}`);
console.log(`install with: code --install-extension "${out}" --force`);
