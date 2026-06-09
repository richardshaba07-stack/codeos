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
use codeos_types::bus::{CallPathReply, CallPathStatus, QueryRequest, QueryResponse};
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

        // Raccoglie i BUCHI NOTI: i riferimenti che partono dalle entità
        // selezionate e che il resolver NON ha collegato a un'entità del progetto.
        // Il contesto li NOMINA (non li conta soltanto): un buco nascosto farebbe
        // credere all'LLM che l'entità sia più connessa di quanto il grafo sappia
        // — un arco mancante spacciato per completezza. È la tesi anti-falso-
        // positivo applicata al contesto: meglio un buco nominato che un numero
        // che lo nasconde.
        let holes = self.collect_unresolved(&selected_ids).await?;

        // Passo 6 — Formattazione del contesto.
        let formatted_context = format_context(text, &selected, &relations, &decisions, &holes);

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

    /// Raccoglie i BUCHI NOTI delle entità selezionate: i riferimenti che il
    /// resolver ha lasciato `Unresolved` (target ignoto). Ogni arco `Unresolved`
    /// porta nei metadata il nome testuale originale (`unresolved_target`) e il
    /// tipo di riferimento mancato (`original_kind`): li riportiamo così com'erano
    /// nel sorgente, senza inventare un bersaglio. De-duplica per (sorgente,
    /// target) e salta gli archi senza nome (non si nomina un buco che non c'è).
    async fn collect_unresolved(
        &self,
        selected_ids: &HashSet<EntityId>,
    ) -> anyhow::Result<Vec<UnresolvedHole>> {
        let mut holes = Vec::new();
        let mut seen: HashSet<(EntityId, String)> = HashSet::new();
        for id in selected_ids {
            let rels = self
                .storage
                .query_relations(RelationFilter {
                    source_id: Some(*id),
                    kind: Some(RelationKind::Unresolved),
                    ..Default::default()
                })
                .await?;
            for rel in rels {
                let Some(target) = rel.metadata.get("unresolved_target") else {
                    continue;
                };
                if target.is_empty() || !seen.insert((*id, target.clone())) {
                    continue;
                }
                holes.push(UnresolvedHole {
                    source_id: *id,
                    target: target.clone(),
                    original_kind: rel
                        .metadata
                        .get("original_kind")
                        .cloned()
                        .unwrap_or_default(),
                });
            }
        }
        Ok(holes)
    }

    /// Cerca il **cammino di chiamata** più corto da `from` a `to` seguendo SOLO
    /// archi `Calls` RISOLTI (target non-nil). BFS diretta con tracciamento del
    /// padre: il primo cammino trovato è anche il più corto.
    ///
    /// È il livello L2 del context builder (pilastro 3) ridotto alla sua primitiva
    /// onesta. Due garanzie anti-falso-positivo, per costruzione:
    /// - `Some(path)` ⇒ OGNI passo consecutivo è una chiamata reale e già risolta
    ///   dal grafo — il cammino non attraversa MAI un arco indovinato, non può mentire;
    /// - `None` ⇒ non esiste cammino nel grafo di chiamate NOTO. È un "non lo so"
    ///   onesto: gli archi non risolti sono marcati `Unresolved` (mai fabbricati),
    ///   perciò un `None` riflette il confine reale della conoscenza, non lo nasconde.
    ///
    /// Non filtra le entità esterne: se il grafo ha risolto una chiamata verso una
    /// dipendenza esterna, quel passo è reale e va mostrato — escluderlo
    /// nasconderebbe un pezzo vero del cammino.
    pub async fn call_path(
        &self,
        from: EntityId,
        to: EntityId,
    ) -> anyhow::Result<Option<CallPath>> {
        // Un'entità "raggiunge" se stessa con un cammino banale di un solo passo.
        if from == to {
            return Ok(self
                .storage
                .get_entity_by_id(&from)
                .await?
                .map(|e| CallPath { steps: vec![e] }));
        }

        // BFS diretta sugli archi `Calls` risolti, tracciando il padre di ogni
        // nodo per ricostruire il cammino una volta raggiunto `to`.
        let mut parent: HashMap<EntityId, EntityId> = HashMap::new();
        let mut visited: HashSet<EntityId> = HashSet::new();
        visited.insert(from);
        let mut queue: VecDeque<EntityId> = VecDeque::new();
        queue.push_back(from);

        let mut reached = false;
        while let Some(id) = queue.pop_front() {
            if id == to {
                reached = true;
                break;
            }
            let calls = self
                .storage
                .query_relations(RelationFilter {
                    source_id: Some(id),
                    kind: Some(RelationKind::Calls),
                    ..Default::default()
                })
                .await?;
            for rel in calls {
                // Difensivo: un arco `Calls` ha sempre un target risolto (gli ignoti
                // sono `Unresolved`), ma non ci fidiamo di un nil scritto per errore.
                if rel.target_id.is_nil() {
                    continue;
                }
                if visited.insert(rel.target_id) {
                    parent.insert(rel.target_id, id);
                    queue.push_back(rel.target_id);
                }
            }
        }

        if !reached {
            return Ok(None);
        }

        // Risali la catena dei padri da `to` a `from`, poi inverti.
        let mut ids = vec![to];
        let mut cur = to;
        while cur != from {
            let p = parent[&cur];
            ids.push(p);
            cur = p;
        }
        ids.reverse();

        // Materializza le entità nell'ordine del cammino. Se un nodo non è più nel
        // grafo, NON restituiamo un cammino monco e bugiardo: nessun cammino.
        let mut steps = Vec::with_capacity(ids.len());
        for id in ids {
            match self.storage.get_entity_by_id(&id).await? {
                Some(entity) => steps.push(entity),
                None => return Ok(None),
            }
        }
        Ok(Some(CallPath { steps }))
    }

    /// Risolve UN nome digitato dall'utente a un'unica entità del grafo, in modo
    /// **onesto**. Lo storage cerca per sottostringa (`LIKE %nome%`), perciò qui
    /// stringiamo ai soli match *precisi*: il nome qualificato è esattamente
    /// `name`, oppure termina con `::name` (l'utente ha dato solo il segmento
    /// finale, es. `charge` per `PaymentService::charge`).
    ///
    /// - esattamente 1 match preciso ⇒ [`NameResolution::Resolved`].
    /// - più match precisi ⇒ [`NameResolution::Ambiguous`] (li elenca tutti: non
    ///   scegliamo noi, sarebbe un'entità che l'utente non ha chiesto).
    /// - nessun match preciso ⇒ [`NameResolution::Unknown`], con gli eventuali
    ///   quasi-omonimi (i match per sola sottostringa) come suggerimento.
    async fn resolve_one_name(&self, name: &str) -> anyhow::Result<NameResolution> {
        let candidates = self.storage.find_entities_by_name_pattern(name).await?;
        let tail = format!("::{name}");
        let mut seen: HashSet<EntityId> = HashSet::new();
        let precise: Vec<Entity> = candidates
            .iter()
            .filter(|e| e.qualified_name == name || e.qualified_name.ends_with(&tail))
            .filter(|e| seen.insert(e.id))
            .cloned()
            .collect();

        match precise.len() {
            1 => Ok(NameResolution::Resolved(
                precise.into_iter().next().expect("len == 1"),
            )),
            0 => Ok(NameResolution::Unknown(candidates)),
            _ => Ok(NameResolution::Ambiguous(precise)),
        }
    }

    /// Livello L2 (per nome): risolve onestamente `from` e `to` a un'unica entità
    /// ciascuno, poi delega a [`QueryEngine::call_path`]. È il confine d'ingresso
    /// del livello L2 dal mondo esterno (CLI/gRPC), dove i nomi sono testo libero.
    ///
    /// Tesi anti-falso-positivo applicata all'ingresso: se un nome è ignoto o
    /// ambiguo lo *dichiariamo* (con i candidati), invece di scegliere a caso
    /// un'entità e restituire un cammino che nessuno ha chiesto. L'ambiguità ha
    /// precedenza sull'ignoto perché è più azionabile (l'utente disambigua).
    pub async fn call_path_by_name(&self, from: &str, to: &str) -> anyhow::Result<CallPathReply> {
        let rf = self.resolve_one_name(from).await?;
        let rt = self.resolve_one_name(to).await?;

        // 1) Ambiguità (prima: l'utente può risolverla scegliendo un candidato).
        let mut ambiguous: Vec<Entity> = Vec::new();
        let mut msg = String::new();
        if let NameResolution::Ambiguous(c) = &rf {
            msg.push_str(&format!(
                "Il nome di partenza \"{from}\" è ambiguo: {} entità corrispondono.\n",
                c.len()
            ));
            ambiguous.extend(c.iter().cloned());
        }
        if let NameResolution::Ambiguous(c) = &rt {
            msg.push_str(&format!(
                "Il nome di arrivo \"{to}\" è ambiguo: {} entità corrispondono.\n",
                c.len()
            ));
            ambiguous.extend(c.iter().cloned());
        }
        if !ambiguous.is_empty() {
            msg.push_str("Specifica quale (nome qualificato completo):\n");
            msg.push_str(&format_candidate_list(&ambiguous));
            return Ok(CallPathReply {
                formatted: msg,
                status: CallPathStatus::Ambiguous,
                steps: Vec::new(),
                candidates: ambiguous,
            });
        }

        // 2) Ignoto (nessun nome corrispondente). Offriamo i quasi-omonimi.
        let mut suggestions: Vec<Entity> = Vec::new();
        let mut unknown = false;
        let mut msg = String::new();
        if let NameResolution::Unknown(s) = &rf {
            unknown = true;
            msg.push_str(&format!("Nessuna entità di nome \"{from}\" nel grafo.\n"));
            suggestions.extend(s.iter().cloned());
        }
        if let NameResolution::Unknown(s) = &rt {
            unknown = true;
            msg.push_str(&format!("Nessuna entità di nome \"{to}\" nel grafo.\n"));
            suggestions.extend(s.iter().cloned());
        }
        if unknown {
            if suggestions.is_empty() {
                msg.push_str("(nessun nome simile trovato)\n");
            } else {
                msg.push_str("Forse intendevi:\n");
                msg.push_str(&format_candidate_list(&suggestions));
            }
            return Ok(CallPathReply {
                formatted: msg,
                status: CallPathStatus::Unknown,
                steps: Vec::new(),
                candidates: suggestions,
            });
        }

        // 3) Entrambi risolti a un'unica entità: cerca il cammino reale.
        let (NameResolution::Resolved(ef), NameResolution::Resolved(et)) = (rf, rt) else {
            // Ambiguità e ignoto sono già stati gestiti e hanno fatto `return`.
            unreachable!("rf e rt sono entrambi Resolved a questo punto");
        };
        let from_q = ef.qualified_name.clone();
        let to_q = et.qualified_name.clone();
        match self.call_path(ef.id, et.id).await? {
            Some(path) => Ok(CallPathReply {
                formatted: format_call_path_found(&from_q, &to_q, &path.steps),
                status: CallPathStatus::Found,
                steps: path.steps,
                candidates: Vec::new(),
            }),
            None => Ok(CallPathReply {
                formatted: format_no_path(&from_q, &to_q),
                status: CallPathStatus::NoPath,
                steps: Vec::new(),
                candidates: Vec::new(),
            }),
        }
    }
}

