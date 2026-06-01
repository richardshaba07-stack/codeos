//! Implementazione SQLite di [`GraphStorage`](crate::GraphStorage).
//!
//! `rusqlite` è sincrono: avvolgiamo la `Connection` in uno `std::sync::Mutex`
//! (così `SqliteStorage` è `Send + Sync` e può stare in un `Arc<dyn
//! GraphStorage>`). Ogni metodo `async` blocca il mutex, esegue il lavoro
//! sincrono e rilascia il guard prima di tornare: non c'è mai un `.await` con il
//! lock in mano. Per la v1 questo è corretto e sufficiente (briefing sez. 14:
//! prima correttezza, poi performance).

use std::sync::Mutex;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use codeos_types::{
    Entity, EntityId, EntityKind, GraphDelta, Relation, RelationKind, SourceLocation,
};
use rusqlite::{params, params_from_iter, Connection};
use uuid::Uuid;

use crate::{GraphStorage, RelationFilter};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS entities (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    qualified_name TEXT UNIQUE NOT NULL,
    file_path     TEXT NOT NULL,
    start_line    INTEGER NOT NULL,
    start_column  INTEGER NOT NULL,
    end_line      INTEGER NOT NULL,
    end_column    INTEGER NOT NULL,
    metadata      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS relations (
    id        TEXT PRIMARY KEY,
    kind      TEXT NOT NULL,
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    metadata  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_entities_file ON entities(file_path);
CREATE INDEX IF NOT EXISTS idx_entities_qname ON entities(qualified_name);
CREATE INDEX IF NOT EXISTS idx_relations_source ON relations(source_id);
CREATE INDEX IF NOT EXISTS idx_relations_target ON relations(target_id);
"#;

/// Lo storage del grafo su SQLite.
pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    /// Apre (o crea) un database su file e inizializza lo schema.
    pub fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let conn = Connection::open(path).context("apertura del database SQLite fallita")?;
        Self::from_connection(conn)
    }

    /// Un database in memoria: ideale per i test e per i run effimeri. Persiste
    /// finché vive la `Connection`.
    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory().context("creazione del DB in memoria fallita")?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(SCHEMA)
            .context("creazione dello schema del grafo fallita")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

#[async_trait]
impl GraphStorage for SqliteStorage {
    async fn apply_delta(&self, delta: GraphDelta) -> anyhow::Result<()> {
        let mut guard = self.conn.lock().expect("mutex SQLite avvelenato");
        let tx = guard
            .transaction()
            .context("apertura transazione fallita")?;

        // Ordine: prima le relazioni rimosse, poi le entità rimosse, poi le
        // entità aggiunte, infine le relazioni aggiunte (che possono referenziare
        // entità appena inserite).
        for rel_id in &delta.removed_relation_ids {
            tx.execute(
                "DELETE FROM relations WHERE id = ?1",
                params![rel_id.0.to_string()],
            )
            .context("rimozione relazione fallita")?;
        }
        for ent_id in &delta.removed_entity_ids {
            tx.execute(
                "DELETE FROM entities WHERE id = ?1",
                params![ent_id.0.to_string()],
            )
            .context("rimozione entità fallita")?;
        }
        for entity in &delta.added_entities {
            let metadata = serde_json::to_string(&entity.metadata)
                .context("serializzazione metadata entità fallita")?;
            tx.execute(
                "INSERT INTO entities \
                 (id, kind, qualified_name, file_path, start_line, start_column, end_line, end_column, metadata) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    entity.id.0.to_string(),
                    entity_kind_to_str(entity.kind),
                    entity.qualified_name,
                    entity.location.file_path,
                    entity.location.start_line,
                    entity.location.start_column,
                    entity.location.end_line,
                    entity.location.end_column,
                    metadata,
                ],
            )
            .with_context(|| format!("inserimento entità '{}' fallito", entity.qualified_name))?;
        }
        for relation in &delta.added_relations {
            let metadata = serde_json::to_string(&relation.metadata)
                .context("serializzazione metadata relazione fallita")?;
            tx.execute(
                "INSERT INTO relations (id, kind, source_id, target_id, metadata) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    relation.id.0.to_string(),
                    relation_kind_to_str(relation.kind),
                    relation.source_id.0.to_string(),
                    relation.target_id.0.to_string(),
                    metadata,
                ],
            )
            .context("inserimento relazione fallito")?;
        }

        tx.commit().context("commit della transazione fallito")?;
        Ok(())
    }

    async fn get_entity_by_id(&self, id: &EntityId) -> anyhow::Result<Option<Entity>> {
        self.query_one_entity(
            "SELECT id, kind, qualified_name, file_path, start_line, start_column, end_line, end_column, metadata \
             FROM entities WHERE id = ?1",
            id.0.to_string(),
        )
    }

    async fn get_entity_by_qname(&self, qname: &str) -> anyhow::Result<Option<Entity>> {
        self.query_one_entity(
            "SELECT id, kind, qualified_name, file_path, start_line, start_column, end_line, end_column, metadata \
             FROM entities WHERE qualified_name = ?1",
            qname.to_string(),
        )
    }

    async fn find_entities_by_name_pattern(&self, pattern: &str) -> anyhow::Result<Vec<Entity>> {
        self.query_entities(
            "SELECT id, kind, qualified_name, file_path, start_line, start_column, end_line, end_column, metadata \
             FROM entities WHERE qualified_name LIKE ?1",
            params_from_iter([format!("%{pattern}%")]),
        )
    }

    async fn get_entities_by_file(&self, file_path: &str) -> anyhow::Result<Vec<Entity>> {
        self.query_entities(
            "SELECT id, kind, qualified_name, file_path, start_line, start_column, end_line, end_column, metadata \
             FROM entities WHERE file_path = ?1",
            params_from_iter([file_path.to_string()]),
        )
    }

    async fn query_relations(&self, filter: RelationFilter) -> anyhow::Result<Vec<Relation>> {
        let mut clauses: Vec<&str> = Vec::new();
        let mut values: Vec<String> = Vec::new();
        if let Some(source) = filter.source_id {
            clauses.push("source_id = ?");
            values.push(source.0.to_string());
        }
        if let Some(target) = filter.target_id {
            clauses.push("target_id = ?");
            values.push(target.0.to_string());
        }
        if let Some(kind) = filter.kind {
            clauses.push("kind = ?");
            values.push(relation_kind_to_str(kind).to_string());
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let sql =
            format!("SELECT id, kind, source_id, target_id, metadata FROM relations{where_clause}");

        let guard = self.conn.lock().expect("mutex SQLite avvelenato");
        let mut stmt = guard
            .prepare(&sql)
            .context("prepare query relazioni fallito")?;
        let rows = stmt
            .query_map(params_from_iter(values.iter()), |row| {
                Ok(RawRelation {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    source_id: row.get(2)?,
                    target_id: row.get(3)?,
                    metadata: row.get(4)?,
                })
            })
            .context("query relazioni fallita")?
            .collect::<Result<Vec<_>, _>>()
            .context("lettura righe relazioni fallita")?;
        drop(stmt);
        drop(guard);

        rows.into_iter().map(raw_to_relation).collect()
    }
}

