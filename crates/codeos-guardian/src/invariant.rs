//! Il cuore del sistema immunitario: scoperta di **invarianti architetturali**
//! dallo *spazio negativo* del grafo.
//!
//! Idea (mai vista negli strumenti tradizionali, che indicizzano solo ciò che il
//! codice *è*): una codebase sana rispetta delle regole di dipendenza
//! direzionale che **nessuno ha mai scritto** — "il layer A dipende da B, ma B
//! non dipende mai da A". Queste regole non si vedono guardando un singolo arco:
//! si vedono guardando il *pattern delle assenze*. Se nel grafo esistono molti
//! archi A → B e **zero** archi B → A, allora la direzione B → A è proibita per
//! costruzione. È un invariante latente, dissotterrato dai dati.
//!
//! Tutte le funzioni qui sono **pure** (nessun I/O): prendono entità/relazioni
//! già in memoria e producono regole o violazioni. Il lato storage vive in
//! [`crate::guardian`].

use std::collections::{HashMap, HashSet};

use codeos_types::bus::{ArchitectureViolation, RuleOrigin, Severity};
use codeos_types::{Entity, EntityId, Relation, RelationKind};

/// Quante componenti iniziali del `qualified_name` definiscono il "layer".
///
/// Con `qualified_name = "app::api::handlers::UserHandler::create"` e profondità
/// 2, il layer è `app::api`. È la granularità a cui ha senso parlare di regole
/// architetturali (package/modulo di alto livello), non il singolo simbolo.
pub const DEFAULT_LAYER_DEPTH: usize = 2;

/// Quanti archi nella stessa direzione servono prima di fidarsi che l'asimmetria
/// non sia un caso. Sotto questa soglia, l'assenza della direzione opposta è
/// troppo poco significativa per dichiararla un invariante.
pub const DEFAULT_MIN_SUPPORT: u32 = 3;

/// La soglia minima perché un'asimmetria sia un *candidato* invece che rumore.
/// Sotto 2 archi non c'è ripetizione: un singolo arco in una direzione (e zero
/// nell'altra) è un caso, non un confine che si sta formando. I candidati vivono
/// nella banda `[DEFAULT_MIN_CANDIDATE_SUPPORT, min_support)`: abbastanza ripetuti
/// da non essere rumore, non abbastanza da diventare invarianti.
pub const DEFAULT_MIN_CANDIDATE_SUPPORT: u32 = 2;

/// Configurazione del miner di layering.
#[derive(Debug, Clone)]
pub struct LayerConfig {
    /// Componenti iniziali del `qualified_name` che definiscono il layer.
    pub layer_depth: usize,
    /// Archi minimi nella direzione osservata perché una regola sia emessa.
    pub min_support: u32,
}

impl Default for LayerConfig {
    fn default() -> Self {
        Self {
            layer_depth: DEFAULT_LAYER_DEPTH,
            min_support: DEFAULT_MIN_SUPPORT,
        }
    }
}

/// La "firma di layer" di un'entità: le prime `depth` componenti del suo
/// `qualified_name`, unite da `::`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayerKey(pub String);

impl std::fmt::Display for LayerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Estrae il layer da un `qualified_name`. Mai vuoto: tiene almeno la prima
/// componente.
pub fn layer_of(qualified_name: &str, depth: usize) -> LayerKey {
    let parts: Vec<&str> = qualified_name.split("::").collect();
    let take = depth.clamp(1, parts.len().max(1));
    LayerKey(parts[..take.min(parts.len())].join("::"))
}

/// Il layer di un'**entità**, preferendo il confine **reale** del pacchetto quando
/// noto. Se il parser ha timbrato `metadata["package"]` (P1-c, letto dal manifest
/// del workspace), quello È il layer: robusto ai monorepo con annidamento non
/// uniforme, dove [`layer_of`] su path a profondità fissa accorperebbe pacchetti
/// distinti. Senza il metadato si ricade sull'euristica di profondità — nessuna
/// regressione sui grafi privi di manifest.
pub fn layer_of_entity(entity: &Entity, depth: usize) -> LayerKey {
    match entity.metadata.get("package") {
        Some(pkg) if !pkg.is_empty() => LayerKey(pkg.clone()),
        _ => layer_of(&entity.qualified_name, depth),
    }
}

