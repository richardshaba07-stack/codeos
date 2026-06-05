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
}
