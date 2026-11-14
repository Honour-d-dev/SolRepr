<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="logo.svg">
    <img src="logo.svg" alt="SolRepr logo" width="320" />
  </picture>
</p>

<h1 align="center" style="margin-bottom: 0;">SolRepr</h1>
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
  Solidity's semantic repr. analysis and inference engine, leveraging tree-sitter and salsa
</p>

---

#### Description

- **Semantic, not syntactic.** Name resolution, overload selection, access and
  override checks, builtin member lookup, and `using ... for` resolution.
- **Incremental analysis.** Leverages salsa's incremental computation.
- **Workspace-aware.** Supports Foundry, Hardhat, hybrid Foundry + Hardhat workspaces and even bare `.sol` folders.
- **NatSpec that follows inheritance.** `@inheritdoc` expands against the matching base member, including overload selection and multi-level chains, so hover shows the documentation the author intended.


## Capabilities

| Capability | Status |
| --- | --- |
| Hover: signatures, NatSpec, `@inheritdoc` | Supported |
| Go to definition | Supported |
| Completion: members, inherited, builtins, `using for`, call options | Supported |
| Diagnostics: push, real-time | Supported |
| Foundry / Hardhat / npm workspace discovery | Supported |
| Hybrid Foundry + Hardhat projects | Supported, hardening in progress |
| Signature help, find references, rename, document/workspace symbols, semantic tokens | Planned |


## Installation

### VS Code


- **From a release:** download `solrepr-<version>-<platform>.vsix` and run

  ```bash
  code --install-extension solrepr-<version>-<platform>.vsix
  ```

  Platform packages are published for Linux x64, macOS Apple Silicon, and Windows x64. The
  universal package covers every other platform.

  The release ships `solrepr-installer.sh` and `solrepr-installer.ps1`, which install the
  server binary to `~/.cargo/bin`.

### From source

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
  code --install-extension "<printed path>" --force
  ```

For the universal package (no bundled binary), run `npm run package`; it writes `solrepr.vsix`.
Install the same way, then point `solrepr.serverPath` at the binary or put it on `PATH`.

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



## Status

Early development.

## Development

```bash
cargo build
cd extensions/vscode
npm install
npm run compile
```

Open the repo root in VS Code and press `F5` (the `.vscode/` launch config points at
`extensions/vscode`), or open `extensions/vscode` directly and press `F5`. 

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
