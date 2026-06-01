//! [`Guardian`]: il lato storage del sistema immunitario.
//!
//! Legge il grafo persistito tramite il trait [`GraphStorage`], costruisce la
//! mappa entità → layer e delega a [`crate::invariant`] sia la scoperta delle
//! regole sia il rilevamento delle violazioni.
//!
//! DECISION: per ottenere la mappa entità → layer **non** estendiamo il trait
//! `GraphStorage` con un `get_all_entities()`. Ci servono solo le entità che
//! partecipano a una relazione (le uniche che contano per il layering): le
//! ricaviamo dagli estremi delle relazioni e le carichiamo con
//! `get_entity_by_id`. Così la feature resta confinata in questo crate e il
//! trait di persistenza non cambia.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use codeos_memory::{Decision, DecisionKind, DecisionStore};
use codeos_paleo::{excavate, occasions, Abstention, CommitHistory, DecisionFossil, Z_95};
use codeos_storage::{GraphStorage, RelationFilter};
use codeos_types::bus::{ArchitectureViolation, NewDecision};
use codeos_types::{EntityId, EntityKind, Relation};

use crate::invariant::{
    boundary_entities, layer_of, mine_layering_rules, violations_for, LayerConfig, LayerKey,
    LayeringRule,
};
use crate::meta::{mine_missing_invariants, MetaConfig, MissingInvariant};

/// Prefisso del tag che identifica in modo stabile l'invariante (a prescindere
/// dall'`id`, rigenerato a ogni mining): è la chiave del ciclo di vita di una
/// regola in memoria. Forma: `layering-invariant:{upstream}|{downstream}`.
const RULE_TAG_PREFIX: &str = "layering-invariant:";
/// Tag di stato di una promozione: la regola è in vigore.
const STATUS_ACTIVE: &str = "status:active";
/// Tag di stato di un ritiro: la regola non è più sostenuta dal grafo.
const STATUS_RETIRED: &str = "status:retired";

/// Una voce del "registro" degli invarianti attualmente in vigore, ricostruito
/// ripercorrendo la storia additiva delle decisioni.
struct ActiveInvariant {
    /// La `Decision` che ha promosso l'invariante (la referenzieremo nel ritiro).
    promotion_id: EntityId,
    /// Le entità di confine già agganciate alla promozione: le riportiamo sul
    /// ritiro, così la stessa query che mostrava la regola ne mostra anche la fine.
    related_entity_ids: Vec<EntityId>,
}

/// Il custode dell'architettura, agganciato a uno storage del grafo.
pub struct Guardian {
    storage: Arc<dyn GraphStorage>,
    /// Memoria storica opzionale: se presente, gli invarianti scoperti vengono
    /// **promossi** a `Decision` (kind `ArchitectureRule`) interrogabili dal Query.
    decisions: Option<Arc<dyn DecisionStore>>,
    /// Storia git opzionale: se presente, abilita la confidenza **calibrata sul
    /// tempo** (il Campo di Astensione) in [`mine_rules_calibrated`](Guardian::mine_rules_calibrated).
    history: Option<Arc<dyn CommitHistory>>,
    config: LayerConfig,
    /// Configurazione dello *spazio negativo del secondo ordine*
    /// ([`missing_invariants`](Guardian::missing_invariants)).
    meta_config: MetaConfig,
    /// Regole di layering **dichiarate a mano** dall'umano (`.codeos/config.yaml`).
    /// Si fondono con quelle scoperte: valgono per decreto (confidenza 1.0) anche
    /// dove il grafo non ha evidenza strutturale. Vuoto = nessuna config.
    declared: Vec<LayeringRule>,
}

impl Guardian {
    /// Crea un Guardian con la configurazione di layering di default.
    pub fn new(storage: Arc<dyn GraphStorage>) -> Self {
        Self {
            storage,
            decisions: None,
            history: None,
            config: LayerConfig::default(),
            meta_config: MetaConfig::default(),
            declared: Vec::new(),
        }
    }

    /// Crea un Guardian con una configurazione esplicita.
    pub fn with_config(storage: Arc<dyn GraphStorage>, config: LayerConfig) -> Self {
        Self {
            storage,
            decisions: None,
            history: None,
            config,
            meta_config: MetaConfig::default(),
            declared: Vec::new(),
        }
    }

    /// Aggancia il Memory Engine: gli invarianti scoperti da [`learn`](Self::learn)
    /// diventano memoria storica persistente.
    pub fn with_memory(storage: Arc<dyn GraphStorage>, decisions: Arc<dyn DecisionStore>) -> Self {
        Self {
            storage,
            decisions: Some(decisions),
            history: None,
            config: LayerConfig::default(),
            meta_config: MetaConfig::default(),
            declared: Vec::new(),
        }
    }

    /// Aggancia le regole di layering **dichiarate** dall'umano (tipicamente caricate
    /// da `.codeos/config.yaml` via [`crate::load_declared_rules`]). Chainable su
    /// qualsiasi costruttore. Le regole dichiarate si fondono con quelle scoperte e
    /// vengono fatte rispettare anche senza supporto strutturale nel grafo.
    pub fn with_declared_rules(mut self, declared: Vec<LayeringRule>) -> Self {
        self.declared = declared;
        self
    }

    /// Fonde le regole **dichiarate** con quelle passate (scoperte), senza duplicare
    /// una coppia `(upstream, downstream)` già presente: se l'umano dichiara un
    /// confine che il grafo ha già dissotterrato, vince la versione scoperta (porta
    /// supporto ed evidenza). Le dichiarate aggiungono solo i confini *mancanti*.
    fn merge_declared(&self, mut rules: Vec<LayeringRule>) -> Vec<LayeringRule> {
        if self.declared.is_empty() {
            return rules;
        }
        let existing: HashSet<(LayerKey, LayerKey)> = rules
            .iter()
            .map(|r| (r.upstream.clone(), r.downstream.clone()))
            .collect();
        for rule in &self.declared {
            if !existing.contains(&(rule.upstream.clone(), rule.downstream.clone())) {
                rules.push(rule.clone());
            }
        }
        rules
    }

