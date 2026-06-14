//! Selezione delle decisioni UMANE pertinenti a un insieme di entità.
//!
//! È il **punto unico di verità** usato sia dal context pack di `codeos-query`
//! sia dal Guardian (`codeos-guardian`). Prima questa logica era DUPLICATA nei due
//! crate: una svista costosa (un fix applicato a una copia non aveva effetto
//! sull'altra — visto davvero il 2026-06-14). Unificandola, ogni futura modifica
//! al criterio di aggancio (es. il fix anti-flood sui segmenti di path) si fa in
//! un posto solo.

use std::collections::HashSet;

use codeos_types::{Entity, EntityId};

use crate::{Decision, DecisionKind};

/// Tiene le decisioni UMANE (`DecisionKind::Decision`) pertinenti alle entità
/// `selected`, troncando a `max`. Una decisione è pertinente se:
/// - un suo `related_entity_ids` è tra le entità selezionate (aggancio esplicito), OPPURE
/// - un suo tag coincide ESATTAMENTE con un segmento `::` del qualified_name di
///   un'entità selezionata (le decisioni taggate col nome di modulo/tipo/funzione,
///   come fa `decide --tags`/`--boundary`). Match per SEGMENTO, non sottostringa:
///   il tag "core" non aggancia "codeos-core".
///
/// Gli invarianti AUTO-derivati (`ArchitectureRule`) restano FUORI: sono già nel
/// `report` e nelle BOUNDARIES del pack; qui sarebbero un doppione che costa
/// budget all'agente (IL METRO ne ha misurato il costo in localizzazione).
///
/// NB limitazione nota (documentata in eval/moat-benchmark/scaled/RETRIEVAL_PRECISION.md):
/// un tag che coincide con un segmento di PATH comune (`src`, `tmp`, nome di
/// crate) aggancia ogni entità e "floda" il pack. È il rovescio della stessa
/// feature che fa funzionare i tag di modulo/layer; il fix corretto (escludere i
/// segmenti del prefisso-radice comune) vivrà QUI, in un posto solo.
pub fn select_human_decisions(
    decisions: impl IntoIterator<Item = Decision>,
    selected: &[Entity],
    max: usize,
) -> Vec<Decision> {
    let selected_ids: HashSet<EntityId> = selected.iter().map(|e| e.id).collect();
    let mut out: Vec<Decision> = decisions
        .into_iter()
        .filter(|d| d.kind == DecisionKind::Decision)
        .filter(|d| {
            d.related_entity_ids
                .iter()
                .any(|id| selected_ids.contains(id))
                || d.tags.iter().any(|t| {
                    !t.is_empty()
                        && selected
                            .iter()
                            .any(|e| e.qualified_name.split("::").any(|seg| seg == t))
                })
        })
        .collect();
    out.truncate(max);
    out
}