impl SqliteStorage {
    fn query_one_entity(&self, sql: &str, key: String) -> anyhow::Result<Option<Entity>> {
        Ok(self
            .query_entities(sql, params_from_iter([key]))?
            .into_iter()
            .next())
    }

    fn query_entities<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> anyhow::Result<Vec<Entity>> {
        let guard = self.conn.lock().expect("mutex SQLite avvelenato");
        let mut stmt = guard.prepare(sql).context("prepare query entità fallito")?;
        let rows = stmt
            .query_map(params, |row| {
                Ok(RawEntity {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    qualified_name: row.get(2)?,
                    file_path: row.get(3)?,
                    start_line: row.get(4)?,
                    start_column: row.get(5)?,
                    end_line: row.get(6)?,
                    end_column: row.get(7)?,
                    metadata: row.get(8)?,
                })
            })
            .context("query entità fallita")?
            .collect::<Result<Vec<_>, _>>()
            .context("lettura righe entità fallita")?;
        drop(stmt);
        drop(guard);

        rows.into_iter().map(raw_to_entity).collect()
    }
}

/// Riga grezza della tabella `entities`, prima della deserializzazione tipata.
struct RawEntity {
    id: String,
    kind: String,
    qualified_name: String,
    file_path: String,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
    metadata: String,
}

