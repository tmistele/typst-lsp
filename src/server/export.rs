use std::sync::Arc;

use tower_lsp::lsp_types::{Range, Url};
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
        first_change_range: Option<Range>,
    ) -> anyhow::Result<()> {
        info!("updating UI");

        // NB: Cloning/reading a `Source` is cheap.
        let source = self
            .scope_with_source(source_uri)
            .await
            .expect("No source file?")
            .source;

        self.ui
            .show_document(document, source, first_change_range)
            .await;

        Ok(())
    }
}
