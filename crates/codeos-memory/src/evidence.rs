//! Il tipo [`Evidence`]: una **citazione verificabile** a sostegno di una decisione.
//!
//! È il cuore della tesi anti-falso-positivo applicata alla memoria: una
//! [`Decision`](crate::Decision) generata dal sistema (o da un LLM) non può
//! affermare un *perché* a vuoto — ogni affermazione deve puntare a qualcosa che
//! il grafo o git **già provano**. `Evidence` enumera esattamente queste fonti
//! dimostrabili; una [`Proposal`](crate::Proposal) ne richiede almeno una.
//!
//! Non descrive *un'opinione*, descrive *un fatto già nel sistema*: l'LLM al più
//! lo riassume, non lo inventa.

use codeos_types::{EntityId, RelationKind};

/// Una fonte verificabile citata da una decisione. Ogni variante corrisponde a
/// un dato che esiste indipendentemente dal ragionamento: lo si può andare a
/// controllare nel grafo, nel ledger o nella storia git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// Un commit git (hash). Provato da: il repository lo contiene.
    Commit(String),
    /// Un arco risolto del grafo, nella stessa identità stabile
    /// `(source_qname, kind, target_qname)` con cui il grafo temporale lo data
    /// (vision step 2). Provato da: il grafo contiene quell'arco.
    Edge {
        source: String,
        kind: RelationKind,
        target: String,
    },
    /// Un'entità del grafo, per qualified name. Provato da: il grafo la contiene.
    Entity(String),
    /// Un test, per nome. Provato da: il test esiste (ed è verde).
    Test(String),
    /// Un'altra decisione del ledger. Provato da: il ledger la contiene.
    PriorDecision(EntityId),
    /// Un **documento** di provenienza: un ADR, una RFC, un design doc — per path
    /// relativo o URI. Provato da: il file esiste nel repository (o all'URI citato).
    /// È la fonte delle decisioni che un team ha già scritto come documento, non
    /// derivate dal grafo: il razionale vive lì, questo lo àncora e lo rende
    /// verificabile.
    Document(String),
}

impl std::fmt::Display for Evidence {
    /// Forma **concisa e leggibile** di una citazione: quella che il Query Engine
    /// porta nel contesto accanto al *perché*, così l'LLM vede la prova e non solo
    /// l'affermazione. È una vista di presentazione, distinta dalla serializzazione
    /// round-trip del Markdown store (che antepone tag con `: ` per riparsarsi).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Evidence::Commit(hash) => write!(f, "commit {hash}"),
            Evidence::Edge {
                source,
                kind,
                target,
            } => write!(f, "{source} --{}--> {target}", kind.as_str()),
            Evidence::Entity(qname) => write!(f, "entità {qname}"),
            Evidence::Test(name) => write!(f, "test {name}"),
            Evidence::PriorDecision(id) => write!(f, "decisione {id}"),
            Evidence::Document(src) => write!(f, "documento {src}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_renders_each_citation_concisely() {
        assert_eq!(
            Evidence::Commit("abc123".into()).to_string(),
            "commit abc123"
        );
        assert_eq!(
            Evidence::Edge {
                source: "app::api::run".into(),
                kind: RelationKind::Calls,
                target: "app::core::do_it".into(),
            }
            .to_string(),
            "app::api::run --Calls--> app::core::do_it"
        );
        assert_eq!(
            Evidence::Test("test_login".into()).to_string(),
            "test test_login"
        );
    }
}