/// Riga grezza della tabella `relations`.
struct RawRelation {
    id: String,
    kind: String,
    source_id: String,
    target_id: String,
    metadata: String,
}

fn raw_to_entity(raw: RawEntity) -> anyhow::Result<Entity> {
    Ok(Entity {
        id: parse_entity_id(&raw.id)?,
        kind: entity_kind_from_str(&raw.kind)?,
        qualified_name: raw.qualified_name,
        location: SourceLocation {
            file_path: raw.file_path,
            start_line: raw.start_line,
            start_column: raw.start_column,
            end_line: raw.end_line,
            end_column: raw.end_column,
        },
        metadata: serde_json::from_str(&raw.metadata)
            .context("deserializzazione metadata entità fallita")?,
    })
}

fn raw_to_relation(raw: RawRelation) -> anyhow::Result<Relation> {
    Ok(Relation {
        id: parse_entity_id(&raw.id)?,
        kind: relation_kind_from_str(&raw.kind)?,
        source_id: parse_entity_id(&raw.source_id)?,
        target_id: parse_entity_id(&raw.target_id)?,
        metadata: serde_json::from_str(&raw.metadata)
            .context("deserializzazione metadata relazione fallita")?,
    })
}

fn parse_entity_id(s: &str) -> anyhow::Result<EntityId> {
    Uuid::parse_str(s)
        .map(EntityId)
        .with_context(|| format!("EntityId non valido nel DB: '{s}'"))
}

fn entity_kind_to_str(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Project => "Project",
        EntityKind::Module => "Module",
        EntityKind::Class => "Class",
        EntityKind::Struct => "Struct",
        EntityKind::Interface => "Interface",
        EntityKind::Function => "Function",
        EntityKind::Method => "Method",
        EntityKind::Variable => "Variable",
        EntityKind::Parameter => "Parameter",
        EntityKind::Test => "Test",
        EntityKind::ExternalDependency => "ExternalDependency",
    }
}

fn entity_kind_from_str(s: &str) -> anyhow::Result<EntityKind> {
    Ok(match s {
        "Project" => EntityKind::Project,
        "Module" => EntityKind::Module,
        "Class" => EntityKind::Class,
        "Struct" => EntityKind::Struct,
        "Interface" => EntityKind::Interface,
        "Function" => EntityKind::Function,
        "Method" => EntityKind::Method,
        "Variable" => EntityKind::Variable,
        "Parameter" => EntityKind::Parameter,
        "Test" => EntityKind::Test,
        "ExternalDependency" => EntityKind::ExternalDependency,
        other => return Err(anyhow!("EntityKind sconosciuto nel DB: '{other}'")),
    })
}

fn relation_kind_to_str(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Calls => "Calls",
        RelationKind::Imports => "Imports",
        RelationKind::Implements => "Implements",
        RelationKind::Extends => "Extends",
        RelationKind::Tests => "Tests",
        RelationKind::Uses => "Uses",
        RelationKind::Creates => "Creates",
        RelationKind::Modifies => "Modifies",
        RelationKind::BelongsTo => "BelongsTo",
        RelationKind::Unresolved => "Unresolved",
    }
}