/// Un **cammino di chiamata** onesto: la sequenza ordinata di entità da una
/// sorgente a una destinazione dove OGNI passo consecutivo è un arco `Calls`
/// risolto del grafo. `steps.first()` è la sorgente, `steps.last()` la
/// destinazione. Restituito SOLO quando il cammino esiste davvero nel grafo di
/// chiamate noto: non contiene mai un passo indovinato — tesi anti-falso-positivo
/// applicata al livello L2 del context builder.
#[derive(Debug, Clone)]
pub struct CallPath {
    pub steps: Vec<Entity>,
}

/// Esito interno della risoluzione onesta di UN nome a un'entità del grafo
/// (vedi [`QueryEngine::resolve_one_name`]). Non collassa mai «ignoto» con
/// «ambiguo»: sono due verità diverse, e l'utente ha bisogno di saperle distinte.
enum NameResolution {
    /// Un solo match preciso: l'entità.
    Resolved(Entity),
    /// Più match precisi: i candidati tra cui disambiguare.
    Ambiguous(Vec<Entity>),
    /// Nessun match preciso: i quasi-omonimi per sottostringa (può essere vuoto).
    Unknown(Vec<Entity>),
}

/// Risultato dell'espansione BFS, prima della selezione finale.
struct Expansion {
    entities: HashMap<EntityId, Entity>,
    scores: HashMap<EntityId, u32>,
    relations: Vec<Relation>,
}

