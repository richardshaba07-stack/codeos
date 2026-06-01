//! `codeos-rpc` — la **facciata gRPC** (tonic) di CodeOS.
//!
//! È il guscio più esterno della cipolla (invariante 1.5): può dipendere da
//! tutto, ma niente dipende da lui. Non contiene business logic — ogni RPC è un
//! sottile **ponte** che traduce un messaggio gRPC in un [`Command`] per il
//! Dispatcher di `codeos-core`, attende la risposta sul canale `reply_to` e la
//! ritraduce in un messaggio gRPC.
//!
//! - [`CodeOsService`]: l'implementazione del servizio `CodeOs`.
//! - [`serve`]: avvia il server su un indirizzo TCP.
//! - [`proto`]: i tipi generati da `proto/codeos.proto` (client e server).
//!
//! Lo stream [`proto::EventMessage`] di `WatchEvents` espone in tempo reale gli
//! eventi del sistema — in particolare le violazioni del **sistema immunitario** —
//! a un client come il plugin VS Code.

// `tonic::Status` è il tipo d'errore *imposto* da gRPC su tutto questo crate ed è
// intrinsecamente grosso (~176 byte). Boxarlo nei nostri helper romperebbe il
// `?` verso i metodi del servizio (che devono restituire `Status` nudo): qui il
// lint è un falso positivo, lo silenziamo a livello di crate.
#![allow(clippy::result_large_err)]

use std::net::SocketAddr;
use std::pin::Pin;

use codeos_core::DispatcherHandle;
use codeos_types::bus::{
    ArchitecturalGapInfo, CodeOsEvent, Command, DecisionFossilInfo, LayeringInvariantInfo,
    NewDecision, QueryRequest,
};
use codeos_types::{Entity, EntityId, EntityKind, Relation, RelationKind, SourceLocation};
use tokio::sync::{broadcast, mpsc};
use tokio::time::{timeout, Duration};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};
use uuid::Uuid;

/// I tipi generati a build-time da `proto/codeos.proto`.
pub mod proto {
    tonic::include_proto!("codeos.v1");
}

use proto::code_os_server::{CodeOs, CodeOsServer};

const GRAPH_UPDATE_WAIT: Duration = Duration::from_secs(5);

/// L'implementazione del servizio gRPC. Tiene una [`DispatcherHandle`] clonabile
/// (un `Sender` verso la front door + il bus eventi): è tutto ciò che serve per
/// fare da ponte, senza alcun riferimento diretto agli attori (invariante 1.3).
#[derive(Clone)]
pub struct CodeOsService {
    dispatcher: DispatcherHandle,
}

impl CodeOsService {
    /// Crea il servizio attorno a un sistema CodeOS già avviato.
    pub fn new(dispatcher: DispatcherHandle) -> Self {
        Self { dispatcher }
    }

    /// Lo avvolge in un [`CodeOsServer`] pronto da montare in un `tonic` router.
    pub fn into_server(self) -> CodeOsServer<Self> {
        CodeOsServer::new(self)
    }
}

/// Avvia il server gRPC e blocca finché non termina.
///
/// `dispatcher` è il sistema CodeOS già avviato (vedi `codeos_core::spawn*`).
pub async fn serve(
    dispatcher: DispatcherHandle,
    addr: SocketAddr,
) -> Result<(), tonic::transport::Error> {
    Server::builder()
        .add_service(CodeOsService::new(dispatcher).into_server())
        .serve(addr)
        .await
}

