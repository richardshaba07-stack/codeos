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

/// Lo stato *derivato* di una [`Decision`] nel ledger architetturale.
///
/// Non è un campo memorizzato: si calcola dal log additivo, mai scritto su una
/// vecchia decisione. Una decisione cambia stato solo perché un'**altra** la
/// punta — [`Superseded`](DecisionStatus::Superseded) se elencata in un
/// `supersedes` (rimpiazzata da una scelta nominata), [`Deprecated`](DecisionStatus::Deprecated)
/// se elencata in un `deprecates` (ritirata, senza un sostituto puntuale).
/// Poiché si può puntare solo una decisione che già esiste, chi la obsoleta
/// arriva sempre *dopo*: la freccia è anche quella del tempo, e lo stato
/// corrente resta una proiezione del passato dimostrato — mai un'invenzione.
///
/// Precedenza quando entrambe valgono: `Superseded` > `Deprecated` > `Accepted`
/// (essere rimpiazzati da qualcosa di nominato dice più che essere solo ritirati).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionStatus {
    /// Nessuna decisione l'ha obsoletata: è la verità corrente.
    Accepted,
    /// Una decisione successiva la elenca nel proprio `supersedes`.
    Superseded,
    /// Una decisione successiva la elenca nel proprio `deprecates`: non più in
    /// vigore (es. il Guardian ritira un invariante che il grafo non sostiene
    /// più), ma senza un rimpiazzo puntuale.
    Deprecated,
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
    /// Le decisioni che questa **rimpiazza**. Additivo e direzionale: registrare
    /// una supersessione non muta la vecchia decisione (la storia non si
    /// riscrive), aggiunge solo questo puntatore nella nuova. Da qui si deriva
    /// lo stato [`DecisionStatus::Superseded`].
    pub supersedes: Vec<EntityId>,
    /// Le decisioni che questa **ritira** senza rimpiazzarle con una scelta
    /// puntuale (additivo e direzionale come `supersedes`). Da qui si deriva lo
    /// stato [`DecisionStatus::Deprecated`]. È il gancio per il ritiro di un
    /// invariante da parte del Guardian.
    pub deprecates: Vec<EntityId>,
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
            // Il bus (`NewDecision`) non esprime ancora supersessione/deprecazione:
            // campi vuoti, onesto. I produttori in-process (es. il Guardian) li
            // popolano direttamente; il cablaggio sul bus è una slice successiva.
            supersedes: Vec::new(),
            deprecates: Vec::new(),
            tags: new.tags,
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }
}
