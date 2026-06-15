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

/// Segmenti **strutturali** di path/build che non sono MAI un'ancora di intento:
/// compaiono nel qualified_name di (quasi) ogni entità perché sono cartelle di
/// layout sorgente, output di build o temporanei — non nomi di dominio. Un tag che
/// coincide con uno di questi aggancerebbe ogni entità e *inonderebbe* il pack (il
/// falso positivo misurato in il test interno di precisione del retrieval con un tag `src`). Escluderli dal
/// match-per-segmento toglie quel flood senza scalfire i tag di modulo/layer veri
/// (`api`, `core`, `billing`…), che qui NON compaiono. Lista volutamente stretta e
/// universale: solo parole strutturali, mai un dominio che un umano taggherebbe.
const STRUCTURAL_SEGMENTS: &[&str] = &[
    "src",
    "lib",
    "libs",
    "test",
    "tests",
    "spec",
    "specs",
    "target",
    "build",
    "dist",
    "out",
    "bin",
    "obj",
    "node_modules",
    "vendor",
    "tmp",
    "temp",
    "__pycache__",
    // Nomi-FILE non semantici (un tag derivato da `mod.rs`/`index.ts`/`__init__.py`
    // non identifica un dominio): esclusi qui così valgono sia per il filtro anti-flood
    // sia per la derivazione dei tag dai path (un punto solo di verità).
    "mod",
    "index",
    "__init__",
];

/// `true` se `tag` è un segmento strutturale (confronto case-insensitive), quindi
/// inadatto come ancora di intento. Nel dubbio si esclude: meglio una decisione che
/// non emerge (la ritrovi con `why`) che una che inonda ogni pack — un arco mancante
/// è meglio di uno che mente.
pub fn is_structural_segment(tag: &str) -> bool {
    let t = tag.to_ascii_lowercase();
    STRUCTURAL_SEGMENTS.contains(&t.as_str())
}

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
/// Anti-flood (fix del falso positivo misurato in
/// un test interno di precisione del retrieval): un tag che coincide con un
/// segmento STRUTTURALE di path/build (`src`, `tmp`, `tests`…) aggancerebbe ogni
/// entità e inonderebbe il pack. Questi tag sono filtrati ([`is_structural_segment`]),
/// così resta solo l'aggancio per nome di modulo/layer reale. Residuo onesto: un tag
/// che coincide col NOME del repository (rarissimo, e palese errore d'uso) non è in
/// lista — servirebbe il prefisso-radice globale del grafo, un costo non giustificato
/// per quel caso.
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
                        && !is_structural_segment(t)
                        && selected
                            .iter()
                            .any(|e| e.qualified_name.split("::").any(|seg| seg == t))
                })
        })
        .collect();
    out.truncate(max);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_types::{EntityKind, SourceLocation};

    fn entity(qn: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            qualified_name: qn.to_string(),
            location: SourceLocation {
                file_path: String::new(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            metadata: std::collections::HashMap::new(),
        }
    }

    fn tagged(title: &str, tags: &[&str]) -> Decision {
        Decision {
            id: EntityId::new(),
            kind: DecisionKind::Decision,
            author: "human:test".into(),
            title: title.into(),
            context: String::new(),
            rationale: "perché".into(),
            related_entity_ids: vec![],
            related_decision_ids: vec![],
            supersedes: vec![],
            deprecates: vec![],
            evidence: vec![],
            tags: tags.iter().map(|t| t.to_string()).collect(),
            timestamp: "2026-06-15T00:00:00+00:00".into(),
        }
    }

    #[test]
    fn a_real_module_tag_anchors_the_decision() {
        // Un tag di dominio (`billing`) combacia col suo segmento → la decisione emerge.
        let selected = vec![entity("private::tmp::repo::src::billing::charge_card")];
        let out = select_human_decisions(vec![tagged("addebito", &["billing"])], &selected, 10);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_layer_tag_still_anchors_like_before() {
        // Non-regressione del comportamento legittimo (api/core): il tag di layer regge.
        let selected = vec![entity("app::api::handler_0::run")];
        let out =
            select_human_decisions(vec![tagged("api non tocca il db", &["api"])], &selected, 10);
        assert_eq!(out.len(), 1, "il tag di layer deve ancora agganciare");
    }

    #[test]
    fn a_structural_path_tag_no_longer_floods() {
        // IL FIX: `src` è un segmento di OGNI entità ma è strutturale → NON aggancia.
        // Era il falso positivo misurato in il test interno di precisione del retrieval.
        let selected = vec![
            entity("private::tmp::repo::src::billing::charge_card"),
            entity("private::tmp::repo::src::auth::login"),
        ];
        let out = select_human_decisions(vec![tagged("nota generica", &["src"])], &selected, 10);
        assert!(
            out.is_empty(),
            "un tag strutturale non deve inondare il pack"
        );
    }

    #[test]
    fn structural_filter_is_case_insensitive() {
        let selected = vec![entity("x::SRC::y")];
        let out = select_human_decisions(vec![tagged("t", &["SRC"])], &selected, 10);
        assert!(
            out.is_empty(),
            "anche `SRC` è strutturale (case-insensitive)"
        );
    }

    #[test]
    fn explicit_anchor_survives_even_a_structural_tag() {
        // related_entity_ids resta la via maestra: l'aggancio esplicito per ID porta
        // la decisione anche se il (solo) tag è strutturale.
        let e = entity("private::tmp::repo::src::billing::charge_card");
        let mut d = tagged("ancorata per id", &["src"]);
        d.related_entity_ids = vec![e.id];
        let out = select_human_decisions(vec![d], &[e], 10);
        assert_eq!(
            out.len(),
            1,
            "l'aggancio esplicito non passa dal filtro tag"
        );
    }
}