/// Un invariante di layering scoperto dal grafo.
///
/// Semantica: nel grafo la dipendenza va **sempre** da `downstream` verso
/// `upstream` (osservata `support` volte) e **mai** nel verso opposto. Quindi un
/// arco `upstream → downstream` è una **violazione**: inverte la freccia
/// architetturale.
#[derive(Debug, Clone)]
pub struct LayeringRule {
    /// Identità della regola (per agganciarci `ArchitectureViolation` e, in
    /// futuro, una `Decision` del Memory che la conferma o la deroga).
    pub id: EntityId,
    /// Il layer da cui si dipende (a valle nessuno; è la base). Target lecito.
    pub upstream: LayerKey,
    /// Il layer che dipende. Source lecito.
    pub downstream: LayerKey,
    /// Quanti archi `downstream → upstream` sono stati osservati.
    pub support: u32,
    /// Stima euristica (NON calibrata) di quanto l'asimmetria sia significativa:
    /// `1 - 1/(support+1)`. Più archi nella direzione osservata, più ci fidiamo.
    /// Da rimpiazzare con una confidenza calibrata quando avremo il ground truth.
    pub confidence: f32,
    /// Se la regola è stata dissotterrata dal grafo o dichiarata a mano nella
    /// config. Una regola dichiarata vale per decreto e non si calibra sul tempo.
    pub origin: RuleOrigin,
}

/// Un invariante **in formazione**: la stessa asimmetria pura di una
/// [`LayeringRule`], ma con supporto ancora sotto la soglia. È lo **stadio 1** del
/// flusso (candidato → proposta → decisione): un segnale grezzo, *derivato* e
/// **mai persistito** nel ledger (che custodisce solo storia confermata). Niente
/// `confidence` euristica qui: un candidato non è ancora abbastanza solido da
/// meritare una stima, e l'assenza del float lascia il tipo `Eq` (comodo nei test
/// e onesto: non c'è nulla da approssimare finché il confine non si è formato).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayeringCandidate {
    /// Il layer da cui si dipenderebbe (a valle nessun arco osservato). Target.
    pub upstream: LayerKey,
    /// Il layer che dipende (la direzione osservata). Source.
    pub downstream: LayerKey,
    /// Quanti archi `downstream → upstream` sono stati osservati finora.
    pub support: u32,
    /// Quanti archi ancora mancano per diventare un invariante: `min_support -
    /// support`. È il campo che dichiara apertamente l'immaturità (trap #3: non
    /// fossilizziamo un segnale acerbo spacciandolo per confine già stabilito).
    pub needed: u32,
}

/// Solo le relazioni che esprimono una **dipendenza direzionale** definiscono
/// l'architettura. `BelongsTo` è contenimento strutturale, `Tests` è atteso che
/// attraversi i layer, `Unresolved` non ha un target noto: tutte escluse.
fn is_dependency_kind(kind: RelationKind) -> bool {
    matches!(
        kind,
        RelationKind::Calls
            | RelationKind::Imports
            | RelationKind::Uses
            | RelationKind::Creates
            | RelationKind::Extends
            | RelationKind::Implements
            | RelationKind::Modifies
    )
}

/// Confidenza euristica in funzione del supporto (vedi [`LayeringRule::confidence`]).
fn confidence_for(support: u32) -> f32 {
    1.0 - 1.0 / (support as f32 + 1.0)
}

/// Una relazione risolta con confidenza `low` non è abbastanza affidabile per
/// reggere un ragionamento architetturale: il resolver l'ha agganciata con
/// un'euristica fuzzy/cross-package che potrebbe mentire. La escludiamo dal mining
/// a monte (in [`cross_layer`]), così non entra né nel conteggio degli archi né
/// nelle violazioni. Le relazioni **senza** il metadato (es. i `BelongsTo`
/// strutturali, o relazioni create prima di questo campo) sono trattate come
/// **non-low**: nessuna regressione, lo spazio negativo storico resta invariato.
fn relation_confidence_is_low(rel: &Relation) -> bool {
    rel.metadata
        .get("resolution_confidence")
        .map(|c| c == "low")
        .unwrap_or(false)
}

