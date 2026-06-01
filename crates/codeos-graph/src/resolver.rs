//! `GraphResolver`: trasforma i `ParsedFileResult` grezzi in un [`GraphDelta`]
//! con `EntityId` globali (briefing sez. 7.2, l'algoritmo più critico).
//!
//! È il punto in cui i `local_id` di file e i `target_qualified_name` testuali
//! diventano nodi e archi del grafo. Il Parser non lo fa mai (invariante 1.4):
//! la separazione parsing/resolution è netta.

use std::collections::{HashMap, HashSet};

use codeos_storage::{GraphStorage, RelationFilter};
use codeos_types::{
    Entity, EntityId, GraphDelta, ParsedEntity, ParsedFileResult, Relation, RelationKind,
};

/// Risolve i risultati grezzi del parser in un delta del grafo.
pub struct GraphResolver {
    /// Prefisso da rimuovere dai path per costruire i `qualified_name` relativi.
    /// `None` ⇒ il path viene usato così com'è (normalizzato).
    project_root: Option<String>,
}

impl GraphResolver {
    pub fn new(project_root: Option<String>) -> Self {
        Self { project_root }
    }

    /// Esegue i 5 passi del briefing su tutti i file del batch e restituisce un
    /// unico `GraphDelta` atomico.
    pub async fn resolve(
        &self,
        results: &[ParsedFileResult],
        storage: &dyn GraphStorage,
    ) -> anyhow::Result<GraphDelta> {
        let mut delta = GraphDelta::default();

        // Indici globali sul batch: servono al name resolution (Passo 3) per
        // collegare chiamate fra entità appena create, ancora assenti dal DB.
        let mut new_by_qname: HashMap<String, EntityId> = HashMap::new();
        let mut new_by_name: HashMap<String, Vec<(EntityId, String)>> = HashMap::new();
        let mut new_by_id_lang: HashMap<EntityId, String> = HashMap::new();

        // Contesto per-file da rielaborare nel Passo 3 (dopo aver indicizzato
        // TUTTE le entità del batch).
        let mut file_ctxs: Vec<FileContext> = Vec::new();

        for file in results {
            let module_prefix = self.module_prefix(&file.file_path);

            // Passo 0 — Pulizia: rimuovi dal grafo le entità (e relazioni) già
            // presenti per questo file, così non si accumulano dati stantii.
            self.collect_removals(&file.file_path, storage, &mut delta)
                .await?;

            // Passo 1 — Creazione entità + costruzione della mappa local_id→EntityId.
            let mut local_map: HashMap<String, EntityId> = HashMap::new();
            let mut local_qname: HashMap<String, String> = HashMap::new();
            for parsed in &file.entities {
                let id = EntityId::new();
                let qname = self.qualified_name(parsed, &module_prefix, &local_qname);
                let lang = parsed.metadata.get("language").cloned().unwrap_or_else(|| detect_language(&file.file_path));

                local_map.insert(parsed.local_id.clone(), id);
                local_qname.insert(parsed.local_id.clone(), qname.clone());
                new_by_qname.insert(qname.clone(), id);
                new_by_name
                    .entry(parsed.name.clone())
                    .or_default()
                    .push((id, qname.clone()));
                new_by_id_lang.insert(id, lang);

                delta.added_entities.push(Entity {
                    id,
                    kind: parsed.kind,
                    qualified_name: qname,
                    location: parsed.location.clone(),
                    metadata: parsed.metadata.clone(),
                });
            }

            // Passo 2 — Relazioni BelongsTo (struttura figlio→genitore).
            for parsed in &file.entities {
                let (Some(parent_local), Some(child_id)) =
                    (&parsed.parent_local_id, local_map.get(&parsed.local_id))
                else {
                    continue;
                };
                let Some(parent_id) = local_map.get(parent_local) else {
                    continue;
                };
                delta.added_relations.push(Relation {
                    id: EntityId::new(),
                    kind: RelationKind::BelongsTo,
                    source_id: *child_id,
                    target_id: *parent_id,
                    metadata: HashMap::new(),
                });
            }

            // Namespace table del file: nome importato → target (per il Passo 3).
            let mut namespace: HashMap<String, String> = HashMap::new();
            for rel in &file.relations {
                if rel.kind == RelationKind::Imports {
                    if let Some(last) = last_segment(&rel.target_qualified_name) {
                        namespace.insert(last.to_string(), rel.target_qualified_name.clone());
                    }
                }
            }

            file_ctxs.push(FileContext {
                module_prefix,
                language: detect_language(&file.file_path),
                local_map,
                namespace,
                relations: file.relations.clone(),
            });
        }

        // Passo 3 — Name resolution: ora che l'indice del batch è completo,
        // risolvi ogni relazione del parser in un arco con EntityId.
        for ctx in &file_ctxs {
            for parsed in &ctx.relations {
                let Some(source_id) = ctx.local_map.get(&parsed.source_local_id) else {
                    tracing::warn!(
                        source = %parsed.source_local_id,
                        "relazione con sorgente sconosciuta, salto"
                    );
                    continue;
                };

                let resolved = resolve_target(
                    &parsed.target_qualified_name,
                    &ctx.module_prefix,
                    &ctx.language,
                    &ctx.namespace,
                    &new_by_qname,
                    &new_by_name,
                    &new_by_id_lang,
                    storage,
                )
                .await?;

                let relation = match resolved {
                    Some(target_id) => Relation {
                        id: EntityId::new(),
                        kind: parsed.kind,
                        source_id: *source_id,
                        target_id,
                        metadata: HashMap::new(),
                    },
                    None => {
                        // Passo 3.4 — Fallback Unresolved: NON è un errore. Il nome
                        // originale e il tipo di relazione mancato finiscono nei
                        // metadata, così l'UI e il Memory Engine possono mostrarli.
                        let mut metadata = HashMap::new();
                        metadata.insert(
                            "unresolved_target".to_string(),
                            parsed.target_qualified_name.clone(),
                        );
                        metadata.insert("original_kind".to_string(), format!("{:?}", parsed.kind));
                        Relation {
                            id: EntityId::new(),
                            kind: RelationKind::Unresolved,
                            source_id: *source_id,
                            target_id: EntityId::nil(),
                            metadata,
                        }
                    }
                };
                delta.added_relations.push(relation);
            }
        }

        Ok(delta)
    }

