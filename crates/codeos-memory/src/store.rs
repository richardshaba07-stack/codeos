//! Il trait [`DecisionStore`] e un'implementazione in memoria.
//!
//! L'astrazione separa il Memory Engine dal *come* le decisioni sono persistite.
//! La v1 ha due implementazioni: [`InMemoryDecisionStore`] (effimera, per test e
//! run usa-e-getta) e [`crate::MarkdownDecisionStore`] (file Markdown ispezionabili
//! a mano — la forma canonica del briefing).

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use codeos_types::EntityId;

use crate::decision::Decision;

/// Persistenza delle [`Decision`].
#[async_trait]
pub trait DecisionStore: Send + Sync {
    /// Registra una decisione. Non sovrascrive le precedenti: la storia è additiva.
    async fn record(&self, decision: &Decision) -> anyhow::Result<()>;

    /// Tutte le decisioni note, in ordine deterministico (per timestamp poi id).
    async fn all(&self) -> anyhow::Result<Vec<Decision>>;

    /// Le decisioni agganciate ad **almeno una** delle entità indicate.
    ///
    /// È il gancio del Passo 3 del Query Engine: dato il sottografo rilevante per
    /// una query, recupera il *perché* ad esso associato. Implementazione di
    /// default sopra [`all`](DecisionStore::all); gli store che possono fare di
    /// meglio (indici) la sovrascrivono.
    async fn related_to(&self, entity_ids: &[EntityId]) -> anyhow::Result<Vec<Decision>> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: HashSet<EntityId> = entity_ids.iter().copied().collect();
        Ok(self
            .all()
            .await?
            .into_iter()
            .filter(|d| d.related_entity_ids.iter().any(|e| wanted.contains(e)))
            .collect())
    }
}

/// Store effimero in memoria. Persiste finché vive il processo.
#[derive(Default)]
pub struct InMemoryDecisionStore {
    decisions: Mutex<Vec<Decision>>,
}

impl InMemoryDecisionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl DecisionStore for InMemoryDecisionStore {
    async fn record(&self, decision: &Decision) -> anyhow::Result<()> {
        let mut guard = self.decisions.lock().expect("mutex decisioni avvelenato");
        guard.push(decision.clone());
        Ok(())
    }

    async fn all(&self) -> anyhow::Result<Vec<Decision>> {
        let guard = self.decisions.lock().expect("mutex decisioni avvelenato");
        let mut out = guard.clone();
        drop(guard);
        out.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::DecisionKind;

    fn decision(title: &str, related: Vec<EntityId>) -> Decision {
        Decision {
            id: EntityId::new(),
            kind: DecisionKind::Decision,
            author: "human:test".to_string(),
            title: title.to_string(),
            context: String::new(),
            rationale: String::new(),
            related_entity_ids: related,
            related_decision_ids: Vec::new(),
            tags: Vec::new(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[tokio::test]
    async fn records_and_lists() {
        let store = InMemoryDecisionStore::new();
        store.record(&decision("uno", vec![])).await.unwrap();
        store.record(&decision("due", vec![])).await.unwrap();
        assert_eq!(store.all().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn related_to_filters_by_entity() {
        let store = InMemoryDecisionStore::new();
        let target = EntityId::new();
        store
            .record(&decision("rilevante", vec![target]))
            .await
            .unwrap();
        store
            .record(&decision("irrilevante", vec![EntityId::new()]))
            .await
            .unwrap();

        let hits = store.related_to(&[target]).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "rilevante");

        // Lista di entità vuota ⇒ nessuna decisione (non "tutte").
        assert!(store.related_to(&[]).await.unwrap().is_empty());
    }
}
