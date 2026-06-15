//! [`MemoryActor`]: l'attore che custodisce la memoria storica.
//!
//! Riceve i [`Command::RecordDecision`] instradati dal Dispatcher, costruisce una
//! [`Decision`] completa (id + timestamp), la **ancora** alle entità reali del
//! grafo (vedi `anchor_decision`), la persiste tramite un [`DecisionStore`] e
//! risponde con l'`EntityId` assegnato.
//!
//! Non conosce gli altri attori (invariante 1.3): comunica solo via canali e via
//! i trait di storage.

use std::collections::HashSet;
use std::sync::Arc;

use codeos_storage::GraphStorage;
use codeos_types::bus::Command;
use codeos_types::{EntityId, EntityKind};
use tokio::sync::mpsc;

use crate::decision::{Decision, DecisionKind};
use crate::store::DecisionStore;

/// Una keyword del titolo/tag deve matchare AL PIÙ questo numero di entità per
/// ancorare: oltre è una parola comune (rumore), non un identificatore. Stessa
/// disciplina IDF del context builder — un match raro è un segnale forte.
const MAX_ANCHOR_MATCHES: usize = 8;
/// Tetto ai tag di ancoraggio aggiunti, per non gonfiare il ledger.
const MAX_ANCHOR_TAGS: usize = 12;
/// Lunghezza minima di una keyword candidata all'ancoraggio.
const MIN_ANCHOR_LEN: usize = 4;

/// L'attore della memoria.
pub struct MemoryActor {
    store: Arc<dyn DecisionStore>,
    /// Lo storage del grafo, per ANCORARE le decisioni alle entità reali alla
    /// registrazione. `None` nei contesti senza grafo (test in isolamento): in
    /// quel caso l'ancoraggio è un no-op e la decisione si salva com'è.
    graph: Option<Arc<dyn GraphStorage>>,
}

impl MemoryActor {
    pub fn new(store: Arc<dyn DecisionStore>) -> Self {
        Self { store, graph: None }
    }

