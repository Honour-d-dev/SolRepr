<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="logo-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="logo.svg">
    <img src="logo.svg" alt="SolRepr logo" width="320" />
  </picture>
</p>

<h1 align="center" style="margin-bottom: 0;">SolRepr</h1>
<p align="center" style="margin-top: 4px;">
  A Solidity semantic repr. analysis engine and language server, built in Rust.
</p>
<p align="center" style="margin-top: 4px;">
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/rust-000000?style=flat-square&logo=rust" alt="Rust"></a>
  <a href="https://soliditylang.org/"><img src="https://img.shields.io/badge/solidity-363636?style=flat-square&logo=solidity" alt="Solidity"></a>
  <a href="https://tree-sitter.github.io/"><img src="https://img.shields.io/badge/tree--sitter-000000?style=flat-square&logo=tree-sitter" alt="Tree-sitter"></a>
  <a href="LICENSE-Apache"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square" alt="License"></a>
  <a href="https://github.com/Honour-d-dev/SolReapr"><img src="https://img.shields.io/github/stars/Honour-d-dev/SolReapr?style=flat-square" alt="Stars"></a>
</p>

<p align="center">
  A Rust-Analyzer-like language server for Solidity. Early development stage.<br />
  Best of both worlds: Incremental reparsing + incremental recomputation leveraging tree-sitter + salsa.
</p>

---

## Getting Started

To run:

```bash
cargo build
cd extensions/vscode
npm install
npm run compile
```

Then open the project (`extensions/vscode/src/extension.ts`) in VS Code and press `F5` to start the extension in debug mode.

## Capabilities

- Hover 👍
- Goto definition 👍
- Auto-completion 👍
- Diagnostics (push) 👍

### Notice
Windows pathing and hardhat/hardhat-foundry hybrid projects not fully supported yet.