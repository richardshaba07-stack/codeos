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
use codeos_types::bus::{
    CallPathReply, CallPathStatus, ImpactReply, ImpactStatus, ImpactTransitiveReply,
    PossibleCallerInfo, QueryRequest, QueryResponse, TransitiveCallerInfo,
};
use codeos_types::{Entity, EntityId, EntityKind, Relation, RelationKind};

/// Profondità massima della BFS (Passo 4).
const DEFAULT_MAX_DEPTH: u32 = 3;
/// Numero massimo di entità nel sottografo, per non sforare la context window.
const DEFAULT_MAX_ENTITIES: usize = 50;
/// Quante entità chiave (le più rilevanti) analizzare per il raggio d'impatto
/// reverse (chi le chiama). Tenuto piccolo: l'impatto è informazione di contorno
/// sulle entità centrali, non su tutto il sottografo — e ogni analisi costa una
/// scansione degli archi non risolti.
const IMPACT_FOCUS_ENTITIES: usize = 3;
/// Tetto ai chiamanti POSSIBILI mostrati per entità nel contesto. I possibili
/// sono solo corrispondenze di nome (vedi `impact`): un nome molto comune
/// (`new`, `get`) ne genererebbe a decine. Li tronciamo con un conteggio onesto
/// del residuo, invece di inondare la context window o di nasconderli del tutto.
const MAX_POSSIBLE_PER_ENTITY: usize = 5;
/// Tetto ai chiamanti CONFERMATI (diretti + transitivi) mostrati per entità nel
/// contesto. Ora che includono i chiamanti a più hop, possono essere molti su
/// un'entità molto usata; li tronciamo coi PIÙ VICINI in testa (sono ordinati per
/// distanza) e un conteggio onesto del residuo — mai inondare, mai nascondere.
const MAX_CONFIRMED_CALLERS: usize = 8;
/// Quante entità chiave collegare all'entità centrale coi PERCORSI DI CHIAMATA
/// (catene `Calls` in avanti). Come [`IMPACT_FOCUS_ENTITIES`], tenuto piccolo:
/// ogni target costa una BFS sugli archi del grafo, e i percorsi sono contorno
/// sulle entità centrali, non una mappa di tutto il sottografo.
const PATH_FOCUS_TARGETS: usize = 3;
/// Profondità massima della BFS a ritroso dell'impatto TRANSITIVO (chi raggiunge X
/// seguendo archi `Calls` risolti). Oltre poche hop l'"impatto" è troppo diffuso
/// per essere azionabile, e su grafi grandi la BFS sarebbe costosa; quando il tetto
/// tronca un chiamante reale più lontano, [`TransitiveImpact::depth_capped`] lo
/// dichiara invece di fingere completezza — astensione onesta, non silenzio.
const MAX_IMPACT_DEPTH: u32 = 5;

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

        // Passo 5.5 — Il RAGGIO D'IMPATTO reverse: chi DIPENDE dalle entità chiave.
        // L'espansione BFS segue solo archi USCENTI (cosa USANO i semi), quindi il
        // contesto è strutturalmente "in avanti": non mostra MAI chi CHIAMA le
        // entità centrali — la metà mancante del "cosa cambia se tocco X?". Qui la
        // aggiungiamo, ma solo per le entità più rilevanti e solo coi chiamanti
        // FUORI dal set già selezionato (quelli dentro sono già in DIPENDENZE
        // CHIAVE: ripeterli sarebbe rumore, non informazione).
        let impacts = self.collect_impact(&selected, &selected_ids).await?;

        // Passo 5.6 — I PERCORSI DI CHIAMATA in avanti: come l'entità centrale
        // RAGGIUNGE le altre entità chiave. È la metà gemella dell'IMPATTO (che è
        // reverse): DIPENDENZE CHIAVE elenca archi singoli e di tipo misto, ma non
        // mostra MAI la *catena* di chiamate `Calls` che collega due entità chiave
        // attraverso passi intermedi. Questa sezione la rende esplicita, ereditando
        // l'onestà di `call_path` (mai scavalca un `Unresolved`) e mostrando solo i
        // cammini multi-hop — un cammino diretto è già una riga di DIPENDENZE CHIAVE.
        let call_paths = self.collect_call_paths(&selected).await?;

        // Passo 6 — Formattazione del contesto.
        let formatted_context = format_context(
            text,
            &selected,
            &relations,
            &decisions,
            &holes,
            &impacts,
            &call_paths,
        );

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

    /// Raccoglie il RAGGIO D'IMPATTO reverse per le prime [`IMPACT_FOCUS_ENTITIES`]
    /// entità (le più rilevanti, già ordinate per punteggio): chi le CHIAMA — sia
    /// DIRETTAMENTE sia a TRANSITIVE distanze — fuori dal sottografo già selezionato.
    /// È la metà che la BFS in avanti non vede, e ora a TUTTA la sua profondità: se
    /// `A → B → X`, toccare X impatta anche A, e il contesto deve dirlo.
    ///
    /// Tesi anti-falso-positivo, qui in tre modi: (1) escludiamo i chiamanti già
    /// selezionati — sono visibili in DIPENDENZE CHIAVE / FILE RILEVANTI, ripeterli
    /// gonfierebbe il contesto; (2) i CONFERMATI (da [`QueryEngine::impact_transitive`],
    /// ogni hop un arco `Calls` risolto) restano distinti dai POSSIBILI (riferimenti
    /// non risolti omonimi, da [`QueryEngine::impact`], solo 1 hop — i match-di-nome non
    /// si compongono transitivamente senza mentire), e ogni confermato porta la sua
    /// distanza in hop; (3) tronciamo confermati e possibili a tetti onesti registrando
    /// quanti ne restano, e propaghiamo `depth_capped` quando il raggio è stato tagliato
    /// dalla profondità massima — mai inondare la context window, mai nascondere.
    async fn collect_impact(
        &self,
        selected: &[Entity],
        selected_ids: &HashSet<EntityId>,
    ) -> anyhow::Result<Vec<ImpactSummary>> {
        let mut summaries = Vec::new();
        for entity in selected.iter().take(IMPACT_FOCUS_ENTITIES) {
            // CONFERMATI — chiamanti diretti E transitivi col loro hop, fuori dal set
            // già selezionato. Già ordinati per (distanza, nome) dalla capability:
            // il filtro preserva l'ordine, quindi il troncamento tiene i PIÙ VICINI.
            let (mut confirmed, depth_capped) = match self.impact_transitive(entity.id).await? {
                Some(ti) => (
                    ti.callers
                        .into_iter()
                        .filter(|c| !selected_ids.contains(&c.entity.id))
                        .collect::<Vec<TransitiveCaller>>(),
                    ti.depth_capped,
                ),
                None => (Vec::new(), false),
            };
            let confirmed_truncated = confirmed.len().saturating_sub(MAX_CONFIRMED_CALLERS);
            confirmed.truncate(MAX_CONFIRMED_CALLERS);

            // POSSIBILI — solo i riferimenti non risolti omonimi (1 hop, da `impact`),
            // fuori dal set. La transitività NON li tocca: un possibile-di-un-possibile
            // sarebbe congettura al quadrato.
            let mut possible: Vec<PossibleCaller> = match self.impact(entity.id).await? {
                Some(im) => im
                    .possible_callers
                    .into_iter()
                    .filter(|p| !selected_ids.contains(&p.source.id))
                    .collect(),
                None => Vec::new(),
            };
            possible.sort_by(|a, b| a.source.qualified_name.cmp(&b.source.qualified_name));
            let possible_truncated = possible.len().saturating_sub(MAX_POSSIBLE_PER_ENTITY);
            possible.truncate(MAX_POSSIBLE_PER_ENTITY);

            // Niente da aggiungere su quest'entità ⇒ niente voce (nessuna sezione
            // vuota: non si annuncia un impatto che il contesto già mostra).
            if confirmed.is_empty() && possible.is_empty() {
                continue;
            }
            summaries.push(ImpactSummary {
                entity: entity.clone(),
                confirmed,
                confirmed_truncated,
                depth_capped,
                possible,
                possible_truncated,
            });
        }
        Ok(summaries)
    }

    /// Raccoglie i PERCORSI DI CHIAMATA in avanti dall'entità CENTRALE (la prima,
    /// massimo punteggio di rilevanza) verso le altre prime [`PATH_FOCUS_TARGETS`]
    /// entità chiave: la catena di archi `Calls` risolti che le collega. È la metà
    /// gemella dell'impatto reverse — qui in avanti (chi raggiunge chi).
    ///
    /// Tesi anti-falso-positivo, in tre modi: (1) eredita da [`QueryEngine::call_path`]
    /// la garanzia che ogni passo è una chiamata reale già risolta — nessun cammino
    /// indovinato, e `None` onesto quando non esiste invece di un ponte fabbricato;
    /// (2) tiene SOLO i cammini multi-hop (≥ 3 passi, cioè con almeno un nodo
    /// intermedio): un cammino diretto `centro → target` è già una riga di DIPENDENZE
    /// CHIAVE, ri-mostrarlo qui sarebbe un'eco, non informazione; (3) niente cammino,
    /// niente voce — la sezione non annuncia un percorso che non c'è.
    ///
    /// Il valore aggiunto rispetto a DIPENDENZE CHIAVE: quella elenca archi singoli e
    /// di tipo misto, questa rende esplicita la *raggiungibilità transitiva* come
    /// catena ordinata di sole chiamate, e può far emergere un nodo intermedio che il
    /// cap di selezione aveva tagliato.
    async fn collect_call_paths(&self, selected: &[Entity]) -> anyhow::Result<Vec<CallPath>> {
        let mut paths = Vec::new();
        // L'entità CENTRALE dev'essere un callable: un Module o una Class non hanno
        // archi `Calls` uscenti e non potrebbero mai essere sorgente di un cammino.
        // E nel sottografo il Module tende a ordinarsi PER PRIMO (gli arrivano i
        // `BelongsTo` di tutti i membri, quindi punteggio alto, e il suo qualname
        // — `pipeline` — precede quello dei membri — `pipeline::ingest`): senza
        // questo filtro l'entità centrale sarebbe spesso un modulo da cui nessun
        // cammino parte. Ancoriamo alla prima Function/Method per rilevanza.
        let Some(focus) = selected.iter().find(|e| is_callable(e.kind)) else {
            return Ok(paths);
        };
        for target in selected
            .iter()
            .filter(|e| is_callable(e.kind) && e.id != focus.id)
            .take(PATH_FOCUS_TARGETS)
        {
            let Some(path) = self.call_path(focus.id, target.id).await? else {
                continue;
            };
            // Solo multi-hop: un cammino di 2 passi (centro → target diretto) è già
            // visibile in DIPENDENZE CHIAVE come singolo arco — qui sarebbe un'eco.
            if path.steps.len() < 3 {
                continue;
            }
            paths.push(path);
        }
        Ok(paths)
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

    /// Livello L2 — **impatto**: *chi chiama X?* Risponde a "cosa cambia se tocco X?"
    /// elencando i chiamanti dell'entità, tenuti in due verità distinte e mai mescolate:
    ///
    /// - **confermati**: esiste un arco `Calls` RISOLTO che punta a X. Il grafo ha
    ///   già legato la chiamata — toccare X impatta questi chiamanti con certezza.
    /// - **possibili**: un arco `Unresolved` il cui nome testuale combacia, sull'
    ///   ultimo segmento, col nome semplice di X. Il resolver NON l'ha legato (tipo
    ///   del receiver ignoto, omonimia…), ma il nome corrisponde: POTREBBE chiamare
    ///   X. Non lo nascondiamo (sottostimerebbe il raggio d'impatto) né lo spacciamo
    ///   per certo (sarebbe un arco che mente): lo mostriamo come *possibile*, col
    ///   riferimento grezzo, da verificare.
    ///
    /// È la tesi anti-falso-positivo applicata all'analisi d'impatto: il confine tra
    /// «so che dipende» e «potrebbe dipendere» resta esplicito, invece di collassare
    /// in un sì/no che mentirebbe in una delle due direzioni.
    ///
    /// `None` SOLO se `entity_id` non è nel grafo (non si calcola l'impatto di
    /// un'entità inventata). Un'entità che nessuno chiama dà `Some` con entrambe le
    /// liste vuote: «niente la chiama» è una risposta onesta, non un'assenza di dato.
    pub async fn impact(&self, entity_id: EntityId) -> anyhow::Result<Option<Impact>> {
        let Some(entity) = self.storage.get_entity_by_id(&entity_id).await? else {
            return Ok(None);
        };

        // Chiamanti CONFERMATI — archi `Calls` risolti che puntano a X. De-duplica
        // per sorgente: più siti di chiamata nella stessa funzione = un chiamante.
        let calls_in = self
            .storage
            .query_relations(RelationFilter {
                target_id: Some(entity_id),
                kind: Some(RelationKind::Calls),
                ..Default::default()
            })
            .await?;
        let mut confirmed_ids: HashSet<EntityId> = HashSet::new();
        let mut confirmed_callers: Vec<Entity> = Vec::new();
        for rel in calls_in {
            if rel.source_id.is_nil() || !confirmed_ids.insert(rel.source_id) {
                continue;
            }
            if let Some(source) = self.storage.get_entity_by_id(&rel.source_id).await? {
                confirmed_callers.push(source);
            }
        }

        // Chiamanti POSSIBILI — archi `Unresolved` il cui ultimo segmento combacia
        // col nome semplice di X. Escludiamo le sorgenti già confermate (sappiamo
        // già che chiamano X) e de-duplichiamo per sorgente. Saltiamo i target senza
        // nome, e l'intera fase se X non ha un nome semplice (non si confronta col
        // vuoto: matcherebbe qualunque buco anonimo, un falso positivo di massa).
        let simple = last_segment(&entity.qualified_name);
        let mut possible_callers: Vec<PossibleCaller> = Vec::new();
        if !simple.is_empty() {
            let unresolved = self
                .storage
                .query_relations(RelationFilter {
                    kind: Some(RelationKind::Unresolved),
                    ..Default::default()
                })
                .await?;
            let mut possible_ids: HashSet<EntityId> = HashSet::new();
            for rel in unresolved {
                let Some(reference) = rel.metadata.get("unresolved_target") else {
                    continue;
                };
                if last_segment(reference) != simple {
                    continue;
                }
                if rel.source_id.is_nil()
                    || confirmed_ids.contains(&rel.source_id)
                    || !possible_ids.insert(rel.source_id)
                {
                    continue;
                }
                if let Some(source) = self.storage.get_entity_by_id(&rel.source_id).await? {
                    possible_callers.push(PossibleCaller {
                        source,
                        reference: reference.clone(),
                    });
                }
            }
        }

        Ok(Some(Impact {
            entity,
            confirmed_callers,
            possible_callers,
        }))
    }

    /// Livello L2 — **impatto TRANSITIVO**: chi raggiunge X a ritroso seguendo SOLO
    /// archi `Calls` RISOLTI, a QUALUNQUE distanza (non solo i chiamanti diretti di
    /// [`QueryEngine::impact`]). È la risposta completa a "cosa cambia se tocco X?":
    /// se `A` chiama `B` che chiama `X`, toccare `X` può rompere anche `A` — il raggio
    /// d'impatto vero è transitivo, e fermarsi a 1 hop lo sottostima.
    ///
    /// BFS a ritroso dal target verso i chiamanti, hop dopo hop. Ogni chiamante porta
    /// la sua distanza MINIMA in hop ([`TransitiveCaller::hops`]): `1` = chiama X
    /// direttamente, `2` = chiama qualcosa che chiama X, e così via.
    ///
    /// Tesi anti-falso-positivo, in tre punti: (1) si compone **solo su archi
    /// risolti** — un chiamante raggiunto attraverso un arco `Unresolved` NON è un
    /// chiamante transitivo (sarebbe un ponte su un buco, esattamente ciò che
    /// [`QueryEngine::call_path`] rifiuta); i "possibili" (match-di-nome) restano fuori
    /// di proposito: non si compongono transitivamente senza moltiplicare l'incertezza
    /// e mentire. (2) La profondità è limitata a [`MAX_IMPACT_DEPTH`], e se il tetto
    /// taglia un chiamante reale più lontano lo si DICHIARA (`depth_capped`), non lo si
    /// nasconde. (3) `None` SOLO se X non è nel grafo; un'entità che nessuno chiama dà
    /// `Some` con `callers` vuoto — «niente la chiama» è una verità, non un'assenza.
    pub async fn impact_transitive(
        &self,
        entity_id: EntityId,
    ) -> anyhow::Result<Option<TransitiveImpact>> {
        let Some(entity) = self.storage.get_entity_by_id(&entity_id).await? else {
            return Ok(None);
        };

        // `distance[id]` = lunghezza minima della catena di chiamate da `id` fino a X
        // (0 = X stesso). Visited-per-id sulla stessa mappa: ogni chiamante è contato
        // una volta sola, alla sua distanza minima, e i cicli (ricorsione) non ciclano.
        let mut distance: HashMap<EntityId, u32> = HashMap::new();
        distance.insert(entity_id, 0);
        let mut queue: VecDeque<(EntityId, u32)> = VecDeque::new();
        queue.push_back((entity_id, 0));

        let mut depth_capped = false;
        while let Some((id, dist)) = queue.pop_front() {
            let calls_in = self
                .storage
                .query_relations(RelationFilter {
                    target_id: Some(id),
                    kind: Some(RelationKind::Calls),
                    ..Default::default()
                })
                .await?;
            for rel in calls_in {
                // Difensivo: un arco `Calls` ha sempre sorgente risolta; un nil scritto
                // per errore non diventa un chiamante fantasma.
                if rel.source_id.is_nil() || distance.contains_key(&rel.source_id) {
                    continue;
                }
                if dist + 1 > MAX_IMPACT_DEPTH {
                    // Un chiamante reale oltre il tetto: esiste ma non lo includiamo.
                    // Lo dichiariamo, invece di affermarlo o ignorarlo in silenzio.
                    depth_capped = true;
                    continue;
                }
                distance.insert(rel.source_id, dist + 1);
                queue.push_back((rel.source_id, dist + 1));
            }
        }

        // Materializza i chiamanti (tutti tranne X stesso, a distanza 0), ordinati per
        // distanza crescente poi per nome: output stabile e deterministico.
        let mut callers: Vec<TransitiveCaller> = Vec::new();
        for (id, hops) in &distance {
            if *hops == 0 {
                continue;
            }
            if let Some(caller) = self.storage.get_entity_by_id(id).await? {
                callers.push(TransitiveCaller {
                    entity: caller,
                    hops: *hops,
                });
            }
        }
        callers.sort_by(|a, b| {
            a.hops
                .cmp(&b.hops)
                .then_with(|| a.entity.qualified_name.cmp(&b.entity.qualified_name))
        });

        Ok(Some(TransitiveImpact {
            entity,
            callers,
            depth_capped,
        }))
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

    /// Livello L2 (per nome): risolve onestamente `name` a un'unica entità, poi
    /// delega a [`QueryEngine::impact`]. È il confine d'ingresso dell'analisi
    /// d'impatto dal mondo esterno (CLI/gRPC), dove il nome è testo libero.
    ///
    /// Tesi anti-falso-positivo applicata all'ingresso: se il nome è ignoto o
    /// ambiguo lo *dichiariamo* (con i candidati), invece di scegliere a caso
    /// un'entità e misurare un impatto che nessuno ha chiesto.
    pub async fn impact_by_name(&self, name: &str) -> anyhow::Result<ImpactReply> {
        match self.resolve_one_name(name).await? {
            NameResolution::Ambiguous(candidates) => {
                let mut msg = format!(
                    "Il nome \"{name}\" è ambiguo: {} entità corrispondono.\n",
                    candidates.len()
                );
                msg.push_str("Specifica quale (nome qualificato completo):\n");
                msg.push_str(&format_candidate_list(&candidates));
                Ok(ImpactReply {
                    formatted: msg,
                    status: ImpactStatus::Ambiguous,
                    confirmed: Vec::new(),
                    possible: Vec::new(),
                    candidates,
                })
            }
            NameResolution::Unknown(suggestions) => {
                let mut msg = format!("Nessuna entità di nome \"{name}\" nel grafo.\n");
                if suggestions.is_empty() {
                    msg.push_str("(nessun nome simile trovato)\n");
                } else {
                    msg.push_str("Forse intendevi:\n");
                    msg.push_str(&format_candidate_list(&suggestions));
                }
                Ok(ImpactReply {
                    formatted: msg,
                    status: ImpactStatus::Unknown,
                    confirmed: Vec::new(),
                    possible: Vec::new(),
                    candidates: suggestions,
                })
            }
            NameResolution::Resolved(entity) => {
                // L'entità è appena stata risolta dal grafo, perciò `impact` non
                // può restituire None qui. In via difensiva trattiamo l'eventuale
                // None come impatto vuoto sull'entità nota: mai un panico sul
                // percorso utente, mai un chiamante inventato.
                let impact = self.impact(entity.id).await?.unwrap_or_else(|| Impact {
                    entity: entity.clone(),
                    confirmed_callers: Vec::new(),
                    possible_callers: Vec::new(),
                });
                let formatted = format_impact(&impact);
                let possible = impact
                    .possible_callers
                    .into_iter()
                    .map(|p| PossibleCallerInfo {
                        source: p.source,
                        reference: p.reference,
                    })
                    .collect();
                Ok(ImpactReply {
                    formatted,
                    status: ImpactStatus::Found,
                    confirmed: impact.confirmed_callers,
                    possible,
                    candidates: Vec::new(),
                })
            }
        }
    }

    /// Risolve `name` a un'unica entità (stessa onestà di [`QueryEngine::impact_by_name`]:
    /// `Ambiguous`/`Unknown` espliciti coi candidati, mai un'entità indovinata) e ne
    /// calcola l'impatto **TRANSITIVO**: i chiamanti a qualunque distanza, ciascuno
    /// con i suoi hop, più il flag `depth_capped` quando il tetto tronca il raggio.
    pub async fn impact_transitive_by_name(
        &self,
        name: &str,
    ) -> anyhow::Result<ImpactTransitiveReply> {
        match self.resolve_one_name(name).await? {
            NameResolution::Ambiguous(candidates) => {
                let mut msg = format!(
                    "Il nome \"{name}\" è ambiguo: {} entità corrispondono.\n",
                    candidates.len()
                );
                msg.push_str("Specifica quale (nome qualificato completo):\n");
                msg.push_str(&format_candidate_list(&candidates));
                Ok(ImpactTransitiveReply {
                    formatted: msg,
                    status: ImpactStatus::Ambiguous,
                    callers: Vec::new(),
                    depth_capped: false,
                    candidates,
                })
            }
            NameResolution::Unknown(suggestions) => {
                let mut msg = format!("Nessuna entità di nome \"{name}\" nel grafo.\n");
                if suggestions.is_empty() {
                    msg.push_str("(nessun nome simile trovato)\n");
                } else {
                    msg.push_str("Forse intendevi:\n");
                    msg.push_str(&format_candidate_list(&suggestions));
                }
                Ok(ImpactTransitiveReply {
                    formatted: msg,
                    status: ImpactStatus::Unknown,
                    callers: Vec::new(),
                    depth_capped: false,
                    candidates: suggestions,
                })
            }
            NameResolution::Resolved(entity) => {
                // L'entità è appena stata risolta dal grafo: `impact_transitive` non
                // può restituire None qui. In via difensiva trattiamo l'eventuale None
                // come raggio vuoto sull'entità nota — mai un panico sul percorso utente.
                let impact =
                    self.impact_transitive(entity.id)
                        .await?
                        .unwrap_or_else(|| TransitiveImpact {
                            entity: entity.clone(),
                            callers: Vec::new(),
                            depth_capped: false,
                        });
                let formatted = format_transitive_impact(&impact);
                let depth_capped = impact.depth_capped;
                let callers = impact
                    .callers
                    .into_iter()
                    .map(|c| TransitiveCallerInfo {
                        source: c.entity,
                        hops: c.hops,
                    })
                    .collect();
                Ok(ImpactTransitiveReply {
                    formatted,
                    status: ImpactStatus::Found,
                    callers,
                    depth_capped,
                    candidates: Vec::new(),
                })
            }
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

/// L'**impatto** di un'entità: chi la chiama, secondo il grafo. Tiene separate due
/// verità che non vanno mai confuse — i chiamanti CONFERMATI (archi `Calls` risolti
/// verso l'entità) dai chiamanti POSSIBILI (riferimenti `Unresolved` il cui nome
/// combacia, non confermati). Restituito da [`QueryEngine::impact`]: è la risposta
/// onesta a "cosa cambia se tocco X?", dove il confine tra certo e possibile resta
/// visibile invece di collassare in un numero o in un sì/no che mente.
#[derive(Debug, Clone)]
pub struct Impact {
    /// L'entità di cui si misura l'impatto.
    pub entity: Entity,
    /// Chiamanti certi: per ciascuno esiste un arco `Calls` risolto verso l'entità.
    pub confirmed_callers: Vec<Entity>,
    /// Chiamanti possibili: un riferimento `Unresolved` ne combacia il nome, ma il
    /// grafo non l'ha legato. Da verificare, non da assumere né presenti né assenti.
    pub possible_callers: Vec<PossibleCaller>,
}

/// Un chiamante **possibile**: una sorgente con un riferimento `Unresolved` il cui
/// nome combacia (sull'ultimo segmento) col nome semplice dell'entità d'impatto. Il
/// `reference` è il nome grezzo così com'era nel sorgente (es. `schema.validate`),
/// mai inventato: dice all'utente PERCHÉ la sorgente è sospettata, senza spacciare
/// la corrispondenza di nome per una chiamata certa.
#[derive(Debug, Clone)]
pub struct PossibleCaller {
    /// L'entità sorgente che POTREBBE chiamare l'entità d'impatto.
    pub source: Entity,
    /// Il riferimento testuale non risolto che combacia, dal sorgente.
    pub reference: String,
}

/// L'impatto **TRANSITIVO** di un'entità: chi la raggiunge a ritroso seguendo SOLO
/// archi `Calls` risolti, a qualunque distanza — non solo i chiamanti diretti di
/// [`Impact`]. Ogni chiamante porta la sua distanza in hop; tutti gli archi del
/// cammino sono risolti ⇒ è una dipendenza CERTA, solo più o meno lontana (mai un
/// "possibile": i match-di-nome non si compongono transitivamente senza mentire).
/// `depth_capped` è la nota d'onestà: `true` se la BFS si è fermata al tetto
/// [`MAX_IMPACT_DEPTH`] mentre oltre esisteva ancora un chiamante — dichiarato
/// invece di fingere completezza. Restituito da [`QueryEngine::impact_transitive`].
#[derive(Debug, Clone)]
pub struct TransitiveImpact {
    /// L'entità di cui si misura l'impatto transitivo.
    pub entity: Entity,
    /// I chiamanti transitivi confermati, ordinati per distanza crescente poi nome.
    pub callers: Vec<TransitiveCaller>,
    /// `true` se un chiamante reale oltre [`MAX_IMPACT_DEPTH`] è stato tagliato dal
    /// tetto: il raggio mostrato è parziale, e lo si dice.
    pub depth_capped: bool,
}

/// Un chiamante transitivo confermato, con la distanza MINIMA (in hop di chiamata)
/// che lo separa dall'entità d'impatto: `hops == 1` la chiama direttamente, `2`
/// chiama qualcosa che la chiama, e così via. Ogni hop è un arco `Calls` risolto.
#[derive(Debug, Clone)]
pub struct TransitiveCaller {
    /// L'entità che (in)direttamente chiama l'entità d'impatto.
    pub entity: Entity,
    /// Distanza minima in hop di chiamata fino all'entità d'impatto (≥ 1).
    pub hops: u32,
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

/// Il RAGGIO D'IMPATTO reverse di UNA entità chiave: chi la chiama — direttamente e
/// a distanze TRANSITIVE — fuori dal sottografo già selezionato. È ciò che la BFS in
/// avanti (callee → callee) non può strutturalmente mostrare. Mantiene la separazione
/// fra `confirmed` (chiamanti certi, ognuno con la sua distanza in hop: ogni passo è
/// un arco `Calls` risolto) e `possible` (riferimenti `Unresolved` omonimi, non
/// confermati, solo 1 hop): il confine fra certo e sospetto resta visibile nel
/// contesto. `confirmed_truncated`/`possible_truncated` contano gli scartati dai
/// tetti e `depth_capped` segnala se il raggio è stato tagliato dalla profondità
/// massima — il troncamento è onesto invece che silenzioso.
struct ImpactSummary {
    entity: Entity,
    confirmed: Vec<TransitiveCaller>,
    confirmed_truncated: usize,
    depth_capped: bool,
    possible: Vec<PossibleCaller>,
    possible_truncated: usize,
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

/// `true` per le entità che possono stare sui due capi di un arco `Calls`
/// (funzioni e metodi). I PERCORSI DI CHIAMATA si ancorano solo a queste: un
/// Module o una Class non chiamano né sono chiamati, quindi non possono essere
/// né l'origine né la destinazione di un cammino di sole chiamate.
fn is_callable(kind: EntityKind) -> bool {
    matches!(kind, EntityKind::Function | EntityKind::Method)
}

/// L'ultimo segmento di un nome di riferimento, separato da `.` o `:` (così copre
/// sia la sintassi a punti di JS/TS/Python — `schema.validate` → `validate` — sia i
/// path Rust `::` — `Repo::open` → `open`). È il "nome semplice" su cui l'analisi
/// d'impatto confronta un riferimento non risolto col nome di un'entità: il match è
/// sul leaf, non sull'intera espressione, così non ci si fa ingannare dal receiver o
/// dal percorso. Salta i segmenti vuoti (es. la coppia `::`) e, su un nome senza
/// separatori o solo separatori, ripiega sul nome stesso.
fn last_segment(name: &str) -> &str {
    name.rsplit(['.', ':'])
        .find(|seg| !seg.is_empty())
        .unwrap_or(name)
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
    impacts: &[ImpactSummary],
    call_paths: &[CallPath],
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

    // Passo 5.5 — IMPATTO reverse. DIPENDENZE CHIAVE qui sopra mostra da COSA
    // dipendono le entità (archi in avanti, callee); questa sezione mostra il
    // rovescio — CHI le chiama, DIRETTAMENTE e a distanze TRANSITIVE — che la BFS in
    // avanti non può vedere. Compare solo quando c'è davvero un chiamante esterno al
    // sottografo: niente sezione vuota. Tesi anti-falso-positivo resa visibile: i
    // chiamanti CONFERMATI (`-`, ogni hop un arco `Calls` risolto; gli indiretti con
    // la loro distanza «a N hop») restano distinti dai POSSIBILI (`?`, sola
    // corrispondenza di nome su un riferimento non risolto), e i troncamenti — sui
    // confermati lontani, sui possibili, e il tetto di profondità — sono contati, mai
    // silenziosi.
    if !impacts.is_empty() {
        out.push_str(
            "\nIMPATTO (chi CHIAMA le entità chiave — il rovescio delle DIPENDENZE CHIAVE):\n",
        );
        let mut any_indirect = false;
        for summary in impacts {
            out.push_str(&format!(
                "- {} ← chiamato da:\n",
                summary.entity.qualified_name
            ));
            for caller in &summary.confirmed {
                if caller.hops <= 1 {
                    out.push_str(&format!("    - {}\n", caller.entity.qualified_name));
                } else {
                    any_indirect = true;
                    out.push_str(&format!(
                        "    - {} (a {} hop)\n",
                        caller.entity.qualified_name, caller.hops
                    ));
                }
            }
            if summary.confirmed_truncated > 0 {
                out.push_str(&format!(
                    "    - (+{} altri chiamanti più lontani)\n",
                    summary.confirmed_truncated
                ));
            }
            if summary.depth_capped {
                out.push_str(&format!(
                    "    - (… e oltre {MAX_IMPACT_DEPTH} hop il raggio continua, non elencato)\n"
                ));
            }
            for caller in &summary.possible {
                out.push_str(&format!(
                    "    ? {} (via `{}`)\n",
                    caller.source.qualified_name, caller.reference
                ));
            }
            if summary.possible_truncated > 0 {
                out.push_str(&format!(
                    "    ? (+{} altri possibili, da verificare)\n",
                    summary.possible_truncated
                ));
            }
        }
        out.push_str("  («-» chiamante confermato da arco `Calls`;");
        if any_indirect {
            out.push_str(" «(a N hop)» = confermato ma indiretto (N chiamate di distanza);");
        }
        out.push_str(
            " «?» solo corrispondenza di nome su\n   un riferimento non risolto, da verificare — non è prova di chiamata.)\n",
        );
    }

    // Passo 5.6 — PERCORSI DI CHIAMATA in avanti. La metà gemella dell'IMPATTO:
    // quello mostra chi CHIAMA le entità chiave (reverse), questo mostra come
    // l'entità centrale le RAGGIUNGE (forward), come catena ordinata di sole
    // chiamate `Calls`. Diverso da DIPENDENZE CHIAVE, che elenca archi singoli e di
    // tipo misto senza mai connetterli in un percorso. Compare solo coi cammini
    // multi-hop (un cammino diretto è già un arco lì sopra: qui sarebbe un'eco) e
    // mai con un passo indovinato — `call_path` non scavalca un `Unresolved`.
    if !call_paths.is_empty() {
        out.push_str(
            "\nPERCORSI DI CHIAMATA (come l'entità centrale raggiunge le altre entità chiave):\n",
        );
        for path in call_paths {
            let chain: Vec<&str> = path
                .steps
                .iter()
                .map(|e| e.qualified_name.as_str())
                .collect();
            out.push_str(&format!("- {}\n", chain.join(" → ")));
        }
        out.push_str(
            "  («→» catena di archi `Calls` risolti, passo per passo; nessun passo è indovinato —\n   se il grafo non conosce un cammino, la riga non compare.)\n",
        );
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

/// Rende l'impatto di un'entità in due sezioni *distinte*: i chiamanti CERTI e i
/// POSSIBILI. È la tesi anti-falso-positivo resa visibile all'occhio: i certi
/// (arco `Calls` risolto) col prefisso `-`, i possibili (sola corrispondenza di
/// nome su un riferimento non risolto) col prefisso `?` e il riferimento grezzo
/// dal sorgente — il PERCHÉ del sospetto. La chiusa ricorda che «possibile» non è
/// «certo» e che l'assenza di chiamanti noti non è prova di assenza d'uso.
fn format_impact(impact: &Impact) -> String {
    let mut out = format!("Impatto di \"{}\":\n\n", impact.entity.qualified_name);

    out.push_str(&format!(
        "Chiamanti CONFERMATI ({}) — arco `Calls` risolto verso l'entità:\n",
        impact.confirmed_callers.len()
    ));
    if impact.confirmed_callers.is_empty() {
        out.push_str("  (nessuno noto)\n");
    } else {
        let mut lines: Vec<String> = impact
            .confirmed_callers
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
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }

    out.push('\n');
    out.push_str(&format!(
        "Chiamanti POSSIBILI ({}) — un riferimento non risolto ne combacia il nome, da VERIFICARE:\n",
        impact.possible_callers.len()
    ));
    if impact.possible_callers.is_empty() {
        out.push_str("  (nessuno noto)\n");
    } else {
        let mut lines: Vec<String> = impact
            .possible_callers
            .iter()
            .map(|p| {
                format!(
                    "  ? {} — via `{}` — {}",
                    p.source.qualified_name, p.reference, p.source.location.file_path
                )
            })
            .collect();
        lines.sort();
        lines.dedup();
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }

    out.push_str(
        "\n(Confermati: arco `Calls` realmente presente nel grafo. Possibili: solo una\n\
         corrispondenza di nome su un riferimento non risolto — potrebbe NON essere\n\
         questa entità. Assenza di chiamanti noti non è prova di assenza d'uso.)\n",
    );
    out
}

/// Rende l'impatto TRANSITIVO per il terminale/LLM: i chiamanti in ordine di
/// distanza crescente (già ordinati dalla capability), ognuno con i suoi hop, e —
/// se il tetto ha troncato il raggio — una nota d'onestà che è parziale. Tutti i
/// chiamanti sono CONFERMATI (ogni hop è un arco risolto): il testo lo dichiara, per
/// non confonderli col confine certo/possibile dell'impatto diretto.
fn format_transitive_impact(impact: &TransitiveImpact) -> String {
    let mut out = format!(
        "Impatto TRANSITIVO di \"{}\" (chi la raggiunge a ritroso, a qualunque distanza):\n\n",
        impact.entity.qualified_name
    );
    out.push_str(&format!(
        "Chiamanti CONFERMATI ({}) — ogni hop è un arco `Calls` risolto:\n",
        impact.callers.len()
    ));
    if impact.callers.is_empty() {
        out.push_str("  (nessuno noto)\n");
    } else {
        for c in &impact.callers {
            out.push_str(&format!(
                "  - {} [{:?}] — a {} hop — {}\n",
                c.entity.qualified_name, c.entity.kind, c.hops, c.entity.location.file_path
            ));
        }
    }
    if impact.depth_capped {
        out.push_str(&format!(
            "\n  … oltre i {MAX_IMPACT_DEPTH} hop il raggio continua: esiste almeno un chiamante\n   \
             più lontano, non elencato (tetto di profondità). Non lo nascondiamo.\n"
        ));
    }
    out.push_str(
        "\n(Tutti CONFERMATI: ogni passo del cammino a ritroso è un arco `Calls` realmente\n\
         presente nel grafo; la distanza in hop dice quanto è lontano il chiamante.\n\
         Assenza di chiamanti noti non è prova di assenza d'uso.)\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_graph::GraphResolver;
    use codeos_parser::{LanguageParser, PythonParser, TypeScriptParser};
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

    /// Variante TypeScript: serve dove conta il guard "Fix #10" (un membro su
    /// receiver foreign deve restare `Unresolved`, non legarsi a una funzione libera
    /// omonima). Passa una project-root così i nomi qualificati sono `src::…::…`.
    async fn graph_from_ts(path: &str, src: &str) -> Arc<SqliteStorage> {
        let parsed = TypeScriptParser::new()
            .parse_file(Path::new(path), src)
            .await;
        let storage = SqliteStorage::in_memory().unwrap();
        let delta = GraphResolver::new(Some("/repo".to_string()))
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
    async fn context_surfaces_who_calls_the_key_entity_under_impatto() {
        // Il senso dell'integrazione di `impact` in `query()`. `target` è chiamato da
        // `caller_one` e `caller_two`, i cui nomi NON contengono la keyword "target":
        // non sono semi, e la BFS in avanti (callee → callee) non li può raggiungere.
        // L'unico modo in cui compaiono nel contesto è la sezione IMPATTO reverse,
        // dove devono figurare come chiamanti CONFERMATI.
        let storage = graph_from(
            "app.py",
            "def target():\n    pass\n\n\
             def caller_one():\n    target()\n\n\
             def caller_two():\n    target()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let ctx = engine.query(&nl("target")).await.unwrap().formatted_context;

        assert!(ctx.contains("IMPATTO"), "manca la sezione IMPATTO:\n{ctx}");
        assert!(
            ctx.contains("← chiamato da"),
            "la sezione IMPATTO deve elencare i chiamanti dell'entità chiave:\n{ctx}"
        );
        // I chiamanti non sono semi né selezionati: compaiono SOLO grazie a IMPATTO.
        let impatto = ctx
            .split("IMPATTO")
            .nth(1)
            .expect("la sezione IMPATTO è appena stata verificata presente");
        assert!(
            impatto.contains("caller_one"),
            "caller_one (chiamante confermato) dev'essere nominato sotto IMPATTO:\n{ctx}"
        );
        assert!(
            impatto.contains("caller_two"),
            "caller_two (chiamante confermato) dev'essere nominato sotto IMPATTO:\n{ctx}"
        );
    }

    #[tokio::test]
    async fn no_impatto_section_when_the_only_caller_is_already_in_context() {
        // `login` → `verify_password`: la BFS tira dentro verify_password. Chi chiama
        // login? Nessuno. Chi chiama verify_password? Solo login — che è GIÀ
        // selezionato e visibile in DIPENDENZE CHIAVE. Ripeterlo sotto IMPATTO sarebbe
        // un'eco, non informazione nuova ⇒ la sezione non deve comparire. È la prova
        // che il filtro sui chiamanti già selezionati rende IMPATTO additivo, non
        // ridondante.
        let storage = graph_from(
            "auth.py",
            "def verify_password():\n    pass\n\ndef login():\n    verify_password()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let ctx = engine.query(&nl("login")).await.unwrap().formatted_context;

        assert!(
            !ctx.contains("IMPATTO"),
            "l'unico chiamante (login) è già nel contesto: niente sezione IMPATTO d'eco:\n{ctx}"
        );
    }

    #[tokio::test]
    async fn impatto_surfaces_transitive_callers_with_their_distance() {
        // L'integrazione del raggio TRANSITIVO in `query()`. `target` (l'unico seme)
        // non chiama nulla, quindi la BFS in avanti seleziona solo lui: `direct` e
        // `indirect` sono chiamanti A MONTE, NON selezionati. `direct` lo chiama a 1
        // hop, `indirect` lo raggiunge a 2 (indirect → direct → target). Senza il
        // transitivo, `indirect` non comparirebbe MAI nel contesto: è proprio la
        // sezione IMPATTO, ora a tutta profondità, a renderlo visibile con la sua
        // distanza.
        let storage = graph_from(
            "app.py",
            "def target():\n    pass\n\n\
             def direct():\n    target()\n\n\
             def indirect():\n    direct()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let ctx = engine.query(&nl("target")).await.unwrap().formatted_context;

        assert!(ctx.contains("IMPATTO"), "manca la sezione IMPATTO:\n{ctx}");
        let impatto = ctx
            .split("IMPATTO")
            .nth(1)
            .expect("la sezione IMPATTO è appena stata verificata presente");
        assert!(
            impatto.contains("::indirect"),
            "il chiamante transitivo `indirect` deve comparire sotto IMPATTO:\n{ctx}"
        );
        assert!(
            impatto.contains("(a 2 hop)"),
            "il chiamante transitivo deve portare la sua distanza in hop:\n{ctx}"
        );
    }

    #[tokio::test]
    async fn impatto_does_not_annotate_distance_when_callers_are_only_direct() {
        // `target` è chiamato SOLO direttamente (da `caller`, a 1 hop): non esiste
        // alcun chiamante transitivo. La sezione deve mostrare il chiamante diretto
        // senza alcuna annotazione di distanza — niente `(a N hop)` inventato, e la
        // legenda non spiega un marcatore indiretto che non compare. È la prova che il
        // livello transitivo è additivo e onesto: distanza solo dove c'è davvero.
        let storage = graph_from(
            "app.py",
            "def target():\n    pass\n\ndef caller():\n    target()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let ctx = engine.query(&nl("target")).await.unwrap().formatted_context;

        assert!(ctx.contains("IMPATTO"), "manca la sezione IMPATTO:\n{ctx}");
        let impatto = ctx
            .split("IMPATTO")
            .nth(1)
            .expect("la sezione IMPATTO è appena stata verificata presente");
        assert!(
            impatto.contains("::caller"),
            "il chiamante diretto dev'essere elencato:\n{ctx}"
        );
        assert!(
            !impatto.contains("(a "),
            "nessun chiamante transitivo ⇒ nessuna annotazione di distanza:\n{ctx}"
        );
    }

    #[tokio::test]
    async fn context_surfaces_the_multi_hop_call_route_under_percorsi() {
        // Il senso dell'integrazione di `call_path` in `query()`. `ingest` (l'unico
        // seme, quindi l'entità centrale) raggiunge `store` SOLO passando per
        // `transform`. DIPENDENZE CHIAVE elenca i due archi `ingest CALLS transform`
        // e `transform CALLS store` separati; la sezione PERCORSI li connette nella
        // catena transitiva `ingest → transform → store`, che è l'informazione L2 che
        // la lista piatta di archi non rende esplicita.
        let storage = graph_from(
            "pipeline.py",
            "def store():\n    pass\n\n\
             def transform():\n    store()\n\n\
             def ingest():\n    transform()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let ctx = engine.query(&nl("ingest")).await.unwrap().formatted_context;

        assert!(
            ctx.contains("PERCORSI DI CHIAMATA"),
            "manca la sezione PERCORSI DI CHIAMATA:\n{ctx}"
        );
        let percorsi = ctx
            .split("PERCORSI DI CHIAMATA")
            .nth(1)
            .expect("la sezione PERCORSI è appena stata verificata presente");
        // La catena deve nominare tutti e tre i passi, intermedio compreso: è proprio
        // il nodo intermedio a rendere il percorso più ricco di una singola riga di
        // dipendenza.
        assert!(
            percorsi.contains("ingest")
                && percorsi.contains("transform")
                && percorsi.contains("store"),
            "il percorso deve mostrare la catena ingest → transform → store:\n{ctx}"
        );
        assert!(
            percorsi.contains('→'),
            "i passi del percorso vanno connessi con la freccia di catena:\n{ctx}"
        );
    }

    #[tokio::test]
    async fn no_percorsi_section_for_a_direct_call_already_in_dipendenze() {
        // `ingest` → `store` è una chiamata DIRETTA: appare già come singolo arco in
        // DIPENDENZE CHIAVE. Un cammino di 2 passi non aggiunge nulla, quindi la
        // sezione PERCORSI (riservata ai cammini multi-hop) non deve comparire — la
        // prova che il filtro multi-hop rende PERCORSI additivo, non un'eco.
        let storage = graph_from(
            "pipeline.py",
            "def store():\n    pass\n\ndef ingest():\n    store()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let ctx = engine.query(&nl("ingest")).await.unwrap().formatted_context;

        assert!(
            !ctx.contains("PERCORSI DI CHIAMATA"),
            "una chiamata diretta è già in DIPENDENZE CHIAVE: niente sezione PERCORSI d'eco:\n{ctx}"
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

    #[tokio::test]
    async fn impact_lists_every_confirmed_caller() {
        // Due funzioni chiamano `target`: entrambe sono chiamanti CONFERMATI (archi
        // `Calls` risolti). Tutto si risolve ⇒ nessun chiamante possibile.
        let storage = graph_from(
            "app.py",
            "def target():\n    pass\n\n\
             def caller_one():\n    target()\n\n\
             def caller_two():\n    target()\n",
        )
        .await;
        let target = id_of(&storage, "::target").await;
        let engine = QueryEngine::new(storage);

        let impact = engine
            .impact(target)
            .await
            .unwrap()
            .expect("atteso un impatto per un'entità esistente");
        let confirmed: Vec<&str> = impact
            .confirmed_callers
            .iter()
            .map(|e| e.qualified_name.as_str())
            .collect();
        assert_eq!(
            impact.confirmed_callers.len(),
            2,
            "attesi due chiamanti confermati: {confirmed:?}"
        );
        assert!(
            confirmed.iter().any(|n| n.ends_with("::caller_one")),
            "{confirmed:?}"
        );
        assert!(
            confirmed.iter().any(|n| n.ends_with("::caller_two")),
            "{confirmed:?}"
        );
        assert!(
            impact.possible_callers.is_empty(),
            "tutto è risolto: nessun chiamante possibile"
        );
    }

    #[tokio::test]
    async fn impact_separates_possible_callers_from_confirmed_ones() {
        // Il cuore anti-falso-positivo del livello L2 dal lato dei chiamanti, sul
        // caso reale di Fix #10. `validate` è una funzione libera; `callBare` la
        // chiama NUDA (`validate(1)`) ⇒ arco `Calls` risolto ⇒ chiamante CONFERMATO.
        // `parseInput` fa `schema.validate(data)`: receiver foreign di tipo ignoto, il
        // resolver NON lega (un arco verso la funzione libera omonima mentirebbe),
        // resta `Unresolved`. impact() lo deve mostrare come chiamante POSSIBILE —
        // non confermato, ma nemmeno nascosto — col riferimento grezzo dal sorgente.
        let storage = graph_from_ts(
            "/repo/src/v.ts",
            "export function validate(x: unknown): unknown {\n  return x;\n}\n\
             export function parseInput(schema: any, data: unknown): unknown {\n  return schema.validate(data);\n}\n\
             export function callBare(): unknown {\n  return validate(1);\n}\n",
        )
        .await;
        let validate = id_of(&storage, "::validate").await;
        let engine = QueryEngine::new(storage);

        let impact = engine
            .impact(validate)
            .await
            .unwrap()
            .expect("atteso un impatto");

        // Confermato: SOLO la call nuda di callBare.
        let confirmed: Vec<&str> = impact
            .confirmed_callers
            .iter()
            .map(|e| e.qualified_name.as_str())
            .collect();
        assert_eq!(
            impact.confirmed_callers.len(),
            1,
            "atteso un solo chiamante confermato (callBare): {confirmed:?}"
        );
        assert!(
            confirmed[0].ends_with("::callBare"),
            "il chiamante confermato è callBare: {confirmed:?}"
        );

        // Possibile: parseInput, via il membro foreign `schema.validate` non risolto.
        let possible: Vec<String> = impact
            .possible_callers
            .iter()
            .map(|p| p.source.qualified_name.clone())
            .collect();
        assert_eq!(
            impact.possible_callers.len(),
            1,
            "atteso un solo chiamante possibile (parseInput): {possible:?}"
        );
        let pc = &impact.possible_callers[0];
        assert!(
            pc.source.qualified_name.ends_with("::parseInput"),
            "il chiamante possibile è parseInput: {}",
            pc.source.qualified_name
        );
        assert_eq!(
            last_segment(&pc.reference),
            "validate",
            "il riferimento possibile combacia sul leaf `validate`: {}",
            pc.reference
        );

        // La prova anti-FP: parseInput NON è tra i confermati — Fix #10 ha evitato la
        // bugia, e impact() non la reintroduce promuovendo un possibile a certo.
        assert!(
            !impact
                .confirmed_callers
                .iter()
                .any(|e| e.qualified_name.ends_with("::parseInput")),
            "parseInput non deve comparire tra i confermati: la sua call è un membro foreign non risolto"
        );
    }

    #[tokio::test]
    async fn impact_is_some_with_no_callers_for_an_uncalled_entity() {
        // Una funzione che nessuno chiama: l'impatto esiste (l'entità c'è) ed è
        // onestamente VUOTO. Non `None` (l'entità esiste) e non chiamanti inventati.
        let storage = graph_from("app.py", "def lonely():\n    pass\n").await;
        let lonely = id_of(&storage, "::lonely").await;
        let engine = QueryEngine::new(storage);

        let impact = engine
            .impact(lonely)
            .await
            .unwrap()
            .expect("un'entità esistente ma non chiamata ha comunque un impatto (vuoto)");
        assert!(
            impact.confirmed_callers.is_empty(),
            "niente la chiama: nessun confermato"
        );
        assert!(
            impact.possible_callers.is_empty(),
            "niente la chiama: nessun possibile"
        );
        assert!(
            impact.entity.qualified_name.ends_with("::lonely"),
            "l'impatto riguarda l'entità giusta: {}",
            impact.entity.qualified_name
        );
    }

    #[tokio::test]
    async fn impact_is_none_for_an_unknown_entity() {
        // Un id che non è nel grafo ⇒ None onesto: non si calcola l'impatto di
        // un'entità inventata, né si restituisce un Impact vuoto su un fantasma.
        let storage = graph_from("app.py", "def something():\n    pass\n").await;
        let engine = QueryEngine::new(storage);

        let impact = engine.impact(EntityId::new()).await.unwrap();
        assert!(
            impact.is_none(),
            "un'entità sconosciuta non ha impatto calcolabile: None, non un Impact vuoto"
        );
    }

    #[tokio::test]
    async fn impact_transitive_lists_callers_with_their_hop_distance() {
        // Catena `deep` → `mid` → `leaf`. Chi impatta `leaf`? `mid` lo chiama
        // DIRETTAMENTE (1 hop), `deep` lo raggiunge ATTRAVERSO mid (2 hop). Entrambi
        // sono chiamanti CERTI — ogni arco della catena è un `Calls` risolto — solo a
        // distanza diversa, ed è proprio la distanza la cosa nuova rispetto a impact().
        let storage = graph_from(
            "app.py",
            "def leaf():\n    pass\n\n\
             def mid():\n    leaf()\n\n\
             def deep():\n    mid()\n",
        )
        .await;
        let leaf = id_of(&storage, "::leaf").await;
        let engine = QueryEngine::new(storage);

        let impact = engine
            .impact_transitive(leaf)
            .await
            .unwrap()
            .expect("leaf è nel grafo");

        let mid = impact
            .callers
            .iter()
            .find(|c| c.entity.qualified_name.ends_with("::mid"))
            .expect("mid chiama leaf direttamente");
        assert_eq!(mid.hops, 1, "mid chiama leaf direttamente: 1 hop");

        let deep = impact
            .callers
            .iter()
            .find(|c| c.entity.qualified_name.ends_with("::deep"))
            .expect("deep raggiunge leaf via mid");
        assert_eq!(deep.hops, 2, "deep raggiunge leaf in 2 hop (via mid)");

        assert!(
            !impact.depth_capped,
            "la catena è corta: il tetto di profondità non è stato toccato"
        );
    }

    #[tokio::test]
    async fn impact_transitive_does_not_chain_through_an_unresolved_hop() {
        // Il cuore anti-falso-positivo del transitivo. `mid` chiama `leaf` con una
        // chiamata NUDA ⇒ arco `Calls` risolto. `outer` chiama `schema.mid(...)` su un
        // receiver di tipo ignoto ⇒ il resolver lo lascia `Unresolved` (Fix #10): NON
        // è un arco `Calls` verso `mid`. Quindi `outer` NON raggiunge `leaf` per archi
        // risolti: non è un chiamante transitivo, e la BFS a ritroso non scavalca il
        // buco per inventarlo. `mid` invece c'è, a 1 hop.
        let storage = graph_from_ts(
            "app.ts",
            "export function leaf() {}\n\
             export function mid() { leaf(); }\n\
             export function outer(schema: any) { schema.mid(); }\n",
        )
        .await;
        let leaf = id_of(&storage, "::leaf").await;
        let engine = QueryEngine::new(storage);

        let impact = engine
            .impact_transitive(leaf)
            .await
            .unwrap()
            .expect("leaf è nel grafo");
        let names: Vec<&str> = impact
            .callers
            .iter()
            .map(|c| c.entity.qualified_name.as_str())
            .collect();

        assert!(
            names.iter().any(|n| n.ends_with("::mid")),
            "mid chiama leaf con un arco risolto: dev'essere un chiamante: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("::outer")),
            "outer raggiunge mid solo via un arco Unresolved: NON è un chiamante transitivo (niente ponti sui buchi): {names:?}"
        );
    }

    #[tokio::test]
    async fn impact_transitive_declares_when_the_depth_cap_truncates_the_radius() {
        // Catena più lunga del tetto (MAX_IMPACT_DEPTH = 5): f6 → f5 → … → f1 → f0.
        // Chi impatta f0? f1..f5 entro il tetto (hop 1..5); f6 sarebbe a 6 hop, OLTRE
        // il tetto. Non lo affermiamo come chiamante (sarebbe oltre ciò che abbiamo
        // davvero percorso) ma NON lo nascondiamo: `depth_capped` lo dichiara. È
        // l'astensione onesta applicata al raggio d'impatto — un confine esplicito,
        // non un troncamento silenzioso.
        let storage = graph_from(
            "app.py",
            "def f0():\n    pass\n\n\
             def f1():\n    f0()\n\n\
             def f2():\n    f1()\n\n\
             def f3():\n    f2()\n\n\
             def f4():\n    f3()\n\n\
             def f5():\n    f4()\n\n\
             def f6():\n    f5()\n",
        )
        .await;
        let f0 = id_of(&storage, "::f0").await;
        let engine = QueryEngine::new(storage);

        let impact = engine
            .impact_transitive(f0)
            .await
            .unwrap()
            .expect("f0 è nel grafo");
        let names: Vec<&str> = impact
            .callers
            .iter()
            .map(|c| c.entity.qualified_name.as_str())
            .collect();

        assert!(
            impact.depth_capped,
            "un chiamante reale (f6) è oltre il tetto: depth_capped deve dirlo: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("::f5")),
            "f5 è al tetto (5 hop): dev'esserci: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("::f6")),
            "f6 è oltre il tetto (6 hop): NON va affermato come chiamante: {names:?}"
        );
    }

    #[tokio::test]
    async fn impact_transitive_by_name_resolves_the_name_and_reports_distances() {
        // Il wrapper by-name (usato dal wire): risolve il nome a un'unica entità e
        // riporta il raggio transitivo coi chiamanti e la loro distanza, più un testo
        // formattato che li dichiara CONFERMATI e ne nomina gli hop. deep→mid→leaf.
        let storage = graph_from(
            "app.py",
            "def leaf():\n    pass\n\n\
             def mid():\n    leaf()\n\n\
             def deep():\n    mid()\n",
        )
        .await;
        let engine = QueryEngine::new(storage);

        let reply = engine.impact_transitive_by_name("leaf").await.unwrap();

        assert_eq!(reply.status, ImpactStatus::Found);
        let mid = reply
            .callers
            .iter()
            .find(|c| c.source.qualified_name.ends_with("::mid"))
            .expect("mid è un chiamante diretto");
        assert_eq!(mid.hops, 1, "mid chiama leaf direttamente");
        let deep = reply
            .callers
            .iter()
            .find(|c| c.source.qualified_name.ends_with("::deep"))
            .expect("deep è un chiamante a 2 hop");
        assert_eq!(deep.hops, 2, "deep raggiunge leaf via mid");
        assert!(!reply.depth_capped);
        assert!(
            reply.formatted.contains("TRANSITIVO") && reply.formatted.contains("hop"),
            "il testo deve dichiarare il raggio transitivo e le distanze:\n{}",
            reply.formatted
        );
    }
}
