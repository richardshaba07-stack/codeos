//! Il tipo [`Proposal`]: una decisione *bozza* che deve citare la propria evidenza.
//!
//! È il secondo stadio del flusso **Candidate → Proposal → Decision** (vision
//! step 3, pilastro 2) e il punto in cui il **trap #1 diventa codice**: il
//! briefing impone che un razionale generato dal sistema possa solo *riassumere*
//! evidenza che il grafo o git già provano, mai inventarla. Una `Proposal` non è
//! costruibile senza almeno una [`Evidence`]: il controllo è nel costruttore, non
//! affidato alla buona volontà di chi la crea.
//!
//! Una decisione di pura **autorità umana** (un umano scrive il suo perché) NON
//! passa di qui: può essere registrata direttamente senza evidenza. Il cancello
//! vale per le proposte *generate dal sistema*, dove l'invenzione è il rischio.

use codeos_types::bus::NewDecision;

use crate::decision::{Decision, DecisionKind};
use crate::evidence::Evidence;

/// Una decisione candidata, completa di bozza e citazioni, in attesa di conferma.
///
/// Garanzia di tipo: se esiste un `Proposal`, allora cita ≥1 [`Evidence`] (i
/// campi sono privati e l'unico costruttore è [`Proposal::new`], che rifiuta una
/// lista vuota). Confermarla ([`Proposal::confirm`]) produce una [`Decision`] che
/// **conserva** l'evidenza nel ledger, così il *perché* resta verificabile a
/// posteriori — non solo al momento della proposta.
pub struct Proposal {
    draft: NewDecision,
    kind: DecisionKind,
    evidence: Vec<Evidence>,
}

impl Proposal {
    /// Costruisce una proposta, **rifiutando** quelle senza evidenza.
    ///
    /// È qui che il trap #1 è applicato: niente citazioni ⇒ niente proposta. Il
    /// chiamante non può aggirarlo perché i campi non sono pubblici.
    pub fn new(
        draft: NewDecision,
        kind: DecisionKind,
        evidence: Vec<Evidence>,
    ) -> anyhow::Result<Self> {
        if evidence.is_empty() {
            anyhow::bail!(
                "una Proposal deve citare almeno un'evidenza: un razionale generato dal \
                 sistema può solo riassumere ciò che il grafo o git già provano, non inventarlo"
            );
        }
        Ok(Self {
            draft,
            kind,
            evidence,
        })
    }

    /// Le evidenze citate (sempre ≥1, per costruzione).
    pub fn evidence(&self) -> &[Evidence] {
        &self.evidence
    }

    /// Conferma la proposta in una [`Decision`], trasferendone l'evidenza così che
    /// la provenienza sopravviva nel ledger.
    pub fn confirm(self) -> Decision {
        let mut decision = Decision::from_new(self.draft, self.kind);
        decision.evidence = self.evidence;
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_types::RelationKind;

    fn draft() -> NewDecision {
        NewDecision {
            author: "ai:ProposalEngine".to_string(),
            title: "Isolare il dominio dall'infrastruttura".to_string(),
            context: String::new(),
            rationale: "Il grafo mostra zero archi dominio → infra.".to_string(),
            related_entity_ids: Vec::new(),
            related_decision_ids: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn refuses_a_proposal_without_evidence() {
        // Trap #1: nessuna citazione ⇒ nessuna proposta.
        let err = Proposal::new(draft(), DecisionKind::ArchitectureRule, vec![]);
        assert!(err.is_err());
    }

    #[test]
    fn confirm_carries_evidence_into_the_decision() {
        let evidence = vec![
            Evidence::Commit("abc123".to_string()),
            Evidence::Edge {
                source: "crate::a::foo".to_string(),
                kind: RelationKind::Calls,
                target: "crate::b::bar".to_string(),
            },
        ];
        let proposal =
            Proposal::new(draft(), DecisionKind::ArchitectureRule, evidence.clone()).unwrap();
        let decision = proposal.confirm();
        // La provenienza sopravvive nel ledger: non evapora alla conferma.
        assert_eq!(decision.evidence, evidence);
        assert_eq!(decision.kind, DecisionKind::ArchitectureRule);
    }
}
