//! Il bus dei comandi e degli eventi (sezione 4 del briefing).
//!
//! - I [`Command`] entrano nel sistema dal Dispatcher e vengono instradati a un
//!   singolo attore tramite un canale `mpsc`. Ogni comando porta con sé un
//!   `reply_to` su cui l'attore deposita la risposta.
//! - I [`CodeOsEvent`] sono pubblicati da un attore e ricevuti da tutti i
//!   sottoscrittori tramite un canale `broadcast`.

use tokio::sync::mpsc;

use crate::{Entity, EntityId, GraphDelta, ParsedFileResult, Relation, SourceLocation};

/// Comando inviato al sistema (da CLI, plugin VS Code o server gRPC).
///
/// Il campo `reply_to` di ogni variante è il canale su cui l'attore destinatario
/// scrive l'esito. Il chiamante crea un `mpsc` con capacità 1, invia il comando
/// e attende un singolo messaggio di risposta.
#[derive(Debug)]
pub enum Command {
    /// Indicizza l'intero progetto a partire dalla sua root.
    IndexProject {
        project_root: String,
        reply_to: mpsc::Sender<anyhow::Result<()>>,
    },
    /// Indicizza un insieme esplicito di file. Risponde con gli `EntityId` radice
    /// creati/aggiornati.
    IndexFiles {
        files: Vec<String>,
        reply_to: mpsc::Sender<anyhow::Result<Vec<EntityId>>>,
    },
    /// Re-indicizza un singolo file (tipicamente dopo un salvataggio).
    ReIndexFile {
        file_path: String,
        reply_to: mpsc::Sender<anyhow::Result<()>>,
    },
    /// Rimuove dal grafo le entità e le relazioni dei file indicati.
    RemoveFiles {
        files: Vec<String>,
        reply_to: mpsc::Sender<anyhow::Result<()>>,
    },
    /// Interroga il grafo / costruisce il contesto per una query.
    QueryGraph {
        query: QueryRequest,
        reply_to: mpsc::Sender<anyhow::Result<QueryResponse>>,
    },
    /// Livello L2 del context builder: il **cammino di chiamata** onesto tra due
    /// entità nominate dall'utente. `from`/`to` sono nomi (qualificati o solo il
    /// segmento finale): il Query Engine li risolve a un'unica entità — senza mai
    /// indovinare — e poi cerca il cammino lungo i soli archi `Calls` risolti.
    CallPath {
        from: String,
        to: String,
        reply_to: mpsc::Sender<anyhow::Result<CallPathReply>>,
    },
    /// Registra una decisione architetturale nel Memory Engine.
    RecordDecision {
        decision: NewDecision,
        reply_to: mpsc::Sender<anyhow::Result<EntityId>>,
    },
    /// Chiede al Guardian un **referto architetturale** completo: gli invarianti di
    /// layering scoperti (con la confidenza calibrata dal Campo di Astensione), i
    /// Fossili di Decisione (la nascita storica di ogni confine) e le lacune dello
    /// spazio negativo del secondo ordine (gli invarianti mancanti). È una lettura
    /// pura e diagnostica: non muta né il grafo né la memoria.
    ArchitectureReport {
        reply_to: mpsc::Sender<anyhow::Result<ArchitectureReport>>,
    },
    /// AI Architecture Firewall: pre-check prima delle modifiche
    GuardBefore {
        goal: String,
        reply_to: mpsc::Sender<anyhow::Result<GuardBeforeResponse>>,
    },
    /// AI Architecture Firewall: post-check dopo le modifiche
    GuardAfter {
        reply_to: mpsc::Sender<anyhow::Result<GuardAfterResponse>>,
    },
    /// Genera un Context Pack mirato per un LLM/AI Agent
    GetContextPack {
        goal: String,
        for_ai: bool,
        reply_to: mpsc::Sender<anyhow::Result<GetContextPackResponse>>,
    },
    /// PR Architecture MRI: scansione impatto di una PR o commit diff
    PrMri {
        base: String,
        head: String,
        reply_to: mpsc::Sender<anyhow::Result<PrMriResponse>>,
    },
    /// Time machine architetturale: perché esiste un confine o regola
    Why {
        expr: String,
        reply_to: mpsc::Sender<anyhow::Result<WhyResponse>>,
    },
    /// What-If Refactor Simulator: simula spostamenti/ristrutturazioni
    Simulate {
        expr: String,
        reply_to: mpsc::Sender<anyhow::Result<SimulateResponse>>,
    },
}

