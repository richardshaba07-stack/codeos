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
use codeos_types::{CommitContext, ParsedFileResult};
use tokio::sync::broadcast;

use crate::resolver::GraphResolver;

/// Fornisce, su richiesta, il commit corrente (`HEAD`) con cui timbrare la
/// **nascita** dei nodi del grafo temporale (vision step 2). Restituisce `None`
/// quando non c'è un commit onesto da timbrare (niente repo git, `git` assente,
/// `HEAD` non nato): mai un istante inventato.
///
/// DECISION: è un tipo-funzione iniettato dal *compositore* (`codeos-core`), non
/// una chiamata diretta a git da dentro il grafo. Così `codeos-graph` resta un
/// crate magro (solo `codeos-types` + `codeos-storage`) e **non dipende dal lettore
/// della storia git** (`codeos-paleo`): il layer del grafo non deve conoscere
/// l'analisi paleontologica. Il compositore — l'unico a sapere dov'è il repo e già
/// dipendente da `codeos-paleo` per il Guardian — cattura la root e chiama
/// `head_commit`. È anche ciò che rende il timbro testabile **senza git**: i test
/// iniettano un provider costante.
pub type CommitProvider = Arc<dyn Fn() -> Option<CommitContext> + Send + Sync>;

/// L'attore del grafo. Consuma `FilesIndexed`, produce `GraphUpdated`.
pub struct GraphActor {
    storage: Arc<dyn GraphStorage>,
    /// Prefisso radice per i `qualified_name` (config fissa dell'attore).
    project_root: Option<String>,
    /// Sorgente del commit corrente, iniettata dal compositore. `None` ⇒ niente
    /// timbro temporale (comportamento storico invariato).
    commit_provider: Option<CommitProvider>,
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
            project_root,
            commit_provider: None,
            events,
        }
    }

    /// Inietta la sorgente del commit con cui timbrare la nascita dei nodi.
    /// Builder additivo: senza questa chiamata l'attore non timbra nulla (i test e
    /// i percorsi senza repo git restano identici a prima).
    pub fn with_commit_provider(mut self, commit_provider: CommitProvider) -> Self {
        self.commit_provider = Some(commit_provider);
        self
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
        // HEAD letto **fresco a ogni indicizzazione**: gli attori sono long-lived e
        // tra un index e l'altro HEAD può muoversi (timbrare l'HEAD di avvio
        // mentirebbe sulla nascita). Una sola `FilesIndexed` per comando di index
        // (vedi ParserActor::publish) ⇒ una sola lettura git per index, mai N.
        // `None` se non c'è provider o non c'è un commit onesto: nessun timbro.
        let commit_context = self.commit_provider.as_ref().and_then(|p| p());
        let resolver =
            GraphResolver::new(self.project_root.clone()).with_commit_context(commit_context);
        let delta = resolver.resolve(&results, self.storage.as_ref()).await?;
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
        let parsed = RustParser::new().parse_file(Path::new("src/lib.rs"), "pub fn alpha() {}\n");
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

    /// Attende il prossimo `GraphUpdated` sul bus, saltando l'eco di altri eventi.
    async fn recv_graph_updated(rx: &mut broadcast::Receiver<CodeOsEvent>) -> GraphDelta {
        timeout(Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(CodeOsEvent::GraphUpdated { delta }) => break delta,
                    Ok(_) => continue,
                    Err(e) => panic!("bus chiuso senza GraphUpdated: {e:?}"),
                }
            }
        })
        .await
        .expect("nessun GraphUpdated entro il timeout")
    }

    /// Cablaggio temporale (vision step 2): con un `CommitProvider` iniettato, ogni
    /// entità creata nasce timbrata col commit che il provider restituisce. Il
    /// provider è una semplice closure ⇒ il cablaggio è testabile SENZA git reale.
    #[tokio::test]
    async fn injected_commit_provider_stamps_birth() {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let (tx, _rx0) = broadcast::channel(16);
        let mut rx = tx.subscribe();
        let provider: CommitProvider = Arc::new(|| {
            Some(CommitContext {
                commit: "live-head".to_string(),
                ts: 4242,
            })
        });
        let actor = GraphActor::new(storage, tx.clone(), None).with_commit_provider(provider);

        let parsed = RustParser::new().parse_file(Path::new("src/lib.rs"), "pub fn alpha() {}\n");
        actor.handle_files_indexed(vec![parsed]).await.unwrap();

        let delta = recv_graph_updated(&mut rx).await;
        assert!(!delta.added_entities.is_empty());
        assert!(
            delta.added_entities.iter().all(|e| {
                e.metadata.get("born_commit").map(String::as_str) == Some("live-head")
                    && e.metadata.get("born_ts").map(String::as_str) == Some("4242")
            }),
            "ogni entità deve nascere col commit iniettato dal provider"
        );
    }

    /// Retro-compatibilità: senza provider iniettato non si timbra alcuna nascita —
    /// il comportamento storico (e i percorsi senza repo git) resta identico.
    #[tokio::test]
    async fn without_commit_provider_no_birth_is_stamped() {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let (tx, _rx0) = broadcast::channel(16);
        let mut rx = tx.subscribe();
        let actor = GraphActor::new(storage, tx.clone(), None); // nessun provider

        let parsed = RustParser::new().parse_file(Path::new("src/lib.rs"), "pub fn alpha() {}\n");
        actor.handle_files_indexed(vec![parsed]).await.unwrap();

        let delta = recv_graph_updated(&mut rx).await;
        assert!(!delta.added_entities.is_empty());
        assert!(
            delta.added_entities.iter().all(|e| {
                !e.metadata.contains_key("born_commit") && !e.metadata.contains_key("born_ts")
            }),
            "senza provider non si timbra alcuna nascita"
        );
    }
}
