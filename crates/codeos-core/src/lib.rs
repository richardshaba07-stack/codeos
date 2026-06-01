//! `codeos-core` — il sistema nervoso di CodeOS.
//!
//! Fornisce due meccanismi di comunicazione (invariante 1.3, Actor Model):
//!
//! - L'[`EventBus`]: un canale `broadcast` su cui gli attori pubblicano
//!   [`CodeOsEvent`] e a cui chiunque può sottoscriversi.
//! - Il **Dispatcher**: la "front door" del sistema. Espone un singolo
//!   `mpsc::Sender<Command>`; in loop riceve i [`Command`] e li instrada
//!   all'attore competente in base al *tipo* del comando. Non contiene business
//!   logic: se un comando non è instradabile, logga un warning e lo scarta.
//!
//! Nessun attore tiene un riferimento diretto a un altro attore: ognuno possiede
//! solo dei `Sender` verso canali. Questo rende ogni attore testabile in
//! isolamento.

use std::path::PathBuf;
use std::sync::Arc;

use codeos_graph::GraphActor;
use codeos_memory::{DecisionStore, InMemoryDecisionStore, MemoryActor};
use codeos_storage::{GraphStorage, SqliteStorage};
use codeos_types::bus::{CodeOsEvent, Command};
use tokio::sync::{broadcast, mpsc};

/// Capacità della mailbox di comandi di ciascun attore.
const ACTOR_MAILBOX: usize = 256;
/// Capacità del canale broadcast degli eventi (sufficiente per la v1).
const EVENT_BUS_CAPACITY: usize = 1024;

/// Il bus broadcast degli eventi di sistema.
///
/// È clonabile: ogni attore produttore ne tiene una copia per pubblicare, ogni
/// consumatore ottiene un `Receiver` con [`EventBus::subscribe`].
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<CodeOsEvent>,
}

impl EventBus {
    /// Crea un bus vuoto.
    pub fn new() -> Self {
        let (sender, _initial_rx) = broadcast::channel(EVENT_BUS_CAPACITY);
        Self { sender }
    }

    /// Restituisce un nuovo ricevitore. Riceverà solo gli eventi pubblicati
    /// *dopo* questa chiamata.
    pub fn subscribe(&self) -> broadcast::Receiver<CodeOsEvent> {
        self.sender.subscribe()
    }

    /// Pubblica un evento a tutti i sottoscrittori.
    ///
    /// L'assenza di sottoscrittori non è un errore: in tal caso `send` fallisce e
    /// l'evento viene semplicemente scartato.
    pub fn publish(&self, event: CodeOsEvent) {
        let _ = self.sender.send(event);
    }

