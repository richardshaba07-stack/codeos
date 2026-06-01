//! `GraphActor`: l'attore che tiene vivo il grafo.
//!
//! Si sottoscrive all'event bus, attende `CodeOsEvent::FilesIndexed` (prodotto
//! dal `ParserActor`), invoca il [`GraphResolver`] per ottenere un `GraphDelta`,
//! lo applica allo storage in modo atomico e ripubblica `CodeOsEvent::GraphUpdated`.
//!
//! Non conosce gli altri attori (invariante 1.3): comunica solo via bus e via
//! trait `GraphStorage`.

use std::sync::Arc;

use codeos_storage::GraphStorage;
use codeos_types::bus::CodeOsEvent;
use codeos_types::ParsedFileResult;
use tokio::sync::broadcast;

use crate::resolver::GraphResolver;

/// L'attore del grafo. Consuma `FilesIndexed`, produce `GraphUpdated`.
pub struct GraphActor {
    storage: Arc<dyn GraphStorage>,
    resolver: GraphResolver,
    events: broadcast::Sender<CodeOsEvent>,
}

impl GraphActor {
    pub fn new(
        storage: Arc<dyn GraphStorage>,
        events: broadcast::Sender<CodeOsEvent>,
        project_root: Option<String>,
    ) -> Self {
        Self {
            storage,
            resolver: GraphResolver::new(project_root),
            events,
        }
    }

    /// Consuma gli eventi del bus finché il canale resta aperto.
    pub async fn run(self, mut events_rx: broadcast::Receiver<CodeOsEvent>) {
        loop {
            match events_rx.recv().await {
                Ok(CodeOsEvent::FilesIndexed { results }) => {
                    if let Err(err) = self.handle_files_indexed(results).await {
                        // Un fallimento di resolution/persistenza non deve abbattere
                        // l'attore: lo registriamo e restiamo in ascolto.
                        tracing::error!(error = %err, "GraphActor: aggiornamento del grafo fallito");
                    }
                }
                // Gli altri eventi (incluso il nostro GraphUpdated) non ci riguardano.
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "GraphActor: in ritardo sul bus, alcuni eventi sono andati persi"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!("GraphActor: bus eventi chiuso, esco");
                    break;
                }
            }
        }
    }

    async fn handle_files_indexed(&self, results: Vec<ParsedFileResult>) -> anyhow::Result<()> {
        let delta = self
            .resolver
            .resolve(&results, self.storage.as_ref())
            .await?;
        if delta.is_empty() {
            return Ok(());
        }
        // Il delta serve due volte: allo storage (lo consuma) e all'evento.
        let event_delta = delta.clone();
        self.storage.apply_delta(delta).await?;
        // L'assenza di sottoscrittori non è un errore.
        let _ = self
            .events
            .send(CodeOsEvent::GraphUpdated { delta: event_delta });
        Ok(())
    }
}
