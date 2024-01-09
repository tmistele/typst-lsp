use std::sync::Arc;

use tower_lsp::lsp_types::Url;
use tracing::info;
use typst::foundations::Smart;
use typst::model::Document;

use super::TypstServer;

impl TypstServer {
    #[tracing::instrument(skip(self))]
    pub async fn export_pdf(
        &self,
        source_uri: &Url,
        document: Arc<Document>,
    ) -> anyhow::Result<()> {
        info!("updating UI");

        self.ui.show_document(document).await;
        info!("finished updating UI.");

        Ok(())
    }
}
