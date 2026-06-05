//! `QueryEngine` / Context Builder: la feature principale (briefing sez. 10).
//!
//! Riceve una query in linguaggio naturale e restituisce il **sottografo minimo
//! rilevante** già formattato per un LLM. Implementa l'algoritmo in 6 passi del
//! briefing, incluso il Passo 3: il *perché* (le [`Decision`] del Memory Engine
//! agganciate alle entità selezionate) viene iniettato nel contesto.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use codeos_memory::{Decision, DecisionStore, InMemoryDecisionStore};
use codeos_storage::{GraphStorage, RelationFilter};
use codeos_types::bus::{QueryRequest, QueryResponse};
use codeos_types::{Entity, EntityId, Relation, RelationKind};

/// Profondità massima della BFS (Passo 4).
const DEFAULT_MAX_DEPTH: u32 = 3;
/// Numero massimo di entità nel sottografo, per non sforare la context window.
const DEFAULT_MAX_ENTITIES: usize = 50;

/// Parametri configurabili dell'espansione.
#[derive(Debug, Clone)]
pub struct QueryConfig {
    pub max_depth: u32,
    pub max_entities: usize,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_entities: DEFAULT_MAX_ENTITIES,
        }
    }
}

/// Costruisce il contesto minimo rilevante a partire dal grafo persistito.
pub struct QueryEngine {
    storage: Arc<dyn GraphStorage>,
    decisions: Arc<dyn DecisionStore>,
    config: QueryConfig,
}

impl QueryEngine {
    pub fn new(storage: Arc<dyn GraphStorage>) -> Self {
        Self::with_config_and_decisions(storage, QueryConfig::default(), empty_decisions())
    }

    pub fn with_config(storage: Arc<dyn GraphStorage>, config: QueryConfig) -> Self {
        Self::with_config_and_decisions(storage, config, empty_decisions())
    }

    /// Aggancia il Memory Engine: le decisioni relative alle entità selezionate
    /// entreranno nel contesto (Passo 3).
    pub fn with_decisions(
        storage: Arc<dyn GraphStorage>,
        decisions: Arc<dyn DecisionStore>,
    ) -> Self {
        Self::with_config_and_decisions(storage, QueryConfig::default(), decisions)
    }

    pub fn with_config_and_decisions(
        storage: Arc<dyn GraphStorage>,
        config: QueryConfig,
        decisions: Arc<dyn DecisionStore>,
    ) -> Self {
        Self {
            storage,
            decisions,
            config,
        }
    }

    /// Esegue i passi 1–2 e 4–6 del briefing su una query in linguaggio naturale.
    pub async fn query(&self, request: &QueryRequest) -> anyhow::Result<QueryResponse> {
        let QueryRequest::NaturalLanguage { text } = request;

        // Passo 1 — Estrazione keyword. (Il Passo 3, le decisioni, è più sotto:
        // si interroga il Memory solo sulle entità davvero selezionate.)
        let keywords = extract_keywords(text);

        // Passo 2 — Semi: entità il cui qualified_name contiene una keyword.
        let seeds = self.find_seeds(&keywords).await?;
        if seeds.is_empty() {
            return Ok(QueryResponse {
                formatted_context: format!(
                    "Nessuna entità rilevante trovata per: \"{text}\"\n\
                     (keyword estratte: {})",
                    if keywords.is_empty() {
                        "nessuna".to_string()
                    } else {
                        keywords.join(", ")
                    }
                ),
                entities: Vec::new(),
                relations: Vec::new(),
            });
        }

        // Passi 4-5 — Espansione BFS con punteggio di rilevanza.
        let expansion = self.expand(&seeds).await?;

        // Seleziona le entità più rilevanti (punteggio decrescente, cap a max).
        let mut selected = select_top(
            expansion.entities,
            &expansion.scores,
            self.config.max_entities,
        );
        let mut selected_ids: HashSet<EntityId> = selected.iter().map(|e| e.id).collect();

        // Passo 5 — Inclusione dei test correlati.
        let test_relations = self.include_tests(&mut selected, &mut selected_ids).await?;

        // Passo 3 — Il *perché*: le decisioni del Memory Engine agganciate alle
        // entità selezionate. Lo facciamo dopo la selezione (e i test) così da
        // chiedere allo store solo le entità che entreranno davvero nel contesto.
        // Solo le decisioni CORRENTI: una scelta già rimpiazzata o ritirata non è
        // il perché di oggi e portarla nel contesto sarebbe un arco che mente.
        let selected_id_list: Vec<EntityId> = selected.iter().map(|e| e.id).collect();
        let decisions = self.decisions.current_related_to(&selected_id_list).await?;

        // Tieni solo le relazioni i cui due estremi sono nel set selezionato.
        let mut relations: Vec<Relation> = expansion
            .relations
            .into_iter()
            .filter(|r| selected_ids.contains(&r.source_id) && selected_ids.contains(&r.target_id))
            .collect();
        relations.extend(test_relations);

        // Conta le relazioni irrisolte che partono dalle entità selezionate
        // (per la NOTA finale).
        let unresolved = self.count_unresolved(&selected_ids).await?;

        // Passo 6 — Formattazione del contesto.
        let formatted_context = format_context(text, &selected, &relations, &decisions, unresolved);

        Ok(QueryResponse {
            formatted_context,
            entities: selected,
            relations,
        })
    }