    /// Passo 0: raccoglie le entità e relazioni esistenti del file nel delta di
    /// rimozione.
    async fn collect_removals(
        &self,
        file_path: &str,
        storage: &dyn GraphStorage,
        delta: &mut GraphDelta,
    ) -> anyhow::Result<()> {
        let existing = storage.get_entities_by_file(file_path).await?;
        let mut relation_ids: HashSet<EntityId> = HashSet::new();
        for entity in &existing {
            for rel in storage
                .query_relations(RelationFilter {
                    source_id: Some(entity.id),
                    ..Default::default()
                })
                .await?
            {
                relation_ids.insert(rel.id);
            }
            for rel in storage
                .query_relations(RelationFilter {
                    target_id: Some(entity.id),
                    ..Default::default()
                })
                .await?
            {
                relation_ids.insert(rel.id);
            }
            delta.removed_entity_ids.push(entity.id);
        }
        delta.removed_relation_ids.extend(relation_ids);
        Ok(())
    }

    /// Passo 1: costruisce il `qualified_name` di un'entità.
    ///
    /// - L'entità `Module` radice ha come nome il prefisso del file stesso.
    /// - Ogni altra entità è `<qname del genitore>::<proprio nome>`. I genitori
    ///   sono sempre processati prima dei figli (il parser li emette in
    ///   quest'ordine), quindi `local_qname` del genitore è già disponibile.
    fn qualified_name(
        &self,
        parsed: &ParsedEntity,
        module_prefix: &str,
        local_qname: &HashMap<String, String>,
    ) -> String {
        match &parsed.parent_local_id {
            None => module_prefix.to_string(),
            Some(parent_local) => match local_qname.get(parent_local) {
                Some(parent_qname) => format!("{parent_qname}::{}", parsed.name),
                None => format!("{module_prefix}::{}", parsed.name),
            },
        }
    }

