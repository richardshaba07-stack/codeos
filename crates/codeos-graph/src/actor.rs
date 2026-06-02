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
                        // ...ma va *dichiarato* sul bus (P0: niente falsi successi).
                        // Chi attende l'aggiornamento deve poter restituire un errore
                        // onesto invece di un «completato» che mente.
                        let _ = self.events.send(CodeOsEvent::GraphUpdateFailed {
                            reason: err.to_string(),
                        });
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
        // Anche un delta vuoto è un esito legittimo (file senza entità nuove): va
        // comunque annunciato come `GraphUpdated`, altrimenti chi attende
        // l'aggiornamento non riceverebbe né successo né errore e lo scambierebbe
        // per un timeout. Saltiamo solo il lavoro inutile di `apply_delta`.
        if !delta.is_empty() {
            // Il delta serve due volte: allo storage (lo consuma) e all'evento.
            let event_delta = delta.clone();
            self.storage.apply_delta(delta).await?;
            // L'assenza di sottoscrittori non è un errore.
            let _ = self
                .events
                .send(CodeOsEvent::GraphUpdated { delta: event_delta });
        } else {
            let _ = self.events.send(CodeOsEvent::GraphUpdated { delta });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_parser::{LanguageParser, RustParser};
    use codeos_storage::{RelationFilter, SqliteStorage};
    use codeos_types::bus::GraphQualityInfo;
    use codeos_types::{Entity, EntityId, GraphDelta, Relation};
    use std::path::Path;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Uno storage che delega ogni **lettura** a un `SqliteStorage` reale ma fa
    /// fallire `apply_delta`: simula un errore di persistenza così da verificare che
    /// il `GraphActor` lo *dichiari* sul bus invece di restare in silenzio.
    struct ApplyFailsStorage(SqliteStorage);

    #[async_trait::async_trait]
    impl GraphStorage for ApplyFailsStorage {
        async fn apply_delta(&self, _delta: GraphDelta) -> anyhow::Result<()> {
            anyhow::bail!("persistenza forzata a fallire (test)")
        }
        async fn get_entity_by_id(&self, id: &EntityId) -> anyhow::Result<Option<Entity>> {
            self.0.get_entity_by_id(id).await
        }
        async fn get_entity_by_qname(&self, qname: &str) -> anyhow::Result<Option<Entity>> {
            self.0.get_entity_by_qname(qname).await
        }
        async fn find_entities_by_name_pattern(
            &self,
            pattern: &str,
        ) -> anyhow::Result<Vec<Entity>> {
            self.0.find_entities_by_name_pattern(pattern).await
        }
        async fn get_entities_by_file(&self, file_path: &str) -> anyhow::Result<Vec<Entity>> {
            self.0.get_entities_by_file(file_path).await
        }
        async fn query_relations(&self, filter: RelationFilter) -> anyhow::Result<Vec<Relation>> {
            self.0.query_relations(filter).await
        }
        async fn graph_quality(&self) -> anyhow::Result<GraphQualityInfo> {
            self.0.graph_quality().await
        }
    }

    /// Onestà sull'indice vuoto: anche quando il delta è vuoto (niente da
    /// persistere) il `GraphActor` DEVE annunciare `GraphUpdated`. Senza questo
    /// annuncio chi attende l'aggiornamento (i ponti gRPC) scambierebbe un indice
    /// legittimamente vuoto per un timeout — un falso fallimento.
    #[tokio::test]
    async fn empty_delta_still_publishes_graph_updated() {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let (tx, _rx0) = broadcast::channel(16);
        let mut rx = tx.subscribe();
        let actor = GraphActor::new(storage, tx.clone(), None);

        actor.handle_files_indexed(Vec::new()).await.unwrap();

        match timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Ok(CodeOsEvent::GraphUpdated { delta })) => {
                assert!(
                    delta.is_empty(),
                    "il delta di un indice vuoto deve essere vuoto"
                );
            }
            other => panic!("atteso GraphUpdated per delta vuoto, ricevuto {other:?}"),
        }
    }

    /// Onestà sul fallimento: se la persistenza fallisce, il `GraphActor` DEVE
    /// pubblicare `GraphUpdateFailed` (con la causa) invece di tacere. È ciò che
    /// permette al ponte gRPC di restituire un errore onesto al posto di un falso
    /// «indicizzazione completata con successo».
    #[tokio::test]
    async fn failed_persistence_publishes_graph_update_failed() {
        let storage = Arc::new(ApplyFailsStorage(SqliteStorage::in_memory().unwrap()));
        let (tx, _rx0) = broadcast::channel(16);
        let mut rx = tx.subscribe();

        let actor = GraphActor::new(storage, tx.clone(), None);
        let handle = tokio::spawn(actor.run(tx.subscribe()));

        // Un file con entità reali → delta NON vuoto → si arriva ad apply_delta,
        // che il mock fa fallire.
        let parsed = RustParser::new()
            .parse_file(Path::new("src/lib.rs"), "pub fn alpha() {}\n")
            .await;
        tx.send(CodeOsEvent::FilesIndexed {
            results: vec![parsed],
        })
        .unwrap();

        // Attendi GraphUpdateFailed, saltando l'eco di FilesIndexed.
        let reason = timeout(Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(CodeOsEvent::GraphUpdateFailed { reason }) => break reason,
                    Ok(_) => continue,
                    Err(e) => panic!("bus chiuso senza GraphUpdateFailed: {e:?}"),
                }
            }
        })
        .await
        .expect("nessun GraphUpdateFailed entro il timeout");

        assert!(
            reason.contains("persistenza forzata a fallire"),
            "la causa del fallimento deve propagarsi nell'evento: {reason}"
        );

        handle.abort();
    }
}