/// Evento pubblicato sull'event bus broadcast.
///
/// Deve essere `Clone` perché ogni sottoscrittore riceve una copia.
#[derive(Debug, Clone)]
pub enum CodeOsEvent {
    /// Il Parser ha terminato di analizzare un insieme di file.
    ///
    /// DECISION: porta i risultati **grezzi** del parser (`ParsedFileResult`),
    /// non un `GraphDelta`. Il briefing (sez. 4) elencava `delta: GraphDelta`,
    /// ma il Parser non conosce gli `EntityId` (invariante 1.4): non può produrre
    /// un delta. È il `GraphActor`, in ascolto su questo evento, a trasformare i
    /// risultati grezzi in un delta e a pubblicare poi `GraphUpdated`.
    FilesIndexed { results: Vec<ParsedFileResult> },
    /// Il grafo è stato aggiornato con un delta.
    GraphUpdated { delta: GraphDelta },
    /// L'aggiornamento del grafo è **fallito**: il `GraphActor` ha ricevuto i
    /// risultati del parser ma resolution o persistenza hanno dato errore, quindi
    /// il grafo NON riflette i file appena analizzati.
    ///
    /// Filosofia (P0, «un arco mancante è preferibile a uno che mente»): un esito
    /// negativo dev'essere *dichiarato*, non silenziato. Chi attende l'aggiornamento
    /// (i ponti gRPC IndexProject/IndexFiles) lo intercetta e restituisce un errore
    /// onesto al chiamante invece di un falso «completato con successo».
    GraphUpdateFailed { reason: String },
    /// Il Guardian ha rilevato una violazione architetturale.
    ArchitectureViolationDetected { violation: ArchitectureViolation },
    /// Progresso reale dell'indicizzazione (P5-18)
    IndexProgress {
        total_files: u32,
        processed_files: u32,
        current_file: String,
        skipped_files: u32,
        parse_errors: u32,
    },
}

/// Richiesta di interrogazione del grafo / costruzione del contesto.
#[derive(Debug, Clone)]
pub enum QueryRequest {
    /// Query in linguaggio naturale (es. "voglio aggiungere il login OAuth").
    NaturalLanguage { text: String },
}

/// Risposta del Query Engine: il contesto minimo rilevante per la query.
#[derive(Debug, Clone, Default)]
pub struct QueryResponse {
    /// Prompt strutturato pronto da passare a un LLM (vedi sez. 10.1, passo 6).
    pub formatted_context: String,
    /// Entità incluse nel sottografo restituito.
    pub entities: Vec<Entity>,
    /// Relazioni tra le entità incluse.
    pub relations: Vec<Relation>,
}

/// Esito della ricerca di un cammino di chiamata (livello L2). I quattro stati
/// sono *onesti e distinti*: non collassiamo «non trovato» con «non lo so».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPathStatus {
    /// Cammino trovato: ogni passo consecutivo è un arco `Calls` risolto.
    Found,
    /// Entrambi gli estremi sono noti, ma non esiste cammino lungo i soli archi
    /// `Calls` risolti. NON è prova di assenza di chiamata: un riferimento non
    /// risolto potrebbe nasconderne uno.
    NoPath,
    /// Almeno un nome non corrisponde ad alcuna entità del grafo.
    Unknown,
    /// Almeno un nome corrisponde a più entità: serve disambiguare. Non scegliamo
    /// noi un'entità a caso (sarebbe un cammino che l'utente non ha chiesto).
    Ambiguous,
}

