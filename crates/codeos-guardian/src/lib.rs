//! `codeos-guardian` — il **sistema immunitario** di CodeOS.
//!
//! Mentre gli strumenti tradizionali indicizzano ciò che il codice *è*, il
//! Guardian indicizza ciò che il codice *non deve smettere di essere*: gli
//! **invarianti architetturali** impliciti, mai scritti da nessuno, che tengono
//! coerente la codebase.
//!
//! La v0 scopre gli invarianti di **layering** dallo *spazio negativo* del grafo
//! (le dipendenze direzionali che esistono sempre in un verso e mai nell'altro) e
//! agisce da **anticorpo**: data una modifica — ipotetica o appena indicizzata —
//! segnala se inverte una freccia architetturale stabilita.
//!
//! - [`mine_layering_rules`]/[`violations_for`]: il cuore puro (nessun I/O).
//! - [`Guardian`]: il lato storage (scopre regole e rileva violazioni leggendo il grafo).
//! - [`GuardianActor`]: l'attore in ascolto su `GraphUpdated`, che pubblica
//!   `ArchitectureViolationDetected`.

mod actor;
mod declared;
mod guardian;
mod invariant;
pub mod license;
mod meta;

pub use actor::GuardianActor;
pub use declared::{declared_layering_rules, load_declared_rules, DeclaredRule};
pub use guardian::Guardian;
pub use invariant::{
    boundary_entities, layer_of, mine_layering_rules, violations_for, LayerConfig, LayerKey,
    LayeringRule, DEFAULT_LAYER_DEPTH, DEFAULT_MIN_SUPPORT,
};
pub use meta::{
    mine_missing_invariants, MetaConfig, MissingInvariant, DEFAULT_FOUNDATION_MIN_SUPPORT,
};
