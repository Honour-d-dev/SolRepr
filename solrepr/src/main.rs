mod ast;
mod capabilities;
mod hir;
mod ir;
mod live_docs;
mod lsp;
mod salsa;
mod utilities;
mod workspace;

use lsp::SolidityLspServer;

use anyhow::Context;
use lsp_server::Connection;

const USAGE: &str = concat!(
    env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"), "\n",
    "Semantic analyzer and language server for Solidity.\n",
    "\n",
    "USAGE:\n",
    "    ", env!("CARGO_PKG_NAME"), " [OPTIONS]\n",
    "\n",
    "OPTIONS:\n",
    "    -h, --help       Print this help and exit\n",
    "    -V, --version    Print version and exit\n",
    "        --stdio      init the server; stdio is the only transport\n",
    "\n",
    "With no options, the server speaks LSP over stdin/stdout."
);

fn main() -> anyhow::Result<()> {
    // tiny cli wrapper to support basic commands like version etc
    match std::env::args().nth(1).as_deref() {
        None | Some("--stdio") => {
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("solrepr=debug"))
                .add_directive(
                    "lsp_server::msg=off"
                        .parse()
                        .expect("valid tracing directive"),
                )
                .add_directive(
                    "lsp_server::stdio=off"
                        .parse()
                        .expect("valid tracing directive"),
                );

            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .without_time()
                .with_level(false)
                .with_target(false)
                .init();
            tracing::info!("solrepr {} starting", env!("CARGO_PKG_VERSION"));

            let (connection, io_threads) = Connection::stdio();

            SolidityLspServer::new(connection).and_then(|(server, rx)| server.run(rx))?;

            io_threads.join().context("failed to join LSP IO threads")?;
        }
        Some("--version" | "-V") => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
        }
        Some(other) => anyhow::bail!(
            "unrecognized argument `{other}`\nrun `{} --help` for usage",
            env!("CARGO_PKG_NAME")
        ),
    }
    Ok(())
}
