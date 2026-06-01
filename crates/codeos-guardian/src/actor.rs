//! [`GuardianActor`]: il sistema immunitario in ascolto sul bus.
//!
//! Si sottoscrive all'event bus e reagisce a `CodeOsEvent::GraphUpdated`: per
//! ogni delta, (1) verifica le **relazioni appena aggiunte** contro le regole di
//! layering scoperte dal resto del grafo, ripubblicando ogni violazione come
//! `CodeOsEvent::ArchitectureViolationDetected`; (2) **impara**, promuovendo gli
//! invarianti scoperti a memoria storica (se un Memory è agganciato).
//!
//! Non conosce gli altri attori (invariante 1.3): comunica solo via bus e via
//! trait `GraphStorage`/`DecisionStore` (incapsulati nel [`Guardian`]). Non emette
//! mai `GraphUpdated`, quindi non può innescare cicli sul bus.

use std::path::PathBuf;
use std::sync::Arc;

use codeos_memory::DecisionStore;
use codeos_paleo::GitLog;
use codeos_storage::GraphStorage;
use codeos_types::bus::{
    ArchitecturalGapInfo, ArchitectureReport, CodeOsEvent, Command, DecisionFossilInfo,
    LayeringInvariantInfo, Severity,
};
use codeos_types::GraphDelta;
use tokio::sync::{broadcast, mpsc};

use crate::guardian::Guardian;

/// L'attore custode. Consuma `GraphUpdated`, produce `ArchitectureViolationDetected`.
pub struct GuardianActor {
    guardian: Guardian,
    events: broadcast::Sender<CodeOsEvent>,
}

impl GuardianActor {
    pub fn new(storage: Arc<dyn GraphStorage>, events: broadcast::Sender<CodeOsEvent>) -> Self {
        Self {
            guardian: Guardian::new(storage),
            events,
        }
    }

    /// Variante che aggancia il Memory Engine: oltre a segnalare le violazioni,
    /// l'attore **promuove** gli invarianti scoperti a `Decision` persistenti.
    pub fn with_memory(
        storage: Arc<dyn GraphStorage>,
        events: broadcast::Sender<CodeOsEvent>,
        decisions: Arc<dyn DecisionStore>,
    ) -> Self {
        Self {
            guardian: Guardian::with_memory(storage, decisions),
            events,
        }
    }

    /// Variante completa: Memory Engine **e** storia git (il Paleontologo). Aggancia
    /// un [`GitLog`] sulla root del repo, abilitando nel referto architetturale la
    /// confidenza calibrata dal Campo di Astensione e i Fossili di Decisione.
    pub fn with_memory_and_repo(
        storage: Arc<dyn GraphStorage>,
        events: broadcast::Sender<CodeOsEvent>,
        decisions: Arc<dyn DecisionStore>,
        repo_root: PathBuf,
    ) -> Self {
        Self {
            guardian: Guardian::with_memory(storage, decisions)
                .with_commit_history(Arc::new(GitLog::new(repo_root))),
            events,
        }
    }

    /// Consuma **solo** gli eventi del bus finché il canale resta aperto. È la
    /// modalità storica (nessun comando in ingresso): internamente tiene in vita un
    /// canale comandi vuoto, così la logica di servizio è una sola
    /// ([`serve`](Self::serve)) e non si duplica.
    pub async fn run(self, events_rx: broadcast::Receiver<CodeOsEvent>) {
        // Il mittente resta vivo (`_keep`) finché vive il futuro: il ramo comandi
        // della select non si chiuderà mai, semplicemente non riceverà nulla.
        let (_keep, commands_rx) = mpsc::channel(1);
        self.serve(events_rx, commands_rx).await;
    }

    /// Come [`run`](Self::run), ma serve **anche** i comandi diretti (oggi solo
    /// [`Command::ArchitectureReport`]) instradati dal Dispatcher. Eventi e comandi
    /// sono multiplexati con un'unica `select!`: un solo attore, due sorgenti.
    pub async fn run_with_commands(
        self,
        events_rx: broadcast::Receiver<CodeOsEvent>,
        commands_rx: mpsc::Receiver<Command>,
    ) {
        self.serve(events_rx, commands_rx).await;
    }

