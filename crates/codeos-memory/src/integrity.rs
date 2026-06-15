//! Integrità del ledger: trova le decisioni la cui **provenienza si è rotta**.
//!
//! Una decisione del ledger può citare la propria fonte come [`Evidence`]: un
//! commit, un documento (ADR). Quella citazione è ciò che rende il *perché*
//! verificabile. Ma le fonti si muovono: un commit viene riscritto da un rebase o
//! schiacciato da uno squash, un file ADR viene rinominato o cancellato. Quando
//! succede, la decisione resta nel ledger ma la sua prova **non esiste più**: è una
//! memoria che punta al vuoto.
//!
//! Questo modulo lo rileva, con la stessa disciplina anti-FP del resto:
//!
//! - **Solo fatti verificati.** Una citazione è «rotta» unicamente se il
//!   controllo (il commit è irraggiungibile / il file è assente) dà esito CERTO.
//!   Finché l'oggetto è recuperabile non si grida al lupo.
//! - **Per-citazione, non per-decisione.** Si segnala la singola prova sparita: una
//!   decisione con due citazioni di cui una valida conserva una gamba su cui
//!   stare, e lo si vede.
//! - **Le decisioni di pura autorità umana NON hanno provenienza** (evidenza
//!   vuota): non si possono rompere, quindi non vengono mai segnalate.
//!
//! La funzione è **pura**: riceve due predicati di esistenza (iniettati dal
//! chiamante con git/filesystem reali, o falsi nei test), così la logica è
//! verificabile senza un repo.

use codeos_types::EntityId;

use crate::decision::Decision;
use crate::evidence::Evidence;

/// Perché una citazione è considerata rotta. Porta il riferimento sparito così
/// l'output è azionabile (quale commit / quale file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakReason {
    /// Il commit citato non è più raggiungibile nel repository.
    CommitGone(String),
    /// Il documento citato (ADR/RFC) non esiste più al path indicato.
    DocumentGone(String),
}

/// Una prova sparita: la decisione che la cita e il motivo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokenCitation {
    pub decision_id: EntityId,
    pub decision_title: String,
    pub reason: BreakReason,
}

/// Scorre il ledger e raccoglie le citazioni la cui fonte non esiste più, secondo
/// i predicati `commit_exists` / `doc_exists` (iniettati: git/filesystem reali in
/// produzione, finti nei test). Pura e deterministica nell'ordine d'ingresso.
///
/// Considera solo le evidenze con provenienza esterna verificabile senza il grafo
/// ([`Evidence::Commit`] e [`Evidence::Document`]); le altre varianti (archi,
/// entità, decisioni precedenti) richiedono grafo/ledger e restano fuori da questo
/// controllo — meglio non controllare che controllare a metà e mentire.
pub fn find_broken_provenance(
    decisions: &[Decision],
    commit_exists: impl Fn(&str) -> bool,
    doc_exists: impl Fn(&str) -> bool,
) -> Vec<BrokenCitation> {
    let mut out = Vec::new();
    for d in decisions {
        for e in &d.evidence {
            let reason = match e {
                Evidence::Commit(h) if !commit_exists(h) => {
                    Some(BreakReason::CommitGone(h.clone()))
                }
                Evidence::Document(p) if !doc_exists(p) => {
                    Some(BreakReason::DocumentGone(p.clone()))
                }
                _ => None,
            };
            if let Some(reason) = reason {
                out.push(BrokenCitation {
                    decision_id: d.id,
                    decision_title: d.title.clone(),
                    reason,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::DecisionKind;

    fn decision(title: &str, evidence: Vec<Evidence>) -> Decision {
        Decision {
            id: EntityId::new(),
            kind: DecisionKind::Decision,
            author: "ai:DecisionMiner".to_string(),
            title: title.to_string(),
            context: String::new(),
            rationale: "perché".to_string(),
            related_entity_ids: Vec::new(),
            related_decision_ids: Vec::new(),
            supersedes: Vec::new(),
            deprecates: Vec::new(),
            evidence,
            tags: Vec::new(),
            timestamp: "2026-06-15T00:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn flags_a_decision_whose_commit_vanished() {
        let decisions = vec![decision(
            "Passa a SQLite",
            vec![Evidence::Commit("deadbeef".into())],
        )];
        // Nessun commit esiste, nessun documento esiste.
        let broken = find_broken_provenance(&decisions, |_| false, |_| false);
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].reason, BreakReason::CommitGone("deadbeef".into()));
    }

    #[test]
    fn does_not_flag_a_decision_whose_commit_still_exists() {
        let decisions = vec![decision("Vivo", vec![Evidence::Commit("abc123".into())])];
        let broken = find_broken_provenance(&decisions, |_| true, |_| false);
        assert!(broken.is_empty(), "il commit esiste ⇒ niente falso allarme");
    }

    #[test]
    fn flags_a_missing_adr_document() {
        let decisions = vec![decision(
            "Usa gRPC",
            vec![Evidence::Document("docs/adr/0001-grpc.md".into())],
        )];
        let broken = find_broken_provenance(&decisions, |_| false, |_| false);
        assert_eq!(broken.len(), 1);
        assert_eq!(
            broken[0].reason,
            BreakReason::DocumentGone("docs/adr/0001-grpc.md".into())
        );
    }

    #[test]
    fn never_flags_a_human_decision_without_evidence() {
        // Autorità umana: evidenza vuota ⇒ niente provenienza ⇒ niente da rompere.
        let decisions = vec![decision("Scelta umana", vec![])];
        let broken = find_broken_provenance(&decisions, |_| false, |_| false);
        assert!(broken.is_empty());
    }

    #[test]
    fn reports_only_the_broken_citation_when_some_hold() {
        // Una citazione valida + una sparita ⇒ si segnala SOLO quella sparita.
        let decisions = vec![decision(
            "Mista",
            vec![
                Evidence::Commit("alive".into()),
                Evidence::Commit("dead".into()),
            ],
        )];
        let broken = find_broken_provenance(&decisions, |h| h == "alive", |_| false);
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].reason, BreakReason::CommitGone("dead".into()));
    }
}
