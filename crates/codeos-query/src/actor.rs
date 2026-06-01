//! `QueryActor`: l'attore che risponde ai comandi `QueryGraph`.
//!
//! Riceve i comandi su un `mpsc` (instradati dal Dispatcher), delega al
//! [`QueryEngine`] e deposita la risposta sul `reply_to`. Comunica solo via
//! canali e via trait `GraphStorage` (invarianti 1.3 e 1.4).

use std::sync::Arc;

use codeos_memory::DecisionStore;
use codeos_storage::GraphStorage;
use codeos_types::bus::Command;
use tokio::sync::mpsc;

use crate::engine::QueryEngine;

/// L'attore del Query Engine.
pub struct QueryActor {
    engine: QueryEngine,
}

impl QueryActor {
    pub fn new(storage: Arc<dyn GraphStorage>) -> Self {
        Self {
            engine: QueryEngine::new(storage),
        }
    }

    /// Variante che aggancia il Memory Engine: le risposte includeranno il
    /// *perché* (le decisioni relative alle entità selezionate).
    pub fn with_decisions(
        storage: Arc<dyn GraphStorage>,
        decisions: Arc<dyn DecisionStore>,
    ) -> Self {
        Self {
            engine: QueryEngine::with_decisions(storage, decisions),
        }
    }

    /// Consuma i comandi finché il canale resta aperto.
    pub async fn run(self, mut commands: mpsc::Receiver<Command>) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::QueryGraph { query, reply_to } => {
                    let result = self.engine.query(&query).await;
                    if let Err(err) = &result {
                        tracing::warn!(error = %err, "QueryActor: query fallita");
                    }
                    let _ = reply_to.send(result).await;
                }
                other => {
                    tracing::warn!(?other, "query actor: comando non di sua competenza");
                }
            }
        }
        tracing::debug!("query actor: canale comandi chiuso, esco");
    }
}
