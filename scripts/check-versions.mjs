#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");

function cargoVersion() {
    const text = readFileSync(resolve(root, "solrepr/Cargo.toml"), "utf8");
    let inPackage = false;
    for (const line of text.split(/\r?\n/)) {
        const trimmed = line.trim();
        if (trimmed.startsWith("[")) {
            inPackage = trimmed === "[package]";
            continue;
        }
        if (inPackage) {
            const match = /^version\s*=\s*"([^"]+)"/.exec(trimmed);
            if (match) {
                return match[1];
            }
        }
    }
    throw new Error("no version in solrepr/Cargo.toml [package]");
}

function extensionVersion() {
    const manifest = JSON.parse(
        readFileSync(resolve(root, "extensions/vscode/package.json"), "utf8"),
    );
    return manifest.version;
}

const crate = cargoVersion();
const extension = extensionVersion();
const tag = process.argv[2] ?? process.env.GITHUB_REF_NAME ?? "";

const problems = [];
if (crate !== extension) {
    problems.push(
        `solrepr/Cargo.toml is ${crate}, extensions/vscode/package.json is ${extension}`,
    );
}

const tagVersion = tag.startsWith("v") ? tag.slice(1) : tag;
if (/^\d+\.\d+\.\d+/.test(tagVersion) && tagVersion !== crate) {
    problems.push(`tag ${tag} does not match crate version ${crate}`);
}

if (problems.length > 0) {
    console.error("version mismatch:");
    for (const problem of problems) {
        console.error(`  - ${problem}`);
    }
    process.exit(1);
}

console.log(
    `versions agree: crate ${crate}, extension ${extension}${tag ? `, tag ${tag}` : ""}`,
);
