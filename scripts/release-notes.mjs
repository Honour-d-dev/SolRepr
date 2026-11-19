#!/usr/bin/env node
// Assembles a release body: what the release contains, how to install the extension, then
// cargo-dist's generated server notes. Idempotent — dist's body comes from the release's own
// manifest, so re-running the workflow rewrites rather than appends.
// usage: scripts/release-notes.mjs <version> <tag> <dist-manifest.json>
import { readFileSync } from "node:fs";

const [version, tag, manifestPath] = process.argv.slice(2);
if (!version || !tag || !manifestPath) {
    console.error(
        "usage: scripts/release-notes.mjs <version> <tag> <dist-manifest.json>",
    );
    process.exit(1);
}

const repository = process.env.GITHUB_REPOSITORY ?? "Honour-d-dev/SolRepr";
const base = `https://github.com/${repository}/releases/download/${tag}`;

let dist = "";
try {
    dist = JSON.parse(readFileSync(manifestPath, "utf8")).announcement_github_body ?? "";
} catch (error) {
    console.error(`could not read ${manifestPath}: ${error.message}`);
    process.exit(1);
}

const vsix = (target, platform) => {
    const name = `solrepr-${version}-${target}.vsix`;
    return `| [${name}](${base}/${name}) | ${platform} |`;
};

const body = `## SolRepr ${version}

A semantic analyzer and language server for Solidity. Parsing, name resolution and type
inference run in-process, so nothing is compiled and no \`solc\` process runs as you type.

### What's in this release

- **Hover** — resolved types and signatures, with NatSpec and inline \`///\` docs rendered as
  markdown. \`@inheritdoc\` is expanded through the inheritance chain.
- **Go to definition** — declarations, inherited members, and imports across packages.
- **Completions** — locals, inherited members, builtins, \`using ... for\` functions, call
  options and named arguments.
- **Diagnostics** — semantic errors in real time, with the related locations they were decided
  from.
- **Workspace discovery** — Foundry, Hardhat, hybrid projects, npm and remappings, multi-root
  workspaces, and standalone \`.sol\` files, with no configuration.
- **Inspection** — the lowered HIR and the parsed tree-sitter AST for any selection.

### VS Code extension

| File | Platform |
| --- | --- |
${vsix("linux-x64", "Linux x64")}
${vsix("darwin-arm64", "Apple Silicon macOS")}
${vsix("win32-x64", "x64 Windows")}
${vsix("universal", "Universal — set `solrepr.serverPath`, or put `solrepr` on `PATH`")}

Install it either way:

- **From the UI** — open the Extensions view, open the **More Actions** menu (the \`...\` button
  at the top of the view), choose **Install from VSIX…**, then pick the file for your platform.
- **From the command line**:

\`\`\`bash
code --install-extension solrepr-${version}-<platform>.vsix
\`\`\`

Requires VS Code 1.91 or newer. SolRepr ships no grammar and no colours — pair it with
[Solidity Language & Themes](https://marketplace.visualstudio.com/items?itemName=tintinweb.vscode-solidity-language)
for syntax highlighting.

---

${dist.trim()}

---

See the [README](https://github.com/${repository}#readme) for what each feature looks like.
`;

process.stdout.write(body);
