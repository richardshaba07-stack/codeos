//! **Spazio negativo del secondo ordine**: l'invariante che *manca dove dovrebbe
//! esserci*.
//!
//! # L'idea (il quarto asse: il meta)
//!
//! Lo spazio negativo del *primo* ordine vive nel grafo entità × dipendenze: "quale
//! arco non esiste mai?" → gli invarianti di layering. Questo modulo punta lo
//! **stesso algoritmo un livello più in alto**, sulla griglia *layer × layer*, dove
//! i "nodi" sono i layer e gli "archi" sono gli invarianti scoperti.
//!
//! > Un layer `U` è una **fondazione** quando molti altri layer lo trattano come
//! > una base da cui dipendono a senso unico (esiste l'invariante "`U` protegge da
//! > `D`" per tanti `D`). Se in questo coro di rispetto **un** layer `X` invece si
//! > accoppia a `U` in **entrambi** i versi, l'invariante che ci si aspetterebbe
//! > tra `U` e `X` **manca**. Quel buco — un'assenza nel positivo, non nel grafo —
//! > è lo spazio negativo del secondo ordine: la singola eccezione a una
//! > convenzione altrimenti universale.
//!
//! Nessun tool segnala "ti manca un invariante qui": tutti sanno dirti solo cosa
//! c'è. Noi leggiamo l'assenza di una regola dove il resto dell'architettura
//! griderebbe che dovrebbe esserci — il candidato perfetto da far rivedere a un
//! umano (o a un LLM) perché è quasi sempre o un debito tecnico o un bug latente.

use std::collections::{HashMap, HashSet};

use codeos_types::{EntityId, Relation};

use crate::invariant::{count_cross_layer_edges, LayerKey, LayeringRule};

/// Quanti *altri* layer devono rispettare un upstream come fondazione a senso
/// unico perché la convenzione sia considerata stabilita — e quindi un buco in
/// essa significativo. Al livello meta la popolazione sono i layer (pochi), perciò
/// la soglia è più bassa di quella del primo ordine sugli archi.
pub const DEFAULT_FOUNDATION_MIN_SUPPORT: u32 = 2;

/// Configurazione del miner del secondo ordine.
#[derive(Debug, Clone)]
pub struct MetaConfig {
    /// Vedi [`DEFAULT_FOUNDATION_MIN_SUPPORT`].
    pub foundation_min_support: u32,
}

impl Default for MetaConfig {
    fn default() -> Self {
        Self {
            foundation_min_support: DEFAULT_FOUNDATION_MIN_SUPPORT,
        }
    }
}

/// Una **lacuna del secondo ordine**: un invariante che manca dove la convenzione
/// architetturale dice che dovrebbe esserci.
///
/// `upstream` è una fondazione rispettata a senso unico da `foundation_support`
/// altri layer; `downstream` è l'eccezione che invece vi si accoppia in entrambi i
/// versi, lasciando scoperto l'invariante atteso.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingInvariant {
    /// Il layer-fondazione su cui la convenzione a senso unico è attesa.
    pub upstream: LayerKey,
    /// Il layer anomalo, accoppiato bidirezionalmente alla fondazione.
    pub downstream: LayerKey,
    /// Quanti *altri* layer rispettano `upstream` come fondazione a senso unico.
    /// Più alto ⇒ più isolata (e più sospetta) è l'eccezione.
    pub foundation_support: u32,
}