/// Un BUCO NOTO del contesto: un riferimento che il resolver non ha collegato a
/// un'entità del progetto. Il contesto lo NOMINA (target testuale originale +
/// tipo di riferimento) invece di nasconderlo — tesi anti-falso-positivo applicata
/// al contesto: un buco nominato è meglio di un numero che lo nasconde.
struct UnresolvedHole {
    source_id: EntityId,
    /// Il nome così come scritto nel sorgente (es. `schema.safeParse`), dai
    /// metadata `unresolved_target` dell'arco. Mai inventato.
    target: String,
    /// Il tipo di riferimento mancato (`Calls`, `Imports`, …), dai metadata
    /// `original_kind`. Può essere vuoto se assente.
    original_kind: String,
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
    holes: &[UnresolvedHole],
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
            // Le prove a sostegno del perché: l'LLM vede la citazione verificabile
            // (commit, arco, test), non solo l'affermazione. Mostrate solo quando
            // ci sono — una decisione scritta a mano può legittimamente non averne.
            if !decision.evidence.is_empty() {
                let proofs: Vec<String> = decision.evidence.iter().map(|e| e.to_string()).collect();
                out.push_str(&format!("  prove: {}\n", proofs.join("; ")));
            }
        }
    }

    // BUCHI NOTI — i riferimenti non risolti delle entità selezionate, NOMINATI.
    // Il vecchio formato ne dava solo il NUMERO ("N relazioni non risolte"): un
    // conteggio nasconde QUALE riferimento manca e fa sembrare l'entità più
    // connessa di quanto il grafo sappia davvero. Qui ognuno è mostrato col suo
    // nome originale (dal sorgente) e col tipo di riferimento mancato, ordinati
    // per stabilità. Tesi anti-falso-positivo al livello del contesto: meglio un
    // buco nominato che un numero che lo nasconde.
    if !holes.is_empty() {
        out.push_str(
            "\nBUCHI NOTI (riferimenti non risolti a un'entità del progetto — mostrati, non nascosti):\n",
        );
        let mut lines: Vec<String> = holes
            .iter()
            .map(|h| {
                let source = by_id
                    .get(&h.source_id)
                    .map(|e| e.qualified_name.as_str())
                    .unwrap_or("?");
                let kind = match h.original_kind.as_str() {
                    "" => String::new(),
                    k => format!(" ({})", k.to_lowercase()),
                };
                format!("- {source} → {}{kind}", h.target)
            })
            .collect();
        lines.sort();
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str(
            "  (non collegati a un'unica entità nota: da verificare, non assumere assenti)\n",
        );
    }

    out
}