/// Risposta del Query Engine alla richiesta [`Command::CallPath`].
///
/// Filosofia anti-falso-positivo applicata al livello L2: `steps` è valorizzato
/// **solo** quando `status == Found`, e ogni freccia tra due passi consecutivi è
/// un arco `Calls` realmente presente nel grafo. `candidates` porta i nomi tra
/// cui scegliere (stato `Ambiguous`) o i quasi-omonimi suggeriti (stato
/// `Unknown`), così l'utente sa *perché* non c'è un cammino, invece di ricevere
/// un risultato inventato.
#[derive(Debug, Clone)]
pub struct CallPathReply {
    /// Testo già formattato, pronto per il terminale o per un LLM.
    pub formatted: String,
    /// Lo stato esplicito dell'esito (vedi [`CallPathStatus`]). Sempre noto.
    pub status: CallPathStatus,
    /// Il cammino, dalla sorgente alla destinazione, quando `status == Found`.
    pub steps: Vec<Entity>,
    /// I candidati da disambiguare (`Ambiguous`) o i suggerimenti (`Unknown`).
    pub candidates: Vec<Entity>,
}

/// Dati per registrare una nuova decisione. La `Decision` completa (con `id` e
/// `timestamp`) è costruita dal Memory Engine; qui c'è solo l'input.
#[derive(Debug, Clone)]
pub struct NewDecision {
    /// Es. `"human:Marco"` o `"ai:ArchitectureGuardian"`.
    pub author: String,
    pub title: String,
    pub context: String,
    pub rationale: String,
    pub related_entity_ids: Vec<EntityId>,
    pub related_decision_ids: Vec<EntityId>,
    /// Le decisioni che questa **rimpiazza**: ne erediterà lo stato derivato
    /// `Superseded` nel ledger (la storia non si riscrive, si stratifica). Vuoto
    /// per una decisione che non sostituisce nulla. Permette a un umano (via
    /// CLI/RPC) di rimpiazzare una scelta passata — non solo ai produttori
    /// in-process.
    pub supersedes: Vec<EntityId>,
    /// Le decisioni che questa **ritira** senza un rimpiazzo nominato: ne deriva lo
    /// stato `Deprecated`. Distinto da [`supersedes`](Self::supersedes): un ritiro
    /// puro dice meno di un rimpiazzo (per questo `Superseded` ha precedenza).
    pub deprecates: Vec<EntityId>,
    pub tags: Vec<String>,
}

/// Il **referto architetturale**: la fotografia degli invarianti impliciti che il
/// Guardian ha scoperto leggendo lo *spazio negativo* della codebase, lungo tutti e
/// quattro gli assi (struttura, tempo, intento, meta).
///
/// DECISION: è composto da soli tipi di **dato puro** (stringhe e numeri).
/// `codeos-types` è il cuore della cipolla (invariante 1.5) e non può dipendere da
/// `codeos-guardian` né da `codeos-paleo`: i tipi ricchi (`LayeringRule`,
/// `DecisionFossil`, `MissingInvariant`) restano confinati nei loro crate e vengono
/// "appiattiti" qui al confine del trasporto.
#[derive(Debug, Clone, Default)]
pub struct ArchitectureReport {
    /// Gli invarianti di layering scoperti (asse struttura), con la confidenza
    /// eventualmente ricalibrata dal Campo di Astensione (asse tempo).
    pub invariants: Vec<LayeringInvariantInfo>,
    /// Gli invarianti **in formazione**: asimmetrie pure ancora sotto soglia (stadio
    /// 1 del flusso). Derivati e mai persistiti, accanto agli invarianti maturi così
    /// che un consumatore veda anche i confini che *stanno* nascendo, non solo i nati.
    pub candidates: Vec<LayeringCandidateInfo>,
    /// I Fossili di Decisione: la nascita storica di ciascun confine (asse intento).
    pub fossils: Vec<DecisionFossilInfo>,
    /// Le lacune del secondo ordine: gli invarianti mancanti dove la convenzione
    /// architetturale direbbe che dovrebbero esserci (asse meta).
    pub gaps: Vec<ArchitecturalGapInfo>,
    /// La **qualità del grafo** da cui il referto è derivato: quanto fidarsi del
    /// dato di partenza (roadmap P2-7). Non è un asse dello spazio negativo, è la
    /// sua misura di affidabilità.
    pub quality: GraphQualityInfo,
    /// true se la storia git è insufficiente per tracciare i confini in modo affidabile (P2-8).
    pub history_insufficient: bool,
}

