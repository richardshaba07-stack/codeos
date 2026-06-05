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

use crate::decision::{Decision, DecisionStatus};

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

    /// Il ledger completo: ogni decisione con il suo [`DecisionStatus`] derivato.
    ///
    /// È la vista d'ispezione del briefing: dice cosa è ancora valido e cosa è
    /// stato rimpiazzato *senza nascondere la storia* (nessuna decisione sparisce
    /// dal log, cambia solo l'etichetta derivata). Default sopra
    /// [`all`](DecisionStore::all).
    async fn ledger(&self) -> anyhow::Result<Vec<(Decision, DecisionStatus)>> {
        let all = self.all().await?;
        let superseded = collect_ids(&all, |d| &d.supersedes);
        let deprecated = collect_ids(&all, |d| &d.deprecates);
        Ok(all
            .into_iter()
            .map(|d| {
                // Precedenza: Superseded > Deprecated > Accepted.
                let status = if superseded.contains(&d.id) {
                    DecisionStatus::Superseded
                } else if deprecated.contains(&d.id) {
                    DecisionStatus::Deprecated
                } else {
                    DecisionStatus::Accepted
                };
                (d, status)
            })
            .collect())
    }

    /// Le decisioni **correnti**: il ledger filtrato a [`DecisionStatus::Accepted`].
    ///
    /// È la proiezione "verità di oggi" del log additivo — quella che il Query
    /// Engine porta nel contesto come *perché* ancora valido, senza trascinarsi
    /// dietro le scelte già rimpiazzate.
    async fn current_decisions(&self) -> anyhow::Result<Vec<Decision>> {
        Ok(self
            .ledger()
            .await?
            .into_iter()
            .filter(|(_, status)| *status == DecisionStatus::Accepted)
            .map(|(d, _)| d)
            .collect())
    }
}

/// L'insieme degli id puntati da un certo campo (`supersedes` o `deprecates`)
/// attraverso tutto il log. Calcolato in un colpo solo così che derivare lo stato
/// di una decisione sia O(1).
fn collect_ids(
    decisions: &[Decision],
    pick: impl Fn(&Decision) -> &[EntityId],
) -> HashSet<EntityId> {
    decisions
        .iter()
        .flat_map(|d| pick(d).iter().copied())
        .collect()
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
            supersedes: Vec::new(),
            deprecates: Vec::new(),
            evidence: Vec::new(),
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

    #[tokio::test]
    async fn supersession_derives_status_without_rewriting_history() {
        let store = InMemoryDecisionStore::new();
        let old = decision("vecchia scelta", vec![]);
        let mut newer = decision("nuova scelta", vec![]);
        newer.supersedes = vec![old.id];
        store.record(&old).await.unwrap();
        store.record(&newer).await.unwrap();

        // La storia resta intatta: entrambe vivono ancora nel log.
        assert_eq!(store.all().await.unwrap().len(), 2);

        // Lo stato è derivato, non scritto: la vecchia è rimpiazzata.
        let ledger = store.ledger().await.unwrap();
        let status_of = |id| ledger.iter().find(|(d, _)| d.id == id).map(|(_, s)| *s);
        assert_eq!(status_of(old.id), Some(DecisionStatus::Superseded));
        assert_eq!(status_of(newer.id), Some(DecisionStatus::Accepted));

        // La proiezione corrente tiene solo la nuova.
        let current = store.current_decisions().await.unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, newer.id);
    }

    #[tokio::test]
    async fn current_decisions_without_supersession_keeps_everything() {
        let store = InMemoryDecisionStore::new();
        store.record(&decision("uno", vec![])).await.unwrap();
        store.record(&decision("due", vec![])).await.unwrap();
        // Nessun `supersedes` ⇒ nessuna è rimpiazzata: il corrente è tutto il log.
        assert_eq!(store.current_decisions().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn deprecation_marks_status_and_drops_from_current() {
        let store = InMemoryDecisionStore::new();
        let retired = decision("invariante stantio", vec![]);
        let mut retirement = decision("ritiro dell'invariante", vec![]);
        retirement.deprecates = vec![retired.id];
        store.record(&retired).await.unwrap();
        store.record(&retirement).await.unwrap();

        let ledger = store.ledger().await.unwrap();
        let status_of = |id| ledger.iter().find(|(d, _)| d.id == id).map(|(_, s)| *s);
        assert_eq!(status_of(retired.id), Some(DecisionStatus::Deprecated));
        assert_eq!(status_of(retirement.id), Some(DecisionStatus::Accepted));

        // Deprecated non è corrente: resta solo il ritiro (esso stesso Accepted).
        let current = store.current_decisions().await.unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, retirement.id);
    }

    #[tokio::test]
    async fn supersession_wins_over_deprecation_for_the_same_decision() {
        // Se una decisione è sia rimpiazzata che deprecata, vince Superseded
        // (un rimpiazzo nominato dice più di un semplice ritiro).
        let store = InMemoryDecisionStore::new();
        let target = decision("scelta contesa", vec![]);
        let mut replacer = decision("rimpiazzo", vec![]);
        replacer.supersedes = vec![target.id];
        let mut deprecator = decision("nota di ritiro", vec![]);
        deprecator.deprecates = vec![target.id];
        store.record(&target).await.unwrap();
        store.record(&replacer).await.unwrap();
        store.record(&deprecator).await.unwrap();

        let ledger = store.ledger().await.unwrap();
        let status = ledger
            .iter()
            .find(|(d, _)| d.id == target.id)
            .map(|(_, s)| *s);
        assert_eq!(status, Some(DecisionStatus::Superseded));
    }
}