async fn wait_for_graph_update(mut graph_rx: broadcast::Receiver<CodeOsEvent>, operation: &str) {
    let observed = timeout(GRAPH_UPDATE_WAIT, async {
        loop {
            match graph_rx.recv().await {
                Ok(CodeOsEvent::GraphUpdated { .. }) => return true,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        operation,
                        skipped,
                        "attesa GraphUpdated: eventi persi dal subscriber"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await;

    match observed {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            operation,
            "attesa GraphUpdated interrotta: event bus chiuso"
        ),
        Err(_) => tracing::warn!(
            operation,
            timeout_ms = GRAPH_UPDATE_WAIT.as_millis(),
            "attesa GraphUpdated scaduta: il comando di indicizzazione torna comunque"
        ),
    }
}

#[tonic::async_trait]
impl CodeOs for CodeOsService {
    async fn query_graph(
        &self,
        request: Request<proto::QueryGraphRequest>,
    ) -> Result<Response<proto::QueryGraphResponse>, Status> {
        let req = request.into_inner();
        let (reply_to, mut reply_rx) = mpsc::channel(1);
        self.dispatcher
            .commands
            .send(Command::QueryGraph {
                query: QueryRequest::NaturalLanguage {
                    text: req.natural_language,
                },
                reply_to,
            })
            .await
            .map_err(|_| Status::unavailable("dispatcher non raggiungibile"))?;

        let response = reply_rx
            .recv()
            .await
            .ok_or_else(|| Status::internal("nessuna risposta dal query actor"))?
            .map_err(|e| Status::internal(format!("query fallita: {e}")))?;

        Ok(Response::new(proto::QueryGraphResponse {
            formatted_context: response.formatted_context,
            entities: response.entities.into_iter().map(entity_to_proto).collect(),
            relations: response
                .relations
                .into_iter()
                .map(relation_to_proto)
                .collect(),
        }))
    }

    async fn record_decision(
        &self,
        request: Request<proto::RecordDecisionRequest>,
    ) -> Result<Response<proto::RecordDecisionResponse>, Status> {
        let req = request.into_inner();
        // Le stringhe UUID arrivano dal filo: validale qui, ai confini del sistema,
        // così gli attori interni lavorano solo con `EntityId` ben formati.
        let related_entity_ids = req
            .related_entity_ids
            .iter()
            .map(|s| parse_entity_id(s))
            .collect::<Result<Vec<_>, _>>()?;
        let related_decision_ids = req
            .related_decision_ids
            .iter()
            .map(|s| parse_entity_id(s))
            .collect::<Result<Vec<_>, _>>()?;

        let (reply_to, mut reply_rx) = mpsc::channel(1);
        self.dispatcher
            .commands
            .send(Command::RecordDecision {
                decision: NewDecision {
                    author: req.author,
                    title: req.title,
                    context: req.context,
                    rationale: req.rationale,
                    related_entity_ids,
                    related_decision_ids,
                    tags: req.tags,
                },
                reply_to,
            })
            .await
            .map_err(|_| Status::unavailable("dispatcher non raggiungibile"))?;

        let id = reply_rx
            .recv()
            .await
            .ok_or_else(|| Status::internal("nessuna risposta dal memory actor"))?
            .map_err(|e| Status::internal(format!("registrazione decisione fallita: {e}")))?;

        Ok(Response::new(proto::RecordDecisionResponse {
            decision_id: id.to_string(),
        }))
    }

    async fn index_project(
        &self,
        request: Request<proto::IndexProjectRequest>,
    ) -> Result<Response<proto::IndexProjectResponse>, Status> {
        let req = request.into_inner();
        let graph_rx = self.dispatcher.events.subscribe();
        let (reply_to, mut reply_rx) = mpsc::channel(1);
        self.dispatcher
            .commands
            .send(Command::IndexProject {
                project_root: req.project_root,
                reply_to,
            })
            .await
            .map_err(|_| Status::unavailable("dispatcher non raggiungibile"))?;

        reply_rx
            .recv()
            .await
            .ok_or_else(|| Status::internal("nessuna risposta dal parser actor"))?
            .map_err(|e| Status::internal(format!("indicizzazione progetto fallita: {e}")))?;

        wait_for_graph_update(graph_rx, "IndexProject").await;
        Ok(Response::new(proto::IndexProjectResponse {}))
    }

    async fn index_files(
        &self,
        request: Request<proto::IndexFilesRequest>,
    ) -> Result<Response<proto::IndexFilesResponse>, Status> {
        let req = request.into_inner();
        let graph_rx = self.dispatcher.events.subscribe();
        let (reply_to, mut reply_rx) = mpsc::channel(1);
        self.dispatcher
            .commands
            .send(Command::IndexFiles {
                files: req.files,
                reply_to,
            })
            .await
            .map_err(|_| Status::unavailable("dispatcher non raggiungibile"))?;

        let ids = reply_rx
            .recv()
            .await
            .ok_or_else(|| Status::internal("nessuna risposta dal parser actor"))?
            .map_err(|e| Status::internal(format!("indicizzazione file fallita: {e}")))?;

        wait_for_graph_update(graph_rx, "IndexFiles").await;
        Ok(Response::new(proto::IndexFilesResponse {
            entity_ids: ids.iter().map(|id| id.to_string()).collect(),
        }))
    }

    async fn get_architecture_report(
        &self,
        _request: Request<proto::GetArchitectureReportRequest>,
    ) -> Result<Response<proto::GetArchitectureReportResponse>, Status> {
        let (reply_to, mut reply_rx) = mpsc::channel(1);
        self.dispatcher
            .commands
            .send(Command::ArchitectureReport { reply_to })
            .await
            .map_err(|_| Status::unavailable("dispatcher non raggiungibile"))?;

        let report = reply_rx
            .recv()
            .await
            .ok_or_else(|| Status::internal("nessuna risposta dal guardian"))?
            .map_err(|e| Status::internal(format!("referto architetturale fallito: {e}")))?;

        Ok(Response::new(proto::GetArchitectureReportResponse {
            invariants: report
                .invariants
                .into_iter()
                .map(layering_invariant_to_proto)
                .collect(),
            fossils: report
                .fossils
                .into_iter()
                .map(decision_fossil_to_proto)
                .collect(),
            gaps: report
                .gaps
                .into_iter()
                .map(architectural_gap_to_proto)
                .collect(),
        }))
    }

    /// Lo stream di `WatchEvents`: una pipe boxata che inoltra ogni evento del bus
    /// tradotto in [`proto::EventMessage`].
    type WatchEventsStream =
        Pin<Box<dyn Stream<Item = Result<proto::EventMessage, Status>> + Send + 'static>>;

    async fn watch_events(
        &self,
        _request: Request<proto::WatchEventsRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        // Sottoscrivi PRIMA di restituire, così il client riceve ogni evento
        // pubblicato dopo l'apertura dello stream. Un task ponte travasa dal
        // canale `broadcast` (che può "laggare") a un `mpsc` ordinato verso il client.
        let mut bus = self.dispatcher.events.subscribe();
        let (tx, out_rx) = mpsc::channel(64);

        tokio::spawn(async move {
            loop {
                match bus.recv().await {
                    Ok(event) => {
                        if tx.send(Ok(event_to_proto(event))).await.is_err() {
                            break; // il client ha chiuso lo stream
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "WatchEvents in ritardo: alcuni eventi persi");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx))))
    }
}

// ---------------------------------------------------------------------------
// Conversioni tipi del bus → tipi proto (confine del sistema).
// ---------------------------------------------------------------------------

fn parse_entity_id(raw: &str) -> Result<EntityId, Status> {
    Uuid::parse_str(raw)
        .map(EntityId)
        .map_err(|e| Status::invalid_argument(format!("EntityId '{raw}' non valido: {e}")))
}

fn entity_to_proto(entity: Entity) -> proto::Entity {
    proto::Entity {
        id: entity.id.to_string(),
        kind: entity_kind_name(entity.kind).to_string(),
        qualified_name: entity.qualified_name,
        location: Some(location_to_proto(entity.location)),
        metadata: entity.metadata,
    }
}

fn relation_to_proto(relation: Relation) -> proto::Relation {
    proto::Relation {
        id: relation.id.to_string(),
        kind: relation_kind_name(relation.kind).to_string(),
        source_id: relation.source_id.to_string(),
        target_id: relation.target_id.to_string(),
        metadata: relation.metadata,
    }
}

fn location_to_proto(location: SourceLocation) -> proto::SourceLocation {
    proto::SourceLocation {
        file_path: location.file_path,
        start_line: location.start_line,
        start_column: location.start_column,
        end_line: location.end_line,
        end_column: location.end_column,
    }
}

fn layering_invariant_to_proto(info: LayeringInvariantInfo) -> proto::LayeringInvariant {
    proto::LayeringInvariant {
        upstream: info.upstream,
        downstream: info.downstream,
        support: info.support,
        confidence: info.confidence,
        calibrated: info.calibrated,
    }
}

fn decision_fossil_to_proto(info: DecisionFossilInfo) -> proto::DecisionFossil {
    proto::DecisionFossil {
        upstream: info.upstream,
        downstream: info.downstream,
        born_at: info.born_at,
        born_at_unix: info.born_at_unix,
        intent: info.intent,
        born_structure: info.born_structure,
    }
}

fn architectural_gap_to_proto(info: ArchitecturalGapInfo) -> proto::ArchitecturalGap {
    proto::ArchitecturalGap {
        upstream: info.upstream,
        downstream: info.downstream,
        foundation_support: info.foundation_support,
    }
}

fn event_to_proto(event: CodeOsEvent) -> proto::EventMessage {
    use proto::event_message::Event;
    let inner = match event {
        CodeOsEvent::FilesIndexed { results } => Event::FilesIndexed(proto::FilesIndexedEvent {
            file_paths: results.into_iter().map(|r| r.file_path).collect(),
        }),
        CodeOsEvent::GraphUpdated { delta } => Event::GraphUpdated(proto::GraphUpdatedEvent {
            added_entities: delta.added_entities.len() as u32,
            removed_entities: delta.removed_entity_ids.len() as u32,
            added_relations: delta.added_relations.len() as u32,
            removed_relations: delta.removed_relation_ids.len() as u32,
        }),
        CodeOsEvent::ArchitectureViolationDetected { violation } => {
            Event::Violation(proto::ArchitectureViolationEvent {
                rule_id: violation.rule_id.to_string(),
                relation_id: violation.relation_id.to_string(),
                source_id: violation.source_id.to_string(),
                target_id: violation.target_id.to_string(),
                message: violation.message,
                location: violation.location.map(location_to_proto),
            })
        }
    };
    proto::EventMessage { event: Some(inner) }
}

/// Nome stabile e leggibile della variante (indipendente dall'ordinale enum).
fn entity_kind_name(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Project => "Project",
        EntityKind::Module => "Module",
        EntityKind::Class => "Class",
        EntityKind::Struct => "Struct",
        EntityKind::Interface => "Interface",
        EntityKind::Function => "Function",
        EntityKind::Method => "Method",
        EntityKind::Variable => "Variable",
        EntityKind::Parameter => "Parameter",
        EntityKind::Test => "Test",
    }
}

fn relation_kind_name(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Calls => "Calls",
        RelationKind::Imports => "Imports",
        RelationKind::Implements => "Implements",
        RelationKind::Extends => "Extends",
        RelationKind::Tests => "Tests",
        RelationKind::Uses => "Uses",
        RelationKind::Creates => "Creates",
        RelationKind::Modifies => "Modifies",
        RelationKind::BelongsTo => "BelongsTo",
        RelationKind::Unresolved => "Unresolved",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::code_os_client::CodeOsClient;

    /// Avvia un sistema effimero + il server gRPC su una porta libera; restituisce
    /// un client connesso e l'[`EventBus`] del sistema (per pubblicare eventi nei
    /// test dello stream). Il server vive finché vive il runtime del test.
    async fn start_server() -> (
        CodeOsClient<tonic::transport::Channel>,
        codeos_core::EventBus,
    ) {
        let system = codeos_core::spawn();
        let events = system.events.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let service = CodeOsService::new(system).into_server();
        tokio::spawn(async move {
            Server::builder()
                .add_service(service)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        // Riprova la connessione finché il server non è in ascolto.
        let endpoint = format!("http://{addr}");
        for _ in 0..50 {
            if let Ok(client) = CodeOsClient::connect(endpoint.clone()).await {
                return (client, events);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("impossibile connettersi al server gRPC su {endpoint}");
    }

    #[tokio::test]
    async fn record_decision_over_the_wire_returns_a_uuid() {
        let (mut client, _events) = start_server().await;

        let response = client
            .record_decision(proto::RecordDecisionRequest {
                author: "human:test".to_string(),
                title: "Scelta via gRPC".to_string(),
                context: "ctx".to_string(),
                rationale: "perché sì".to_string(),
                related_entity_ids: vec![],
                related_decision_ids: vec![],
                tags: vec!["test".to_string()],
            })
            .await
            .expect("RPC RecordDecision fallita")
            .into_inner();

        // L'id restituito deve essere un UUID valido.
        Uuid::parse_str(&response.decision_id).expect("decision_id non è un UUID");
    }

    #[tokio::test]
    async fn query_graph_over_the_wire_succeeds_on_empty_graph() {
        let (mut client, _events) = start_server().await;

        let response = client
            .query_graph(proto::QueryGraphRequest {
                natural_language: "login oauth".to_string(),
            })
            .await
            .expect("RPC QueryGraph fallita")
            .into_inner();

        // Grafo vuoto: nessuna entità, ma il contesto formattato è sempre prodotto.
        assert!(response.entities.is_empty());
        assert!(!response.formatted_context.is_empty());
    }

    #[tokio::test]
    async fn record_decision_rejects_a_malformed_entity_id() {
        let (mut client, _events) = start_server().await;

        let status = client
            .record_decision(proto::RecordDecisionRequest {
                author: "human:test".to_string(),
                title: "Decisione con id rotto".to_string(),
                context: String::new(),
                rationale: String::new(),
                related_entity_ids: vec!["non-un-uuid".to_string()],
                related_decision_ids: vec![],
                tags: vec![],
            })
            .await
            .expect_err("un EntityId malformato doveva essere rifiutato");

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn index_then_query_over_the_wire_finds_the_entity() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeos_rpc_{nanos}.py"));
        tokio::fs::write(
            &path,
            "class PaymentService:\n    def charge(self):\n        pass\n",
        )
        .await
        .unwrap();

        let (mut client, _events) = start_server().await;

        // IndexFiles risponde con `entity_ids` VUOTO per costruzione: il Parser non
        // conia gli `EntityId` globali (invariante 1.4) — le entità reali nascono
        // dal GraphActor e compaiono nella query successiva. Qui basta che l'RPC
        // vada a buon fine.
        client
            .index_files(proto::IndexFilesRequest {
                files: vec![path.to_string_lossy().to_string()],
            })
            .await
            .expect("RPC IndexFiles fallita");

        // Il GraphActor è asincrono: ritenta la query finché l'entità compare.
        let mut found = false;
        for _ in 0..50 {
            let response = client
                .query_graph(proto::QueryGraphRequest {
                    natural_language: "voglio sistemare il payment".to_string(),
                })
                .await
                .expect("RPC QueryGraph fallita")
                .into_inner();
            if response
                .entities
                .iter()
                .any(|e| e.qualified_name.contains("PaymentService"))
            {
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(found, "il contesto doveva includere PaymentService");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn architecture_report_over_the_wire_succeeds_on_empty_graph() {
        let (mut client, _events) = start_server().await;

        let response = client
            .get_architecture_report(proto::GetArchitectureReportRequest {})
            .await
            .expect("RPC GetArchitectureReport fallita")
            .into_inner();

        // Grafo vuoto: nessun invariante, nessun fossile, nessuna lacuna — ma l'RPC
        // attraversa tutta la pila (filo → Dispatcher → Guardian → ritorno).
        assert!(response.invariants.is_empty());
        assert!(response.fossils.is_empty());
        assert!(response.gaps.is_empty());
    }

    #[tokio::test]
    async fn watch_events_streams_violation_with_location() {
        use codeos_types::bus::ArchitectureViolation;

        let (mut client, events) = start_server().await;

        // Apri lo stream PRIMA di pubblicare: così la sottoscrizione del server è
        // già attiva quando l'evento arriva sul bus.
        let mut stream = client
            .watch_events(proto::WatchEventsRequest {})
            .await
            .expect("RPC WatchEvents fallita")
            .into_inner();

        // La violazione da consegnare, completa di posizione (la riga esatta che
        // l'editor evidenzierà nel pannello "Problemi").
        let violation = ArchitectureViolation {
            rule_id: EntityId::new(),
            relation_id: EntityId::new(),
            source_id: EntityId::new(),
            target_id: EntityId::new(),
            message: "app::core non deve dipendere da app::api".to_string(),
            location: Some(SourceLocation {
                file_path: "app/core/service.py".to_string(),
                start_line: 42,
                start_column: 4,
                end_line: 42,
                end_column: 30,
            }),
        };

        // C'è una piccola corsa fra l'apertura dello stream e l'attivazione della
        // sottoscrizione lato server (un task spawnato). Ripubblichiamo finché il
        // primo messaggio non arriva; il broadcast scarta i duplicati non ricevuti.
        let publisher = {
            let events = events.clone();
            let violation = violation.clone();
            tokio::spawn(async move {
                for _ in 0..100 {
                    events.publish(CodeOsEvent::ArchitectureViolationDetected {
                        violation: violation.clone(),
                    });
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
        };

        let message = tokio::time::timeout(std::time::Duration::from_secs(5), stream.message())
            .await
            .expect("nessun EventMessage entro il timeout")
            .expect("errore di trasporto sullo stream")
            .expect("lo stream si è chiuso senza eventi");
        publisher.abort();

        // Il messaggio deve essere la violazione, con la posizione preservata
        // end-to-end (bus → server → proto → filo gRPC).
        match message.event {
            Some(proto::event_message::Event::Violation(v)) => {
                assert_eq!(v.message, violation.message);
                let loc = v.location.expect("violazione sul filo senza posizione");
                assert_eq!(loc.file_path, "app/core/service.py");
                assert_eq!(loc.start_line, 42);
                assert_eq!(loc.start_column, 4);
            }
            other => panic!("atteso un evento Violation, ricevuto {other:?}"),
        }
    }
}