    /// Aggancia una sorgente di storia git (il Paleontologo). Abilita la
    /// confidenza calibrata sul tempo in
    /// [`mine_rules_calibrated`](Self::mine_rules_calibrated). Chainable su
    /// qualsiasi costruttore: `Guardian::new(storage).with_commit_history(git)`.
    pub fn with_commit_history(mut self, history: Arc<dyn CommitHistory>) -> Self {
        self.history = Some(history);
        self
    }

    /// `true` se una storia git è agganciata: la confidenza degli invarianti è
    /// allora ricalibrata dal Campo di Astensione, non solo strutturale. Permette a
    /// chi costruisce il referto di etichettare ogni confidenza come *calibrata*.
    pub fn has_history(&self) -> bool {
        self.history.is_some()
    }

    /// La **qualità del grafo** sottostante (roadmap P2-7): i contatori che dicono
    /// quanto fidarsi del referto (relazioni risolte vs irrisolte vs a bassa
    /// confidenza, entità esterne). Pura lettura: delega allo storage.
    pub async fn graph_quality(&self) -> anyhow::Result<codeos_types::bus::GraphQualityInfo> {
        self.storage.graph_quality().await
    }

    /// Scopre le regole di layering dall'**intero grafo persistito**.
    pub async fn mine_rules(&self) -> anyhow::Result<Vec<LayeringRule>> {
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let layer_map = self.build_layer_map(&relations).await?;
        let mined = mine_layering_rules(&relations, &layer_map, &self.config);
        Ok(self.merge_declared(mined))
    }

    /// Come [`mine_rules`](Self::mine_rules), ma **calibra** la confidenza di ogni
    /// regola sul *negativo del tempo* — il **Campo di Astensione**.
    ///
    /// La confidenza strutturale euristica (`1 - 1/(support+1)`) non distingue un
    /// invariante sopravvissuto a mille occasioni di essere violato da una
    /// coincidenza di un grafo giovane. Qui la rimpiazziamo col **lower bound di
    /// Wilson** sul tasso di astensione: per ogni regola contiamo le *occasioni*
    /// (commit che hanno co-toccato i due layer, in cui qualcuno avrebbe potuto
    /// invertire la freccia) e, poiché una regola appena scoperta ha zero archi nel
    /// verso proibito, ogni occasione è un'**astensione**. Più esposizione senza
    /// violazione ⇒ più confidenza.
    ///
    /// Degrada con grazia: senza storia git agganciata
    /// ([`with_commit_history`](Self::with_commit_history)), o per le regole i cui
    /// layer non sono mai stati co-toccati (zero occasioni), la confidenza resta
    /// quella strutturale. L'assenza di evidenza temporale **non abbassa** una
    /// regola: la conferma solo quando l'evidenza c'è.
    pub async fn mine_rules_calibrated(&self) -> anyhow::Result<Vec<LayeringRule>> {
        let mut rules = self.mine_rules().await?;
        let Some(history) = self.history.clone() else {
            return Ok(rules);
        };

        // `git` è I/O bloccante: leggiamo la storia fuori dal runtime async.
        let commits = tokio::task::spawn_blocking(move || history.commits()).await??;

        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let file_layers = self.build_file_layers(&relations).await?;

        for rule in &mut rules {
            // Le regole dichiarate valgono per decreto: niente calibrazione sul
            // tempo, la confidenza resta 1.0 voluta dall'umano.
            if rule.origin == codeos_types::bus::RuleOrigin::Declared {
                continue;
            }
            let occ = occasions(&rule.upstream.0, &rule.downstream.0, &file_layers, &commits);
            if occ == 0 {
                continue; // nessuna evidenza temporale: tieni la confidenza strutturale.
            }
            // Regola appena scoperta ⇒ direzione proibita assente nel grafo ⇒ zero
            // violazioni nel tempo: ogni occasione è stata un'astensione.
            rule.confidence = Abstention::new(occ, 0).wilson_lower_bound(Z_95) as f32;
        }
        Ok(rules)
    }

    /// **Fossili di Decisione.** Per ogni regola scoperta, scava nella storia git
    /// l'istante di *cristallizzazione* del confine: il commit più vecchio che ha
    /// co-toccato i due layer, con il diff strutturale di nascita e l'**intento**
    /// (il messaggio del commit, le parole di chi ha disegnato il confine).
    ///
    /// È il complemento del Campo di Astensione: là misuriamo *quanto* un
    /// invariante è battle-tested, qui recuperiamo *quando* e *perché* è nato —
    /// l'invariante dedotto dallo spazio negativo viene ancorato a un intento umano
    /// reale, datato e citabile.
    ///
    /// Vuoto senza storia git agganciata ([`with_commit_history`](Self::with_commit_history))
    /// o per le regole i cui layer non si sono mai co-toccati. Ordinato per istante
    /// di nascita (i confini più antichi per primi).
    pub async fn fossils(&self) -> anyhow::Result<Vec<DecisionFossil>> {
        let rules = self.mine_rules().await?;
        let by_key = self.excavate_fossils(&rules).await?;
        let mut out: Vec<DecisionFossil> = rules
            .iter()
            .filter_map(|rule| by_key.get(&rule_key(rule)).cloned())
            .collect();
        out.sort_by(|a, b| {
            a.born_at_unix
                .cmp(&b.born_at_unix)
                .then_with(|| a.upstream.cmp(&b.upstream))
        });
        Ok(out)
    }

    /// Scava i fossili per un insieme di regole, indicizzati per [`rule_key`].
    /// Mappa vuota (no-op) senza storia git o senza regole. Centralizza la lettura
    /// bloccante di git (in `spawn_blocking`) e il ponte file → layer, così sia
    /// [`fossils`](Self::fossils) sia [`learn`](Self::learn) la condividono.
    async fn excavate_fossils(
        &self,
        rules: &[LayeringRule],
    ) -> anyhow::Result<HashMap<String, DecisionFossil>> {
        let Some(history) = self.history.clone() else {
            return Ok(HashMap::new());
        };
        if rules.is_empty() {
            return Ok(HashMap::new());
        }

        let commits = tokio::task::spawn_blocking(move || history.commits()).await??;
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let file_layers = self.build_file_layers(&relations).await?;

        let mut out = HashMap::new();
        for rule in rules {
            if let Some(fossil) =
                excavate(&rule.upstream.0, &rule.downstream.0, &file_layers, &commits)
            {
                out.insert(rule_key(rule), fossil);
            }
        }
        Ok(out)
    }