    /// Il cuore dell'attore: multiplexa eventi del bus e comandi diretti. Esce quando
    /// **entrambe** le sorgenti sono chiuse (il bus eventi e il canale comandi).
    async fn serve(
        self,
        mut events_rx: broadcast::Receiver<CodeOsEvent>,
        mut commands_rx: mpsc::Receiver<Command>,
    ) {
        let mut events_open = true;
        let mut commands_open = true;
        while events_open || commands_open {
            tokio::select! {
                event = events_rx.recv(), if events_open => match event {
                    Ok(CodeOsEvent::GraphUpdated { delta }) => {
                        if let Err(err) = self.handle_graph_updated(&delta).await {
                            // Un fallimento di analisi non deve abbattere l'attore.
                            tracing::error!(error = %err, "GuardianActor: analisi architetturale fallita");
                        }
                    }
                    // Gli altri eventi (incluse le nostre violazioni) non ci riguardano.
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "GuardianActor: in ritardo sul bus, alcuni eventi persi");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!("GuardianActor: bus eventi chiuso");
                        events_open = false;
                    }
                },
                command = commands_rx.recv(), if commands_open => match command {
                    Some(command) => self.handle_command(command).await,
                    None => {
                        tracing::debug!("GuardianActor: canale comandi chiuso");
                        commands_open = false;
                    }
                },
            }
        }
        tracing::debug!("GuardianActor: entrambe le sorgenti chiuse, esco");
    }

    /// Esegue un comando diretto. Oggi l'unico è la richiesta di referto
    /// architetturale: lo costruisce e lo deposita sul `reply_to` del chiamante.
    async fn handle_command(&self, command: Command) {
        match command {
            Command::ArchitectureReport { reply_to } => {
                let report = self.build_report().await;
                // Se il chiamante è andato via, la risposta si perde: non è un errore.
                let _ = reply_to.send(report).await;
            }
            // Il Dispatcher non instrada altri comandi a noi; per robustezza li ignoriamo.
            other => {
                tracing::warn!(?other, "GuardianActor: comando inatteso, ignorato");
            }
        }
    }

    /// Costruisce il **referto architetturale** lungo i quattro assi dello spazio
    /// negativo: invarianti di layering (con confidenza calibrata dal Campo di
    /// Astensione, se c'è storia git), Fossili di Decisione e lacune del secondo
    /// ordine. Appiattisce i tipi ricchi del Guardian/Paleo nei tipi di puro dato
    /// del trasporto ([`ArchitectureReport`]).
    async fn build_report(&self) -> anyhow::Result<ArchitectureReport> {
        let calibrated = self.guardian.has_history();
        let rules = self.guardian.mine_rules_calibrated().await?;
        let fossils = self.guardian.fossils().await?;
        let gaps = self.guardian.missing_invariants().await?;

        let invariants = rules
            .into_iter()
            .map(|rule| {
                let confidence = rule.confidence as f64;
                LayeringInvariantInfo {
                    upstream: rule.upstream.0,
                    downstream: rule.downstream.0,
                    support: rule.support,
                    confidence,
                    calibrated,
                    severity: Severity::for_invariant(confidence),
                }
            })
            .collect();

        let fossils = fossils
            .into_iter()
            .map(|f| DecisionFossilInfo {
                upstream: f.upstream,
                downstream: f.downstream,
                born_at: f.born_at,
                born_at_unix: f.born_at_unix,
                intent: f.intent,
                born_structure: f.born_structure,
            })
            .collect();

        let gaps = gaps
            .into_iter()
            .map(|g| ArchitecturalGapInfo {
                upstream: g.upstream.0,
                downstream: g.downstream.0,
                foundation_support: g.foundation_support,
                severity: Severity::for_gap(g.foundation_support),
            })
            .collect();

        Ok(ArchitectureReport {
            invariants,
            fossils,
            gaps,
        })
    }

    async fn handle_graph_updated(&self, delta: &GraphDelta) -> anyhow::Result<()> {
        if delta.added_relations.is_empty() {
            return Ok(());
        }

        // (1) Anticorpo: segnala gli archi appena aggiunti che invertono una freccia.
        let violations = self.guardian.check(&delta.added_relations).await?;
        for violation in violations {
            tracing::warn!(message = %violation.message, "violazione architetturale rilevata");
            // L'assenza di sottoscrittori non è un errore.
            let _ = self
                .events
                .send(CodeOsEvent::ArchitectureViolationDetected { violation });
        }

        // (2) Apprendimento: promuovi gli invarianti scoperti a memoria storica.
        // No-op se nessun Memory è agganciato. Un fallimento qui non deve invalidare
        // le violazioni già pubblicate: lo logghiamo e proseguiamo.
        match self.guardian.learn().await {
            Ok(ids) if !ids.is_empty() => {
                tracing::info!(
                    count = ids.len(),
                    "nuovi invarianti architetturali persistiti"
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!(error = %err, "GuardianActor: promozione invarianti fallita");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use codeos_storage::SqliteStorage;
    use codeos_types::{Entity, EntityId, EntityKind, Relation, RelationKind, SourceLocation};

    fn entity(qname: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            qualified_name: qname.to_string(),
            location: SourceLocation {
                file_path: format!("{}.py", qname.replace("::", "/")),
                start_line: 1,
                start_column: 0,
                end_line: 2,
                end_column: 0,
            },
            metadata: HashMap::new(),
        }
    }

    fn relation(kind: RelationKind, source: EntityId, target: EntityId) -> Relation {
        Relation {
            id: EntityId::new(),
            kind,
            source_id: source,
            target_id: target,
            metadata: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn publishes_a_violation_when_a_bad_edge_is_added() {
        // Grafo: tre dipendenze app::api → app::core, già persistite, più l'arco
        // proibito core → api (anch'esso persistito, come se fosse appena stato
        // scritto e indicizzato).
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::api::handler_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::core::service_{i}::do_it")))
            .collect();
        let good: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Calls, api[i].id, core[i].id))
            .collect();
        let bad = relation(RelationKind::Calls, core[0].id, api[0].id);

        storage
            .apply_delta(codeos_types::GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: good
                    .into_iter()
                    .chain(std::iter::once(bad.clone()))
                    .collect(),
                ..Default::default()
            })
            .await
            .unwrap();

        // Bus broadcast condiviso fra attore (input + output) e test (osservatore).
        let (tx, _keep) = broadcast::channel(16);
        let actor = GuardianActor::new(storage, tx.clone());
        let actor_rx = tx.subscribe();
        let mut observer = tx.subscribe();
        tokio::spawn(actor.run(actor_rx));

        // Simula il GraphUpdated che annuncia l'arco proibito appena aggiunto.
        tx.send(CodeOsEvent::GraphUpdated {
            delta: codeos_types::GraphDelta {
                added_relations: vec![bad.clone()],
                ..Default::default()
            },
        })
        .unwrap();

        // Attendo la violazione (con timeout, così il test non si blocca mai).
        let violation = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match observer.recv().await.unwrap() {
                    CodeOsEvent::ArchitectureViolationDetected { violation } => break violation,
                    _ => continue,
                }
            }
        })
        .await
        .expect("nessuna ArchitectureViolationDetected entro il timeout");

        assert_eq!(violation.relation_id, bad.id);
        assert!(violation.message.contains("app::core"));
        assert!(violation.message.contains("app::api"));
        // La violazione porta la posizione dell'entità sorgente (core[0]): è ciò
        // che permette all'editor di piazzare la diagnostica sulla riga giusta.
        let location = violation.location.expect("violazione senza posizione");
        assert!(
            location.file_path.contains("core"),
            "file_path = {}",
            location.file_path
        );
    }

    #[tokio::test]
    async fn persists_discovered_invariant_when_memory_is_attached() {
        use codeos_memory::{DecisionKind, InMemoryDecisionStore};

        // Grafo sano: tre dipendenze app::api → app::core, nessun arco proibito.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::api::handler_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::core::service_{i}::do_it")))
            .collect();
        let good: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Calls, api[i].id, core[i].id))
            .collect();
        storage
            .apply_delta(codeos_types::GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: good.clone(),
                ..Default::default()
            })
            .await
            .unwrap();

        let store = Arc::new(InMemoryDecisionStore::new());
        let (tx, _keep) = broadcast::channel(16);
        let actor = GuardianActor::with_memory(storage, tx.clone(), store.clone());
        let actor_rx = tx.subscribe();
        tokio::spawn(actor.run(actor_rx));

        // Annuncia il GraphUpdated: l'attore deve scoprire e persistere l'invariante.
        tx.send(CodeOsEvent::GraphUpdated {
            delta: codeos_types::GraphDelta {
                added_relations: good,
                ..Default::default()
            },
        })
        .unwrap();

        let persisted = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let all = store.all().await.unwrap();
                if !all.is_empty() {
                    break all;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("nessun invariante persistito entro il timeout");

        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].kind, DecisionKind::ArchitectureRule);
        assert!(persisted[0].title.contains("app::core"));
    }

    #[tokio::test]
    async fn answers_an_architecture_report_command() {
        use codeos_types::bus::Command;

        // Grafo sano a due layer: l'invariante app::api → app::core deve emergere.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::api::handler_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::core::service_{i}::do_it")))
            .collect();
        let good: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Calls, api[i].id, core[i].id))
            .collect();
        storage
            .apply_delta(codeos_types::GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: good,
                ..Default::default()
            })
            .await
            .unwrap();

        let (tx, _keep) = broadcast::channel(16);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let actor = GuardianActor::new(storage, tx.clone());
        tokio::spawn(actor.run_with_commands(tx.subscribe(), cmd_rx));

        // Chiedi il referto via comando, come farebbe il Dispatcher.
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
        cmd_tx
            .send(Command::ArchitectureReport { reply_to: reply_tx })
            .await
            .unwrap();

        let report = tokio::time::timeout(Duration::from_secs(5), reply_rx.recv())
            .await
            .expect("nessuna risposta al referto entro il timeout")
            .expect("canale di risposta chiuso")
            .expect("referto fallito");

        assert_eq!(report.invariants.len(), 1, "report = {report:?}");
        assert_eq!(report.invariants[0].upstream, "app::core");
        assert_eq!(report.invariants[0].downstream, "app::api");
        // Nessuna storia git agganciata ⇒ confidenza solo strutturale.
        assert!(!report.invariants[0].calibrated);
        // Senza storia non ci sono fossili; il grafo a due layer non ha lacune.
        assert!(report.fossils.is_empty());
        assert!(report.gaps.is_empty());
    }
}
