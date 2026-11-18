#!/usr/bin/env node
// Bumps the release version everywhere it is recorded, then verifies the tree agrees.
// The tag is left to the caller: workflows check out the tag, so it must point at the bump.
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");

const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

function fail(message) {
    console.error(message);
    process.exit(1);
}

const argument = process.argv[2];
if (!argument) {
    fail(
        "usage: node scripts/bump-version.mjs <version>   (e.g. 0.1.0-beta.2 or v0.1.0-beta.2)",
    );
}
const version = argument.startsWith("v") ? argument.slice(1) : argument;
if (!SEMVER.test(version)) {
    fail(`not a semver version: ${argument}`);
}

/// The `version` key of `[section]`; `undefined` when the section has none.
function sectionVersion(text, section, replacement) {
    const lines = text.split("\n");
    let inside = false;
    for (let index = 0; index < lines.length; index++) {
        const trimmed = lines[index].trim();
        if (trimmed.startsWith("[")) {
            inside = trimmed === `[${section}]`;
            continue;
        }
        if (!inside) {
            continue;
        }
        const match = /^(\s*version\s*=\s*")([^"]+)(")/.exec(lines[index]);
        if (match) {
            lines[index] = `${match[1]}${replacement ?? match[2]}${match[3]}`;
            return { previous: match[2], text: lines.join("\n") };
        }
    }
    return undefined;
}

/// The `version` line of the `[[package]]` block named `name`.
function lockEntryVersion(text, name, replacement) {
    const lines = text.split("\n");
    for (let index = 0; index < lines.length; index++) {
        if (lines[index].trim() !== `name = "${name}"`) {
            continue;
        }
        const match = /^(\s*version\s*=\s*")([^"]+)(")/.exec(lines[index + 1] ?? "");
        if (!match) {
            return undefined;
        }
        lines[index + 1] = `${match[1]}${replacement ?? match[2]}${match[3]}`;
        return { previous: match[2], text: lines.join("\n") };
    }
    return undefined;
}

/// A package manifest, or a lockfile's two project entries (`version` and
/// `packages[""]`), re-serialized — npm writes both files this way.
function manifestVersion(text, replacement, lockfile) {
    const manifest = JSON.parse(text);
    const previous = manifest.version;
    manifest.version = replacement ?? previous;
    if (lockfile && manifest.packages?.[""]) {
        manifest.packages[""].version = manifest.version;
    }
    return { previous, text: `${JSON.stringify(manifest, null, 2)}\n` };
}

const crate = sectionVersion(
    readFileSync(resolve(root, "solrepr/Cargo.toml"), "utf8"),
    "package",
);
if (!crate) {
    fail("no version in solrepr/Cargo.toml [package]");
}
if (crate.previous === version) {
    fail(`already at ${version}`);
}

const edits = [
    [
        "solrepr/Cargo.toml",
        (text) => sectionVersion(text, "package", version)?.text,
    ],
    [
        "Cargo.lock",
        (text) => lockEntryVersion(text, "solrepr", version)?.text,
    ],
    [
        "extensions/vscode/package.json",
        (text) => manifestVersion(text, version, false).text,
    ],
    [
        "extensions/vscode/package-lock.json",
        (text) => manifestVersion(text, version, true).text,
    ],
];

const changed = [];
for (const [relative, bump] of edits) {
    const absolute = resolve(root, relative);
    if (!existsSync(absolute)) {
        console.log(`skipped ${relative} (not present)`);
        continue;
    }
    const before = readFileSync(absolute, "utf8");
    const after = bump(before);
    if (after === undefined) {
        fail(`could not find the version in ${relative}`);
    }
    if (after !== before) {
        changed.push({ absolute, after });
    }
}

if (changed.length === 0) {
    fail(`nothing to bump in any manifest (already at ${version}?)`);
}
for (const { absolute, after } of changed) {
    writeFileSync(absolute, after);
}

console.log(`bumped ${crate.previous} -> ${version}`);
for (const [relative] of edits) {
    console.log(`  ${relative}`);
}

execFileSync(
    process.execPath,
    [resolve(root, "scripts/check-versions.mjs"), `v${version}`],
    { stdio: "inherit" },
);

console.log(`
next:
  git add solrepr/Cargo.toml Cargo.lock extensions/vscode/package.json extensions/vscode/package-lock.json
  git commit -m "v${version}"
  git tag v${version}
  git push && git push --tags
`);
