//! `codeos-storage` — persistenza del grafo.
//!
//! Definisce il trait [`GraphStorage`] (l'astrazione che il resto del sistema
//! usa per leggere/scrivere il grafo) e la sua implementazione [`SqliteStorage`]
//! basata su `rusqlite` (feature `bundled`, nessun processo esterno).
//!
//! Il grafo vive in due tabelle, `entities` e `relations`. La scelta di SQLite è
//! deliberata per la v1: embedded, ACID, SQL completo. La migrazione a un graph
//! DB resta possibile perché tutto passa da questo trait (briefing sez. 15).

mod sqlite;

pub use sqlite::SqliteStorage;

use async_trait::async_trait;
use codeos_types::{Entity, EntityId, GraphDelta, RelationKind};

/// Filtro per interrogare le relazioni del grafo.
///
/// Tutti i campi sono opzionali e si combinano in AND. Il campo `depth` è pensato
/// per la BFS del Query Engine (Blocco 6): qui non viene usato per espandere il
/// risultato (una singola `query_relations` resta a un hop), ma è parte del
/// vocabolario condiviso del trait.
#[derive(Debug, Clone, Default)]
pub struct RelationFilter {
    pub source_id: Option<EntityId>,
    pub target_id: Option<EntityId>,
    pub kind: Option<RelationKind>,
    pub depth: Option<u32>,
}

/// L'astrazione di persistenza del grafo.
///
/// DECISION: il briefing (sez. 7.1) elenca anche `begin_transaction() ->
/// Box<dyn StorageTransaction>`. Con `rusqlite` una transazione prende in
/// prestito la `Connection`, quindi un oggetto-transazione boxato e `'static`
/// richiederebbe `unsafe` o una self-referential struct senza alcun consumatore
/// attuale. L'unica operazione che deve essere atomica è [`apply_delta`], che
/// gestisce la propria transazione internamente. Aggiungeremo un'API di
/// transazione esplicita solo se e quando un chiamante la richiederà.
///
/// [`apply_delta`]: GraphStorage::apply_delta
#[async_trait]
pub trait GraphStorage: Send + Sync {
    /// Applica un delta in modo atomico: prima le rimozioni, poi gli inserimenti.
    async fn apply_delta(&self, delta: GraphDelta) -> anyhow::Result<()>;

    /// Recupera un'entità dal suo `EntityId`.
    async fn get_entity_by_id(&self, id: &EntityId) -> anyhow::Result<Option<Entity>>;

    /// Recupera un'entità dal suo `qualified_name` (unico nel grafo).
    async fn get_entity_by_qname(&self, qname: &str) -> anyhow::Result<Option<Entity>>;

    /// Tutte le entità il cui `qualified_name` contiene `pattern` (LIKE `%pattern%`).
    async fn find_entities_by_name_pattern(&self, pattern: &str) -> anyhow::Result<Vec<Entity>>;

    /// Tutte le entità che vivono in un dato file. Serve al GraphResolver per la
    /// pulizia (Passo 0) prima di re-indicizzare un file.
    async fn get_entities_by_file(&self, file_path: &str) -> anyhow::Result<Vec<Entity>>;

    /// Le relazioni che soddisfano il filtro.
    async fn query_relations(
        &self,
        filter: RelationFilter,
    ) -> anyhow::Result<Vec<codeos_types::Relation>>;

    /// La **qualità del grafo**: contatori aggregati che dicono quanto fidarsi del
    /// dato (roadmap P2-7). È una pura lettura: alcune `SELECT COUNT(*)` sul grafo
    /// persistito, senza materializzare entità o relazioni in memoria.
    async fn graph_quality(&self) -> anyhow::Result<codeos_types::bus::GraphQualityInfo>;
}
