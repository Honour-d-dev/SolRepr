//! SolRepr — a semantic analyzer and language server for Solidity.
//!
//! `main.rs` is the CLI shell; `tests/e2e.rs` drives the protocol through
//! [`lsp::SolidityLspServer`]. Everything else is crate-internal.

mod ast;
mod capabilities;
mod hir;
mod ir;
mod live_docs;
mod salsa;
mod utilities;
mod workspace;

pub mod lsp;