/// Comprime il testo multilinea del razionale in una sola riga leggibile.
fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Rende il cammino di chiamata trovato: ogni passo su una riga, con la freccia
/// `→` tra l'uno e il successivo. Ogni freccia è un arco `Calls` risolto.
fn format_call_path_found(from_q: &str, to_q: &str, steps: &[Entity]) -> String {
    let mut out = format!("Cammino di chiamata da \"{from_q}\" a \"{to_q}\":\n\n");
    for (i, entity) in steps.iter().enumerate() {
        if i == 0 {
            out.push_str(&format!("  {}\n", entity.qualified_name));
        } else {
            out.push_str(&format!("    → {}\n", entity.qualified_name));
        }
    }
    out.push_str(&format!(
        "\n({} passi, ogni freccia è un arco `Calls` risolto del grafo.)\n",
        steps.len()
    ));
    out
}

/// Rende l'assenza di cammino in modo onesto: ricorda che la ricerca segue i soli
/// archi `Calls` risolti, quindi «nessun cammino noto» non è prova di «nessuna
/// chiamata» — un riferimento non risolto potrebbe nasconderne uno.
fn format_no_path(from_q: &str, to_q: &str) -> String {
    format!(
        "Nessun cammino di chiamata noto da \"{from_q}\" a \"{to_q}\".\n\n\
         (Cercato solo lungo archi `Calls` risolti. Un riferimento non risolto — un\n\
         BUCO NOTO — potrebbe nascondere un collegamento reale: assenza di cammino\n\
         noto non è prova di assenza di chiamata.)\n"
    )
}