    /// Aggiunge lo storage del grafo per abilitare l'auto-anchoring. È il
    /// composition root a iniettarlo (lo stesso `GraphStorage` del GraphActor).
    pub fn with_graph(mut self, graph: Arc<dyn GraphStorage>) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Consuma i comandi finché il canale resta aperto.
    pub async fn run(self, mut commands: mpsc::Receiver<Command>) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::RecordDecision { decision, reply_to } => {
                    let mut decision = Decision::from_new(decision, DecisionKind::Decision);
                    self.anchor_decision(&mut decision).await;
                    let result = self.record(decision).await;
                    if let Err(err) = &result {
                        tracing::error!(error = %err, "MemoryActor: registrazione della decisione fallita");
                    }
                    // Se il chiamante ha già rinunciato all'attesa, ignoriamo l'errore di send.
                    let _ = reply_to.send(result).await;
                }
                // Il Dispatcher non dovrebbe instradarci altro: lo segnaliamo senza panic.
                other => {
                    tracing::warn!(?other, "MemoryActor: comando inatteso, ignorato");
                }
            }
        }
        tracing::debug!("MemoryActor: canale comandi chiuso, esco");
    }

    /// AUTO-ANCHORING (la raccomandazione #1 sul moat): una
    /// decisione umana nasce spesso «fluttuante» — `related_entity_ids` vuoto e
    /// tag CONCETTUALI (`wilson`, `confidenza`). Il context builder la include
    /// solo se un tag combacia ESATTAMENTE con un segmento `::` di un'entità
    /// selezionata; i tag concettuali non combaciano mai → la decisione cade
    /// fuori dal pack PROPRIO dove conta di più (il *perché* non-derivabile).
    ///
    /// Qui, alla registrazione, cerchiamo nel grafo le entità i cui nomi sono
    /// EVOCATI dalle keyword del titolo/tag e aggiungiamo i loro nomi-foglia
    /// (segmenti finali del qualified_name) come TAG. Scegliamo i tag-segmento e
    /// NON gli `EntityId` perché i tag sono STABILI al re-index (gli `EntityId`
    /// si rigenerano; i qualified_name no) — l'ancoraggio sopravvive a un nuovo
    /// `index`.
    ///
    /// Anti-FP: ancoriamo solo per keyword SPECIFICHE (che matchano ≤
    /// `MAX_ANCHOR_MATCHES` entità — un match raro è un identificatore, non
    /// prosa); le esterne sono escluse; nessuna keyword evocativa ⇒ nessun
    /// ancoraggio (la decisione resta com'era, niente regressione). Se l'autore
    /// ha già ancorato a mano (related_entity_ids non vuoto), non tocchiamo
    /// nulla.
    async fn anchor_decision(&self, decision: &mut Decision) {
        let Some(graph) = &self.graph else {
            return;
        };
        if !decision.related_entity_ids.is_empty() {
            return; // ancorata esplicitamente: rispettiamo la scelta umana.
        }

        let existing: HashSet<String> = decision.tags.iter().cloned().collect();
        let mut added: Vec<String> = Vec::new();
        for keyword in anchor_keywords(&decision.title, &decision.tags) {
            if added.len() >= MAX_ANCHOR_TAGS {
                break;
            }
            let Ok(matches) = graph.find_entities_by_name_pattern(&keyword).await else {
                continue;
            };
            let real: Vec<_> = matches
                .into_iter()
                .filter(|e| e.kind != EntityKind::ExternalDependency)
                .collect();
            // Vuoto = parola concettuale senza riscontro nel codice (es. wilson
            // se non c'è alcun `wilson*`): nessun ancoraggio. Troppi = parola
            // comune: rumore, si astiene.
            if real.is_empty() || real.len() > MAX_ANCHOR_MATCHES {
                continue;
            }
            for e in real {
                let leaf = e
                    .qualified_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&e.qualified_name)
                    .to_string();
                if !leaf.is_empty() && !existing.contains(&leaf) && !added.contains(&leaf) {
                    added.push(leaf);
                }
            }
        }
        added.truncate(MAX_ANCHOR_TAGS);
        if !added.is_empty() {
            tracing::info!(
                anchored = added.len(),
                title = %decision.title,
                "decisione auto-ancorata a entità reali (tag-segmento)"
            );
            decision.tags.extend(added);
        }
    }

    async fn record(&self, decision: Decision) -> anyhow::Result<EntityId> {
        let id = decision.id;
        self.store.record(&decision).await?;
        tracing::info!(title = %decision.title, %id, "decisione registrata");
        Ok(id)
    }
}

/// Le keyword candidate all'ancoraggio: i token alfanumerici (≥ `MIN_ANCHOR_LEN`,
/// non stopword) del titolo PIÙ i tag così come sono (un tag è già un termine
/// scelto). I token del titolo conservano `_` interni (gli identificatori
/// snake_case restano interi: `wilson_lower_bound` non si spezza).
fn anchor_keywords(title: &str, tags: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let kw = raw.trim().to_lowercase();
        if kw.len() >= MIN_ANCHOR_LEN && !is_anchor_stopword(&kw) && seen.insert(kw.clone()) {
            out.push(kw);
        }
    };
    for tag in tags {
        // Un tag tipo `license-deny:GPL-3.0` non è un nome di entità: gli interi
        // tag-macchina (con ':') si saltano, gli altri si tentano interi.
        if !tag.contains(':') {
            push(tag);
        }
    }
    for token in title.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        push(token);
    }
    out
}