/// Una relazione **nata in un file di test** non descrive l'architettura di
/// produzione: un test importa e chiama liberamente attraverso i layer (è il suo
/// mestiere). La escludiamo dal mining a monte, esattamente come una relazione a
/// bassa confidenza. Il resolver timbra `source_kind` = `test`/`prod` sul file
/// SORGENTE dell'arco. Relazioni senza il metadato (storiche o strutturali) =
/// **non-test**: nessuna regressione sullo spazio negativo già appreso.
fn relation_source_is_test(rel: &Relation) -> bool {
    rel.metadata
        .get("source_kind")
        .map(|s| s == "test")
        .unwrap_or(false)
}

/// La coppia (layer sorgente, layer destinazione) di una relazione, se entrambi
/// gli estremi sono noti, la relazione è una dipendenza, attraversa due layer
/// diversi ed è risolta con confidenza sufficiente. Altrimenti `None` (non
/// contribuisce al ragionamento sui layer).
fn cross_layer<'a>(
    rel: &Relation,
    entity_layer: &'a HashMap<EntityId, LayerKey>,
) -> Option<(&'a LayerKey, &'a LayerKey)> {
    if !is_dependency_kind(rel.kind)
        || rel.target_id.is_nil()
        || relation_confidence_is_low(rel)
        || relation_source_is_test(rel)
    {
        return None;
    }
    let (Some(s), Some(t)) = (
        entity_layer.get(&rel.source_id),
        entity_layer.get(&rel.target_id),
    ) else {
        return None;
    };
    if s == t {
        return None;
    }
    Some((s, t))
}

/// Conta gli archi di dipendenza tra coppie di layer distinti. `pub(crate)` perché
/// lo riusa [`crate::meta`] per lo *spazio negativo del secondo ordine*.
pub(crate) fn count_cross_layer_edges(
    relations: &[Relation],
    entity_layer: &HashMap<EntityId, LayerKey>,
) -> HashMap<(LayerKey, LayerKey), u32> {
    let mut counts: HashMap<(LayerKey, LayerKey), u32> = HashMap::new();
    for rel in relations {
        if let Some((s, t)) = cross_layer(rel, entity_layer) {
            *counts.entry((s.clone(), t.clone())).or_insert(0) += 1;
        }
    }
    counts
}

/// Tutte le **asimmetrie pure** tra coppie di layer: una direzione osservata
/// (supporto > 0) con l'opposta a *zero* archi (spazio negativo puro). **Nessuna
/// soglia** applicata qui: è il mattone comune a regole e candidati, così che
/// restino lo *stesso* pattern classificato solo dal supporto, incapaci di
/// divergere (una regola è un candidato che ha superato `min_support`, non una
/// cosa scoperta da un codice diverso). Ogni tupla è `(downstream, upstream,
/// support)`: `downstream` dipende da `upstream` — la direzione osservata, mai il
/// contrario. La coppia non ordinata è trattata una sola volta.
fn pure_asymmetries(counts: &HashMap<(LayerKey, LayerKey), u32>) -> Vec<(LayerKey, LayerKey, u32)> {
    let mut out = Vec::new();
    let mut handled: HashSet<(LayerKey, LayerKey)> = HashSet::new();

    for ((a, b), &ab) in counts {
        // Tratta la coppia non ordinata {a, b} una sola volta.
        if handled.contains(&(a.clone(), b.clone())) || handled.contains(&(b.clone(), a.clone())) {
            continue;
        }
        handled.insert((a.clone(), b.clone()));

        let ba = counts.get(&(b.clone(), a.clone())).copied().unwrap_or(0);

        // Asimmetria pura: una sola direzione ha archi, l'altra è vuota.
        if ab > 0 && ba == 0 {
            // archi a → b ⇒ a è downstream (dipende), b è upstream (base).
            out.push((a.clone(), b.clone(), ab));
        } else if ba > 0 && ab == 0 {
            out.push((b.clone(), a.clone(), ba));
        }
    }
    out
}