/// La **qualità del grafo**: la misura di quanta fiducia merita il referto.
///
/// Filosofia (roadmap P0, «un arco mancante è preferibile a un arco che mente»):
/// un referto è onesto solo se dichiara *esplicitamente* la solidità dei dati su
/// cui si fonda. Questi contatori — derivati dal grafo persistito al momento del
/// referto — distinguono le relazioni saldamente agganciate dai riferimenti
/// lasciati `Unresolved` di proposito e dagli archi a bassa confidenza, e isolano
/// le entità esterne (tracciate ma escluse dal mining, P0-2).
///
/// `total = resolved + unresolved + low_confidence` partiziona le relazioni: ogni
/// arco cade in esattamente una delle tre classi di fiducia.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphQualityInfo {
    /// Entità totali nel grafo (incluse quelle esterne).
    pub total_entities: u64,
    /// Entità `ExternalDependency`: tracciate per query/impatto ma fuori dal mining
    /// architetturale (P0-2). Le isoliamo perché non sono codice del progetto.
    pub external_entities: u64,
    /// Relazioni totali nel grafo.
    pub total_relations: u64,
    /// Relazioni agganciate a un target reale con confidenza alta o media: la spina
    /// dorsale fidata del grafo (`total - unresolved - low_confidence`).
    pub resolved_relations: u64,
    /// Relazioni `Unresolved` (target nullo): un riferimento visto ma deliberatamente
    /// non agganciato a un omonimo incerto. Un arco mancante, non un arco che mente.
    pub unresolved_relations: u64,
    /// Relazioni agganciate ma a **bassa** confidenza (`resolution_confidence=low`):
    /// escluse dal mining (P0-1b). Oggi tipicamente zero — è una rete di sicurezza
    /// per le future euristiche fuzzy/cross-package.
    pub low_confidence_relations: u64,
}

/// Livello di gravità di un esito architetturale (invariante, lacuna o violazione).
///
/// È l'unica fonte di verità per classificare la priorità: i costruttori
/// `for_invariant`/`for_gap`/`for_violation` mappano i segnali grezzi (confidenza,
/// supporto) su tre livelli, così Guardian, CLI e plugin VS Code concordano sempre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Severity {
    /// Bassa priorità: probabile falso positivo o regola debole.
    #[default]
    Info,
    /// Da tenere d'occhio: confine plausibile ma non ancora battle-tested.
    Warning,
    /// Alto rischio: confine forte da difendere, o anomalia conclamata.
    HighRisk,
}

impl Severity {
    /// Severità di un invariante di layering scoperto, dalla sua confidenza in
    /// `[0,1]`. Sotto 0.5 = quasi sicuramente rumore (probabile falso positivo);
    /// `>= 0.85` = confine provato, difenderlo è prioritario.
    pub fn for_invariant(confidence: f64) -> Self {
        if confidence >= 0.85 {
            Severity::HighRisk
        } else if confidence >= 0.5 {
            Severity::Warning
        } else {
            Severity::Info
        }
    }

    /// Severità di una lacuna del secondo ordine, da quanti *altri* layer
    /// rispettano la fondazione violata: più coro c'è, più l'eccezione è anomala.
    pub fn for_gap(foundation_support: u32) -> Self {
        if foundation_support >= 3 {
            Severity::HighRisk
        } else {
            Severity::Warning
        }
    }

    /// Una violazione *attiva* di un confine già persistito è sempre il segnale
    /// più serio: qualcuno ha appena invertito una freccia stabilita.
    pub fn for_violation() -> Self {
        Severity::HighRisk
    }