/// Stopword minime per l'ancoraggio: parole di prosa che non sono mai nomi di
/// entità ma che potrebbero matchare per coincidenza.
fn is_anchor_stopword(token: &str) -> bool {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "deve", "della", "dello", "delle",
        "degli", "come", "dove", "quando", "perche", "perché", "solo", "mai", "sempre", "tutto",
        "tutti", "essere", "viene", "fare", "anche", "senza", "sopra", "sotto", "ogni", "loro",
        "nostro", "questo", "questa", "quello",
    ];
    STOP.contains(&token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_types::bus::NewDecision;

    use crate::store::InMemoryDecisionStore;

    #[tokio::test]
    async fn auto_anchoring_adds_real_entity_leaf_tags_for_a_floating_decision() {
        use codeos_storage::{GraphStorage, SqliteStorage};
        use codeos_types::{Entity, EntityId, EntityKind, GraphDelta, SourceLocation};

        // Grafo con un'entità il cui nome-foglia è `wilson_lower_bound`.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let ent = Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            qualified_name: "crate::cli::wilson_lower_bound".to_string(),
            location: SourceLocation {
                file_path: "cli.rs".to_string(),
                start_line: 1,
                start_column: 0,
                end_line: 2,
                end_column: 0,
            },
            metadata: Default::default(),
        };
        let mut delta = GraphDelta::default();
        delta.added_entities.push(ent);
        storage.apply_delta(delta).await.unwrap();

        let store = Arc::new(InMemoryDecisionStore::new());
        let actor = MemoryActor::new(store.clone()).with_graph(storage.clone());
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(actor.run(rx));

        // Decisione FLUTTUANTE: nessuna entità ancorata, tag CONCETTUALI.
        let (reply, mut reply_rx) = mpsc::channel(1);
        tx.send(Command::RecordDecision {
            decision: NewDecision {
                author: "human:test".into(),
                title: "la confidenza è solo il wilson_lower_bound, mai sostituito".into(),
                context: String::new(),
                rationale: "trap #2".into(),
                related_entity_ids: Vec::new(),
                related_decision_ids: Vec::new(),
                supersedes: Vec::new(),
                deprecates: Vec::new(),
                tags: vec!["wilson".into(), "confidenza".into()],
            },
            reply_to: reply,
        })
        .await
        .unwrap();
        reply_rx.recv().await.unwrap().unwrap();

        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 1);
        // L'anchoring ha aggiunto il nome-foglia REALE come tag: ora il filtro
        // (b) del context builder (tag == segmento) può agganciare la decisione
        // quando il goal seleziona `wilson_lower_bound`.
        assert!(
            all[0].tags.iter().any(|t| t == "wilson_lower_bound"),
            "tag ancorati attesi, trovati: {:?}",
            all[0].tags
        );
        // I tag concettuali originali restano.
        assert!(all[0].tags.iter().any(|t| t == "wilson"));
    }

    #[tokio::test]
    async fn no_graph_means_no_anchoring_no_regression() {
        // Senza grafo (test in isolamento), la decisione si salva intatta.
        let store = Arc::new(InMemoryDecisionStore::new());
        let actor = MemoryActor::new(store.clone());
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(actor.run(rx));
        let (reply, mut reply_rx) = mpsc::channel(1);
        tx.send(Command::RecordDecision {
            decision: NewDecision {
                author: "human:test".into(),
                title: "una scelta".into(),
                context: String::new(),
                rationale: String::new(),
                related_entity_ids: Vec::new(),
                related_decision_ids: Vec::new(),
                supersedes: Vec::new(),
                deprecates: Vec::new(),
                tags: vec!["x".into()],
            },
            reply_to: reply,
        })
        .await
        .unwrap();
        reply_rx.recv().await.unwrap().unwrap();
        let all = store.all().await.unwrap();
        assert_eq!(all[0].tags, vec!["x".to_string()]);
    }

    #[tokio::test]
    async fn records_a_decision_and_replies_with_its_id() {
        let store = Arc::new(InMemoryDecisionStore::new());
        let actor = MemoryActor::new(store.clone());
        let (commands_tx, commands_rx) = mpsc::channel(4);
        tokio::spawn(actor.run(commands_rx));

        let (reply_to, mut reply_rx) = mpsc::channel(1);
        commands_tx
            .send(Command::RecordDecision {
                decision: NewDecision {
                    author: "human:test".to_string(),
                    title: "Scelta di prova".to_string(),
                    context: "contesto".to_string(),
                    rationale: "razionale".to_string(),
                    related_entity_ids: vec![EntityId::new()],
                    related_decision_ids: Vec::new(),
                    supersedes: Vec::new(),
                    deprecates: Vec::new(),
                    tags: vec!["test".to_string()],
                },
                reply_to,
            })
            .await
            .expect("canale comandi chiuso");

        let id = reply_rx
            .recv()
            .await
            .expect("nessuna risposta dal MemoryActor")
            .expect("registrazione fallita");

        // La decisione è davvero finita nello store, con l'id restituito.
        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        assert_eq!(all[0].title, "Scelta di prova");
    }
}
