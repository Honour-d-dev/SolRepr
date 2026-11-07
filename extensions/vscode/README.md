# SolRepr

A semantic analyzer and language server for Solidity.

This extension runs the `solrepr` language server and provides hover, go-to-definition,
completion, diagnostics, and AST/HIR inspection.

## Requirements

- VS Code 1.91 or newer.
- The extension ships the server binary for your platform. If you built the server yourself,
  point `solrepr.serverPath` at it, or put `solrepr` on your `PATH`.

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `solrepr.serverPath` | `""` | Path to the server binary. Overrides the bundled copy. |
| `solrepr.trace.server` | `"verbose"` | LSP trace level: `off`, `messages`, `verbose`. |

## Commands

- **SolRepr: View HIR (Lowered)** — show the lowered HIR for the selection.
- **SolRepr: View AST (Named Tree)** — show the tree-sitter AST for the selection.

## Source

https://github.com/Honour-d-dev/SolRepr
