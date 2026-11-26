<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="logo.svg">
    <img src="logo.svg" alt="SolRepr logo" width="320" />
  </picture>
</p>

<h1 align="center" style="margin-bottom: 0;">SolRepr.</h1>
<p align="center" style="margin-top: 4px;">
  A language server for Solidity, built in Rust.
</p>
<p align="center" style="margin-top: 4px;">
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-000000?style=flat-square&logo=rust" alt="Rust"></a>
  <a href="https://soliditylang.org/"><img src="https://img.shields.io/badge/solidity-363636?style=flat-square&logo=solidity" alt="Solidity"></a>
  <a href="https://tree-sitter.github.io/"><img src="https://img.shields.io/badge/tree--sitter-000000?style=flat-square&logo=tree-sitter" alt="Tree-sitter"></a>
  <a href="https://github.com/Honour-d-dev/SolRepr/actions/workflows/ci.yml"><img src="https://github.com/Honour-d-dev/SolRepr/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE-Apache"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square" alt="License"></a>
  <a href="https://github.com/Honour-d-dev/SolRepr"><img src="https://img.shields.io/github/stars/Honour-d-dev/SolRepr?style=flat-square" alt="Stars"></a>
</p>

<p align="center">
  Parsing, name resolution, semantic analysis and type inference.
</p>

---

## Why it's different

SolRepr analyzes Solidity in-process: tree-sitter for syntax, its own HIR, name resolution, semantic analysis and
type inference, memoized by Salsa. Compared with a language server that wraps the compiler, there is
no extra process that runs — no `solc` process per keystroke, no project rebuild per change. That is what keeps
hover, completion and diagnostics responsive and light, on files that have never been compiled and
in projects `solc` was never configured for.

`solc` is planned as a completeness layer for what the analyser deliberately leaves to the compiler
— invoked on demand, never per edit.

## Demos

See [VS Code extension README](extensions/vscode/README.md#features).
hover (including `@inheritdoc` and rendered NatSpec), go to definition,
completion (members, builtins and named arguments), and diagnostics.

## Capabilities

| Capability | Status |
| --- | --- |
| Hover: signatures, NatSpec, `@inheritdoc` | Supported |
| Go to definition | Supported |
| Completion: members, inherited, builtins, `using for`, call options | Supported |
| Diagnostics: push, real-time | Supported |
| Foundry / Hardhat / npm workspace discovery | Supported |
| Hybrid Foundry + Hardhat projects | Supported |
| `solc` completeness layer | Planned |
| Signature help, find references, rename, document/workspace symbols, semantic tokens, syntax highlighting | Planned |

## Status

Early development, pre-1.0: the HIR and the internal APIs still move. Diagnostics come from our own
analysis and aim at semantic correctness; compilation remains `solc`'s job.

## Installation

### VS Code

- **From a release:** download `solrepr-<version>-<platform>.vsix` and run

  ```bash
  code --install-extension solrepr-<version>-<platform>.vsix
  ```
  ##### OR

  Extensions -> ...(views and more actions) -> install from vsix -> the downloaded vsix

  Platform packages are published for Linux x64, macOS Apple Silicon, and Windows x64. The
  universal package covers every other platform.

  The release ships `solrepr-installer.sh` and `solrepr-installer.ps1`, which install the
  server binary to `~/.cargo/bin`.

### Build server binary from source

Prebuilt server binaries exist for `linux-x64`, `darwin-arm64` and `win32-x64`. On any other
platform — or when you would rather build it yourself — install the server with one command:

```bash
cargo install --git https://github.com/Honour-d-dev/SolRepr --tag v<version> --locked
```

- `v<version>` must match the extension's version, so the server speaks the protocol that
  client expects.
- `--locked` builds from the committed `Cargo.lock` — the dependency set CI tested.
- The binary lands in `~/.cargo/bin`, which SolRepr probes directly: a GUI-launched editor
  often does not inherit the shell `PATH` that would otherwise make it visible.
- Needs Rust (<https://rustup.rs>) and a linker — Xcode Command Line Tools on macOS,
  `build-essential` on Linux, the MSVC build tools on Windows.

### Build vsix from source for supported platforms

Build the release server, then package the extension for your platform:

```bash
cargo build --release
cd extensions/vscode
npm ci
npm run package:platform
```

`package:platform` compiles the client, stages the release server, and writes `solrepr-<version>-<platform>.vsix` in `extensions/vscode`.

- The `platform` key is based on the build machine, e.g. `linux-x64`, `darwin-arm64`, or
  `win32-x64`.
- `version` is based on the `extensions/vscode/package.json` version.
- To install:

  ```bash
  code --install-extension "<printed path>"
  ```
  ##### OR

  Extensions -> ...(views and more actions) -> install from vsix -> the built vsix

The universal package (`npm run package`, writes `solrepr.vsix`) bundles **no** binary — it is
for platforms without a prebuilt build. Install it, then build the server with the command in
[Build server binary from source](#build-server-binary-from-source). `solrepr.serverPath` still
overrides every lookup if you keep the binary somewhere else.

### Any editor

`solrepr` speaks LSP over stdio. Put it on `PATH` and point any editor at it. The release's
`solrepr-installer.sh` / `solrepr-installer.ps1` installs it to `~/.cargo/bin`.
The editor still needs a Solidity syntax highlighter; the server provides the analysis.

## Configuration

VS Code settings under **Settings → Extensions → SolRepr**:

| Setting | Default | Description |
| --- | --- | --- |
| `solrepr.serverPath` | `""` | Absolute path to a `solrepr` binary. Overrides the bundled copy, `PATH`, and the development fallback. |
| `solrepr.trace.server` | `"verbose"` | How much LSP traffic the client writes to the **SolRepr Trace** output channel: `off`, `messages`, or `verbose`. |

## Development

```bash
cargo build
cd extensions/vscode
npm install
npm run compile
```

Open the repo root in VS Code and press `F5` (the `.vscode/` launch config points at
`extensions/vscode`), or open `extensions/vscode` directly and press `F5`. A development host runs
the workspace build from `target/debug`, ahead of anything on `PATH`.

Checks:

```bash
cargo test
cargo clippy

cd extensions/vscode
npm run typecheck
```

The server crate is `solrepr`; the VS Code client is `extensions/vscode`.

## References

- [Solc](https://github.com/ethereum/solidity)
- [Rust-analyzer](https://github.com/rust-lang/rust-analyzer) 
- [Solar](https://github.com/paradigmxyz/solar) 
- [Tree-sitter](https://tree-sitter.github.io/)
- [Salsa](https://github.com/salsa-rs/salsa)


## License

MIT OR Apache-2.0.