    /// **Spazio negativo del secondo ordine.** Punta l'algoritmo dello spazio
    /// negativo un livello più in alto, sulla griglia *layer × layer*, e segnala gli
    /// invarianti che *mancano dove dovrebbero esserci*: un layer-fondazione,
    /// rispettato a senso unico da molti altri layer, che ha però **un'unica
    /// eccezione** accoppiata bidirezionalmente. Quel buco nella convenzione è quasi
    /// sempre debito tecnico o un bug latente — il candidato perfetto da far rivedere
    /// a un umano (o a un LLM).
    ///
    /// È diagnostica, non muta la memoria: l'assenza di un invariante è un
    /// *sospetto*, non un fatto da promuovere a `Decision`. Ordinato per quanto è
    /// isolata l'eccezione (`foundation_support` decrescente).
    pub async fn missing_invariants(&self) -> anyhow::Result<Vec<MissingInvariant>> {
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let layer_map = self.build_layer_map(&relations).await?;
        let rules = mine_layering_rules(&relations, &layer_map, &self.config);
        Ok(mine_missing_invariants(
            &relations,
            &layer_map,
            &rules,
            &self.meta_config,
        ))
    }

    /// **Il ciclo di apprendimento.** Riconcilia la memoria storica con la verità
    /// architetturale corrente del grafo:
    ///
    /// - **promuove** gli invarianti appena scoperti a `Decision` (kind
    ///   `ArchitectureRule`), così il *perché* — finora implicito nello spazio
    ///   negativo — diventa esplicito e interrogabile dal Query;
    /// - **ritira** gli invarianti diventati *stale*: una regola un tempo valida che
    ///   il grafo non sostiene più (ora ci sono dipendenze in entrambi i versi).
    ///
    /// Tutto è **additivo**: non si cancella nulla (invariante della memoria). Un
    /// ritiro è una nuova `Decision` che referenzia la promozione via
    /// `related_decision_ids` — la storia resta leggibile, ma lo stato corrente è
    /// quello che conta. È **idempotente**: a parità di grafo, una seconda passata
    /// non produce nulla. No-op se nessuna memoria è agganciata. Restituisce gli
    /// `EntityId` delle decisioni create (promozioni + ritiri).
    pub async fn learn(&self) -> anyhow::Result<Vec<EntityId>> {
        let Some(store) = &self.decisions else {
            return Ok(Vec::new());
        };

        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let layer_map = self.build_layer_map(&relations).await?;
        let rules = mine_layering_rules(&relations, &layer_map, &self.config);
        let current_keys: HashSet<String> = rules.iter().map(rule_key).collect();

        // Fossili di Decisione: se c'è una storia git, datiamo ogni confine alla
        // sua nascita per ancorare la promozione a un intento umano reale. Vuoto
        // (e a costo zero) senza storia: la promozione resta com'era.
        let fossils = self.excavate_fossils(&rules).await?;

        // Ricostruisci il registro degli invarianti ATTIVI ripercorrendo la storia
        // (le decisioni arrivano ordinate per timestamp): una promozione attiva la
        // chiave, un ritiro la disattiva. È la storia additiva a definire lo stato.
        let mut active: HashMap<String, ActiveInvariant> = HashMap::new();
        for decision in store.all().await? {
            let Some(key) = decision
                .tags
                .iter()
                .find(|t| t.starts_with(RULE_TAG_PREFIX))
                .cloned()
            else {
                continue;
            };
            if decision.tags.iter().any(|t| t == STATUS_RETIRED) {
                active.remove(&key);
            } else {
                active.insert(
                    key,
                    ActiveInvariant {
                        promotion_id: decision.id,
                        related_entity_ids: decision.related_entity_ids,
                    },
                );
            }
        }

        let mut changes = Vec::new();

        // (A) Promozione: gli invarianti scoperti ora e non ancora in registro.
        for rule in &rules {
            if active.contains_key(&rule_key(rule)) {
                continue;
            }
            let related = boundary_entities(rule, &relations, &layer_map);
            let decision = promotion_decision(rule, related, fossils.get(&rule_key(rule)));
            store.record(&decision).await?;
            tracing::info!(
                upstream = %rule.upstream,
                downstream = %rule.downstream,
                support = rule.support,
                "invariante di layering promosso a memoria storica"
            );
            changes.push(decision.id);
        }

        // (B) Ritiro: gli invarianti attivi che il grafo non sostiene più. Ordinati
        // per chiave così l'output (e i timestamp dei file Markdown) è deterministico.
        let mut stale: Vec<(String, ActiveInvariant)> = active
            .into_iter()
            .filter(|(key, _)| !current_keys.contains(key))
            .collect();
        stale.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, entry) in stale {
            let Some((upstream, downstream)) = parse_rule_key(&key) else {
                continue;
            };
            let decision = retraction_decision(
                upstream,
                downstream,
                entry.promotion_id,
                entry.related_entity_ids,
            );
            store.record(&decision).await?;
            tracing::info!(
                upstream,
                downstream,
                "invariante di layering ritirato: asimmetria non più valida"
            );
            changes.push(decision.id);
        }

