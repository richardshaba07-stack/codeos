//! `codeos-query` — Query Engine / Context Builder (la feature principale).
//!
//! Espone:
//! - [`QueryEngine`]: dato un testo in linguaggio naturale, costruisce il
//!   sottografo minimo rilevante e lo formatta come contesto per un LLM
//!   (algoritmo in 6 passi, briefing sez. 10).
//! - [`QueryActor`]: l'attore che gestisce i comandi `QueryGraph`.
//!
//! Legge il grafo tramite il trait `GraphStorage`: non conosce né il parser né
//! il resolver, solo i dati persistiti.

mod actor;
mod engine;

pub use actor::QueryActor;
pub use engine::{
    CallPath, Impact, PossibleCaller, QueryConfig, QueryEngine, TransitiveCaller, TransitiveImpact,
};