    async fn find_seeds(&self, keywords: &[String]) -> anyhow::Result<Vec<Entity>> {
        let mut by_id: HashMap<EntityId, Entity> = HashMap::new();
        for keyword in keywords {
            for entity in self.storage.find_entities_by_name_pattern(keyword).await? {
                by_id.entry(entity.id).or_insert(entity);
            }
        }
        Ok(by_id.into_values().collect())
    }

    async fn expand(&self, seeds: &[Entity]) -> anyhow::Result<Expansion> {
        let mut entities: HashMap<EntityId, Entity> = HashMap::new();
        let mut scores: HashMap<EntityId, u32> = HashMap::new();
        let mut relations: HashMap<EntityId, Relation> = HashMap::new();

        for seed in seeds {
            entities.entry(seed.id).or_insert_with(|| seed.clone());

            let mut visited: HashSet<EntityId> = HashSet::new();
            visited.insert(seed.id);
            let mut queue: VecDeque<(EntityId, u32)> = VecDeque::new();
            queue.push_back((seed.id, 0));

            while let Some((id, depth)) = queue.pop_front() {
                // Rilevanza: +1 per ogni seme da cui questo nodo è raggiungibile.
                *scores.entry(id).or_insert(0) += 1;
                if depth >= self.config.max_depth {
                    continue;
                }

                let outgoing = self
                    .storage
                    .query_relations(RelationFilter {
                        source_id: Some(id),
                        ..Default::default()
                    })
                    .await?;

                for rel in outgoing {
                    if !should_follow(rel.kind) || rel.target_id.is_nil() {
                        continue;
                    }
                    let target_entity = match entities.get(&rel.target_id) {
                        Some(existing) => existing.clone(),
                        None => match self.storage.get_entity_by_id(&rel.target_id).await? {
                            Some(found) => found,
                            None => continue,
                        },
                    };
                    // Non scendere nelle dipendenze esterne (std, site-packages…).
                    if is_external(&target_entity.qualified_name) {
                        continue;
                    }

                    entities.entry(rel.target_id).or_insert(target_entity);
                    relations.entry(rel.id).or_insert(rel.clone());
                    if visited.insert(rel.target_id) {
                        queue.push_back((rel.target_id, depth + 1));
                    }
                }
            }
        }

        Ok(Expansion {
            entities,
            scores,
            relations: relations.into_values().collect(),
        })
    }

    async fn include_tests(
        &self,
        selected: &mut Vec<Entity>,
        selected_ids: &mut HashSet<EntityId>,
    ) -> anyhow::Result<Vec<Relation>> {
        let mut test_relations = Vec::new();
        let current: Vec<EntityId> = selected.iter().map(|e| e.id).collect();
        for id in current {
            let tests = self
                .storage
                .query_relations(RelationFilter {
                    target_id: Some(id),
                    kind: Some(RelationKind::Tests),
                    ..Default::default()
                })
                .await?;
            for rel in tests {
                if let Some(test_entity) = self.storage.get_entity_by_id(&rel.source_id).await? {
                    if selected_ids.insert(test_entity.id) {
                        selected.push(test_entity);
                    }
                    test_relations.push(rel);
                }
            }
        }
        Ok(test_relations)
    }

