//! Il tipo [`Decision`]: una memoria storica persistente del *perché*.
//!
//! È la risposta di CodeOS alla seconda domanda fondamentale ("perché è scritto
//! così?"). Una `Decision` aggancia un pezzo di ragionamento umano o dell'AI a un
//! insieme di entità del grafo, così che chi (umano o LLM) toccherà quel codice
//! in futuro erediti il contesto invece di reinventarlo.
//!
//! DECISION: il briefing distingue `Decision` (scelta puntuale) e
//! `ArchitectureRule` (regola persistente). Condividono tutti i campi, quindi qui
//! sono **un solo tipo** discriminato da [`DecisionKind`]: meno duplicazione, e la
//! promozione di una violazione del Guardian a regola diventa solo un cambio di
//! `kind`. Si potranno separare se e quando divergeranno davvero.

use codeos_types::bus::NewDecision;
use codeos_types::EntityId;

/// Che tipo di memoria è.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    /// Una scelta architetturale puntuale ("abbiamo usato SQLite perché...").
    Decision,
    /// Una regola persistente che il codice deve continuare a rispettare
    /// ("il layer dominio non dipende dall'infrastruttura"). Tipicamente nasce
    /// confermando un invariante scoperto dal Guardian.
    ArchitectureRule,
}

impl DecisionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DecisionKind::Decision => "Decision",
            DecisionKind::ArchitectureRule => "ArchitectureRule",
        }
    }

    pub(crate) fn from_str_lenient(s: &str) -> Self {
        match s {
            "ArchitectureRule" => DecisionKind::ArchitectureRule,
            // Default conservativo: qualunque cosa non riconosciamo è una Decision.
            _ => DecisionKind::Decision,
        }
    }
}

/// Una memoria storica completa, con identità e timestamp assegnati dal Memory
/// Engine. Immutabile dopo la creazione (la storia non si riscrive: una nuova
/// scelta è una nuova `Decision` che può referenziare le precedenti via
/// `related_decision_ids`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub id: EntityId,
    pub kind: DecisionKind,
    /// Es. `"human:Marco"` o `"ai:ArchitectureGuardian"`.
    pub author: String,
    pub title: String,
    pub context: String,
    pub rationale: String,
    /// Le entità del grafo a cui questa memoria si aggancia (l'aggancio è la base
    /// del Passo 3 del Query Engine: portare il *perché* nel contesto).
    pub related_entity_ids: Vec<EntityId>,
    /// Altre decisioni collegate (catena storica).
    pub related_decision_ids: Vec<EntityId>,
    pub tags: Vec<String>,
    /// Istante di registrazione, in formato RFC 3339.
    pub timestamp: String,
}

impl Decision {
    /// Costruisce una `Decision` completa da un [`NewDecision`] (l'input grezzo
    /// che arriva sul bus), assegnando `id` e `timestamp`.
    pub fn from_new(new: NewDecision, kind: DecisionKind) -> Self {
        Self {
            id: EntityId::new(),
            kind,
            author: new.author,
            title: new.title,
            context: new.context,
            rationale: new.rationale,
            related_entity_ids: new.related_entity_ids,
            related_decision_ids: new.related_decision_ids,
            tags: new.tags,
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }
}