/// Elenca delle entità candidate (per disambiguazione o suggerimento), una per
/// riga, ordinate e deduplicate per stabilità dell'output.
fn format_candidate_list(entities: &[Entity]) -> String {
    let mut lines: Vec<String> = entities
        .iter()
        .map(|e| {
            format!(
                "  - {} [{:?}] — {}",
                e.qualified_name, e.kind, e.location.file_path
            )
        })
        .collect();
    lines.sort();
    lines.dedup();
    let mut out = lines.join("\n");
    out.push('\n');
    out
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

    /// Recupera l'id dell'entità il cui qualified_name termina con `suffix`
    /// (es. `"::handler"`), per ancorare i test del cammino di chiamata.
    async fn id_of(storage: &Arc<SqliteStorage>, suffix: &str) -> EntityId {
        let pattern = suffix.trim_start_matches(':');
        storage
            .find_entities_by_name_pattern(pattern)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.qualified_name.ends_with(suffix))
            .unwrap_or_else(|| panic!("attesa un'entità che termina con {suffix}"))
            .id
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
                    supersedes: Vec::new(),
                    deprecates: Vec::new(),
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
            supersedes: Vec::new(),
            deprecates: Vec::new(),
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
    async fn context_shows_the_evidence_behind_the_why() {
        // Il contesto non porta solo l'affermazione, ma la PROVA: l'LLM vede la
        // citazione verificabile (qui un commit) accanto al razionale.
        use codeos_memory::{DecisionKind, Evidence};
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

        let decisions = Arc::new(InMemoryDecisionStore::new());
        let mut decision = Decision::from_new(
            NewDecision {
                author: "ai:ArchitectureGuardian".to_string(),
                title: "Login lato server".to_string(),
                context: String::new(),
                rationale: "Non esporre i segreti al client.".to_string(),
                related_entity_ids: vec![target],
                related_decision_ids: Vec::new(),
                supersedes: Vec::new(),
                deprecates: Vec::new(),
                tags: Vec::new(),
            },
            DecisionKind::ArchitectureRule,
        );
        decision.evidence = vec![Evidence::Commit("birth01".to_string())];
        decisions.record(&decision).await.unwrap();

        let engine = QueryEngine::with_decisions(storage, decisions);
        let ctx = engine
            .query(&nl("voglio sistemare il login"))
            .await
            .unwrap()
            .formatted_context;

        assert!(
            ctx.contains("prove: commit birth01"),
            "manca la prova accanto al perché:\n{ctx}"
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

    #[tokio::test]
    async fn context_names_the_unresolved_hole_not_just_counts_it() {
        // `parse_input` chiama `validate_external`, che NON esiste nel progetto: il
        // resolver lascia un arco Unresolved. Il contesto deve NOMINARE quel buco
        // (col nome del riferimento mancato), non darne solo il conteggio.
        let storage = graph_from(
            "ingest/parser.py",
            "def parse_input(data):\n    return validate_external(data)\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let ctx = engine
            .query(&nl("parse_input"))
            .await
            .unwrap()
            .formatted_context;

        assert!(
            ctx.contains("BUCHI NOTI"),
            "manca la sezione dei buchi noti:\n{ctx}"
        );
        assert!(
            ctx.contains("validate_external"),
            "il target non risolto dev'essere NOMINATO, non solo contato:\n{ctx}"
        );
    }

    #[tokio::test]
    async fn no_holes_section_when_every_reference_resolves() {
        // Qui ogni chiamata si risolve (`login` → `verify_password`, entrambi nel
        // progetto): non esistono buchi, quindi la sezione non deve comparire — non
        // si annuncia un'incertezza che non c'è.
        let storage = graph_from(
            "auth.py",
            "def verify_password():\n    pass\n\ndef login():\n    verify_password()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let ctx = engine.query(&nl("login")).await.unwrap().formatted_context;

        assert!(
            !ctx.contains("BUCHI NOTI"),
            "tutto è risolto: la sezione dei buchi non deve comparire:\n{ctx}"
        );
    }

    #[tokio::test]
    async fn call_path_finds_the_direct_call() {
        // `handler` chiama `service`: il cammino è esattamente handler → service.
        let storage = graph_from(
            "app.py",
            "def service():\n    pass\n\ndef handler():\n    service()\n",
        )
        .await;
        let handler = id_of(&storage, "::handler").await;
        let service = id_of(&storage, "::service").await;
        let engine = QueryEngine::new(storage);

        let path = engine
            .call_path(handler, service)
            .await
            .unwrap()
            .expect("atteso un cammino handler → service");
        let names: Vec<&str> = path
            .steps
            .iter()
            .map(|e| e.qualified_name.as_str())
            .collect();
        assert_eq!(path.steps.len(), 2, "cammino diretto = 2 passi: {names:?}");
        assert!(
            names[0].ends_with("::handler"),
            "il primo passo è la sorgente: {names:?}"
        );
        assert!(
            names[1].ends_with("::service"),
            "l'ultimo passo è la destinazione: {names:?}"
        );
    }

    #[tokio::test]
    async fn call_path_follows_a_transitive_chain() {
        // handler → service → repo: il cammino attraversa il passo intermedio.
        let storage = graph_from(
            "app.py",
            "def repo():\n    pass\n\ndef service():\n    repo()\n\ndef handler():\n    service()\n",
        )
        .await;
        let handler = id_of(&storage, "::handler").await;
        let repo = id_of(&storage, "::repo").await;
        let engine = QueryEngine::new(storage);

        let path = engine
            .call_path(handler, repo)
            .await
            .unwrap()
            .expect("atteso un cammino handler → service → repo");
        let names: Vec<String> = path
            .steps
            .iter()
            .map(|e| e.qualified_name.clone())
            .collect();
        assert_eq!(
            path.steps.len(),
            3,
            "cammino transitivo = 3 passi: {names:?}"
        );
        assert!(names[0].ends_with("::handler"), "sorgente: {names:?}");
        assert!(
            names[1].ends_with("::service"),
            "passo intermedio: {names:?}"
        );
        assert!(names[2].ends_with("::repo"), "destinazione: {names:?}");
    }

    #[tokio::test]
    async fn call_path_is_none_when_no_call_chain_exists() {
        // Due funzioni che non si chiamano: nessun cammino noto ⇒ None onesto, mai
        // un cammino inventato per "accontentare" la domanda.
        let storage = graph_from(
            "app.py",
            "def handler():\n    pass\n\ndef orphan():\n    pass\n",
        )
        .await;
        let handler = id_of(&storage, "::handler").await;
        let orphan = id_of(&storage, "::orphan").await;
        let engine = QueryEngine::new(storage);

        let path = engine.call_path(handler, orphan).await.unwrap();
        assert!(
            path.is_none(),
            "nessuna chiamata tra le due ⇒ nessun cammino: {:?}",
            path.map(|p| p
                .steps
                .iter()
                .map(|e| e.qualified_name.clone())
                .collect::<Vec<_>>())
        );
    }

    #[tokio::test]
    async fn call_path_does_not_bridge_an_unresolved_hop() {
        // Il cuore anti-falso-positivo del livello L2: `handler` FA una chiamata, ma
        // a `missing_external` (fuori dal progetto ⇒ arco `Unresolved`). Non esiste
        // alcun cammino RISOLTO handler → target, quindi il risultato è None: la
        // ricerca non scavalca MAI un buco per fabbricare un collegamento.
        let storage = graph_from(
            "app.py",
            "def target():\n    pass\n\ndef handler():\n    missing_external()\n",
        )
        .await;
        let handler = id_of(&storage, "::handler").await;
        let target = id_of(&storage, "::target").await;
        let engine = QueryEngine::new(storage);

        let path = engine.call_path(handler, target).await.unwrap();
        assert!(
            path.is_none(),
            "l'unica chiamata di handler è Unresolved: nessun cammino verso target, \
             non un collegamento inventato"
        );
    }

    #[tokio::test]
    async fn call_path_by_name_resolves_short_names_and_finds_the_chain() {
        // L'utente digita solo il segmento finale ("handler", "repo"): il confine
        // d'ingresso li risolve all'unica entità e trova il cammino transitivo.
        let storage = graph_from(
            "app.py",
            "def repo():\n    pass\n\ndef service():\n    repo()\n\ndef handler():\n    service()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let reply = engine.call_path_by_name("handler", "repo").await.unwrap();
        assert_eq!(reply.status, CallPathStatus::Found);
        let names: Vec<String> = reply
            .steps
            .iter()
            .map(|e| e.qualified_name.clone())
            .collect();
        assert_eq!(reply.steps.len(), 3, "cammino transitivo: {names:?}");
        assert!(names[0].ends_with("::handler"), "sorgente: {names:?}");
        assert!(names[2].ends_with("::repo"), "destinazione: {names:?}");
        assert!(
            reply.formatted.contains("Cammino di chiamata"),
            "atteso il rendering del cammino, trovato:\n{}",
            reply.formatted
        );
    }

    #[tokio::test]
    async fn call_path_by_name_reports_no_path_without_inventing_one() {
        // Entrambi i nomi risolvono, ma non si chiamano: stato NoPath onesto, mai
        // un cammino fabbricato per accontentare la domanda.
        let storage = graph_from(
            "app.py",
            "def handler():\n    pass\n\ndef orphan():\n    pass\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let reply = engine.call_path_by_name("handler", "orphan").await.unwrap();
        assert_eq!(reply.status, CallPathStatus::NoPath);
        assert!(reply.steps.is_empty(), "NoPath non porta passi");
        assert!(
            reply.formatted.contains("Nessun cammino"),
            "atteso il messaggio onesto di assenza, trovato:\n{}",
            reply.formatted
        );
    }

    #[tokio::test]
    async fn call_path_by_name_declares_unknown_instead_of_guessing() {
        // Un nome che non corrisponde ad alcuna entità ⇒ Unknown esplicito, nessun
        // passo: non inventiamo un'entità per poterci appendere un cammino.
        let storage = graph_from("app.py", "def handler():\n    pass\n").await;
        let engine = QueryEngine::new(storage);

        let reply = engine
            .call_path_by_name("handler", "does_not_exist")
            .await
            .unwrap();
        assert_eq!(reply.status, CallPathStatus::Unknown);
        assert!(reply.steps.is_empty());
        assert!(
            reply
                .formatted
                .contains("Nessuna entità di nome \"does_not_exist\""),
            "atteso il nome ignoto, trovato:\n{}",
            reply.formatted
        );
    }

    #[tokio::test]
    async fn call_path_by_name_declares_ambiguous_instead_of_picking_one() {
        // "charge" corrisponde a due metodi (A::charge, B::charge): NON ne scegliamo
        // uno a caso — sarebbe un cammino che l'utente non ha chiesto. Stato
        // Ambiguous, con entrambi i candidati elencati per disambiguare.
        let storage = graph_from(
            "app.py",
            "class A:\n    def charge(self):\n        pass\n\n\
             class B:\n    def charge(self):\n        pass\n\n\
             def handler():\n    pass\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let reply = engine.call_path_by_name("charge", "handler").await.unwrap();
        assert_eq!(reply.status, CallPathStatus::Ambiguous);
        assert!(
            reply.steps.is_empty(),
            "un nome ambiguo non produce cammino"
        );
        assert_eq!(
            reply.candidates.len(),
            2,
            "attesi i due metodi charge come candidati: {:?}",
            reply
                .candidates
                .iter()
                .map(|e| e.qualified_name.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            reply.formatted.contains("ambiguo"),
            "atteso il messaggio di ambiguità, trovato:\n{}",
            reply.formatted
        );
    }
}