/// **Il miner.** Scopre le regole di layering dalle relazioni: per ogni coppia di
/// layer, se una direzione ha supporto sufficiente e l'altra è completamente
/// assente (spazio negativo), emette un invariante.
///
/// Scelta conservativa deliberata: emettiamo una regola **solo** quando la
/// direzione proibita ha *zero* archi osservati. Bastasse un solo arco contrario,
/// non sarebbe più "negativo puro" e il rischio di falso positivo crescerebbe.
/// Preferiamo poche regole solide a molte regole rumorose. La selezione del
/// negativo puro è delegata a [`pure_asymmetries`]; qui si applica solo la soglia.
pub fn mine_layering_rules(
    relations: &[Relation],
    entity_layer: &HashMap<EntityId, LayerKey>,
    config: &LayerConfig,
) -> Vec<LayeringRule> {
    let counts = count_cross_layer_edges(relations, entity_layer);

    let mut rules: Vec<LayeringRule> = pure_asymmetries(&counts)
        .into_iter()
        .filter(|(_, _, support)| *support >= config.min_support)
        .map(|(downstream, upstream, support)| LayeringRule {
            id: EntityId::new(),
            upstream,
            downstream,
            support,
            confidence: confidence_for(support),
            origin: RuleOrigin::Discovered,
        })
        .collect();

    // Output deterministico: prima le regole con più supporto, poi per nome.
    rules.sort_by(|x, y| {
        y.support
            .cmp(&x.support)
            .then_with(|| x.downstream.cmp(&y.downstream))
            .then_with(|| x.upstream.cmp(&y.upstream))
    });
    rules
}

/// **Lo stadio 1 del flusso.** Le asimmetrie che si stanno *formando*: stesso
/// spazio negativo puro di una regola, ma con supporto nella banda
/// `[DEFAULT_MIN_CANDIDATE_SUPPORT, min_support)` — abbastanza ripetute da non
/// essere rumore, non abbastanza da essere invarianti. **Derivate, mai
/// persistite** (il ledger custodisce solo storia confermata): un candidato è un
/// *segnale*, non una verità. Condividendo [`pure_asymmetries`] con
/// [`mine_layering_rules`], un candidato e una regola sono lo stesso pattern, e la
/// promozione resta solo questione di supporto — non possono divergere.
pub fn mine_layering_candidates(
    relations: &[Relation],
    entity_layer: &HashMap<EntityId, LayerKey>,
    config: &LayerConfig,
) -> Vec<LayeringCandidate> {
    let counts = count_cross_layer_edges(relations, entity_layer);
    let band = DEFAULT_MIN_CANDIDATE_SUPPORT..config.min_support;

    let mut candidates: Vec<LayeringCandidate> = pure_asymmetries(&counts)
        .into_iter()
        .filter(|(_, _, support)| band.contains(support))
        .map(|(downstream, upstream, support)| LayeringCandidate {
            upstream,
            downstream,
            support,
            needed: config.min_support - support,
        })
        .collect();

    // Stesso ordine deterministico delle regole: più supporto prima, poi per nome.
    candidates.sort_by(|x, y| {
        y.support
            .cmp(&x.support)
            .then_with(|| x.downstream.cmp(&y.downstream))
            .then_with(|| x.upstream.cmp(&y.upstream))
    });
    candidates
}

/// Gli archi di **supporto** di una regola: le dipendenze `downstream → upstream`
/// realmente osservate che la giustificano. Parallelo di [`boundary_entities`]
/// (che ne restituisce gli estremi), ma espone gli **archi** interi: servono al
/// Memory Engine per citarli come evidenza strutturata (`Evidence::Edge`) quando
/// promuove l'invariante a `Decision`, senza una sola query in più. Stesso filtro
/// di [`cross_layer`]; output deterministico (ordinato per id dell'arco).
pub fn support_edges<'a>(
    rule: &LayeringRule,
    relations: &'a [Relation],
    entity_layer: &HashMap<EntityId, LayerKey>,
) -> Vec<&'a Relation> {
    let mut out: Vec<&Relation> = relations
        .iter()
        .filter(|rel| {
            matches!(
                cross_layer(rel, entity_layer),
                Some((s, t)) if s == &rule.downstream && t == &rule.upstream
            )
        })
        .collect();
    out.sort_by_key(|rel| rel.id.0);
    out
}

