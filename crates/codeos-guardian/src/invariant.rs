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

use codeos_types::bus::ArchitectureViolation;
use codeos_types::{EntityId, Relation, RelationKind};

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

/// La coppia (layer sorgente, layer destinazione) di una relazione, se entrambi
/// gli estremi sono noti, la relazione è una dipendenza e attraversa due layer
/// diversi. Altrimenti `None` (non contribuisce al ragionamento sui layer).
fn cross_layer<'a>(
    rel: &Relation,
    entity_layer: &'a HashMap<EntityId, LayerKey>,
) -> Option<(&'a LayerKey, &'a LayerKey)> {
    if !is_dependency_kind(rel.kind) || rel.target_id.is_nil() {
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

/// **Il miner.** Scopre le regole di layering dalle relazioni: per ogni coppia di
/// layer, se una direzione ha supporto sufficiente e l'altra è completamente
/// assente (spazio negativo), emette un invariante.
///
/// Scelta conservativa deliberata: emettiamo una regola **solo** quando la
/// direzione proibita ha *zero* archi osservati. Bastasse un solo arco contrario,
/// non sarebbe più "negativo puro" e il rischio di falso positivo crescerebbe.
/// Preferiamo poche regole solide a molte regole rumorose.
pub fn mine_layering_rules(
    relations: &[Relation],
    entity_layer: &HashMap<EntityId, LayerKey>,
    config: &LayerConfig,
) -> Vec<LayeringRule> {
    let counts = count_cross_layer_edges(relations, entity_layer);

    let mut rules = Vec::new();
    let mut handled: HashSet<(LayerKey, LayerKey)> = HashSet::new();

    for ((a, b), &ab) in &counts {
        // Tratta la coppia non ordinata {a, b} una sola volta.
        if handled.contains(&(a.clone(), b.clone())) || handled.contains(&(b.clone(), a.clone())) {
            continue;
        }
        handled.insert((a.clone(), b.clone()));

        let ba = counts.get(&(b.clone(), a.clone())).copied().unwrap_or(0);

        // La direzione con supporto è quella "vera"; l'altra dev'essere assente.
        if ab >= config.min_support && ba == 0 {
            // archi a → b ⇒ a è downstream (dipende), b è upstream (base).
            rules.push(LayeringRule {
                id: EntityId::new(),
                upstream: b.clone(),
                downstream: a.clone(),
                support: ab,
                confidence: confidence_for(ab),
            });
        } else if ba >= config.min_support && ab == 0 {
            rules.push(LayeringRule {
                id: EntityId::new(),
                upstream: a.clone(),
                downstream: b.clone(),
                support: ba,
                confidence: confidence_for(ba),
            });
        }
    }

    // Output deterministico: prima le regole con più supporto, poi per nome.
    rules.sort_by(|x, y| {
        y.support
            .cmp(&x.support)
            .then_with(|| x.downstream.cmp(&y.downstream))
            .then_with(|| x.upstream.cmp(&y.upstream))
    });
    rules
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
    for rel in relations {
        if let Some((s, t)) = cross_layer(rel, entity_layer) {
            if s == &rule.downstream && t == &rule.upstream {
                set.insert(rel.source_id);
                set.insert(rel.target_id);
            }
        }
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
}
