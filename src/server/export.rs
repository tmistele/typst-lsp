use std::sync::Arc;

use tower_lsp::lsp_types::{Range, Url};
use tracing::info;
use typst::foundations::Smart;
use typst::model::Document;

use super::ui;
use super::TypstServer;

impl TypstServer {
    #[tracing::instrument(skip(self))]
    pub async fn export_pdf(
        &self,
        source_uri: &Url,
        document: Arc<Document>,
        first_change_range: Option<Range>,
    ) -> anyhow::Result<()> {
        info!("updating UI");

        self.to_ui_tx
            .send(ui::NewDocumentMessage {
                document,
                source_uri: source_uri.clone(),
                first_change_range,
            })
            .await?;

        Ok(())
    }
}