/// Le entità che **attraversano il confine** governato da una regola: gli estremi
/// degli archi `downstream → upstream` realmente osservati. Sono le entità più
/// rilevanti per "spiegare" l'invariante: agganciandole a una `Decision`, il Query
/// Engine ritroverà il *perché* ogni volta che il sottografo rilevante le tocca.
/// Output deterministico (ordinato per id).
pub fn boundary_entities(
    rule: &LayeringRule,
    relations: &[Relation],
    entity_layer: &HashMap<EntityId, LayerKey>,
) -> Vec<EntityId> {
    let mut set: HashSet<EntityId> = HashSet::new();
    for rel in support_edges(rule, relations, entity_layer) {
        set.insert(rel.source_id);
        set.insert(rel.target_id);
    }
    let mut out: Vec<EntityId> = set.into_iter().collect();
    out.sort_by_key(|id| id.0);
    out
}

/// **L'anticorpo.** Date delle relazioni *candidate* (ipotetiche o appena
/// aggiunte) e le regole scoperte, restituisce le violazioni: ogni candidata che
/// va nel verso proibito `upstream → downstream`.
pub fn violations_for(
    candidates: &[Relation],
    entity_layer: &HashMap<EntityId, LayerKey>,
    rules: &[LayeringRule],
) -> Vec<ArchitectureViolation> {
    let mut out = Vec::new();
    for rel in candidates {
        let Some((s, t)) = cross_layer(rel, entity_layer) else {
            continue;
        };
        // Violazione se esiste una regola in cui `s` è l'upstream e `t` il
        // downstream: la candidata `s → t` inverte la freccia stabilita.
        if let Some(rule) = rules
            .iter()
            .find(|r| &r.upstream == s && &r.downstream == t)
        {
            out.push(ArchitectureViolation {
                rule_id: rule.id,
                relation_id: rel.id,
                source_id: rel.source_id,
                target_id: rel.target_id,
                // La posizione è ignota a questa funzione pura (non vede le
                // entità): la riempie [`crate::guardian::Guardian::check`], che ha
                // accesso allo storage. Qui resta `None`.
                location: None,
                severity: Severity::for_violation(),
                message: format!(
                    "Violazione di layering: '{s}' → '{t}'. Nel grafo '{downstream}' dipende da \
                     '{upstream}' ({support} archi, confidenza {confidence:.2}), mai il contrario: \
                     questa dipendenza inverte la direzione architetturale stabilita.",
                    s = s,
                    t = t,
                    downstream = rule.downstream,
                    upstream = rule.upstream,
                    support = rule.support,
                    confidence = rule.confidence,
                ),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn rel(kind: RelationKind, source: EntityId, target: EntityId) -> Relation {
        Relation {
            id: EntityId::new(),
            kind,
            source_id: source,
            target_id: target,
            metadata: HashMap::new(),
        }
    }

    /// Come `rel`, ma con una confidenza di risoluzione esplicita nei metadata.
    fn rel_conf(
        kind: RelationKind,
        source: EntityId,
        target: EntityId,
        confidence: &str,
    ) -> Relation {
        let mut r = rel(kind, source, target);
        r.metadata
            .insert("resolution_confidence".to_string(), confidence.to_string());
        r
    }

    /// Costruisce N entità di un dato layer e ritorna (id, mappa id→layer).
    fn layered(n: usize, layer: &str, map: &mut HashMap<EntityId, LayerKey>) -> Vec<EntityId> {
        (0..n)
            .map(|_| {
                let id = EntityId::new();
                map.insert(id, LayerKey(layer.to_string()));
                id
            })
            .collect()
    }

    #[test]
    fn layer_of_takes_the_first_components() {
        assert_eq!(
            layer_of("app::api::handlers::UserHandler::create", 2),
            LayerKey("app::api".to_string())
        );
        // qualified_name più corto della profondità: tiene tutto, senza panico.
        assert_eq!(layer_of("app", 2), LayerKey("app".to_string()));
        // profondità 0 degrada ad almeno 1 componente.
        assert_eq!(layer_of("app::api", 0), LayerKey("app".to_string()));
    }

    fn entity_with(qname: &str, package: Option<&str>) -> Entity {
        let mut metadata = HashMap::new();
        if let Some(p) = package {
            metadata.insert("package".to_string(), p.to_string());
        }
        Entity {
            id: EntityId::new(),
            kind: codeos_types::EntityKind::Module,
            qualified_name: qname.to_string(),
            location: codeos_types::SourceLocation {
                file_path: "f".to_string(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            metadata,
        }
    }

    #[test]
    fn layer_of_entity_prefers_the_real_package_boundary() {
        // Monorepo con annidamento non uniforme: a profondità 2 due pacchetti
        // distinti collasserebbero entrambi nel layer "packages::group".
        let deep = entity_with("packages::group::alpha::src::lib::f", Some("alpha"));
        assert_eq!(
            layer_of_entity(&deep, DEFAULT_LAYER_DEPTH),
            LayerKey("alpha".to_string()),
            "il package del manifest vince sull'euristica di profondità"
        );

        // Senza metadato `package` si ricade sull'euristica: nessuna regressione.
        let no_pkg = entity_with("packages::group::beta::src::lib::f", None);
        assert_eq!(
            layer_of_entity(&no_pkg, DEFAULT_LAYER_DEPTH),
            LayerKey("packages::group".to_string())
        );

        // Un `package` vuoto è trattato come assente (degrada all'euristica).
        let empty_pkg = entity_with("a::b::c", Some(""));
        assert_eq!(
            layer_of_entity(&empty_pkg, DEFAULT_LAYER_DEPTH),
            LayerKey("a::b".to_string())
        );
    }

    #[test]
    fn mines_a_one_way_dependency_as_a_rule() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);

        // Tre archi api → core, zero core → api: lo spazio negativo è puro.
        let relations = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
            rel(RelationKind::Imports, api[2], core[2]),
        ];

        let rules = mine_layering_rules(&relations, &map, &LayerConfig::default());
        assert_eq!(rules.len(), 1, "regole = {rules:?}");
        assert_eq!(rules[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("app::api".to_string()));
        assert_eq!(rules[0].support, 3);
    }

    #[test]
    fn no_rule_when_dependency_is_bidirectional() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);

        let mut relations = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
            rel(RelationKind::Calls, api[2], core[2]),
        ];
        // Anche un solo arco nel verso opposto smonta l'invariante.
        relations.push(rel(RelationKind::Calls, core[0], api[0]));

        let rules = mine_layering_rules(&relations, &map, &LayerConfig::default());
        assert!(rules.is_empty(), "regole inattese = {rules:?}");
    }

    #[test]
    fn no_rule_below_min_support() {
        let mut map = HashMap::new();
        let api = layered(2, "app::api", &mut map);
        let core = layered(2, "app::core", &mut map);

        let relations = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
        ];
        // Solo 2 archi, soglia di default 3: troppo poco per fidarsi.
        let rules = mine_layering_rules(&relations, &map, &LayerConfig::default());
        assert!(rules.is_empty());

        // Con soglia abbassata, la regola emerge.
        let cfg = LayerConfig {
            layer_depth: DEFAULT_LAYER_DEPTH,
            min_support: 2,
        };
        let rules = mine_layering_rules(&relations, &map, &cfg);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn flags_the_reversed_edge_as_a_violation() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);
        let relations = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
            rel(RelationKind::Calls, api[2], core[2]),
        ];
        let rules = mine_layering_rules(&relations, &map, &LayerConfig::default());

        // Candidata proibita: core → api (inverte la freccia).
        let bad = rel(RelationKind::Calls, core[0], api[0]);
        let violations = violations_for(std::slice::from_ref(&bad), &map, &rules);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].relation_id, bad.id);
        assert!(violations[0].message.contains("app::core"));
        assert!(violations[0].message.contains("app::api"));

        // Candidata lecita: api → core (direzione consentita) ⇒ nessuna violazione.
        let good = rel(RelationKind::Calls, api[0], core[0]);
        assert!(violations_for(&[good], &map, &rules).is_empty());
    }

    #[test]
    fn boundary_entities_are_the_crossing_endpoints() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);
        let relations = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
            rel(RelationKind::Calls, api[2], core[2]),
        ];
        let rules = mine_layering_rules(&relations, &map, &LayerConfig::default());

        let boundary = boundary_entities(&rules[0], &relations, &map);
        // Tutti e sei gli estremi degli archi che attraversano il confine.
        assert_eq!(boundary.len(), 6);
        assert!(boundary.contains(&api[0]));
        assert!(boundary.contains(&core[2]));
    }

    #[test]
    fn low_confidence_relations_do_not_form_rules() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);

        // Tre archi api → core, ma tutti risolti con confidenza `low`: il resolver
        // li ha agganciati con un'euristica fragile. Niente regola: una base
        // architetturale non può poggiare su archi che potrebbero mentire.
        let low = vec![
            rel_conf(RelationKind::Calls, api[0], core[0], "low"),
            rel_conf(RelationKind::Calls, api[1], core[1], "low"),
            rel_conf(RelationKind::Calls, api[2], core[2], "low"),
        ];
        assert!(
            mine_layering_rules(&low, &map, &LayerConfig::default()).is_empty(),
            "archi a bassa confidenza non devono produrre invarianti"
        );

        // Controprova: gli stessi archi con confidenza `high` formano la regola.
        let high = vec![
            rel_conf(RelationKind::Calls, api[0], core[0], "high"),
            rel_conf(RelationKind::Calls, api[1], core[1], "high"),
            rel_conf(RelationKind::Calls, api[2], core[2], "high"),
        ];
        let rules = mine_layering_rules(&high, &map, &LayerConfig::default());
        assert_eq!(rules.len(), 1, "archi affidabili devono formare la regola");

        // Una candidata `low` nel verso proibito non è nemmeno una violazione:
        // troppo incerta per accusare il codice.
        let bad_low = rel_conf(RelationKind::Calls, core[0], api[0], "low");
        assert!(violations_for(&[bad_low], &map, &rules).is_empty());
    }

    #[test]
    fn structural_and_unresolved_edges_are_ignored() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);
        let rules = mine_layering_rules(
            &[
                rel(RelationKind::Calls, api[0], core[0]),
                rel(RelationKind::Calls, api[1], core[1]),
                rel(RelationKind::Calls, api[2], core[2]),
            ],
            &map,
            &LayerConfig::default(),
        );

        // BelongsTo nel verso proibito NON è una violazione (è strutturale).
        let belongs = rel(RelationKind::BelongsTo, core[0], api[0]);
        assert!(violations_for(&[belongs], &map, &rules).is_empty());

        // Unresolved (target nullo) viene ignorato senza panico.
        let unresolved = rel(RelationKind::Unresolved, core[0], EntityId::nil());
        assert!(violations_for(&[unresolved], &map, &rules).is_empty());
    }

    /// Come `rel`, ma marcato come nato in un file di test (`source_kind=test`).
    fn rel_test(kind: RelationKind, source: EntityId, target: EntityId) -> Relation {
        let mut r = rel(kind, source, target);
        r.metadata
            .insert("source_kind".to_string(), "test".to_string());
        r
    }

    #[test]
    fn test_sourced_edges_do_not_form_rules() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);

        // Tre archi api → core, ma TUTTI nati in file di test: un test attraversa i
        // layer per mestiere, non descrive l'architettura di produzione. Nessuna
        // regola deve emergere dal loro spazio negativo.
        let from_tests = vec![
            rel_test(RelationKind::Calls, api[0], core[0]),
            rel_test(RelationKind::Calls, api[1], core[1]),
            rel_test(RelationKind::Imports, api[2], core[2]),
        ];
        assert!(
            mine_layering_rules(&from_tests, &map, &LayerConfig::default()).is_empty(),
            "gli archi dei test non devono fondare invarianti di layering"
        );
    }

    #[test]
    fn a_reverse_edge_from_a_test_does_not_break_the_invariant() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);

        // Tre archi di produzione api → core fondano l'invariante "core non dipende
        // da api".
        let mut relations = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
            rel(RelationKind::Imports, api[2], core[2]),
        ];
        // Un test che chiama "all'insù" (core ← test) NON deve smontare l'invariante:
        // è marcato source_kind=test e va ignorato dal mining. Senza l'esclusione,
        // questo singolo arco inverso azzererebbe la regola.
        relations.push(rel_test(RelationKind::Calls, core[0], api[0]));

        let rules = mine_layering_rules(&relations, &map, &LayerConfig::default());
        assert_eq!(
            rules.len(),
            1,
            "il solo arco inverso è di test: l'invariante di produzione regge ({rules:?})"
        );
        assert_eq!(rules[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("app::api".to_string()));
    }

    #[test]
    fn a_forming_asymmetry_surfaces_as_a_candidate() {
        let mut map = HashMap::new();
        let api = layered(2, "app::api", &mut map);
        let core = layered(2, "app::core", &mut map);

        // Due archi api → core, zero core → api: asimmetria pura ma sotto la soglia
        // (3). Non è ancora un invariante, ma non è nemmeno rumore: è un candidato.
        let relations = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
        ];

        let cfg = LayerConfig::default();
        assert!(
            mine_layering_rules(&relations, &map, &cfg).is_empty(),
            "due archi non bastano per un invariante"
        );

        let candidates = mine_layering_candidates(&relations, &map, &cfg);
        assert_eq!(candidates.len(), 1, "candidati = {candidates:?}");
        assert_eq!(candidates[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(candidates[0].downstream, LayerKey("app::api".to_string()));
        assert_eq!(candidates[0].support, 2);
        assert_eq!(
            candidates[0].needed, 1,
            "manca un solo arco alla promozione"
        );
    }

    #[test]
    fn a_candidate_graduates_to_a_rule_at_min_support() {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let core = layered(3, "app::core", &mut map);
        let cfg = LayerConfig::default();

        // Con due archi è un candidato e NON una regola.
        let forming = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
        ];
        assert_eq!(mine_layering_candidates(&forming, &map, &cfg).len(), 1);
        assert!(mine_layering_rules(&forming, &map, &cfg).is_empty());

        // Il terzo arco lo promuove: ora è una regola e NON più un candidato. Lo
        // stesso pattern attraversa la soglia, classificato solo dal supporto —
        // è la prova strutturale che candidato e regola non possono divergere.
        let mut mature = forming;
        mature.push(rel(RelationKind::Imports, api[2], core[2]));
        assert_eq!(mine_layering_rules(&mature, &map, &cfg).len(), 1);
        assert!(
            mine_layering_candidates(&mature, &map, &cfg).is_empty(),
            "a soglia raggiunta non è più un candidato: è diventato regola"
        );
    }

    #[test]
    fn a_single_edge_is_noise_not_a_candidate() {
        let mut map = HashMap::new();
        let api = layered(1, "app::api", &mut map);
        let core = layered(1, "app::core", &mut map);

        // Un solo arco in una direzione (e zero nell'altra) non è un confine che si
        // forma: è un caso. Sotto DEFAULT_MIN_CANDIDATE_SUPPORT, niente candidato.
        let relations = vec![rel(RelationKind::Calls, api[0], core[0])];
        assert!(
            mine_layering_candidates(&relations, &map, &LayerConfig::default()).is_empty(),
            "un singolo arco è rumore, non un candidato"
        );
    }

    #[test]
    fn a_bidirectional_pair_yields_no_candidate() {
        let mut map = HashMap::new();
        let api = layered(2, "app::api", &mut map);
        let core = layered(2, "app::core", &mut map);

        // Due archi api → core ma anche uno core → api: l'asimmetria non è pura, e un
        // candidato resta comunque spazio negativo puro. Niente candidato.
        let relations = vec![
            rel(RelationKind::Calls, api[0], core[0]),
            rel(RelationKind::Calls, api[1], core[1]),
            rel(RelationKind::Calls, core[0], api[0]),
        ];
        assert!(
            mine_layering_candidates(&relations, &map, &LayerConfig::default()).is_empty(),
            "senza spazio negativo puro non c'è candidato"
        );
    }

    #[test]
    fn low_confidence_and_test_edges_do_not_form_candidates() {
        let mut map = HashMap::new();
        let api = layered(2, "app::api", &mut map);
        let core = layered(2, "app::core", &mut map);

        // Stesso filtro a monte delle regole (entrambe passano da `cross_layer`): un
        // candidato non può nascere da archi che potrebbero mentire (low) o che
        // attraversano i layer per mestiere (test).
        let low = vec![
            rel_conf(RelationKind::Calls, api[0], core[0], "low"),
            rel_conf(RelationKind::Calls, api[1], core[1], "low"),
        ];
        assert!(mine_layering_candidates(&low, &map, &LayerConfig::default()).is_empty());

        let from_tests = vec![
            rel_test(RelationKind::Calls, api[0], core[0]),
            rel_test(RelationKind::Calls, api[1], core[1]),
        ];
        assert!(mine_layering_candidates(&from_tests, &map, &LayerConfig::default()).is_empty());
    }
}