        Ok(changes)
    }

    /// Verifica delle relazioni **candidate** (ipotetiche, o appena aggiunte)
    /// contro le regole scoperte dal grafo.
    ///
    /// Le candidate vengono **escluse** dall'insieme da cui si scoprono le regole:
    /// così una dipendenza vietata che fosse già stata persistita non finisce per
    /// "giustificare sé stessa" annullando lo spazio negativo che la rende
    /// illecita.
    pub async fn check(
        &self,
        candidates: &[Relation],
    ) -> anyhow::Result<Vec<ArchitectureViolation>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let candidate_ids: HashSet<EntityId> = candidates.iter().map(|r| r.id).collect();
        let mut established = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        established.retain(|r| !candidate_ids.contains(&r.id));

        // La mappa dei layer deve coprire gli estremi sia delle relazioni
        // stabilite sia delle candidate.
        let mut endpoints = established.clone();
        endpoints.extend_from_slice(candidates);
        let layer_map = self.build_layer_map(&endpoints).await?;

        let mined = mine_layering_rules(&established, &layer_map, &self.config);
        let rules = self.merge_declared(mined);
        let mut violations = violations_for(candidates, &layer_map, &rules);

        // Arricchisci ogni violazione con la posizione dell'entità SORGENTE: è lì
        // che vive la dipendenza proibita, ed è dove un editor pianterà la
        // diagnostica. La funzione pura `violations_for` non vede le entità; qui
        // sì, via storage. Se l'entità non c'è, la posizione resta `None`.
        for violation in &mut violations {
            if let Some(entity) = self.storage.get_entity_by_id(&violation.source_id).await? {
                violation.location = Some(entity.location);
            }
        }
        Ok(violations)
    }

    /// Mappa ogni entità che compare come estremo di una relazione al suo layer.
    /// Gli `EntityId` nulli (target di `Unresolved`) e le entità non trovate sono
    /// semplicemente assenti dalla mappa: chi le interroga le ignora.
    ///
    /// Le **dipendenze esterne** (`tokio`, `std`, `react`…) sono volutamente
    /// **escluse** (P0-2): non sono layer *della tua* architettura, quindi un arco
    /// `crates::codeos-rpc → external::std` non deve diventare l'invariante assurdo
    /// "external::std non deve dipendere da codeos-rpc". Restano comunque nel grafo
    /// (interrogabili via query/impatto): qui le togliamo solo dal *ragionamento sui
    /// layer*. Essendo assenti dalla mappa, ogni arco che le tocca è ignorato in
    /// blocco da [`crate::invariant::cross_layer`] (mining, gap e violazioni).
    async fn build_layer_map(
        &self,
        relations: &[Relation],
    ) -> anyhow::Result<HashMap<EntityId, LayerKey>> {
        let mut ids: HashSet<EntityId> = HashSet::new();
        for rel in relations {
            if !rel.source_id.is_nil() {
                ids.insert(rel.source_id);
            }
            if !rel.target_id.is_nil() {
                ids.insert(rel.target_id);
            }
        }

        let mut map = HashMap::with_capacity(ids.len());
        for id in ids {
            if let Some(entity) = self.storage.get_entity_by_id(&id).await? {
                if entity.kind == EntityKind::ExternalDependency {
                    continue;
                }
                map.insert(
                    id,
                    layer_of(&entity.qualified_name, self.config.layer_depth),
                );
            }
        }
        Ok(map)
    }

    /// Mappa ogni **file** (il `file_path` dell'entità) ai layer che vi sono
    /// definiti. È il ponte tra il mondo del grafo (i layer derivano dai
    /// `qualified_name`) e il mondo di git (i commit toccano *file*): senza, non
    /// potremmo contare le occasioni di astensione. Un file può ospitare più layer
    /// (raro ma gestito con un set).
    async fn build_file_layers(
        &self,
        relations: &[Relation],
    ) -> anyhow::Result<HashMap<String, HashSet<String>>> {
        let mut ids: HashSet<EntityId> = HashSet::new();
        for rel in relations {
            if !rel.source_id.is_nil() {
                ids.insert(rel.source_id);
            }
            if !rel.target_id.is_nil() {
                ids.insert(rel.target_id);
            }
        }

        let mut map: HashMap<String, HashSet<String>> = HashMap::new();
        for id in ids {
            if let Some(entity) = self.storage.get_entity_by_id(&id).await? {
                // Coerente con build_layer_map: le esterne non sono layer e il loro
                // file fittizio `<external>` non è mai toccato da un commit git.
                if entity.kind == EntityKind::ExternalDependency {
                    continue;
                }
                let layer = layer_of(&entity.qualified_name, self.config.layer_depth).0;
                map.entry(entity.location.file_path)
                    .or_default()
                    .insert(layer);
            }
        }
        Ok(map)
    }
}

/// La chiave stabile di un invariante: identifica la coppia ordinata
/// (upstream, downstream) a prescindere dall'`id` (rigenerato a ogni mining).
fn rule_key(rule: &LayeringRule) -> String {
    format!("{RULE_TAG_PREFIX}{}|{}", rule.upstream, rule.downstream)
}

/// Inverso di [`rule_key`]: ricava `(upstream, downstream)` dalla chiave. `None`
/// se la stringa non ha la forma attesa (decisione manipolata a mano).
fn parse_rule_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix(RULE_TAG_PREFIX)?.split_once('|')
}