    /// Path del file → prefisso modulo: rimuove la root del progetto e
    /// l'estensione, e sostituisce i separatori con `::`.
    /// Es. `src/services/user_service.py` → `src::services::user_service`.
    fn module_prefix(&self, file_path: &str) -> String {
        let mut path = file_path;
        if let Some(root) = &self.project_root {
            if let Some(stripped) = path.strip_prefix(root.as_str()) {
                path = stripped;
            }
        }
        let trimmed = path.trim_start_matches(['/', '\\']);
        let mut parts: Vec<String> = trimmed
            .split(['/', '\\'])
            .filter(|s| !s.is_empty() && *s != ".")
            .map(|s| s.to_string())
            .collect();
        if let Some(last) = parts.last_mut() {
            if let Some((stem, _ext)) = last.rsplit_once('.') {
                if !stem.is_empty() {
                    *last = stem.to_string();
                }
            }
        }
        parts.join("::")
    }
}

/// Stato per-file mantenuto tra il Passo 1-2 e il Passo 3.
struct FileContext {
    module_prefix: String,
    language: String,
    local_map: HashMap<String, EntityId>,
    namespace: HashMap<String, String>,
    relations: Vec<codeos_types::ParsedRelation>,
}

/// Algoritmo a cascata del Passo 3 (briefing sez. 7.2). Restituisce `None` se
/// nessuno stadio risolve il target (⇒ relazione `Unresolved`).
async fn resolve_target(
    target: &str,
    module_prefix: &str,
    src_language: &str,
    namespace: &HashMap<String, String>,
    new_by_qname: &HashMap<String, EntityId>,
    new_by_name: &HashMap<String, Vec<(EntityId, String)>>,
    new_by_id_lang: &HashMap<EntityId, String>,
    storage: &dyn GraphStorage,
) -> anyhow::Result<Option<EntityId>> {
    // 0 — Import path normalisation. I parser mantengono i target come li scrive
    // il linguaggio (`crate::x`, `codeos_types::x`, `./client`); qui li traduciamo
    // nel namespace interno basato sui path (`crates::codeos-types::src::...`).
    for candidate in target_candidates(target, module_prefix) {
        if let Some(id) = lookup_progressive(&candidate, src_language, new_by_qname, new_by_id_lang, storage).await? {
            return Ok(Some(id));
        }
    }

    // 1 — Full-qualified match (sia sul batch sia sul DB).
    if let Some(id) = lookup_exact(target, src_language, new_by_qname, new_by_id_lang, storage).await? {
        return Ok(Some(id));
    }
    // Le `call` usano `.`, i nostri qualified_name usano `::`: prova la variante.
    let colonized = target.replace('.', "::");
    if colonized != target {
        if let Some(id) = lookup_exact(&colonized, src_language, new_by_qname, new_by_id_lang, storage).await? {
            return Ok(Some(id));
        }
    }

    // 2 — Import-based match: se il primo segmento è importato, sostituiscine il
    // prefisso col target dell'import e riprova il match esatto.
    if let Some(seg) = first_segment(target) {
        if let Some(full) = namespace.get(seg) {
            let remainder = &target[seg.len()..];
            let candidate = format!("{full}{remainder}").replace('.', "::");
            if let Some(id) = lookup_exact(&candidate, src_language, new_by_qname, new_by_id_lang, storage).await? {
                return Ok(Some(id));
            }
        }
    }

    // 3 — Scope-local match: per nome semplice, preferendo lo stesso modulo.
    let bare = last_segment(target).unwrap_or(target);
    if let Some(candidates) = new_by_name.get(bare) {
        let lang_candidates: Vec<(EntityId, String)> = candidates
            .iter()
            .filter(|(id, _)| {
                if let Some(t_lang) = new_by_id_lang.get(id) {
                    language_matches(src_language, t_lang)
                } else {
                    false
                }
            })
            .cloned()
            .collect();

        if let Some((id, _)) = lang_candidates
            .iter()
            .find(|(_, qname)| qname.starts_with(module_prefix))
        {
            return Ok(Some(*id));
        }
        if let Some((id, _)) = lang_candidates.first() {
            return Ok(Some(*id));
        }
    }
    let suffix = format!("::{bare}");
    let db_hits = storage.find_entities_by_name_pattern(bare).await?;
    let exact: Vec<&Entity> = db_hits
        .iter()
        .filter(|e| e.qualified_name == bare || e.qualified_name.ends_with(&suffix))
        .filter(|e| {
            let t_lang = e.metadata.get("language").cloned().unwrap_or_else(|| detect_language(&e.location.file_path));
            language_matches(src_language, &t_lang)
        })
        .collect();
    if let Some(e) = exact
        .iter()
        .find(|e| e.qualified_name.starts_with(module_prefix))
    {
        return Ok(Some(e.id));
    }
    if let Some(e) = exact.first() {
        return Ok(Some(e.id));
    }

    // 4 — Nessun match: Unresolved.
    Ok(None)
}

