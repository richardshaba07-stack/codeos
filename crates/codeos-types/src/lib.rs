//! `codeos-types` — il vocabolario condiviso dell'intero sistema.
//!
//! Questo crate NON dipende da nessun altro crate interno (invariante 1.5).
//! Definisce i tipi del grafo, i dati grezzi prodotti dal parser, il delta del
//! grafo e — nel sotto-modulo [`bus`] — i comandi e gli eventi che viaggiano
//! tra gli attori.

pub mod bus;

use std::collections::HashMap;
use uuid::Uuid;

/// Identificativo unico e immutabile di un nodo o di un arco del grafo.
///
/// È un UUID v4 e **non viene mai riciclato** (invariante 1.2). Se un file viene
/// rinominato, il `qualified_name` di un'entità può cambiare ma il suo
/// `EntityId` resta lo stesso quando il parser è in grado di tracciarlo;
/// altrimenti ne viene generato uno nuovo. Le `Decision` del Memory Engine si
/// agganciano a un `EntityId`, quindi la sua stabilità è la base della memoria
/// storica.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct EntityId(pub Uuid);

impl EntityId {
    /// Crea un nuovo identificativo casuale (UUID v4).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// L'identificativo "nullo" (UUID tutto-zeri).
    ///
    /// Usato come `target_id` segnaposto delle relazioni `Unresolved`, che per
    /// definizione non puntano a un'entità nota. Non corrisponde mai a un'entità
    /// reale (le `Entity` nascono sempre con [`EntityId::new`]).
    pub fn nil() -> Self {
        Self(Uuid::nil())
    }

    /// `true` se è l'identificativo nullo (vedi [`EntityId::nil`]).
    pub fn is_nil(&self) -> bool {
        self.0.is_nil()
    }
}

impl Default for EntityId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Tipo di un nodo del grafo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum EntityKind {
    Project,
    Module,
    Class,
    Struct,
    Interface,
    Function,
    Method,
    Variable,
    Parameter,
    Test,
    /// Dipendenza esterna sintetica (es. `std`, `tokio`, `react`, `@scope/pkg`).
    ///
    /// Non corrisponde a un'entità reale nel codice indicizzato: è un nodo
    /// creato dal `GraphResolver` quando un import/uso punta a una libreria fuori
    /// dal progetto, così le relazioni esterne risolvono a un target stabile
    /// invece di diventare `Unresolved` con `target_id` nullo. Abilita query del
    /// tipo "cosa dipende da tokio?".
    ExternalDependency,
}

/// Posizione di un'entità (o di una relazione) nel codice sorgente.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceLocation {
    pub file_path: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// Nodo del grafo. Immutabile dopo la creazione.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub kind: EntityKind,
    /// Unico nel sistema. Es: `src::services::user_service::UserService::createUser`.
    pub qualified_name: String,
    pub location: SourceLocation,
    pub metadata: HashMap<String, String>,
}

/// Tipo di un arco del grafo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RelationKind {
    Calls,
    Imports,
    Implements,
    Extends,
    Tests,
    Uses,
    Creates,
    Modifies,
    BelongsTo,
    /// Relazione che il resolver non è riuscito a collegare a un'entità nota.
    ///
    /// **Non è un errore**: è un dato legittimo (librerie esterne, metaprogrammazione,
    /// dipendenze irrisolvibili). Va mostrato nell'UI e può essere annotato dal
    /// Memory Engine. Non deve mai diventare un `Result::Err` né causare panic.
    Unresolved,
}

/// Arco del grafo.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Relation {
    pub id: EntityId,
    pub kind: RelationKind,
    pub source_id: EntityId,
    pub target_id: EntityId,
    pub metadata: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Dati grezzi del parser
//
// DECISION: questi tipi vivono in `codeos-types`, non in `codeos-parser`.
// Sono il vocabolario condiviso tra il Parser (produttore) e il GraphResolver
// (consumatore) e viaggiano sull'event bus dentro `CodeOsEvent::FilesIndexed`.
// Tenendoli qui, l'event bus (anch'esso in `codeos-types`) può trasportarli
// senza che `codeos-types` debba dipendere da `codeos-parser` — il che
// violerebbe l'invariante 1.5. Il Parser continua a essere l'unico a produrli
// (invariante 1.4).
// ---------------------------------------------------------------------------

/// Risultato grezzo del parsing di un singolo file.
///
/// Contiene `local_id` (validi solo nel file) e `target_qualified_name`
/// (nomi così come scritti nel sorgente). Nessun `EntityId` globale: quello è
/// compito del `GraphResolver`.
#[derive(Debug, Clone, Default)]
pub struct ParsedFileResult {
    pub file_path: String,
    pub entities: Vec<ParsedEntity>,
    pub relations: Vec<ParsedRelation>,
    pub errors: Vec<ParseError>,
}

/// Un'entità così come emerge dall'AST, prima del name resolution.
#[derive(Debug, Clone)]
pub struct ParsedEntity {
    /// Identificativo unico solo all'interno di questo file. Formato libero,
    /// es. `"node_42"`.
    pub local_id: String,
    pub kind: EntityKind,
    pub name: String,
    pub parent_local_id: Option<String>,
    pub location: SourceLocation,
    pub metadata: HashMap<String, String>,
}

/// Una relazione così come emerge dall'AST, prima del name resolution.
#[derive(Debug, Clone)]
pub struct ParsedRelation {
    pub kind: RelationKind,
    pub source_local_id: String,
    /// Nome parziale o full-qualified scritto nel sorgente. Sarà risolto dal
    /// `GraphResolver`: non è ancora un `EntityId`.
    pub target_qualified_name: String,
    pub location: SourceLocation,
}

/// Errore non fatale incontrato durante il parsing di un file.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub location: Option<SourceLocation>,
}

// ---------------------------------------------------------------------------
// Delta del grafo
// ---------------------------------------------------------------------------

/// Insieme atomico di modifiche da applicare al grafo.
///
/// DECISION: `GraphDelta` vive in `codeos-types` (non in `codeos-storage`,
/// dove il briefing lo colloca a livello documentale) perché l'event bus lo
/// trasporta dentro `CodeOsEvent::GraphUpdated`. Lasciarlo in `codeos-storage`
/// costringerebbe `codeos-types` a dipendere da `codeos-storage`, violando
/// l'invariante 1.5. `codeos-storage` lo riusa come `codeos_types::GraphDelta`.
#[derive(Debug, Clone, Default)]
pub struct GraphDelta {
    pub added_entities: Vec<Entity>,
    pub removed_entity_ids: Vec<EntityId>,
    pub added_relations: Vec<Relation>,
    pub removed_relation_ids: Vec<EntityId>,
}

impl GraphDelta {
    /// `true` se il delta non comporta alcuna modifica.
    pub fn is_empty(&self) -> bool {
        self.added_entities.is_empty()
            && self.removed_entity_ids.is_empty()
            && self.added_relations.is_empty()
            && self.removed_relation_ids.is_empty()
    }
}