/// Trasforma una regola scoperta in una `Decision` pronta per la memoria storica.
/// Il *perché* (finora implicito nello spazio negativo) viene reso in prosa, e le
/// entità di confine vengono agganciate perché il Query lo ritrovi.
///
/// Se è disponibile un [`DecisionFossil`] (storia git agganciata), il *contesto*
/// viene **ancorato alla nascita del confine**: il commit che lo ha disegnato, il
/// suo intento (il messaggio) e il diff strutturale di cristallizzazione. Un tag
/// `born-at:{hash}` rende il fossile filtrabile. L'arricchimento NON tocca la
/// chiave di dedup ([`rule_key`]): `learn` resta idempotente.
fn promotion_decision(
    rule: &LayeringRule,
    related_entity_ids: Vec<EntityId>,
    fossil: Option<&DecisionFossil>,
) -> Decision {
    let mut context = format!(
        "Scoperto automaticamente dal Guardian osservando lo spazio negativo del \
         grafo: {support} dipendenze vanno '{downstream}' → '{upstream}' e zero nel \
         verso opposto. L'asimmetria è trattata come invariante architetturale.",
        support = rule.support,
        downstream = rule.downstream,
        upstream = rule.upstream,
    );
    let mut tags = vec![
        "layering".to_string(),
        "invariante".to_string(),
        rule_key(rule),
        STATUS_ACTIVE.to_string(),
    ];

    // Fossile di Decisione: àncora la regola alla sua origine storica.
    if let Some(fossil) = fossil.filter(|f| !f.born_at.is_empty()) {
        let short: String = fossil.born_at.chars().take(12).collect();
        let intent = if fossil.intent.is_empty() {
            "(nessun messaggio di commit)"
        } else {
            fossil.intent.as_str()
        };
        let structure = if fossil.born_structure.is_empty() {
            "(nessun file tracciato)".to_string()
        } else {
            fossil.born_structure.join(", ")
        };
        context.push_str(&format!(
            " Origine storica (Fossile di Decisione): il confine è nato nel commit \
             {short} — «{intent}». Diff di cristallizzazione: {structure}."
        ));
        tags.push(format!("born-at:{short}"));
    }

    let new = NewDecision {
        author: "ai:ArchitectureGuardian".to_string(),
        title: format!(
            "Invariante di layering: '{}' non deve dipendere da '{}'",
            rule.upstream, rule.downstream
        ),
        context,
        rationale: format!(
            "'{upstream}' è un layer di base da cui '{downstream}' dipende, mai il \
             contrario: un arco '{upstream}' → '{downstream}' invertirebbe la freccia \
             architetturale stabilita. Confidenza euristica {confidence:.2} (support \
             {support}); regola scoperta dai dati, non configurata a mano.",
            upstream = rule.upstream,
            downstream = rule.downstream,
            confidence = rule.confidence,
            support = rule.support,
        ),
        related_entity_ids,
        related_decision_ids: Vec::new(),
        tags,
    };
    Decision::from_new(new, DecisionKind::ArchitectureRule)
}