fn target_candidates(target: &str, module_prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let normalized = target.replace('.', "::").replace('/', "::");
    if normalized != target {
        out.push(normalized);
    }

    if target.starts_with('.') {
        if let Some(relative) = relative_candidate(target, module_prefix) {
            out.push(relative);
        }
    }

    if let Some(rest) = target.strip_prefix("crate::") {
        if let Some(prefix) = crate_src_prefix(module_prefix) {
            out.push(format!("{prefix}::{rest}").replace('.', "::"));
        }
    } else if let Some(rest) = target.strip_prefix("self::") {
        out.push(format!("{module_prefix}::{rest}").replace('.', "::"));
    } else if let Some(rest) = target.strip_prefix("super::") {
        if let Some(parent) = parent_module_prefix(module_prefix) {
            out.push(format!("{parent}::{rest}").replace('.', "::"));
        }
    } else if let Some((crate_name, rest)) = target.split_once("::") {
        if is_internal_crate_name(crate_name) {
            let dir = crate_name.replace('_', "-");
            if rest.is_empty() {
                out.push(format!("crates::{dir}::src::lib"));
            } else {
                out.push(format!("crates::{dir}::src::lib::{rest}").replace('.', "::"));
                if rest
                    .split("::")
                    .next()
                    .is_some_and(|seg| starts_lowercase(seg))
                {
                    out.push(format!("crates::{dir}::src::{rest}").replace('.', "::"));
                }
            }
        }
    }

    dedupe(out)
}

fn relative_candidate(target: &str, module_prefix: &str) -> Option<String> {
    let mut base: Vec<&str> = module_prefix.split("::").collect();
    base.pop(); // file corrente
    let mut tail = target;
    while let Some(rest) = tail.strip_prefix("../") {
        base.pop();
        tail = rest;
    }
    while let Some(rest) = tail.strip_prefix("./") {
        tail = rest;
    }
    let tail = strip_known_extension(tail);
    if tail.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = base.into_iter().map(str::to_string).collect();
    parts.extend(
        tail.split('/')
            .filter(|seg| !seg.is_empty() && *seg != ".")
            .map(str::to_string),
    );
    Some(parts.join("::"))
}

fn strip_known_extension(path: &str) -> &str {
    for ext in [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".rs", ".py"] {
        if let Some(stripped) = path.strip_suffix(ext) {
            return stripped;
        }
    }
    path
}

fn crate_src_prefix(module_prefix: &str) -> Option<String> {
    let parts: Vec<&str> = module_prefix.split("::").collect();
    let crates_pos = parts.iter().position(|part| *part == "crates")?;
    if parts.len() <= crates_pos + 2 || parts.get(crates_pos + 2) != Some(&"src") {
        return None;
    }
    Some(parts[..=crates_pos + 2].join("::"))
}

fn parent_module_prefix(module_prefix: &str) -> Option<String> {
    let mut parts: Vec<&str> = module_prefix.split("::").collect();
    if parts.len() <= 1 {
        return None;
    }
    parts.pop();
    Some(parts.join("::"))
}

fn is_internal_crate_name(name: &str) -> bool {
    name.starts_with("codeos_")
}

fn starts_lowercase(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_lowercase() || ch == '_')
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        if !value.is_empty() && seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}

