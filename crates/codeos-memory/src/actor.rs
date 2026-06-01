//! [`MemoryActor`]: l'attore che custodisce la memoria storica.
//!
//! Riceve i [`Command::RecordDecision`] instradati dal Dispatcher, costruisce una
//! [`Decision`] completa (id + timestamp), la persiste tramite un
//! [`DecisionStore`] e risponde con l'`EntityId` assegnato. Sostituisce lo stub
//! `memory_actor` che viveva in `codeos-core`.
//!
//! Non conosce gli altri attori (invariante 1.3): comunica solo via canali e via
//! il trait di storage.

use std::sync::Arc;

use codeos_types::bus::Command;
use codeos_types::EntityId;
use tokio::sync::mpsc;

use crate::decision::{Decision, DecisionKind};
use crate::store::DecisionStore;

/// L'attore della memoria.
pub struct MemoryActor {
    store: Arc<dyn DecisionStore>,
}

impl MemoryActor {
    pub fn new(store: Arc<dyn DecisionStore>) -> Self {
        Self { store }
    }

    /// Consuma i comandi finché il canale resta aperto.
    pub async fn run(self, mut commands: mpsc::Receiver<Command>) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::RecordDecision { decision, reply_to } => {
                    let result = self
                        .record(Decision::from_new(decision, DecisionKind::Decision))
                        .await;
                    if let Err(err) = &result {
                        tracing::error!(error = %err, "MemoryActor: registrazione della decisione fallita");
                    }
                    // Se il chiamante ha già rinunciato all'attesa, ignoriamo l'errore di send.
                    let _ = reply_to.send(result).await;
                }
                // Il Dispatcher non dovrebbe instradarci altro: lo segnaliamo senza panic.
                other => {
                    tracing::warn!(?other, "MemoryActor: comando inatteso, ignorato");
                }
            }
        }
        tracing::debug!("MemoryActor: canale comandi chiuso, esco");
    }

    async fn record(&self, decision: Decision) -> anyhow::Result<EntityId> {
        let id = decision.id;
        self.store.record(&decision).await?;
        tracing::info!(title = %decision.title, %id, "decisione registrata");
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_types::bus::NewDecision;

    use crate::store::InMemoryDecisionStore;

    #[tokio::test]
    async fn records_a_decision_and_replies_with_its_id() {
        let store = Arc::new(InMemoryDecisionStore::new());
        let actor = MemoryActor::new(store.clone());
        let (commands_tx, commands_rx) = mpsc::channel(4);
        tokio::spawn(actor.run(commands_rx));

        let (reply_to, mut reply_rx) = mpsc::channel(1);
        commands_tx
            .send(Command::RecordDecision {
                decision: NewDecision {
                    author: "human:test".to_string(),
                    title: "Scelta di prova".to_string(),
                    context: "contesto".to_string(),
                    rationale: "razionale".to_string(),
                    related_entity_ids: vec![EntityId::new()],
                    related_decision_ids: Vec::new(),
                    tags: vec!["test".to_string()],
                },
                reply_to,
            })
            .await
            .expect("canale comandi chiuso");

        let id = reply_rx
            .recv()
            .await
            .expect("nessuna risposta dal MemoryActor")
            .expect("registrazione fallita");

        // La decisione è davvero finita nello store, con l'id restituito.
        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        assert_eq!(all[0].title, "Scelta di prova");
    }
}