fn relation_kind_from_str(s: &str) -> anyhow::Result<RelationKind> {
    Ok(match s {
        "Calls" => RelationKind::Calls,
        "Imports" => RelationKind::Imports,
        "Implements" => RelationKind::Implements,
        "Extends" => RelationKind::Extends,
        "Tests" => RelationKind::Tests,
        "Uses" => RelationKind::Uses,
        "Creates" => RelationKind::Creates,
        "Modifies" => RelationKind::Modifies,
        "BelongsTo" => RelationKind::BelongsTo,
        "Unresolved" => RelationKind::Unresolved,
        other => return Err(anyhow!("RelationKind sconosciuto nel DB: '{other}'")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entity(qname: &str, kind: EntityKind, file: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind,
            qualified_name: qname.to_string(),
            location: SourceLocation {
                file_path: file.to_string(),
                start_line: 1,
                start_column: 0,
                end_line: 2,
                end_column: 0,
            },
            metadata: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn insert_then_read_back_entities_and_relations() {
        let storage = SqliteStorage::in_memory().unwrap();
        let module = entity("pkg::m", EntityKind::Module, "pkg/m.py");
        let class = entity("pkg::m::Foo", EntityKind::Class, "pkg/m.py");
        let belongs = Relation {
            id: EntityId::new(),
            kind: RelationKind::BelongsTo,
            source_id: class.id,
            target_id: module.id,
            metadata: HashMap::new(),
        };

        storage
            .apply_delta(GraphDelta {
                added_entities: vec![module.clone(), class.clone()],
                added_relations: vec![belongs.clone()],
                ..Default::default()
            })
            .await
            .unwrap();

        let fetched = storage.get_entity_by_id(&class.id).await.unwrap().unwrap();
        assert_eq!(fetched.qualified_name, "pkg::m::Foo");
        assert_eq!(fetched.kind, EntityKind::Class);

        let by_qname = storage
            .get_entity_by_qname("pkg::m")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_qname.id, module.id);

        let rels = storage
            .query_relations(RelationFilter {
                source_id: Some(class.id),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].kind, RelationKind::BelongsTo);
        assert_eq!(rels[0].target_id, module.id);
    }

    #[tokio::test]
    async fn apply_delta_removes_before_inserting() {
        let storage = SqliteStorage::in_memory().unwrap();
        let old = entity("pkg::m::Old", EntityKind::Class, "pkg/m.py");
        storage
            .apply_delta(GraphDelta {
                added_entities: vec![old.clone()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Stesso file ri-indicizzato: rimuove la vecchia entità e ne aggiunge una
        // nuova con lo stesso qualified_name. Senza la rimozione, il vincolo UNIQUE
        // farebbe fallire l'inserimento.
        let new = entity("pkg::m::Old", EntityKind::Class, "pkg/m.py");
        storage
            .apply_delta(GraphDelta {
                removed_entity_ids: vec![old.id],
                added_entities: vec![new.clone()],
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(storage.get_entity_by_id(&old.id).await.unwrap().is_none());
        let current = storage
            .get_entity_by_qname("pkg::m::Old")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.id, new.id);
    }

    #[tokio::test]
    async fn find_by_name_pattern_matches_substring() {
        let storage = SqliteStorage::in_memory().unwrap();
        storage
            .apply_delta(GraphDelta {
                added_entities: vec![
                    entity("app::auth::login", EntityKind::Function, "app/auth.py"),
                    entity("app::user::profile", EntityKind::Function, "app/user.py"),
                ],
                ..Default::default()
            })
            .await
            .unwrap();

        let hits = storage
            .find_entities_by_name_pattern("login")
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].qualified_name, "app::auth::login");
    }

    #[tokio::test]
    async fn get_entities_by_file_scopes_to_one_file() {
        let storage = SqliteStorage::in_memory().unwrap();
        storage
            .apply_delta(GraphDelta {
                added_entities: vec![
                    entity("app::auth::login", EntityKind::Function, "app/auth.py"),
                    entity("app::user::profile", EntityKind::Function, "app/user.py"),
                ],
                ..Default::default()
            })
            .await
            .unwrap();

        let in_auth = storage.get_entities_by_file("app/auth.py").await.unwrap();
        assert_eq!(in_auth.len(), 1);
        assert_eq!(in_auth[0].qualified_name, "app::auth::login");
    }
}