    async fn count_unresolved(&self, selected_ids: &HashSet<EntityId>) -> anyhow::Result<usize> {
        let mut total = 0;
        for id in selected_ids {
            total += self
                .storage
                .query_relations(RelationFilter {
                    source_id: Some(*id),
                    kind: Some(RelationKind::Unresolved),
                    ..Default::default()
                })
                .await?
                .len();
        }
        Ok(total)
    }
}

/// Risultato dell'espansione BFS, prima della selezione finale.
struct Expansion {
    entities: HashMap<EntityId, Entity>,
    scores: HashMap<EntityId, u32>,
    relations: Vec<Relation>,
}

/// Le relazioni lungo cui la BFS scende (Passo 4). Le altre (es. `Unresolved`,
/// `Implements`, `Extends`) non vengono attraversate per l'espansione di contesto.
fn should_follow(kind: RelationKind) -> bool {
    matches!(
        kind,
        RelationKind::Calls | RelationKind::Imports | RelationKind::Uses | RelationKind::BelongsTo
    )
}

/// `true` per le entità di librerie esterne, da cui non si espande.
fn is_external(qualified_name: &str) -> bool {
    qualified_name.starts_with("std::") || qualified_name.contains("site-packages")
}

/// Ordina per punteggio decrescente (a parità, per nome) e taglia a `limit`.
fn select_top(
    entities: HashMap<EntityId, Entity>,
    scores: &HashMap<EntityId, u32>,
    limit: usize,
) -> Vec<Entity> {
    let mut all: Vec<Entity> = entities.into_values().collect();
    all.sort_by(|a, b| {
        let score_a = scores.get(&a.id).copied().unwrap_or(0);
        let score_b = scores.get(&b.id).copied().unwrap_or(0);
        score_b
            .cmp(&score_a)
            .then_with(|| a.qualified_name.cmp(&b.qualified_name))
    });
    all.truncate(limit);
    all
}

/// Tokenizza, normalizza e filtra le stopword. Mantiene l'ordine di apparizione,
/// senza duplicati.
fn extract_keywords(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keywords = Vec::new();
    for token in text.split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        let lowered = token.to_lowercase();
        if lowered.len() < 3 || is_stopword(&lowered) {
            continue;
        }
        if seen.insert(lowered.clone()) {
            keywords.push(lowered);
        }
    }
    keywords
}

/// Stopword minime (italiano + inglese). Non esaustivo: per la v1 basta togliere
/// le parole-funzione più comuni che inquinerebbero i semi.
fn is_stopword(token: &str) -> bool {
    const STOPWORDS: &[&str] = &[
        // Italiano
        "voglio",
        "vorrei",
        "aggiungere",
        "creare",
        "come",
        "dove",
        "quando",
        "perche",
        "perché",
        "della",
        "dello",
        "degli",
        "delle",
        "con",
        "per",
        "una",
        "uno",
        "gli",
        "che",
        "non",
        "del",
        "dei",
        "sul",
        "sulla",
        "nel",
        "nella",
        "questo",
        "questa",
        "fare",
        "deve",
        "sono",
        "essere",
        // Inglese
        "the",
        "and",
        "for",
        "with",
        "that",
        "this",
        "want",
        "add",
        "create",
        "how",
        "where",
        "what",
        "into",
        "from",
        "have",
        "has",
        "are",
        "should",
        "would",
        "could",
        "must",
    ];
    STOPWORDS.contains(&token)
}

/// Store di decisioni vuoto: il default quando il Memory Engine non è agganciato.
/// `related_to` su uno store vuoto restituisce sempre nessuna decisione, così la
/// sezione "DECISIONI ARCHITETTURALI" semplicemente non compare.
fn empty_decisions() -> Arc<dyn DecisionStore> {
    Arc::new(InMemoryDecisionStore::new())
}