    /// Etichetta stabile, machine-readable, per il filo e i log.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::HighRisk => "high_risk",
        }
    }

    /// Ricostruisce la severità dalla sua etichetta (per i confini di trasporto).
    /// Sconosciuto o vuoto → `Info` (degrada con grazia).
    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "high_risk" => Severity::HighRisk,
            "warning" => Severity::Warning,
            _ => Severity::Info,
        }
    }

    /// Etichetta leggibile con badge, per il referto da terminale.
    pub fn badge(self) -> &'static str {
        match self {
            Severity::Info => "⚪️ INFO",
            Severity::Warning => "🟡 WARNING",
            Severity::HighRisk => "🔴 ALTO RISCHIO",
        }
    }
}

/// Provenienza di una regola di layering: dissotterrata dallo spazio negativo del
/// grafo, oppure dichiarata a mano da un umano in `.codeos/config.yaml`.
///
/// Distinguere le due è essenziale (item 16 della roadmap): una regola *scoperta*
/// è un'ipotesi che i dati sostengono e che il tempo calibra; una regola
/// *dichiarata* è una volontà esplicita — non si discute, non si calibra, vale per
/// decreto (confidenza 1.0) anche dove il grafo non ha ancora evidenza strutturale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuleOrigin {
    /// Dedotta automaticamente dal Guardian leggendo l'asimmetria del grafo.
    #[default]
    Discovered,
    /// Imposta esplicitamente dall'umano in `.codeos/config.yaml`.
    Declared,
}

impl RuleOrigin {
    /// Etichetta stabile, machine-readable, per il trasporto (proto/JSON).
    pub fn as_str(self) -> &'static str {
        match self {
            RuleOrigin::Discovered => "discovered",
            RuleOrigin::Declared => "declared",
        }
    }

    /// Ricostruisce la provenienza dalla sua etichetta. Sconosciuto → `Discovered`.
    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "declared" => RuleOrigin::Declared,
            _ => RuleOrigin::Discovered,
        }
    }

    /// Etichetta leggibile in italiano, per il referto da terminale.
    pub fn label(self) -> &'static str {
        match self {
            RuleOrigin::Discovered => "scoperto",
            RuleOrigin::Declared => "dichiarato",
        }
    }
}

/// Un invariante di layering scoperto: `downstream` dipende da `upstream` a senso
/// unico, mai l'inverso. Forma piatta per il trasporto.
#[derive(Debug, Clone, PartialEq)]
pub struct LayeringInvariantInfo {
    /// Il layer-fondazione (la base da cui si dipende).
    pub upstream: String,
    /// Il layer che dipende dalla fondazione a senso unico.
    pub downstream: String,
    /// Quanti archi distinti `downstream → upstream` sostengono la regola.
    pub support: u32,
    /// La confidenza nella regola in `[0, 1]`. Se `calibrated` è `true` proviene dal
    /// limite inferiore di Wilson del Campo di Astensione; altrimenti è la stima
    /// strutturale di base `1 - 1/(support+1)`.
    pub confidence: f64,
    /// `true` se la confidenza è stata ricalibrata sulla storia git (Campo di
    /// Astensione), `false` se è la sola stima strutturale.
    pub calibrated: bool,
    /// Quanto è prioritario difendere questo confine (derivata dalla confidenza).
    pub severity: Severity,
    /// Se l'invariante è stato scoperto dai dati o dichiarato a mano nella config.
    pub origin: RuleOrigin,
}

/// Un invariante **in formazione** in forma piatta per il trasporto: la stessa
/// asimmetria pura di un [`LayeringInvariantInfo`], ma con supporto ancora sotto la
/// soglia. È lo stadio 1 del flusso (candidato → proposta → decisione), **derivato
/// e mai persistito**. Niente `confidence`/`severity`: un confine non ancora formato
/// non si stima (trap #3); `needed` dichiara apertamente quanto manca alla promozione.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayeringCandidateInfo {
    /// Il layer-fondazione (la base da cui si dipenderebbe).
    pub upstream: String,
    /// Il layer che dipende a senso unico (la direzione osservata).
    pub downstream: String,
    /// Quanti archi `downstream → upstream` sono stati osservati finora.
    pub support: u32,
    /// Quanti archi ancora mancano perché diventi un invariante (`min_support - support`).
    pub needed: u32,
}