/// **Il miner del secondo ordine.** Per ogni fondazione (un upstream rispettato a
/// senso unico da almeno `foundation_min_support` layer), segnala i layer che
/// invece vi si accoppiano **bidirezionalmente**: lì l'invariante atteso manca.
///
/// Funzione pura, deterministica (ordinata per `foundation_support` decrescente,
/// poi per nome). Riusa il conteggio degli archi cross-layer del primo ordine, così
/// "interagiscono" significa esattamente ciò che significa per il miner di base.
pub fn mine_missing_invariants(
    relations: &[Relation],
    entity_layer: &HashMap<EntityId, LayerKey>,
    rules: &[LayeringRule],
    config: &MetaConfig,
) -> Vec<MissingInvariant> {
    let edges = count_cross_layer_edges(relations, entity_layer);

    // Profilo di ogni fondazione: l'insieme dei downstream che la rispettano a
    // senso unico (la riga "positiva" della griglia layer × layer).
    let mut protectors: HashMap<&LayerKey, HashSet<&LayerKey>> = HashMap::new();
    for rule in rules {
        protectors
            .entry(&rule.upstream)
            .or_default()
            .insert(&rule.downstream);
    }

    let mut out = Vec::new();
    for (upstream, respected_by) in &protectors {
        let support = respected_by.len() as u32;
        if support < config.foundation_min_support {
            continue; // non è (ancora) una convenzione stabilita: nessun buco da leggere.
        }
        // I candidati anomali: i layer con un arco verso la fondazione...
        for (source, target) in edges.keys() {
            if target != *upstream {
                continue;
            }
            // ...che però hanno anche l'arco di ritorno (accoppiamento bidirezionale)...
            if !edges.contains_key(&((*upstream).clone(), source.clone())) {
                continue;
            }
            // ...e che NON rispettano già la convenzione a senso unico.
            if respected_by.contains(source) {
                continue;
            }
            out.push(MissingInvariant {
                upstream: (*upstream).clone(),
                downstream: source.clone(),
                foundation_support: support,
            });
        }
    }

    out.sort_by(|a, b| {
        b.foundation_support
            .cmp(&a.foundation_support)
            .then_with(|| a.upstream.cmp(&b.upstream))
            .then_with(|| a.downstream.cmp(&b.downstream))
    });
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_types::RelationKind;

    fn rel(kind: RelationKind, source: EntityId, target: EntityId) -> Relation {
        Relation {
            id: EntityId::new(),
            kind,
            source_id: source,
            target_id: target,
            metadata: HashMap::new(),
        }
    }

    fn layered(n: usize, layer: &str, map: &mut HashMap<EntityId, LayerKey>) -> Vec<EntityId> {
        (0..n)
            .map(|_| {
                let id = EntityId::new();
                map.insert(id, LayerKey(layer.to_string()));
                id
            })
            .collect()
    }

    /// Costruisce N archi `Calls` da `from` a `to` (uno per coppia di indici).
    fn calls(from: &[EntityId], to: &[EntityId]) -> Vec<Relation> {
        from.iter()
            .zip(to.iter())
            .map(|(s, t)| rel(RelationKind::Calls, *s, *t))
            .collect()
    }

    /// Scenario: `core` è una fondazione rispettata da `api` e `web` (a senso
    /// unico), ma `jobs` vi si accoppia in entrambi i versi. Il buco è (core, jobs).
    fn foundation_with_one_bidirectional_outlier() -> (Vec<Relation>, HashMap<EntityId, LayerKey>) {
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let web = layered(3, "app::web", &mut map);
        let jobs = layered(3, "app::jobs", &mut map);
        let core = layered(3, "app::core", &mut map);

        let mut relations = Vec::new();
        relations.extend(calls(&api, &core)); // api → core (senso unico)
        relations.extend(calls(&web, &core)); // web → core (senso unico)
        relations.extend(calls(&jobs, &core)); // jobs → core ...
        relations.push(rel(RelationKind::Calls, core[0], jobs[0])); // ...e core → jobs (ritorno!)
        (relations, map)
    }

    #[test]
    fn flags_the_single_bidirectional_outlier_against_a_foundation() {
        let (relations, map) = foundation_with_one_bidirectional_outlier();
        let rules = crate::invariant::mine_layering_rules(
            &relations,
            &map,
            &crate::invariant::LayerConfig::default(),
        );
        // core è protetto a senso unico da api e web (2): è una fondazione.
        // jobs è bidirezionale ⇒ niente regola (core, jobs).
        let gaps = mine_missing_invariants(&relations, &map, &rules, &MetaConfig::default());
        assert_eq!(gaps.len(), 1, "gaps = {gaps:?}");
        assert_eq!(gaps[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(gaps[0].downstream, LayerKey("app::jobs".to_string()));
        assert_eq!(gaps[0].foundation_support, 2);
    }

    #[test]
    fn no_gap_when_the_foundation_is_not_established() {
        // Solo api rispetta core a senso unico (support 1 < soglia 2): core non è
        // ancora una convenzione, quindi l'accoppiamento bidirezionale di jobs non è
        // un'anomalia degna di nota.
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let jobs = layered(3, "app::jobs", &mut map);
        let core = layered(3, "app::core", &mut map);

        let mut relations = Vec::new();
        relations.extend(calls(&api, &core));
        relations.extend(calls(&jobs, &core));
        relations.push(rel(RelationKind::Calls, core[0], jobs[0]));

        let rules = crate::invariant::mine_layering_rules(
            &relations,
            &map,
            &crate::invariant::LayerConfig::default(),
        );
        let gaps = mine_missing_invariants(&relations, &map, &rules, &MetaConfig::default());
        assert!(gaps.is_empty(), "gaps inattesi = {gaps:?}");
    }

    #[test]
    fn no_gap_when_every_layer_respects_the_convention() {
        // api, web, jobs rispettano tutti core a senso unico: nessun buco.
        let mut map = HashMap::new();
        let api = layered(3, "app::api", &mut map);
        let web = layered(3, "app::web", &mut map);
        let jobs = layered(3, "app::jobs", &mut map);
        let core = layered(3, "app::core", &mut map);

        let mut relations = Vec::new();
        relations.extend(calls(&api, &core));
        relations.extend(calls(&web, &core));
        relations.extend(calls(&jobs, &core));

        let rules = crate::invariant::mine_layering_rules(
            &relations,
            &map,
            &crate::invariant::LayerConfig::default(),
        );
        let gaps = mine_missing_invariants(&relations, &map, &rules, &MetaConfig::default());
        assert!(gaps.is_empty(), "gaps inattesi = {gaps:?}");
    }
}