    /// Una copia del `Sender` grezzo del bus.
    ///
    /// Serve agli attori che vivono in altri crate (es. `codeos_parser::ParserActor`)
    /// e devono pubblicare eventi senza dipendere da `codeos-core` — il che
    /// creerebbe un ciclo, dato che `codeos-core` dipende da loro.
    pub fn raw_sender(&self) -> broadcast::Sender<CodeOsEvent> {
        self.sender.clone()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Maniglia restituita da [`spawn`] per interagire con il sistema avviato.
#[derive(Clone)]
pub struct DispatcherHandle {
    /// La "front door": invia qui i comandi diretti al sistema.
    pub commands: mpsc::Sender<Command>,
    /// Il bus degli eventi: sottoscrivi qui per osservare il sistema.
    pub events: EventBus,
}

/// Tabella di instradamento del Dispatcher: un `Sender` verso la mailbox di
/// ciascun attore. Il Dispatcher non conosce i *tipi* degli attori, solo i loro
/// canali — così gli attori reali potranno rimpiazzare gli stub senza toccarlo.
struct Routes {
    parser: mpsc::Sender<Command>,
    query: mpsc::Sender<Command>,
    memory: mpsc::Sender<Command>,
    /// Il Guardian, oltre a essere event-driven, accetta comandi diretti: oggi solo
    /// [`Command::ArchitectureReport`], la lettura on-demand dello spazio negativo.
    guardian: mpsc::Sender<Command>,
}

/// Avvia l'EventBus, gli attori e il loop del Dispatcher con uno storage del
/// grafo **in memoria**.
///
/// Comodo per i test e per i run effimeri: il grafo non sopravvive al processo.
/// Per la persistenza su file usa [`spawn_with_storage`] passando uno
/// `SqliteStorage::open(path)`.
///
/// Richiede un runtime Tokio attivo (chiamala dentro `#[tokio::main]` o
/// `#[tokio::test]`). I task degli attori sono *detached*: vivono finché vive il
/// runtime.
pub fn spawn() -> DispatcherHandle {
    let storage = Arc::new(
        SqliteStorage::in_memory().expect("creazione dello storage SQLite in memoria fallita"),
    );
    spawn_with_storage(storage)
}

/// Come [`spawn`], ma con uno storage del grafo fornito dal chiamante (es. un
/// database SQLite su file per la persistenza). Le decisioni restano effimere (in
/// memoria); per persisterle come Markdown usa [`spawn_with_storage_and_decisions`].
pub fn spawn_with_storage(storage: Arc<dyn GraphStorage>) -> DispatcherHandle {
    spawn_with_storage_and_decisions(storage, Arc::new(InMemoryDecisionStore::new()))
}

/// Come [`spawn_with_storage`], ma con anche lo store delle decisioni fornito dal
/// chiamante — es. un [`MarkdownDecisionStore`](codeos_memory::MarkdownDecisionStore)
/// su una directory versionata in git, per una memoria storica ispezionabile a mano.
///
/// Il Guardian resta senza storia git: il referto architetturale avrà confidenza
/// solo strutturale e nessun Fossile di Decisione. Per agganciare la storia git (e
/// abilitare Campo di Astensione + Fossili) usa [`spawn_with_storage_decisions_and_repo`].
pub fn spawn_with_storage_and_decisions(
    storage: Arc<dyn GraphStorage>,
    decisions: Arc<dyn DecisionStore>,
) -> DispatcherHandle {
    spawn_with_storage_decisions_and_repo(storage, decisions, None)
}

/// La variante completa: storage del grafo, store delle decisioni **e**, se
/// presente, la root del repository git da cui il Guardian leggerà la storia.
///
/// Con `repo_root = Some(root)` il referto architetturale include la confidenza
/// calibrata dal **Campo di Astensione** e i **Fossili di Decisione** (la nascita
/// storica di ogni confine). Con `None` degrada con grazia alla sola analisi
/// strutturale, identica a [`spawn_with_storage_and_decisions`].
pub fn spawn_with_storage_decisions_and_repo(
    storage: Arc<dyn GraphStorage>,
    decisions: Arc<dyn DecisionStore>,
    repo_root: Option<PathBuf>,
) -> DispatcherHandle {
    let events = EventBus::new();

    let (parser_tx, parser_rx) = mpsc::channel(ACTOR_MAILBOX);
    let (query_tx, query_rx) = mpsc::channel(ACTOR_MAILBOX);
    let (memory_tx, memory_rx) = mpsc::channel(ACTOR_MAILBOX);
    let (guardian_tx, guardian_rx) = mpsc::channel(ACTOR_MAILBOX);

    // Tutti gli attori sono ormai reali. Graph e Query condividono lo STESSO
    // storage del grafo: il primo lo scrive, il secondo lo legge. Memory e Query
    // condividono lo STESSO store delle decisioni: il Memory lo scrive (registra
    // il *perché*), il Query lo legge per iniettarlo nel contesto (Passo 3).
    let parser = codeos_parser::ParserActor::new(events.raw_sender());
    tokio::spawn(parser.run(parser_rx));
    tokio::spawn(
        codeos_query::QueryActor::with_decisions(storage.clone(), decisions.clone()).run(query_rx),
    );
    tokio::spawn(MemoryActor::new(decisions.clone()).run(memory_rx));

    // Radice del progetto per il resolver del grafo: la deriviamo dalla root del
    // repo (quando nota). Serve a rendere i `qualified_name` RELATIVI alla radice —
    // così i layer (i primi N segmenti del nome) diventano `api::handlers` invece
    // del prefisso del path assoluto, e l'asimmetria fra layer può emergere. La
    // cloniamo come `String`: `repo_root` (PathBuf) resta intatto per il Guardian.
    let project_root: Option<String> = repo_root
        .as_ref()
        .map(|root| root.to_string_lossy().to_string());

    // Il GraphActor è guidato dagli EVENTI, non dai comandi: si sottoscrive al
    // bus e reagisce a `FilesIndexed`. Non passa dal Dispatcher (che instrada
    // solo comandi). La sottoscrizione avviene qui, prima che il Dispatcher
    // possa inoltrare comandi, così nessun `FilesIndexed` va perso.
    let graph = GraphActor::new(storage.clone(), events.raw_sender(), project_root);
    tokio::spawn(graph.run(events.subscribe()));

    // Il GuardianActor (sistema immunitario) è event-driven E command-driven:
    // reagisce a `GraphUpdated` (verifica gli archi appena aggiunti e promuove gli
    // invarianti scoperti) e risponde a `ArchitectureReport` (la lettura on-demand
    // dello spazio negativo lungo i quattro assi). Condivide lo STESSO storage del
    // GraphActor (lo legge soltanto) e lo STESSO store delle decisioni del
    // Memory/Query. Se è nota la root del repo git, vi aggancia il Paleontologo.
    let guardian = match repo_root {
        Some(root) => codeos_guardian::GuardianActor::with_memory_and_repo(
            storage,
            events.raw_sender(),
            decisions,
            root,
        ),
        None => {
            codeos_guardian::GuardianActor::with_memory(storage, events.raw_sender(), decisions)
        }
    };
    tokio::spawn(guardian.run_with_commands(events.subscribe(), guardian_rx));

    let routes = Routes {
        parser: parser_tx,
        query: query_tx,
        memory: memory_tx,
        guardian: guardian_tx,
    };

    let (command_tx, command_rx) = mpsc::channel(ACTOR_MAILBOX);
    tokio::spawn(dispatch_loop(command_rx, routes));

    DispatcherHandle {
        commands: command_tx,
        events,
    }
}

/// Il loop del Dispatcher: riceve comandi dalla front door e li instrada.
async fn dispatch_loop(mut commands: mpsc::Receiver<Command>, routes: Routes) {
    while let Some(command) = commands.recv().await {
        // La scelta dell'attore dipende SOLO dal tipo del comando.
        let (target, actor) = match &command {
            Command::IndexProject { .. }
            | Command::IndexFiles { .. }
            | Command::ReIndexFile { .. }
            | Command::RemoveFiles { .. } => (&routes.parser, "parser"),
            Command::QueryGraph { .. } => (&routes.query, "query"),
            Command::RecordDecision { .. } => (&routes.memory, "memory"),
            Command::ArchitectureReport { .. } => (&routes.guardian, "guardian"),
        };

        if let Err(err) = target.send(command).await {
            // L'attore non è più in ascolto (task terminato). Il `reply_to` era
            // dentro il comando ormai perso, quindi non possiamo rispondere al
            // chiamante; lo registriamo come anomalia, senza crashare.
            tracing::warn!(actor, error = %err, "comando scartato: attore non raggiungibile");
        }
    }
    tracing::info!("dispatch loop terminato: la front door è chiusa");
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_types::bus::{NewDecision, QueryRequest};

    fn reply_channel<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
        mpsc::channel(1)
    }

    #[tokio::test]
    async fn routes_record_decision_and_returns_an_id() {
        let system = spawn();
        let (reply_to, mut reply_rx) = reply_channel();

        system
            .commands
            .send(Command::RecordDecision {
                decision: NewDecision {
                    author: "human:test".to_string(),
                    title: "Scelta di prova".to_string(),
                    context: String::new(),
                    rationale: String::new(),
                    related_entity_ids: Vec::new(),
                    related_decision_ids: Vec::new(),
                    tags: Vec::new(),
                },
                reply_to,
            })
            .await
            .expect("front door chiusa");

        let reply = reply_rx
            .recv()
            .await
            .expect("nessuna risposta dal memory actor");
        assert!(reply.is_ok(), "la decisione doveva essere registrata");
    }

    #[tokio::test]
    async fn routes_query_to_query_actor() {
        let system = spawn();
        let (reply_to, mut reply_rx) = reply_channel();

        system
            .commands
            .send(Command::QueryGraph {
                query: QueryRequest::NaturalLanguage {
                    text: "login oauth".to_string(),
                },
                reply_to,
            })
            .await
            .expect("front door chiusa");

        let reply = reply_rx
            .recv()
            .await
            .expect("nessuna risposta dal query actor");
        assert!(reply.is_ok());
    }

    #[tokio::test]
    async fn routes_architecture_report_to_guardian() {
        // Responsabilità del core: instradare Command::ArchitectureReport al Guardian
        // e riportare indietro la risposta. La correttezza *semantica* del referto
        // (quali invarianti emergono) è coperta dai test del Guardian; qui verifichiamo
        // il round-trip attraverso il Dispatcher, anche su un sistema appena avviato.
        let system = spawn();
        let (rep_tx, mut rep_rx) = reply_channel();
        system
            .commands
            .send(Command::ArchitectureReport { reply_to: rep_tx })
            .await
            .expect("front door chiusa");

        let report = rep_rx
            .recv()
            .await
            .expect("nessuna risposta dal guardian")
            .expect("referto fallito");

        // Grafo vuoto ⇒ referto vuoto su tutti e tre gli assi, ma la risposta torna.
        assert!(report.invariants.is_empty());
        assert!(report.fossils.is_empty());
        assert!(report.gaps.is_empty());
    }

    #[tokio::test]
    async fn index_files_parses_a_real_file_and_publishes_event() {
        // Il ParserActor reale legge dal disco: serve un file vero.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeos_core_{nanos}.py"));
        tokio::fs::write(&path, "class Foo:\n    pass\n")
            .await
            .unwrap();

        let system = spawn();
        // Sottoscrivo PRIMA di inviare, così non perdo l'evento.
        let mut events = system.events.subscribe();
        let (reply_to, mut reply_rx) = reply_channel();

        system
            .commands
            .send(Command::IndexFiles {
                files: vec![path.to_string_lossy().to_string()],
                reply_to,
            })
            .await
            .expect("front door chiusa");

        let reply = reply_rx
            .recv()
            .await
            .expect("nessuna risposta dal parser actor");
        assert!(reply.is_ok());

        let event = events.recv().await.expect("nessun evento sul bus");
        let CodeOsEvent::FilesIndexed { results } = event else {
            panic!("evento inatteso: {event:?}");
        };
        assert_eq!(results.len(), 1);
        let names: Vec<&str> = results[0]
            .entities
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(names.contains(&"Foo"), "names = {names:?}");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn index_files_builds_the_graph_and_publishes_graph_updated() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeos_core_graph_{nanos}.py"));
        tokio::fs::write(&path, "class Foo:\n    def bar(self):\n        pass\n")
            .await
            .unwrap();

        let system = spawn();
        let mut events = system.events.subscribe();
        let (reply_to, mut reply_rx) = reply_channel();

        system
            .commands
            .send(Command::IndexFiles {
                files: vec![path.to_string_lossy().to_string()],
                reply_to,
            })
            .await
            .expect("front door chiusa");
        reply_rx.recv().await.expect("nessuna risposta").unwrap();

        // Il ParserActor pubblica FilesIndexed; il GraphActor reagisce e pubblica
        // GraphUpdated. Scorriamo gli eventi finché arriva il delta del grafo.
        let delta = loop {
            match events
                .recv()
                .await
                .expect("bus chiuso prima di GraphUpdated")
            {
                CodeOsEvent::GraphUpdated { delta } => break delta,
                _ => continue,
            }
        };

        // Il grafo contiene Module → Foo → bar con qualified_name gerarchici.
        let class = delta
            .added_entities
            .iter()
            .find(|e| e.kind == codeos_types::EntityKind::Class)
            .expect("classe assente nel delta del grafo");
        assert!(
            class.qualified_name.ends_with("::Foo"),
            "qname classe = {}",
            class.qualified_name
        );
        let method = delta
            .added_entities
            .iter()
            .find(|e| e.kind == codeos_types::EntityKind::Method)
            .expect("metodo assente nel delta del grafo");
        assert!(
            method.qualified_name.ends_with("::Foo::bar"),
            "qname metodo = {}",
            method.qualified_name
        );

        // Esiste la relazione strutturale BelongsTo metodo → classe.
        use codeos_types::RelationKind;
        assert!(
            delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::BelongsTo
                    && r.source_id == method.id
                    && r.target_id == class.id),
            "manca BelongsTo metodo→classe"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn index_then_query_returns_relevant_context() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeos_core_login_{nanos}.py"));
        tokio::fs::write(
            &path,
            "class LoginService:\n    def authenticate(self):\n        pass\n",
        )
        .await
        .unwrap();

        let system = spawn();
        let mut events = system.events.subscribe();
        let (reply_to, mut reply_rx) = reply_channel();
        system
            .commands
            .send(Command::IndexFiles {
                files: vec![path.to_string_lossy().to_string()],
                reply_to,
            })
            .await
            .expect("front door chiusa");
        reply_rx.recv().await.expect("nessuna risposta").unwrap();

        // Attendo che il grafo sia stato persistito, così la query lo vede.
        loop {
            match events
                .recv()
                .await
                .expect("bus chiuso prima di GraphUpdated")
            {
                CodeOsEvent::GraphUpdated { .. } => break,
                _ => continue,
            }
        }

        // Ora interrogo: il Query Engine reale deve trovare LoginService.
        let (q_reply_to, mut q_reply_rx) = reply_channel();
        system
            .commands
            .send(Command::QueryGraph {
                query: QueryRequest::NaturalLanguage {
                    text: "voglio sistemare il login".to_string(),
                },
                reply_to: q_reply_to,
            })
            .await
            .expect("front door chiusa");

        let response = q_reply_rx
            .recv()
            .await
            .expect("nessuna risposta dal query actor")
            .expect("query fallita");

        assert!(
            response
                .entities
                .iter()
                .any(|e| e.qualified_name.contains("LoginService")),
            "il contesto doveva includere LoginService, entità = {:?}",
            response
                .entities
                .iter()
                .map(|e| &e.qualified_name)
                .collect::<Vec<_>>()
        );
        assert!(response.formatted_context.contains("FILE RILEVANTI"));

        let _ = tokio::fs::remove_file(&path).await;
    }
}
