#![recursion_limit = "256"]

use bpaf::{construct, OptionParser, Parser};
use logging::{tracing_init, tracing_shutdown};
use server::TypstServer;
use server::{log::LspLayer, ui::Ui};
use tower_lsp::{LspService, Server};
use tracing_subscriber::{reload, Registry};

mod command;
mod config;
mod ext;
mod logging;
mod lsp_typst_boundary;
mod server;
mod workspace;

pub const TYPST_VERSION: &str = env!("TYPST_VERSION");

#[tokio::main]
async fn main() {
    let lsp_tracing_layer_handle = tracing_init();
    run(lsp_tracing_layer_handle).await;
    tracing_shutdown();
}

#[tracing::instrument(skip_all)]
async fn run(lsp_tracing_layer_handle: reload::Handle<Option<LspLayer>, Registry>) {
    let _args = arg_parser().run();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (to_ui_tx, to_ui_rx) = tokio::sync::mpsc::channel(10);

    let workspace: std::sync::Arc<
        once_cell::sync::OnceCell<std::sync::Arc<tokio::sync::RwLock<workspace::Workspace>>>,
    > = Default::default();

    let (tx, rx) = tokio::sync::oneshot::channel();

    let workspace_for_server = std::sync::Arc::clone(&workspace);
    let (service, socket) = LspService::new(move |client| {
        tx.send(client.clone()).unwrap();
        TypstServer::new(
            client,
            lsp_tracing_layer_handle,
            to_ui_tx,
            workspace_for_server,
        )
    });

    let server_fut = Server::new(stdin, stdout, socket).serve(service);
    let ui_fut = Ui::run(workspace, rx.await.unwrap(), to_ui_rx);

    futures::join!(server_fut, ui_fut);
}

#[derive(Debug, Clone)]
struct Args {}

fn arg_parser() -> OptionParser<Args> {
    construct!(Args {}).to_options().version(
        format!(
            "{}, commit {} (Typst version {TYPST_VERSION})",
            env!("CARGO_PKG_VERSION"),
            env!("GIT_COMMIT")
        )
        .as_str(),
    )
}