/// Passo 6: genera il prompt strutturato per l'LLM.
fn format_context(
    text: &str,
    entities: &[Entity],
    relations: &[Relation],
    decisions: &[Decision],
    unresolved: usize,
) -> String {
    let by_id: HashMap<EntityId, &Entity> = entities.iter().map(|e| (e.id, e)).collect();

    let mut out = String::new();
    out.push_str(&format!("Contesto per: \"{text}\"\n\n"));

    out.push_str("FILE RILEVANTI:\n");
    for entity in entities {
        out.push_str(&format!(
            "- {} [{:?}] — {}\n",
            entity.qualified_name, entity.kind, entity.location.file_path
        ));
    }

    out.push_str("\nDIPENDENZE CHIAVE:\n");
    let mut wrote_dep = false;
    for rel in relations {
        let (Some(source), Some(target)) = (by_id.get(&rel.source_id), by_id.get(&rel.target_id))
        else {
            continue;
        };
        out.push_str(&format!(
            "- {} {} {}\n",
            source.qualified_name,
            format!("{:?}", rel.kind).to_uppercase(),
            target.qualified_name
        ));
        wrote_dep = true;
    }
    if !wrote_dep {
        out.push_str("- (nessuna dipendenza interna tra le entità selezionate)\n");
    }

    // Passo 3 — Il *perché*. Mostrato solo se c'è davvero una memoria agganciata,
    // per non aggiungere rumore quando non ci sono decisioni rilevanti.
    if !decisions.is_empty() {
        out.push_str("\nDECISIONI ARCHITETTURALI (il perché):\n");
        for decision in decisions {
            out.push_str(&format!("- {}", decision.title));
            let rationale = decision.rationale.trim();
            if !rationale.is_empty() {
                out.push_str(&format!(": {}", one_line(rationale)));
            }
            out.push_str(&format!(" (autore: {})\n", decision.author));
        }
    }

    if unresolved > 0 {
        out.push_str(&format!(
            "\nNOTA: {unresolved} relazioni non risolte tra le entità selezionate. \
             Verificare le dipendenze esterne.\n"
        ));
    }

    out
}