async fn lookup_exact(
    qname: &str,
    src_language: &str,
    new_by_qname: &HashMap<String, EntityId>,
    new_by_id_lang: &HashMap<EntityId, String>,
    storage: &dyn GraphStorage,
) -> anyhow::Result<Option<EntityId>> {
    if let Some(id) = new_by_qname.get(qname) {
        if let Some(t_lang) = new_by_id_lang.get(id) {
            if language_matches(src_language, t_lang) {
                return Ok(Some(*id));
            }
        }
    }
    if let Some(entity) = storage.get_entity_by_qname(qname).await? {
        let t_lang = entity.metadata.get("language").cloned().unwrap_or_else(|| detect_language(&entity.location.file_path));
        if language_matches(src_language, &t_lang) {
            return Ok(Some(entity.id));
        }
    }
    Ok(None)
}

async fn lookup_progressive(
    qname: &str,
    src_language: &str,
    new_by_qname: &HashMap<String, EntityId>,
    new_by_id_lang: &HashMap<EntityId, String>,
    storage: &dyn GraphStorage,
) -> anyhow::Result<Option<EntityId>> {
    let mut current = qname.to_string();
    loop {
        if let Some(id) = lookup_exact(&current, src_language, new_by_qname, new_by_id_lang, storage).await? {
            return Ok(Some(id));
        }
        let Some((parent, _last)) = current.rsplit_once("::") else {
            return Ok(None);
        };
        current = parent.to_string();
    }
}

fn detect_language(file_path: &str) -> String {
    let ext = file_path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "rs" => "rust".to_string(),
        "py" => "python".to_string(),
        "ts" | "tsx" | "mts" | "cts" => "typescript".to_string(),
        "js" | "jsx" | "mjs" | "cjs" => "javascript".to_string(),
        _ => "unknown".to_string(),
    }
}

fn language_matches(lang_a: &str, lang_b: &str) -> bool {
    if lang_a == lang_b {
        return true;
    }
    let is_web_a = lang_a == "typescript" || lang_a == "javascript";
    let is_web_b = lang_b == "typescript" || lang_b == "javascript";
    is_web_a && is_web_b
}

fn first_segment(target: &str) -> Option<&str> {
    target.split(['.', ':']).find(|s| !s.is_empty())
}