/// Registra il **ritiro** di un invariante diventato stale. Non è una regola in
/// vigore (kind `Decision`, non `ArchitectureRule`) ma una nota storica: referenzia
/// la promozione originale e riusa le sue entità di confine, così la stessa query
/// che mostrava la regola ne mostra anche la fine.
fn retraction_decision(
    upstream: &str,
    downstream: &str,
    promotion_id: EntityId,
    related_entity_ids: Vec<EntityId>,
) -> Decision {
    let new = NewDecision {
        author: "ai:ArchitectureGuardian".to_string(),
        title: format!(
            "Invariante di layering ritirato: '{upstream}' → '{downstream}' ora esiste nel grafo"
        ),
        context: format!(
            "L'invariante che vietava '{upstream}' → '{downstream}' non è più sostenuto \
             dallo spazio negativo: il grafo ora contiene dipendenze in ENTRAMBI i versi \
             tra i due layer."
        ),
        rationale: format!(
            "Ritirato automaticamente dal Guardian: la condizione che l'aveva fatto \
             emergere (zero archi '{upstream}' → '{downstream}') non è più vera. La \
             promozione originale resta in memoria come storia (vedi \
             related_decision_ids): la memoria non si riscrive, si stratifica."
        ),
        related_entity_ids,
        related_decision_ids: vec![promotion_id],
        tags: vec![
            "layering".to_string(),
            "invariante".to_string(),
            format!("{RULE_TAG_PREFIX}{upstream}|{downstream}"),
            STATUS_RETIRED.to_string(),
        ],
    };
    Decision::from_new(new, DecisionKind::Decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    use codeos_storage::SqliteStorage;
    use codeos_types::{Entity, EntityKind, GraphDelta, RelationKind, SourceLocation};

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
            metadata: Map::new(),
        }
    }

    fn relation(kind: RelationKind, source: EntityId, target: EntityId) -> Relation {
        Relation {
            id: EntityId::new(),
            kind,
            source_id: source,
            target_id: target,
            metadata: Map::new(),
        }
    }

    /// Semina un grafo a due layer con tre dipendenze `app::api` → `app::core`.
    /// Ritorna (storage, entità api, entità core).
    async fn seeded_two_layer_graph() -> (Arc<dyn GraphStorage>, Vec<Entity>, Vec<Entity>) {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::api::handler_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::core::service_{i}::do_it")))
            .collect();

        let relations = (0..3)
            .map(|i| relation(RelationKind::Calls, api[i].id, core[i].id))
            .collect();

        storage
            .apply_delta(GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: relations,
                ..Default::default()
            })
            .await
            .unwrap();

        (storage, api, core)
    }

    #[tokio::test]
    async fn discovers_the_layering_rule_from_the_graph() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);

        let rules = guardian.mine_rules().await.unwrap();
        assert_eq!(rules.len(), 1, "regole = {rules:?}");
        assert_eq!(rules[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("app::api".to_string()));
    }

    #[tokio::test]
    async fn external_dependencies_never_become_layering_rules() {
        // Regressione P0-2: un layer interno che importa `tokio` 3 volte è
        // un'asimmetria a senso unico PERFETTA — esattamente la forma che il miner
        // promuoverebbe a invariante. Ma `external::tokio` non è un layer della
        // nostra architettura: non deve generare la regola assurda "external::tokio
        // non deve dipendere da app::api". Deve restare però nel grafo (query/impatto).
        let (storage, api, _core) = seeded_two_layer_graph().await;

        let tokio = Entity {
            id: EntityId::new(),
            kind: EntityKind::ExternalDependency,
            qualified_name: "external::tokio".to_string(),
            location: SourceLocation {
                file_path: "<external>".to_string(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            metadata: Map::new(),
        };
        let ext_edges: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Imports, api[i].id, tokio.id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: vec![tokio.clone()],
                added_relations: ext_edges,
                ..Default::default()
            })
            .await
            .unwrap();

        let guardian = Guardian::new(storage.clone());
        let rules = guardian.mine_rules().await.unwrap();

        // Solo l'invariante reale (app::core ← app::api). Nessuna regola che nomini
        // un layer esterno, in nessuno dei due versi.
        assert_eq!(rules.len(), 1, "atteso solo l'invariante interno: {rules:?}");
        assert!(
            !rules.iter().any(|r| {
                r.upstream.0.starts_with("external")
                    || r.downstream.0.starts_with("external")
            }),
            "nessun invariante deve nominare una dipendenza esterna: {rules:?}"
        );

        // La dipendenza esterna resta interrogabile nel grafo (non è stata rimossa).
        assert!(
            storage
                .get_entity_by_qname("external::tokio")
                .await
                .unwrap()
                .is_some(),
            "external::tokio deve restare nel grafo per query e impatto"
        );
    }

    #[tokio::test]
    async fn flags_a_proposed_reversed_dependency() {
        let (storage, api, core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);

        // Edge proposto core → api: il layer di base che dipende dal layer alto.
        let proposed = relation(RelationKind::Calls, core[0].id, api[0].id);
        let violations = guardian
            .check(std::slice::from_ref(&proposed))
            .await
            .unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].relation_id, proposed.id);
        assert!(violations[0].message.contains("app::core"));

        // Edge proposto nella direzione lecita: nessuna violazione.
        let ok = relation(RelationKind::Calls, api[1].id, core[1].id);
        assert!(guardian.check(&[ok]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn promotes_the_discovered_invariant_to_persistent_memory() {
        use codeos_memory::InMemoryDecisionStore;

        let (storage, api, core) = seeded_two_layer_graph().await;
        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian = Guardian::with_memory(storage, store.clone());

        let created = guardian.learn().await.unwrap();
        assert_eq!(created.len(), 1, "un invariante atteso");

        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 1);
        let decision = &all[0];
        assert_eq!(decision.kind, DecisionKind::ArchitectureRule);
        assert_eq!(decision.author, "ai:ArchitectureGuardian");
        assert!(decision.title.contains("app::core"));
        assert!(decision.title.contains("app::api"));
        // Le entità di confine sono agganciate ⇒ il Query ritroverà il perché
        // interrogando una qualsiasi delle entità coinvolte.
        assert!(decision.related_entity_ids.contains(&api[0].id));
        assert!(decision.related_entity_ids.contains(&core[0].id));

        // Idempotente: una seconda passata non duplica l'invariante.
        let again = guardian.learn().await.unwrap();
        assert!(again.is_empty(), "la seconda learn non deve creare nulla");
        assert_eq!(store.all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn learn_is_a_noop_without_memory() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        assert!(guardian.learn().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn retires_a_stale_invariant_when_the_asymmetry_breaks() {
        use codeos_memory::InMemoryDecisionStore;

        let (storage, api, core) = seeded_two_layer_graph().await;
        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian = Guardian::with_memory(storage.clone(), store.clone());

        // L'invariante emerge e viene promosso.
        assert_eq!(guardian.learn().await.unwrap().len(), 1);

        // Il codice evolve: 3 archi core → api (il verso prima proibito). Ora la
        // dipendenza è bidirezionale e l'asimmetria che giustificava la regola sparisce.
        let reverse: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Calls, core[i].id, api[i].id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_relations: reverse,
                ..Default::default()
            })
            .await
            .unwrap();

        // learn() deve RITIRARE l'invariante, senza cancellare la promozione.
        let changes = guardian.learn().await.unwrap();
        assert_eq!(changes.len(), 1, "atteso esattamente un ritiro");

        let all = store.all().await.unwrap();
        assert_eq!(
            all.len(),
            2,
            "promozione + ritiro coesistono (storia additiva)"
        );
        let promotion = all
            .iter()
            .find(|d| d.kind == DecisionKind::ArchitectureRule)
            .expect("promozione mancante");
        let retraction = all
            .iter()
            .find(|d| d.tags.iter().any(|t| t == "status:retired"))
            .expect("ritiro mancante");
        // Il ritiro referenzia la promozione e ne eredita le entità di confine.
        assert!(retraction.related_decision_ids.contains(&promotion.id));
        assert_eq!(retraction.related_entity_ids, promotion.related_entity_ids);

        // Idempotente: a grafo invariato, un'altra learn non aggiunge nulla.
        assert!(guardian.learn().await.unwrap().is_empty());
        assert_eq!(store.all().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn re_promotes_after_a_retired_invariant_re_emerges() {
        use codeos_memory::InMemoryDecisionStore;

        let (storage, api, core) = seeded_two_layer_graph().await;
        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian = Guardian::with_memory(storage.clone(), store.clone());

        guardian.learn().await.unwrap(); // promozione

        // Rompi l'asimmetria, poi ritira.
        let reverse: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Calls, core[i].id, api[i].id))
            .collect();
        let reverse_ids: Vec<EntityId> = reverse.iter().map(|r| r.id).collect();
        storage
            .apply_delta(GraphDelta {
                added_relations: reverse,
                ..Default::default()
            })
            .await
            .unwrap();
        guardian.learn().await.unwrap(); // ritiro

        // Il codice torna sano: rimuoviamo gli archi proibiti. L'asimmetria si ripristina.
        storage
            .apply_delta(GraphDelta {
                removed_relation_ids: reverse_ids,
                ..Default::default()
            })
            .await
            .unwrap();

        // learn() deve RI-promuovere l'invariante (il ritiro non lo blocca per sempre).
        let changes = guardian.learn().await.unwrap();
        assert_eq!(changes.len(), 1, "l'invariante riemerso va ri-promosso");

        // Storia: promozione, ritiro, ri-promozione — tre strati, due regole attive nel tempo.
        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 3);
        let active = all
            .iter()
            .filter(|d| {
                d.kind == DecisionKind::ArchitectureRule
                    && !d.tags.iter().any(|t| t == "status:retired")
            })
            .count();
        assert_eq!(active, 2, "due promozioni nella storia");
    }

    // ---------------------------------------------------------------------------
    // Regole DICHIARATE: il decreto umano che vale anche senza evidenza nel grafo.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn declared_rule_is_enforced_without_any_structural_support() {
        use crate::declared::declared_layering_rules;

        // Grafo con DUE soli archi api → core: sotto la soglia di min_support (3),
        // quindi NESSUN invariante verrebbe scoperto dallo spazio negativo.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..2)
            .map(|i| entity(&format!("app::api::h_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..2)
            .map(|i| entity(&format!("app::core::s_{i}::do_it")))
            .collect();
        let good: Vec<Relation> = (0..2)
            .map(|i| relation(RelationKind::Calls, api[i].id, core[i].id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: good,
                ..Default::default()
            })
            .await
            .unwrap();

        // Senza config: niente regole (supporto insufficiente).
        let plain = Guardian::new(storage.clone());
        assert!(plain.mine_rules().await.unwrap().is_empty());

        // Con la regola DICHIARATA "app::api non deve dipendere da app::core".
        let declared = declared_layering_rules(
            "architecture:\n  rules:\n    - type: layer_dependency\n      from: [\"app::api\"]\n      to: [\"app::core\"]\n",
        );
        let guardian = Guardian::new(storage).with_declared_rules(declared);

        // Ora la regola esiste (per decreto) e compare nel mining, etichettata.
        let rules = guardian.mine_rules().await.unwrap();
        assert_eq!(rules.len(), 1, "regole = {rules:?}");
        assert_eq!(rules[0].origin, codeos_types::bus::RuleOrigin::Declared);
        assert_eq!(rules[0].upstream, LayerKey("app::api".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("app::core".to_string()));

        // E viene fatta rispettare: un arco api → core inverte la freccia dichiarata.
        let bad = relation(RelationKind::Calls, api[0].id, core[0].id);
        let violations = guardian.check(std::slice::from_ref(&bad)).await.unwrap();
        assert_eq!(violations.len(), 1, "violazioni = {violations:?}");
        assert_eq!(violations[0].relation_id, bad.id);
    }

    #[tokio::test]
    async fn discovered_rule_wins_over_a_duplicate_declaration() {
        use crate::declared::declared_layering_rules;

        // Il grafo scopre app::api → app::core (support 3). La stessa coppia è anche
        // dichiarata: non deve duplicarsi, e deve restare la versione SCOPERTA (che
        // porta supporto ed evidenza).
        // Lo spazio negativo scopre upstream=app::core, downstream=app::api
        // ("core non deve dipendere da api"). Dichiariamo lo STESSO confine:
        // from=app::core, to=app::api.
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let declared = declared_layering_rules(
            "architecture:\n  rules:\n    - type: layer_dependency\n      from: [\"app::core\"]\n      to: [\"app::api\"]\n",
        );
        let guardian = Guardian::new(storage).with_declared_rules(declared);

        let rules = guardian.mine_rules().await.unwrap();
        assert_eq!(rules.len(), 1, "nessun duplicato: {rules:?}");
        assert_eq!(rules[0].origin, codeos_types::bus::RuleOrigin::Discovered);
        assert_eq!(rules[0].support, 3);
    }

    #[tokio::test]
    async fn check_excludes_candidates_so_a_persisted_bad_edge_self_reports() {
        let (storage, api, core) = seeded_two_layer_graph().await;

        // La dipendenza proibita viene effettivamente PERSISTITA (come se un LLM
        // l'avesse già scritta). Senza l'esclusione delle candidate, lo spazio
        // negativo sarebbe sporcato e non rileveremmo nulla.
        let bad = relation(RelationKind::Calls, core[0].id, api[0].id);
        storage
            .apply_delta(GraphDelta {
                added_relations: vec![bad.clone()],
                ..Default::default()
            })
            .await
            .unwrap();

        let guardian = Guardian::new(storage);
        let violations = guardian.check(std::slice::from_ref(&bad)).await.unwrap();
        assert_eq!(violations.len(), 1, "violazioni = {violations:?}");
        assert_eq!(violations[0].relation_id, bad.id);
    }

    // ---------------------------------------------------------------------------
    // Campo di Astensione: la confidenza calibrata sul *negativo del tempo*.
    // ---------------------------------------------------------------------------

    /// La confidenza strutturale di una regola con support 3: `1 - 1/(3+1)`.
    const STRUCTURAL_CONF_SUPPORT_3: f32 = 0.75;

    #[tokio::test]
    async fn mature_history_raises_confidence_above_the_structural_baseline() {
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, api, core) = seeded_two_layer_graph().await;

        // Baseline: senza tempo, è solo l'euristica strutturale (0.75).
        let baseline = Guardian::new(storage.clone()).mine_rules().await.unwrap();
        assert_eq!(baseline.len(), 1);
        assert!((baseline[0].confidence - STRUCTURAL_CONF_SUPPORT_3).abs() < 1e-6);

        // Storia matura: 30 commit co-toccano api e core SENZA mai invertire la
        // freccia — 30 astensioni. La regola ha pagato 30 occasioni di violazione.
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();
        let mature: Vec<Commit> = (0..30)
            .map(|_| Commit::new([api_file.clone(), core_file.clone()]))
            .collect();

        let guardian =
            Guardian::new(storage).with_commit_history(Arc::new(InMemoryHistory::new(mature)));
        let rules = guardian.mine_rules_calibrated().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert!(
            rules[0].confidence > baseline[0].confidence,
            "30 astensioni devono ALZARE la confidenza: {} vs baseline {}",
            rules[0].confidence,
            baseline[0].confidence
        );
        assert!(rules[0].confidence > 0.85, "conf = {}", rules[0].confidence);
    }

    #[tokio::test]
    async fn scarce_history_lowers_confidence_below_the_structural_baseline() {
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();

        // Solo 2 occasioni: l'asimmetria strutturale (3 archi) potrebbe essere una
        // coincidenza di un grafo giovane. Il tempo lo rivela e la confidenza scende
        // SOTTO il baseline strutturale — cosa che l'euristica non sapeva fare.
        let scarce = vec![
            Commit::new([api_file.clone(), core_file.clone()]),
            Commit::new([api_file, core_file]),
        ];
        let guardian =
            Guardian::new(storage).with_commit_history(Arc::new(InMemoryHistory::new(scarce)));
        let rules = guardian.mine_rules_calibrated().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert!(
            rules[0].confidence < STRUCTURAL_CONF_SUPPORT_3,
            "poca esposizione deve ABBASSARE la confidenza: {}",
            rules[0].confidence
        );
    }

    #[tokio::test]
    async fn without_history_confidence_stays_structural() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        // Nessuna storia agganciata: mine_rules_calibrated degrada a mine_rules.
        let rules = guardian.mine_rules_calibrated().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert!((rules[0].confidence - STRUCTURAL_CONF_SUPPORT_3).abs() < 1e-6);
    }

    #[tokio::test]
    async fn history_without_co_changes_keeps_structural_confidence() {
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, _api, _core) = seeded_two_layer_graph().await;
        // Commit che toccano file ESTRANEI ai layer noti: zero occasioni. L'assenza
        // di evidenza temporale non deve abbassare la regola, solo lasciarla com'è.
        let unrelated = vec![
            Commit::new(["docs/readme.md"]),
            Commit::new(["ci/pipeline.yml"]),
        ];
        let guardian =
            Guardian::new(storage).with_commit_history(Arc::new(InMemoryHistory::new(unrelated)));
        let rules = guardian.mine_rules_calibrated().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert!(
            (rules[0].confidence - STRUCTURAL_CONF_SUPPORT_3).abs() < 1e-6,
            "conf = {}",
            rules[0].confidence
        );
    }

    // ---------------------------------------------------------------------------
    // Fossili di Decisione: la nascita del confine (l'asse dell'INTENTO).
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn fossils_recovers_the_birth_of_the_boundary() {
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();

        // Newest-first (come git log): l'ultimo commit è la nascita del confine.
        let history = InMemoryHistory::new(vec![
            Commit::with_meta(
                "newer0",
                200,
                "later refactor",
                [api_file.clone(), core_file.clone()],
            ),
            Commit::with_meta(
                "old00birth",
                100,
                "draw the boundary",
                [api_file.clone(), core_file.clone()],
            ),
        ]);
        let guardian = Guardian::new(storage).with_commit_history(Arc::new(history));

        let fossils = guardian.fossils().await.unwrap();
        assert_eq!(fossils.len(), 1, "un fossile atteso: {fossils:?}");
        let f = &fossils[0];
        assert_eq!(f.born_at, "old00birth");
        assert_eq!(f.born_at_unix, 100);
        assert_eq!(f.intent, "draw the boundary");
        assert_eq!(f.upstream, "app::core");
        assert_eq!(f.downstream, "app::api");
        assert!(f.born_structure.contains(&api_file));
        assert!(f.born_structure.contains(&core_file));
    }

    #[tokio::test]
    async fn fossils_is_empty_without_history() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        assert!(guardian.fossils().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn learn_anchors_the_promotion_to_its_birth_commit() {
        use codeos_memory::InMemoryDecisionStore;
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();

        let history = InMemoryHistory::new(vec![
            Commit::with_meta(
                "zzz999",
                200,
                "refactor api handlers",
                [api_file.clone(), core_file.clone()],
            ),
            Commit::with_meta(
                "birth01",
                100,
                "introduce api layer over core",
                [api_file.clone(), core_file.clone()],
            ),
        ]);
        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian =
            Guardian::with_memory(storage, store.clone()).with_commit_history(Arc::new(history));

        assert_eq!(guardian.learn().await.unwrap().len(), 1);

        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 1);
        let promo = &all[0];
        // Il contesto è ancorato al commit di nascita e ne cita l'intento.
        assert!(promo.context.contains("birth01"), "ctx = {}", promo.context);
        assert!(
            promo.context.contains("introduce api layer over core"),
            "ctx = {}",
            promo.context
        );
        assert!(promo.context.contains("Fossile di Decisione"));
        // Il tag rende il fossile filtrabile dal Query.
        assert!(
            promo.tags.iter().any(|t| t == "born-at:birth01"),
            "tags = {:?}",
            promo.tags
        );

        // Idempotente: l'arricchimento col fossile non altera la chiave di dedup.
        assert!(guardian.learn().await.unwrap().is_empty());
        assert_eq!(store.all().await.unwrap().len(), 1);
    }

    // ---------------------------------------------------------------------------
    // Spazio negativo del 2° ordine: l'invariante che MANCA dove dovrebbe esserci.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn flags_a_missing_invariant_against_an_established_foundation() {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::api::h_{i}::run")))
            .collect();
        let web: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::web::v_{i}::show")))
            .collect();
        let jobs: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::jobs::j_{i}::work")))
            .collect();
        let core: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::core::s_{i}::do_it")))
            .collect();

        // api/web/jobs dipendono da core a senso unico; core → jobs è il ritorno
        // anomalo che lascia scoperto l'invariante atteso (core, jobs).
        let mut relations: Vec<Relation> = Vec::new();
        for i in 0..3 {
            relations.push(relation(RelationKind::Calls, api[i].id, core[i].id));
            relations.push(relation(RelationKind::Calls, web[i].id, core[i].id));
            relations.push(relation(RelationKind::Calls, jobs[i].id, core[i].id));
        }
        relations.push(relation(RelationKind::Calls, core[0].id, jobs[0].id));

        storage
            .apply_delta(GraphDelta {
                added_entities: api
                    .iter()
                    .chain(web.iter())
                    .chain(jobs.iter())
                    .chain(core.iter())
                    .cloned()
                    .collect(),
                added_relations: relations,
                ..Default::default()
            })
            .await
            .unwrap();

        let guardian = Guardian::new(storage);
        let gaps = guardian.missing_invariants().await.unwrap();
        assert_eq!(gaps.len(), 1, "gaps = {gaps:?}");
        assert_eq!(gaps[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(gaps[0].downstream, LayerKey("app::jobs".to_string()));
        // core è rispettato a senso unico da api e web ⇒ support 2.
        assert_eq!(gaps[0].foundation_support, 2);
    }

    #[tokio::test]
    async fn no_missing_invariant_in_a_clean_two_layer_graph() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        // Una sola fondazione rispettata da un solo layer: niente convenzione, niente buco.
        assert!(guardian.missing_invariants().await.unwrap().is_empty());
    }
}
