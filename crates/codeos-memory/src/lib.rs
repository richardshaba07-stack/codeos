//! `codeos-memory` — la memoria storica persistente di CodeOS.
//!
//! Risponde alla domanda *"perché è scritto così?"*: registra le [`Decision`]
//! (scelte puntuali e regole architetturali) e le aggancia alle entità del grafo,
//! così che il *perché* sopravviva al turnover di persone e di sessioni LLM.
//!
//! - [`Decision`]/[`DecisionKind`]: la memoria, con identità e timestamp.
//! - [`DecisionStore`]: l'astrazione di persistenza. [`InMemoryDecisionStore`]
//!   (effimera) e [`MarkdownDecisionStore`] (file `.md` ispezionabili a mano).
//! - [`MemoryActor`]: l'attore che gestisce `RecordDecision` (sostituisce lo stub
//!   che viveva in `codeos-core`).

mod actor;
mod decision;
mod evidence;
mod markdown;
mod proposal;
mod selection;
mod store;

pub use actor::MemoryActor;
pub use decision::{Decision, DecisionKind, DecisionStatus};
pub use evidence::Evidence;
pub use markdown::MarkdownDecisionStore;
pub use proposal::Proposal;
pub use selection::select_human_decisions;
pub use store::{DecisionStore, InMemoryDecisionStore};
