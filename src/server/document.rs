use anyhow::bail;
use tower_lsp::lsp_types::{Range, Url};

use crate::config::ExportPdfMode;

use super::TypstServer;

impl TypstServer {
    pub async fn on_source_changed(
        &self,
        uri: &Url,
        first_change_range: Option<Range>,
    ) -> anyhow::Result<()> {
        let config = self.config.read().await;
        match config.export_pdf {
            ExportPdfMode::OnType => {
                self.run_diagnostics_and_export(uri, first_change_range)
                    .await?
            }
            ExportPdfMode::OnPinnedMainType => {
                if let Some(main_uri) = self.main_url().await {
                    self.run_diagnostics_and_export(&main_uri, first_change_range)
                        .await?
                } else {
                    self.run_diagnostics(uri).await?
                }
            }
            _ => {
                self.run_diagnostics(self.main_url().await.as_ref().unwrap_or(uri))
                    .await?
            }
        }

        Ok(())
    }

    pub async fn run_export(&self, uri: &Url) -> anyhow::Result<()> {
        let (document, _) = self.compile_source(uri).await?;
        match document {
            Some(document) => self.export_pdf(uri, document, None).await?,
            None => bail!("failed to generate document after compilation"),
        }

        Ok(())
    }

    pub async fn run_diagnostics_and_export(
        &self,
        uri: &Url,
        first_change_range: Option<Range>,
    ) -> anyhow::Result<()> {
        let (document, diagnostics) = self.compile_source(uri).await?;

        self.update_all_diagnostics(diagnostics).await;
        if let Some(document) = document {
            self.export_pdf(uri, document, first_change_range).await?;
        } else {
            bail!("failed to generate document after compilation")
        }

        Ok(())
    }

    pub async fn run_diagnostics(&self, uri: &Url) -> anyhow::Result<()> {
        let (_, diagnostics) = self.compile_source(uri).await?;

        self.update_all_diagnostics(diagnostics).await;

        Ok(())
    }
}
