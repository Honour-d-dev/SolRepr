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

fn main() -> anyhow::Result<()> {
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
    tracing::info!("solidity lsp server starting");

    let (connection, io_threads) = Connection::stdio();

    // The initialize handshake runs inside `new`: the server's response —
    // capabilities plus the ignored roots — needs the client's params.
    SolidityLspServer::new(&connection)?.run(connection.receiver)?;

    io_threads.join().context("failed to join LSP IO threads")?;
    Ok(())
}
