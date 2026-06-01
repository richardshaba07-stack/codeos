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
    /// Il Guardian ha rilevato una violazione architetturale.
    ArchitectureViolationDetected { violation: ArchitectureViolation },
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
    /// I Fossili di Decisione: la nascita storica di ciascun confine (asse intento).
    pub fossils: Vec<DecisionFossilInfo>,
    /// Le lacune del secondo ordine: gli invarianti mancanti dove la convenzione
    /// architetturale direbbe che dovrebbero esserci (asse meta).
    pub gaps: Vec<ArchitecturalGapInfo>,
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
}