fn last_segment(target: &str) -> Option<&str> {
    target.rsplit(['.', ':']).find(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_parser::{LanguageParser, PythonParser, RustParser, TypeScriptParser};
    use codeos_storage::SqliteStorage;
    use codeos_types::EntityKind;
    use std::path::Path;

    async fn parse(path: &str, src: &str) -> ParsedFileResult {
        PythonParser::new().parse_file(Path::new(path), src).await
    }

    async fn parse_rust(path: &str, src: &str) -> ParsedFileResult {
        RustParser::new().parse_file(Path::new(path), src).await
    }

    async fn parse_ts(path: &str, src: &str) -> ParsedFileResult {
        TypeScriptParser::new()
            .parse_file(Path::new(path), src)
            .await
    }

    fn find<'a>(delta: &'a GraphDelta, qname: &str) -> &'a Entity {
        delta
            .added_entities
            .iter()
            .find(|e| e.qualified_name == qname)
            .unwrap_or_else(|| panic!("entità '{qname}' assente nel delta"))
    }

    #[tokio::test]
    async fn builds_qualified_names_and_belongs_to() {
        let src = "class UserService:\n    def create_user(self):\n        pass\n";
        let parsed = parse("src/services/user_service.py", src).await;
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        let delta = resolver.resolve(&[parsed], &storage).await.unwrap();

        let module = find(&delta, "src::services::user_service");
        assert_eq!(module.kind, EntityKind::Module);
        let class = find(&delta, "src::services::user_service::UserService");
        assert_eq!(class.kind, EntityKind::Class);
        let method = find(
            &delta,
            "src::services::user_service::UserService::create_user",
        );
        assert_eq!(method.kind, EntityKind::Method);

        // BelongsTo: il metodo appartiene alla classe.
        let belongs = delta
            .added_relations
            .iter()
            .filter(|r| r.kind == RelationKind::BelongsTo)
            .count();
        assert_eq!(belongs, 2); // class→module, method→class (il modulo non ha parent)
        assert!(delta
            .added_relations
            .iter()
            .any(|r| r.kind == RelationKind::BelongsTo
                && r.source_id == method.id
                && r.target_id == class.id));
    }

    #[tokio::test]
    async fn resolves_intra_module_call_by_simple_name() {
        // `top_level` chiama `helper`, entrambe nello stesso modulo.
        let src = "def helper():\n    pass\n\ndef top_level():\n    helper()\n";
        let parsed = parse("m.py", src).await;
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        let delta = resolver.resolve(&[parsed], &storage).await.unwrap();
        let helper = find(&delta, "m::helper");
        let top = find(&delta, "m::top_level");

        let call = delta
            .added_relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls && r.source_id == top.id)
            .expect("la call risolta a 'helper' è assente");
        assert_eq!(call.target_id, helper.id);
    }

    #[tokio::test]
    async fn resolves_rust_crate_paths_to_internal_modules() {
        let parsed = vec![
            parse_rust(
                "/repo/crates/codeos-types/src/bus.rs",
                "pub struct Command;\n",
            )
            .await,
            parse_rust(
                "/repo/crates/codeos-core/src/lib.rs",
                "use codeos_types::bus::Command;\npub fn route() {}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();

        let source = find(&delta, "crates::codeos-core::src::lib");
        let target = find(&delta, "crates::codeos-types::src::bus::Command");
        assert!(delta.added_relations.iter().any(|r| {
            r.kind == RelationKind::Imports && r.source_id == source.id && r.target_id == target.id
        }));
    }

    #[tokio::test]
    async fn resolves_typescript_relative_imports_to_modules() {
        let parsed = vec![
            parse_ts(
                "/repo/vscode-extension/src/client.ts",
                "export class CodeOsClient {}\n",
            )
            .await,
            parse_ts(
                "/repo/vscode-extension/src/extension.ts",
                "import { CodeOsClient } from './client';\nexport function activate() { new CodeOsClient(); }\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();

        let source = find(&delta, "vscode-extension::src::extension");
        let target = find(&delta, "vscode-extension::src::client");
        assert!(delta.added_relations.iter().any(|r| {
            r.kind == RelationKind::Imports && r.source_id == source.id && r.target_id == target.id
        }));
    }

    #[tokio::test]
    async fn external_import_becomes_unresolved_not_an_error() {
        let src = "import os\n\ndef f():\n    os.getcwd()\n";
        let parsed = parse("m.py", src).await;
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        let delta = resolver.resolve(&[parsed], &storage).await.unwrap();
        let unresolved: Vec<&Relation> = delta
            .added_relations
            .iter()
            .filter(|r| r.kind == RelationKind::Unresolved)
            .collect();
        assert!(
            !unresolved.is_empty(),
            "ci aspettavamo almeno una relazione Unresolved (os.getcwd)"
        );
        // L'Unresolved porta il nome originale nei metadata e un target nullo.
        assert!(unresolved
            .iter()
            .all(|r| r.target_id.is_nil() && r.metadata.contains_key("unresolved_target")));
    }

    #[tokio::test]
    async fn reindex_removes_stale_entities() {
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        // Prima indicizzazione: una classe `Old`.
        let first = resolver
            .resolve(&[parse("m.py", "class Old:\n    pass\n").await], &storage)
            .await
            .unwrap();
        storage.apply_delta(first).await.unwrap();
        assert!(storage
            .get_entity_by_qname("m::Old")
            .await
            .unwrap()
            .is_some());

        // Ri-indicizzazione dello stesso file con una classe diversa `New`.
        let second = resolver
            .resolve(&[parse("m.py", "class New:\n    pass\n").await], &storage)
            .await
            .unwrap();
        // Il delta deve rimuovere le entità del file precedente.
        assert!(!second.removed_entity_ids.is_empty());
        storage.apply_delta(second).await.unwrap();

        assert!(storage
            .get_entity_by_qname("m::Old")
            .await
            .unwrap()
            .is_none());
        assert!(storage
            .get_entity_by_qname("m::New")
            .await
            .unwrap()
            .is_some());
    }
}