/// Comprime il testo multilinea del razionale in una sola riga leggibile.
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_graph::GraphResolver;
    use codeos_parser::{LanguageParser, PythonParser};
    use codeos_storage::SqliteStorage;
    use std::path::Path;

    /// Costruisce uno storage con il grafo di un piccolo sorgente Python.
    async fn graph_from(path: &str, src: &str) -> Arc<SqliteStorage> {
        let parsed = PythonParser::new().parse_file(Path::new(path), src).await;
        let storage = SqliteStorage::in_memory().unwrap();
        let delta = GraphResolver::new(None)
            .resolve(&[parsed], &storage)
            .await
            .unwrap();
        storage.apply_delta(delta).await.unwrap();
        Arc::new(storage)
    }

    fn nl(text: &str) -> QueryRequest {
        QueryRequest::NaturalLanguage {
            text: text.to_string(),
        }
    }

    #[test]
    fn keyword_extraction_drops_stopwords_and_short_tokens() {
        let kws = extract_keywords("Voglio aggiungere il login OAuth");
        assert!(kws.contains(&"login".to_string()), "kws = {kws:?}");
        assert!(kws.contains(&"oauth".to_string()), "kws = {kws:?}");
        assert!(!kws.contains(&"voglio".to_string()), "kws = {kws:?}");
        assert!(!kws.contains(&"il".to_string()), "kws = {kws:?}");
    }

    #[tokio::test]
    async fn finds_seed_by_keyword_and_returns_context() {
        let storage = graph_from(
            "auth/login_service.py",
            "class LoginService:\n    def authenticate(self):\n        pass\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let response = engine
            .query(&nl("voglio sistemare il login"))
            .await
            .unwrap();

        assert!(
            response
                .entities
                .iter()
                .any(|e| e.qualified_name.contains("login_service")),
            "entità = {:?}",
            response
                .entities
                .iter()
                .map(|e| &e.qualified_name)
                .collect::<Vec<_>>()
        );
        assert!(response.formatted_context.contains("FILE RILEVANTI"));
        assert!(response.formatted_context.contains("login"));
        // Senza Memory agganciato la sezione del *perché* non deve comparire.
        assert!(!response
            .formatted_context
            .contains("DECISIONI ARCHITETTURALI"));
    }

    #[tokio::test]
    async fn injects_related_decision_into_context() {
        use codeos_memory::DecisionKind;
        use codeos_types::bus::NewDecision;

        let storage = graph_from(
            "auth/login_service.py",
            "class LoginService:\n    def authenticate(self):\n        pass\n",
        )
        .await;

        // Aggancia una decisione all'entità del login: è il "perché" da iniettare.
        let login = storage
            .find_entities_by_name_pattern("login")
            .await
            .unwrap();
        let target = login.first().expect("entità login attesa").id;

        let decisions = Arc::new(InMemoryDecisionStore::new());
        decisions
            .record(&Decision::from_new(
                NewDecision {
                    author: "ai:ArchitectureGuardian".to_string(),
                    title: "Login lato server".to_string(),
                    context: "Sicurezza".to_string(),
                    rationale: "Non esporre i segreti al client.".to_string(),
                    related_entity_ids: vec![target],
                    related_decision_ids: Vec::new(),
                    tags: vec!["sicurezza".to_string()],
                },
                DecisionKind::ArchitectureRule,
            ))
            .await
            .unwrap();

        let engine = QueryEngine::with_decisions(storage, decisions);
        let response = engine
            .query(&nl("voglio sistemare il login"))
            .await
            .unwrap();

        assert!(
            response
                .formatted_context
                .contains("DECISIONI ARCHITETTURALI"),
            "manca la sezione decisioni:\n{}",
            response.formatted_context
        );
        assert!(
            response
                .formatted_context
                .contains("Non esporre i segreti al client"),
            "manca il razionale:\n{}",
            response.formatted_context
        );
    }

    #[tokio::test]
    async fn context_carries_the_current_why_not_the_retired_one() {
        // Una scelta superata non è il perché di oggi: il contesto deve mostrare la
        // decisione corrente, mai quella già rimpiazzata (un perché stale mente).
        use codeos_memory::DecisionKind;
        use codeos_types::bus::NewDecision;

        let storage = graph_from(
            "auth/login_service.py",
            "class LoginService:\n    def authenticate(self):\n        pass\n",
        )
        .await;
        let target = storage
            .find_entities_by_name_pattern("login")
            .await
            .unwrap()
            .first()
            .expect("entità login attesa")
            .id;

        let new = |title: &str, rationale: &str| NewDecision {
            author: "ai:ArchitectureGuardian".to_string(),
            title: title.to_string(),
            context: String::new(),
            rationale: rationale.to_string(),
            related_entity_ids: vec![target],
            related_decision_ids: Vec::new(),
            tags: Vec::new(),
        };

        let decisions = Arc::new(InMemoryDecisionStore::new());
        let old = Decision::from_new(
            new("Sessioni lato client", "Token JWT nel localStorage."),
            DecisionKind::ArchitectureRule,
        );
        // La nuova scelta rimpiazza la vecchia, agganciata alla STESSA entità.
        let mut newer = Decision::from_new(
            new(
                "Sessioni lato server",
                "Cookie httpOnly, niente token nel client.",
            ),
            DecisionKind::ArchitectureRule,
        );
        newer.supersedes = vec![old.id];
        decisions.record(&old).await.unwrap();
        decisions.record(&newer).await.unwrap();

        let engine = QueryEngine::with_decisions(storage, decisions);
        let ctx = engine
            .query(&nl("voglio sistemare il login"))
            .await
            .unwrap()
            .formatted_context;

        assert!(
            ctx.contains("Cookie httpOnly"),
            "manca il perché corrente:\n{ctx}"
        );
        assert!(
            !ctx.contains("localStorage"),
            "il perché superato non deve entrare nel contesto:\n{ctx}"
        );
    }

    #[tokio::test]
    async fn bfs_pulls_in_called_entity_from_a_seed() {
        // `login` (seme) chiama `verify_password`: la BFS deve includerlo.
        let storage = graph_from(
            "auth.py",
            "def verify_password():\n    pass\n\ndef login():\n    verify_password()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let response = engine.query(&nl("login")).await.unwrap();
        let names: Vec<&str> = response
            .entities
            .iter()
            .map(|e| e.qualified_name.as_str())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("::login")),
            "names = {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("::verify_password")),
            "la BFS doveva includere verify_password: {names:?}"
        );
    }

    #[tokio::test]
    async fn empty_result_when_nothing_matches() {
        let storage = graph_from("m.py", "def unrelated():\n    pass\n").await;
        let engine = QueryEngine::new(storage);

        let response = engine.query(&nl("payment gateway stripe")).await.unwrap();
        assert!(response.entities.is_empty());
        assert!(response
            .formatted_context
            .contains("Nessuna entità rilevante"));
    }
}