/// La nascita storica di un confine architetturale (Fossile di Decisione).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionFossilInfo {
    pub upstream: String,
    pub downstream: String,
    /// L'hash del commit in cui il confine è nato (vuoto se non ricostruibile).
    pub born_at: String,
    /// Il timestamp Unix di quel commit.
    pub born_at_unix: i64,
    /// Il messaggio (subject) del commit di nascita: l'intento dichiarato.
    pub intent: String,
    /// I file dei due layer toccati insieme alla nascita: il diff di cristallizzazione.
    pub born_structure: Vec<String>,
}

/// Una lacuna dello spazio negativo del secondo ordine: l'invariante che *manca*
/// dove la convenzione architetturale direbbe che dovrebbe esserci.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitecturalGapInfo {
    /// Il layer-fondazione su cui la convenzione a senso unico è attesa.
    pub upstream: String,
    /// Il layer anomalo, accoppiato bidirezionalmente alla fondazione.
    pub downstream: String,
    /// Quanti *altri* layer rispettano `upstream` come fondazione a senso unico.
    pub foundation_support: u32,
    /// Gravità dell'anomalia (derivata da `foundation_support`).
    pub severity: Severity,
}

/// Violazione di una regola architetturale rilevata dal Guardian.
#[derive(Debug, Clone)]
pub struct ArchitectureViolation {
    pub rule_id: EntityId,
    pub relation_id: EntityId,
    pub source_id: EntityId,
    pub target_id: EntityId,
    pub message: String,
    /// Dove vive la dipendenza proibita: la posizione dell'entità *sorgente* (chi
    /// introduce l'arco che inverte la freccia architetturale). È ciò che permette
    /// a un editor di piazzare la diagnostica sulla riga giusta. `None` se la
    /// posizione non è ricostruibile (entità non trovata nello storage).
    pub location: Option<SourceLocation>,
    /// Gravità della violazione. Una violazione attiva è sempre alto rischio.
    pub severity: Severity,
}

#[derive(Debug, Clone, Default)]
pub struct GuardBeforeResponse {
    pub target_files: Vec<String>,
    pub boundaries: Vec<String>,
    pub blast_radius: u32,
    pub safe_path: String,
    pub context_pack: String,
}

#[derive(Debug, Clone, Default)]
pub struct GuardAfterResponse {
    pub new_relations: Vec<String>,
    pub violations: Vec<ArchitectureViolation>,
    pub proposed_fixes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GetContextPackResponse {
    pub goal_interpretation: String,
    pub files_to_read: Vec<String>,
    pub relevant_entities: Vec<String>,
    pub key_dependencies: Vec<String>,
    pub boundaries_to_preserve: Vec<String>,
    pub local_patterns: Vec<String>,
    pub suggested_tests: Vec<String>,
    pub estimated_risk: String,
    pub formatted_markdown: String,
}

#[derive(Debug, Clone, Default)]
pub struct PrMriResponse {
    pub new_dependencies: Vec<String>,
    pub violated_boundaries: Vec<String>,
    pub blast_radius_change: i32,
    pub historical_hotspots: Vec<String>,
    pub new_external_dependencies: Vec<String>,
    pub impacted_tests: Vec<String>,
    pub risk_score: String, // "low", "medium", "high"
    pub summary: String,
}

#[derive(Debug, Clone, Default)]
pub struct WhyResponse {
    pub born_commit: String,
    pub born_date: String,
    pub intent: String,
    pub co_changed_files: Vec<String>,
    pub markdown_decisions: Vec<String>,
    pub explanation: String,
    pub history_insufficient: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SimulateResponse {
    pub dependencies_to_rewrite: Vec<String>,
    pub changed_boundaries: Vec<String>,
    pub risks: Vec<String>,
    pub suggested_tests: Vec<String>,
    pub recommendation_plan: Vec<String>,
}
