//! `GraphResolver`: trasforma i `ParsedFileResult` grezzi in un [`GraphDelta`]
//! con `EntityId` globali (briefing sez. 7.2, l'algoritmo più critico).
//!
//! È il punto in cui i `local_id` di file e i `target_qualified_name` testuali
//! diventano nodi e archi del grafo. Il Parser non lo fa mai (invariante 1.4):
//! la separazione parsing/resolution è netta.

use std::collections::{HashMap, HashSet};

use codeos_storage::{GraphStorage, RelationFilter};
use codeos_types::{
    CommitContext, Entity, EntityId, GraphDelta, ParsedEntity, ParsedFileResult, Relation,
    RelationKind,
};

/// Risolve i risultati grezzi del parser in un delta del grafo.
pub struct GraphResolver {
    /// Prefisso da rimuovere dai path per costruire i `qualified_name` relativi.
    /// `None` ⇒ il path viene usato così com'è (normalizzato).
    project_root: Option<String>,
    /// Commit con cui timbrare la *nascita* dei nodi (grafo temporale, vision step
    /// 2). `None` ⇒ niente timbro (nessun repo git, o non lo conosciamo): meglio un
    /// istante mancante che uno inventato.
    commit_context: Option<CommitContext>,
}

impl GraphResolver {
    pub fn new(project_root: Option<String>) -> Self {
        Self {
            project_root,
            commit_context: None,
        }
    }

    /// Imposta il commit con cui timbrare la nascita dei nodi creati in questo
    /// resolve. Builder additivo: chi non lo chiama (test, percorsi senza git)
    /// continua a non timbrare nulla, comportamento identico a prima.
    pub fn with_commit_context(mut self, commit_context: Option<CommitContext>) -> Self {
        self.commit_context = commit_context;
        self
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
        // Reverse del batch (id→qname): nel Passo 3 dà il `qualified_name` del
        // TARGET appena risolto senza una query, per timbrare l'identità stabile
        // dell'arco (`source_qname`/`target_qname`) e conservarne la nascita.
        let mut new_id_to_qname: HashMap<EntityId, String> = HashMap::new();
        // Mappa id→source_kind ("test"/"prod"): nel Passo 3 la consultiamo per
        // timbrare ogni arco di dipendenza con la natura del file SORGENTE, così il
        // Guardian può escludere dal mining gli archi nati nei test (vedi
        // `cross_layer`).
        let mut new_by_id_source_kind: HashMap<EntityId, String> = HashMap::new();
        // Nascita degli ARCHI risolti, raccolta nel Passo 0 dal re-index e
        // conservata fino al Passo 3 (invece di resettarla al commit corrente).
        // Chiave = identità stabile dell'arco `(source_qname, kind, target_qname)`,
        // l'unica che sopravvive al delete+recreate (gli `EntityId` si rigenerano).
        let mut edge_born: EdgeBornMap = HashMap::new();

        // Contesto per-file da rielaborare nel Passo 3 (dopo aver indicizzato
        // TUTTE le entità del batch).
        let mut file_ctxs: Vec<FileContext> = Vec::new();
        // Info ausiliarie (id, nome, rust_kind, file) raccolte nel Passo 1: servono
        // alla canonicalizzazione dei tipi (Passo 1.5) quando conosciamo ancora il
        // nome nudo e la natura (`struct`/`enum`/`impl_target`) di ogni entità.
        let mut entity_aux: Vec<EntityAux> = Vec::new();
        // Foglie dei moduli LOCALI del progetto (es. `eval` per `src/eval.rs`):
        // servono nel Passo 3 a NON esternalizzare una call il cui root è un modulo
        // nostro (`eval::matches_comparator`), che sarebbe sia un arco bugiardo verso
        // un `external::eval` inesistente sia una mancata risoluzione del fratello.
        let mut local_modules: HashSet<String> = HashSet::new();

        for (file_idx, file) in results.iter().enumerate() {
            let module_prefix = self.module_prefix(&file.file_path);
            // Natura del file (test vs prod) dedotta una sola volta dal path: la
            // ereditano tutte le entità del file e gli archi che ne escono.
            let file_source_kind =
                classify_source_kind(&file.file_path, &detect_language(&file.file_path));

            // Passo 0 — Pulizia: rimuovi dal grafo le entità (e relazioni) già
            // presenti per questo file, così non si accumulano dati stantii.
            // `born_map`: la nascita (commit+istante) delle entità preesistenti del
            // file, per conservarla nel re-index invece di resettarla (vedi Passo 1).
            let (born_map, file_edge_born) = self
                .collect_removals(&file.file_path, storage, &mut delta)
                .await?;
            edge_born.extend(file_edge_born);

            // Passo 1 — Creazione entità + costruzione della mappa local_id→EntityId.
            let mut local_map: HashMap<String, EntityId> = HashMap::new();
            let mut local_qname: HashMap<String, String> = HashMap::new();
            for parsed in &file.entities {
                let id = EntityId::new();
                let qname = self.qualified_name(parsed, &module_prefix, &local_qname);
                let lang = parsed
                    .metadata
                    .get("language")
                    .cloned()
                    .unwrap_or_else(|| detect_language(&file.file_path));

                local_map.insert(parsed.local_id.clone(), id);
                local_qname.insert(parsed.local_id.clone(), qname.clone());
                new_by_qname.insert(qname.clone(), id);
                new_id_to_qname.insert(id, qname.clone());
                new_by_name
                    .entry(parsed.name.clone())
                    .or_default()
                    .push((id, qname.clone()));
                new_by_id_lang.insert(id, lang);

                // source_kind: il parser può già averlo dedotto (es. `#[cfg(test)]`
                // inline); altrimenti vale la classificazione per path del file.
                let mut metadata = parsed.metadata.clone();
                let source_kind = metadata
                    .get("source_kind")
                    .cloned()
                    .unwrap_or_else(|| file_source_kind.to_string());
                metadata.insert("source_kind".to_string(), source_kind.clone());
                new_by_id_source_kind.insert(id, source_kind);

                // Nascita (grafo temporale, vision step 2): timbra commit+istante
                // SOLO se conosciamo il commit corrente. Su re-index conserva la
                // nascita preesistente di questa identità (`qname`) — non la resetta
                // al commit di adesso: un tipo che esiste da 100 commit non deve
                // sembrare "nuovo" dopo una re-indicizzazione (anti-rumore temporale).
                if let Some(cc) = &self.commit_context {
                    let (born_commit, born_ts) = born_map
                        .get(&qname)
                        .cloned()
                        .unwrap_or_else(|| (cc.commit.clone(), cc.ts));
                    metadata.insert("born_commit".to_string(), born_commit);
                    metadata.insert("born_ts".to_string(), born_ts.to_string());
                }

                entity_aux.push(EntityAux {
                    id,
                    name: parsed.name.clone(),
                    rust_kind: parsed
                        .metadata
                        .get("rust_kind")
                        .cloned()
                        .unwrap_or_default(),
                    file_idx,
                });

                if parsed.kind == codeos_types::EntityKind::Module {
                    if let Some(leaf) = last_segment(&qname) {
                        local_modules.insert(leaf.to_string());
                    }
                }

                delta.added_entities.push(Entity {
                    id,
                    kind: parsed.kind,
                    qualified_name: qname,
                    location: parsed.location.clone(),
                    metadata,
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
                local_qname,
                namespace,
                relations: file.relations.clone(),
            });
        }

        // Passo 1.5 — Canonicalizzazione anti-frammentazione: fonde i placeholder
        // `impl_target` nell'unica definizione reale omonima del batch, PRIMA della
        // name resolution (così ogni target risolve all'entità canonica, non a una
        // copia per-file).
        let merged_fragments = canonicalize_type_fragments(
            &mut delta,
            &mut new_by_qname,
            &mut new_by_name,
            &entity_aux,
            &file_ctxs,
            storage,
        )
        .await?;
        if merged_fragments > 0 {
            tracing::debug!(
                merged = merged_fragments,
                "canonicalizzati frammenti di tipo (un tipo = un'entità)"
            );
        }

        // Cache delle entità sintetiche per dipendenze esterne create in questo
        // batch (`external::tokio` → id), così più relazioni verso lo stesso
        // crate/pacchetto riusano lo stesso nodo. La persistenza fra batch è
        // garantita da `get_entity_by_qname` (le entità `<external>` non hanno un
        // file reale, quindi `collect_removals` non le rimuove mai).
        let mut external_cache: HashMap<String, EntityId> = HashMap::new();

        // Passo 3 — Name resolution: ora che l'indice del batch è completo,
        // risolvi ogni relazione del parser in un arco con EntityId.
        for ctx in &file_ctxs {
            // Contesto di risoluzione condiviso da tutte le relazioni del file: un
            // solo riferimento agli indici del batch invece di otto argomenti
            // ripetuti a ogni chiamata (clippy `too_many_arguments`).
            let rctx = ResolutionContext {
                module_prefix: ctx.module_prefix.as_str(),
                language: ctx.language.as_str(),
                namespace: &ctx.namespace,
                new_by_qname: &new_by_qname,
                new_by_name: &new_by_name,
                new_by_id_lang: &new_by_id_lang,
                storage,
            };
            for parsed in &ctx.relations {
                let Some(source_id) = ctx.local_map.get(&parsed.source_local_id) else {
                    tracing::warn!(
                        source = %parsed.source_local_id,
                        "relazione con sorgente sconosciuta, salto"
                    );
                    continue;
                };

                // Sanitizzazione (P0-1): un target con whitespace/newline o non
                // simbolico è rumore. Lo normalizziamo e, se resta vuoto o non è un
                // simbolo, scartiamo la relazione del tutto — niente arco, nemmeno
                // Unresolved.
                let Some(target) = sanitize_target(&parsed.target_qualified_name) else {
                    tracing::debug!(
                        raw = %parsed.target_qualified_name,
                        "target non simbolico dopo sanitizzazione, salto"
                    );
                    continue;
                };

                let resolved = resolve_target(&rctx, &target).await?;

                let mut relation = match resolved {
                    Some((target_id, strategy)) => {
                        // Ogni arco risolto porta la strategia e la confidenza con cui
                        // ci siamo arrivati: il Guardian le legge per escludere dal
                        // mining le relazioni a bassa confidenza (vedi `cross_layer`).
                        let mut metadata = HashMap::new();
                        metadata.insert(
                            "resolution_strategy".to_string(),
                            strategy.as_str().to_string(),
                        );
                        metadata.insert(
                            "resolution_confidence".to_string(),
                            strategy.confidence().to_string(),
                        );

                        // Grafo temporale (vision step 2) — identità STABILE + nascita
                        // dell'arco, SOLO per gli archi risolti. La coppia di
                        // `qualified_name` è l'identità che sopravvive al re-index (gli
                        // `EntityId` si rigenerano): la timbriamo nei metadata sia
                        // perché è il riferimento mostrabile da CLI/Memory Engine, sia
                        // perché è la chiave con cui CONSERVIAMO la nascita invece di
                        // resettarla. Unresolved (target nil) ed esterni restano fuori:
                        // una nascita mancante è meglio di una su rumore (trappola #3).
                        let source_qname = ctx.local_qname.get(&parsed.source_local_id).cloned();
                        let target_qname = match new_id_to_qname.get(&target_id) {
                            Some(q) => Some(q.clone()),
                            None => storage
                                .get_entity_by_id(&target_id)
                                .await?
                                .map(|e| e.qualified_name),
                        };
                        if let (Some(sq), Some(tq)) = (source_qname, target_qname) {
                            if let Some(cc) = &self.commit_context {
                                let (born_commit, born_ts) = edge_born
                                    .get(&(sq.clone(), parsed.kind, tq.clone()))
                                    .cloned()
                                    .unwrap_or_else(|| (cc.commit.clone(), cc.ts));
                                metadata.insert("born_commit".to_string(), born_commit);
                                metadata.insert("born_ts".to_string(), born_ts.to_string());
                            }
                            metadata.insert("source_qname".to_string(), sq);
                            metadata.insert("target_qname".to_string(), tq);
                        }

                        Relation {
                            id: EntityId::new(),
                            kind: parsed.kind,
                            source_id: *source_id,
                            target_id,
                            metadata,
                        }
                    }
                    None => {
                        // Passo 3.4 — Prima del fallback Unresolved, prova a
                        // riconoscere una dipendenza esterna (std/tokio/serde,
                        // react, @scope/pkg…). Se il target è un pacchetto fuori
                        // dal progetto, lo aggancio a un'entità sintetica stabile
                        // invece di buttarlo in un Unresolved con target nullo.
                        // Guardia anti-bugia (a2): una CALL il cui root è un modulo
                        // NOSTRO (`eval::matches_comparator`, con `src/eval.rs`
                        // indicizzato) NON va esternalizzata — sarebbe un arco verso un
                        // `external::eval` inesistente (bugia) e insieme una mancata
                        // risoluzione del fratello. La lasciamo cadere a Unresolved
                        // (onesto) invece di inventare una dipendenza. Gli Imports invece
                        // possono avere legittimamente root = modulo locale E crate
                        // esterno omonimo (es. `serde`), quindi restano esternalizzabili.
                        let root_is_local_module = parsed.kind != RelationKind::Imports
                            && first_segment(&target).is_some_and(|r| local_modules.contains(r));
                        let ext = if root_is_local_module {
                            None
                        } else {
                            external_dependency_root(
                                &target,
                                &ctx.language,
                                &ctx.namespace,
                                parsed.kind == RelationKind::Imports,
                            )
                        };
                        match ext {
                            Some(root) => {
                                let target_id = external_entity_id(
                                    &root,
                                    &ctx.language,
                                    &mut external_cache,
                                    &mut delta,
                                    storage,
                                )
                                .await?;
                                let mut metadata = HashMap::new();
                                metadata.insert("external_target".to_string(), target.clone());
                                // Il pacchetto di provenienza (crate/npm/modulo):
                                // chiave uniforme sia sull'arco sia sull'entità
                                // esterna, così CLI/plugin possono raggruppare le
                                // dipendenze per package senza riparsare il target.
                                metadata.insert("package".to_string(), root.clone());
                                metadata.insert(
                                    "resolution_strategy".to_string(),
                                    ResolutionStrategy::External.as_str().to_string(),
                                );
                                metadata.insert(
                                    "resolution_confidence".to_string(),
                                    ResolutionStrategy::External.confidence().to_string(),
                                );
                                Relation {
                                    id: EntityId::new(),
                                    kind: parsed.kind,
                                    source_id: *source_id,
                                    target_id,
                                    metadata,
                                }
                            }
                            None => {
                                // Fallback Unresolved: NON è un errore. Il nome
                                // originale e il tipo di relazione mancato finiscono
                                // nei metadata, così l'UI e il Memory Engine possono
                                // mostrarli.
                                let mut metadata = HashMap::new();
                                metadata.insert("unresolved_target".to_string(), target.clone());
                                metadata.insert(
                                    "original_kind".to_string(),
                                    format!("{:?}", parsed.kind),
                                );
                                metadata.insert(
                                    "resolution_strategy".to_string(),
                                    "unresolved".to_string(),
                                );
                                metadata.insert(
                                    "resolution_confidence".to_string(),
                                    "none".to_string(),
                                );
                                Relation {
                                    id: EntityId::new(),
                                    kind: RelationKind::Unresolved,
                                    source_id: *source_id,
                                    target_id: EntityId::nil(),
                                    metadata,
                                }
                            }
                        }
                    }
                };

                // Timbro la natura (test/prod) del file SORGENTE sull'arco: è il
                // segnale che il Guardian legge in `cross_layer` per non far entrare
                // gli archi dei test nel mining degli invarianti.
                if let Some(sk) = new_by_id_source_kind.get(source_id) {
                    relation
                        .metadata
                        .insert("source_kind".to_string(), sk.clone());
                }
                delta.added_relations.push(relation);
            }
        }

        Ok(delta)
    }

    /// Passo 0: raccoglie le entità e relazioni esistenti del file nel delta di
    /// rimozione e, già che le ha sotto mano, ne **raccoglie la nascita** per
    /// conservarla nel re-index — sia dei NODI (`qualified_name → (commit, ts)`)
    /// sia degli ARCHI risolti (identità `(source_qname, kind, target_qname) →
    /// (commit, ts)`, letta dai metadata che il Passo 3 vi ha timbrato).
    ///
    /// Il re-index sostituisce un file cancellando+ricreando le sue entità (con un
    /// `EntityId` nuovo): senza questa raccolta la nascita verrebbe resettata al
    /// commit corrente a ogni passaggio (rumore temporale). Raccogliendola dalle
    /// entità e relazioni che stiamo già leggendo per la rimozione, la
    /// conservazione costa **zero query aggiuntive**.
    async fn collect_removals(
        &self,
        file_path: &str,
        storage: &dyn GraphStorage,
        delta: &mut GraphDelta,
    ) -> anyhow::Result<BornHarvest> {
        let existing = storage.get_entities_by_file(file_path).await?;
        let mut born: HashMap<String, (String, i64)> = HashMap::new();
        // Relazioni da rimuovere, deduplicate per id: una stessa relazione può
        // toccare due entità del file (come sorgente e come destinazione).
        let mut rels: HashMap<EntityId, Relation> = HashMap::new();
        for entity in &existing {
            for rel in storage
                .query_relations(RelationFilter {
                    source_id: Some(entity.id),
                    ..Default::default()
                })
                .await?
            {
                rels.insert(rel.id, rel);
            }
            for rel in storage
                .query_relations(RelationFilter {
                    target_id: Some(entity.id),
                    ..Default::default()
                })
                .await?
            {
                rels.insert(rel.id, rel);
            }
            if let Some(b) = born_stamp(&entity.metadata) {
                born.insert(entity.qualified_name.clone(), b);
            }
            delta.removed_entity_ids.push(entity.id);
        }

        // Nascita degli archi RISOLTI: il Passo 3 ha timbrato `source_qname`/
        // `target_qname` sull'arco; con la sua stessa nascita formano l'identità
        // stabile che conserviamo. Gli archi privi di quella coppia (Unresolved,
        // esterni, o nati prima di questo schema) non contribuiscono — onesto: una
        // nascita mancante è meglio di una agganciata a un'identità che non c'è.
        let mut edge_born: EdgeBornMap = HashMap::new();
        for (id, rel) in &rels {
            delta.removed_relation_ids.push(*id);
            if let (Some(sq), Some(tq), Some(b)) = (
                rel.metadata.get("source_qname"),
                rel.metadata.get("target_qname"),
                born_stamp(&rel.metadata),
            ) {
                edge_born.insert((sq.clone(), rel.kind, tq.clone()), b);
            }
        }
        Ok((born, edge_born))
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
    /// `local_id → qualified_name` delle entità del file: nel Passo 3 dà il
    /// `source_qname` dell'arco (identità stabile lato sorgente) senza ricostruirlo.
    local_qname: HashMap<String, String>,
    namespace: HashMap<String, String>,
    relations: Vec<codeos_types::ParsedRelation>,
}

/// Info ausiliarie per la canonicalizzazione dei tipi (Passo 1.5), raccolte nel
/// Passo 1 quando ancora conosciamo `name`, `rust_kind` e il file di origine.
struct EntityAux {
    id: EntityId,
    name: String,
    rust_kind: String,
    file_idx: usize,
}

/// Passo 1.5 — Canonicalizzazione dell'identità di tipo (anti-frammentazione).
///
/// Il parser è per-file: un `impl Foo` in `parse.rs` che non vede la definizione
/// `struct Foo` crea un *placeholder* `parse::Foo` (`rust_kind=impl_target`). Lo
/// stesso tipo finisce così spezzato in N entità, una per file con un impl (su
/// semver `VersionReq` ×5). Qui fondiamo ogni placeholder nell'UNICA definizione
/// reale omonima del batch e ridirigiamo i suoi archi (i metodi diventano figli
/// del canonico).
///
/// Regola conservativa — tesi anti-falso-positivo, *un merge mancante batte un
/// merge bugiardo*:
/// - si fonde solo se esiste **esattamente una** definizione nominale
///   (`struct`/`enum`/`trait`) con quel nome nel batch; 0 o ≥2 ⇒ non si indovina;
/// - si salta se il file del placeholder **importa quel nome da un crate esterno**
///   (allora `impl … for Nome` riguarda il tipo esterno, non l'omonimo locale).
///
/// Re-index di un singolo file: la definizione canonica spesso NON è nel batch (vive
/// in un altro file, già persistito). In quel caso la cerchiamo nello storage con la
/// stessa unicità anti-bugia (vedi [`db_canonical_for`]), limitata al crate del
/// chiamante. L'indicizzazione full-project (caso normale) trova invece il canonico
/// direttamente nel batch.
///
/// Ritorna il numero di placeholder fusi.
async fn canonicalize_type_fragments(
    delta: &mut GraphDelta,
    new_by_qname: &mut HashMap<String, EntityId>,
    new_by_name: &mut HashMap<String, Vec<(EntityId, String)>>,
    aux: &[EntityAux],
    file_ctxs: &[FileContext],
    storage: &dyn GraphStorage,
) -> anyhow::Result<usize> {
    // 1. Definizioni nominali reali per nome (i candidati canonici): mai i
    //    placeholder (`impl_target`) né gli alias/associati (`type`).
    let mut canon_by_name: HashMap<&str, Vec<EntityId>> = HashMap::new();
    for a in aux {
        if matches!(a.rust_kind.as_str(), "struct" | "enum" | "trait") {
            canon_by_name.entry(a.name.as_str()).or_default().push(a.id);
        }
    }
    // 2. Decidi i merge: placeholder → canonico (unico e non importato da esterno).
    let mut merge: HashMap<EntityId, EntityId> = HashMap::new();
    for a in aux {
        if a.rust_kind != "impl_target" {
            continue;
        }
        let ctx = &file_ctxs[a.file_idx];
        // Guardia import-esterno: se il file importa il nome da un crate esterno,
        // l'`impl … for Nome` riguarda il tipo esterno, non l'omonimo locale. Vale
        // sia per il canonico nel batch sia per quello cercato nel DB.
        if let Some(target) = ctx.namespace.get(&a.name) {
            if external_dependency_root(target, &ctx.language, &ctx.namespace, true).is_some() {
                continue; // il nome è importato da un crate esterno
            }
        }
        let canonical = match canon_by_name.get(a.name.as_str()) {
            // Canonico nel batch (indicizzazione full-project): unico o si abdica.
            Some(cands) => {
                if cands.len() != 1 {
                    continue; // ≥2 omonimi nel progetto: ambiguo, non fondere
                }
                cands[0]
            }
            // Nessun canonico nel batch: nel re-index di un singolo file la
            // definizione vive già nello storage. La cerchiamo lì, limitata al crate
            // del chiamante e con la stessa unicità anti-bugia.
            None => match db_canonical_for(&a.name, ctx, storage).await? {
                Some(id) => id,
                None => continue, // 0 o ≥2 candidati nel DB: non si indovina
            },
        };
        if canonical == a.id {
            continue;
        }
        merge.insert(a.id, canonical);
    }
    if merge.is_empty() {
        return Ok(0);
    }
    // 3. Redirige gli indici di risoluzione (id placeholder → id canonico): un
    //    riferimento al tipo da QUALSIASI modulo risolve ora all'unico canonico.
    for id in new_by_qname.values_mut() {
        if let Some(c) = merge.get(id) {
            *id = *c;
        }
    }
    for cands in new_by_name.values_mut() {
        for (id, _q) in cands.iter_mut() {
            if let Some(c) = merge.get(id) {
                *id = *c;
            }
        }
    }
    // 4. Elimina le entità placeholder fuse.
    delta.added_entities.retain(|e| !merge.contains_key(&e.id));
    // 5. Aggiusta le relazioni. L'unico arco USCENTE di un placeholder sintetico è
    //    il suo BelongsTo→modulo: il canonico ha già il proprio genitore, quindi lo
    //    scartiamo (niente doppio genitore). Gli archi ENTRANTI (metodo→placeholder)
    //    vengono ridiretti al canonico, così tutti i metodi diventano suoi figli.
    let mut fixed = Vec::with_capacity(delta.added_relations.len());
    for mut r in std::mem::take(&mut delta.added_relations) {
        if let Some(c) = merge.get(&r.source_id).copied() {
            if r.kind == RelationKind::BelongsTo {
                continue;
            }
            r.source_id = c;
        }
        if let Some(c) = merge.get(&r.target_id).copied() {
            r.target_id = c;
        }
        fixed.push(r);
    }
    delta.added_relations = fixed;
    Ok(merge.len())
}

/// Cerca nello storage l'UNICA definizione nominale (`struct`/`enum`/`trait`)
/// omonima a un placeholder `impl_target`, limitata al crate del chiamante. Serve al
/// re-index di un singolo file: la definizione canonica non è nel batch ma già
/// persistita da un'indicizzazione precedente. Stessa disciplina anti-bugia della via
/// batch (un merge mancante batte uno bugiardo):
/// - senza un confine di crate (`caller_crate_prefix`) non sappiamo dove cercare ⇒
///   `None` (conservativo);
/// - solo definizioni reali (`rust_kind` struct/enum/trait), mai placeholder/alias;
/// - foglia esatta (`qname == name` o `…::name`), lingua coerente, dentro il crate;
/// - 0 o ≥2 candidati ⇒ `None` (non si indovina fra omonimi).
async fn db_canonical_for(
    name: &str,
    ctx: &FileContext,
    storage: &dyn GraphStorage,
) -> anyhow::Result<Option<EntityId>> {
    let Some(crate_prefix) = caller_crate_prefix(&ctx.module_prefix) else {
        return Ok(None);
    };
    let suffix = format!("::{name}");
    let mut hits: HashSet<EntityId> = HashSet::new();
    for e in storage.find_entities_by_name_pattern(name).await? {
        // Foglia esatta, non un match parziale (`name` come sottostringa altrove).
        if e.qualified_name != name && !e.qualified_name.ends_with(&suffix) {
            continue;
        }
        // Solo definizioni reali: esclude i placeholder `impl_target` e gli alias.
        if !matches!(
            e.metadata.get("rust_kind").map(String::as_str),
            Some("struct" | "enum" | "trait")
        ) {
            continue;
        }
        // Stesso crate del chiamante: niente sconfinamento cross-crate.
        if !e.qualified_name.starts_with(&crate_prefix) {
            continue;
        }
        // Lingua coerente col file del placeholder.
        let t_lang = e
            .metadata
            .get("language")
            .cloned()
            .unwrap_or_else(|| detect_language(&e.location.file_path));
        if !language_matches(&ctx.language, &t_lang) {
            continue;
        }
        hits.insert(e.id);
    }
    if hits.len() == 1 {
        Ok(hits.into_iter().next())
    } else {
        Ok(None)
    }
}

/// Come un target è stato risolto a un `EntityId`. Determina la **confidenza** della
/// relazione e, di riflesso, se può partecipare al ragionamento architetturale: le
/// relazioni a confidenza `low` vengono escluse dal mining (vedi `Guardian`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolutionStrategy {
    /// Match esatto sul `qualified_name` (path normalizzato o full-qualified).
    Exact,
    /// Risolto via import esplicito del file (la namespace table).
    Import,
    /// Nome semplice risolto nello *stesso modulo* del chiamante: euristica
    /// sull'ultimo segmento, affidabile ma non certa ⇒ confidenza media.
    SameModule,
    /// Dipendenza esterna sintetica (`tokio`, `std`, `react`…).
    External,
}

impl ResolutionStrategy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Import => "import",
            Self::SameModule => "same_module",
            Self::External => "external",
        }
    }

    /// La confidenza associata: `high` per i match basati su path/import espliciti,
    /// `medium` per il match euristico per nome semplice. Nessuna strategia attuale
    /// produce `low`: la soglia esiste come rete di sicurezza per future strategie
    /// fuzzy/cross-package, già escluse dal mining dal Guardian.
    fn confidence(self) -> &'static str {
        match self {
            Self::Exact | Self::Import | Self::External => "high",
            Self::SameModule => "medium",
        }
    }
}

/// Tutto ciò che serve a risolvere i target di **un** file: modulo corrente,
/// linguaggio, import e indici globali del batch + lo storage. Sostituisce la
/// cascata di 8 argomenti di [`resolve_target`] (clippy `too_many_arguments`) con un
/// unico riferimento condiviso, costruito una volta per file.
struct ResolutionContext<'a> {
    module_prefix: &'a str,
    language: &'a str,
    namespace: &'a HashMap<String, String>,
    new_by_qname: &'a HashMap<String, EntityId>,
    new_by_name: &'a HashMap<String, Vec<(EntityId, String)>>,
    new_by_id_lang: &'a HashMap<EntityId, String>,
    storage: &'a dyn GraphStorage,
}

/// Algoritmo a cascata del Passo 3 (briefing sez. 7.2). Restituisce `None` se
/// nessuno stadio risolve il target (⇒ relazione `Unresolved`), altrimenti l'id
/// risolto e la [`ResolutionStrategy`] con cui ci è arrivato.
async fn resolve_target(
    ctx: &ResolutionContext<'_>,
    target: &str,
) -> anyhow::Result<Option<(EntityId, ResolutionStrategy)>> {
    // 0 — Import path normalisation. I parser mantengono i target come li scrive
    // il linguaggio (`crate::x`, `codeos_types::x`, `./client`); qui li traduciamo
    // nel namespace interno basato sui path (`crates::codeos-types::src::...`).
    for candidate in target_candidates(target, ctx.module_prefix) {
        if let Some(id) = lookup_progressive(
            &candidate,
            ctx.language,
            ctx.new_by_qname,
            ctx.new_by_id_lang,
            ctx.storage,
        )
        .await?
        {
            return Ok(Some((id, ResolutionStrategy::Exact)));
        }
    }

    // 1 — Full-qualified match (sia sul batch sia sul DB).
    if let Some(id) = lookup_exact(
        target,
        ctx.language,
        ctx.new_by_qname,
        ctx.new_by_id_lang,
        ctx.storage,
    )
    .await?
    {
        return Ok(Some((id, ResolutionStrategy::Exact)));
    }
    // Le `call` usano `.`, i nostri qualified_name usano `::`: prova la variante.
    let colonized = target.replace('.', "::");
    if colonized != target {
        if let Some(id) = lookup_exact(
            &colonized,
            ctx.language,
            ctx.new_by_qname,
            ctx.new_by_id_lang,
            ctx.storage,
        )
        .await?
        {
            return Ok(Some((id, ResolutionStrategy::Exact)));
        }
    }

    // 2 — Import-based match: se il primo segmento è importato, sostituiscine il
    // prefisso col target dell'import e riprova il match esatto.
    if let Some(seg) = first_segment(target) {
        if let Some(full) = ctx.namespace.get(seg) {
            let remainder = &target[seg.len()..];
            let candidate = format!("{full}{remainder}").replace('.', "::");
            if let Some(id) = lookup_exact(
                &candidate,
                ctx.language,
                ctx.new_by_qname,
                ctx.new_by_id_lang,
                ctx.storage,
            )
            .await?
            {
                return Ok(Some((id, ResolutionStrategy::Import)));
            }

            // 2.1 — Re-export aware (P0-2): l'import nomina crate + tipo
            // (`codeos_storage::SqliteStorage`) ma NON il modulo interno dove il tipo
            // è definito o ri-esportato (`src::sqlite`). Per una call come
            // `SqliteStorage::in_memory` cerchiamo dunque l'UNICA entità interna del
            // crate il cui `qualified_name` termina con `::SqliteStorage::in_memory`.
            // Unicità obbligatoria: 0 o >1 candidati ⇒ Unresolved, mai un arco
            // arbitrario.
            if let Some((crate_name, tail)) = candidate.split_once("::") {
                if is_internal_crate_name(crate_name) {
                    let crate_prefix = format!("crates::{}::", crate_name.replace('_', "-"));
                    if let Some(id) = unique_internal_match(&crate_prefix, tail, ctx).await? {
                        return Ok(Some((id, ResolutionStrategy::Import)));
                    }
                }
            }
        }
    }

    // 2.5 — Qualified crate-local (P0-2): un `Tipo::metodo` il cui `Tipo` NON è
    // importato ma è definito nello *stesso crate* del chiamante (es. `Decision::from_new`
    // usato dentro `codeos-paleo`). Risolviamo all'UNICA entità del crate corrente il
    // cui qname termina con `::Tipo::metodo`. Stesso vincolo di unicità del 2.1; niente
    // match per nomi semplici (serve un path `::`), che resta compito dello Stadio 3.
    if target.contains("::") {
        if let Some(caller_prefix) = caller_crate_prefix(ctx.module_prefix) {
            let colon = target.replace('.', "::");
            if let Some(id) = unique_internal_match(&caller_prefix, &colon, ctx).await? {
                return Ok(Some((id, ResolutionStrategy::SameModule)));
            }
        }
    }

    // 3 — Scope-local match: per nome semplice, SOLO nello stesso modulo e SOLO se
    // il candidato è UNICO. Il match per nome semplice non vede il tipo del receiver
    // (`self.identifier.as_str()` arriva come bare `as_str`); se nel modulo del
    // chiamante vivono più omonimi (es. `Prerelease::as_str` e `BuildMetadata::as_str`,
    // entrambi figli del modulo `lib`), sceglierne uno è un arco che *indovina* — il
    // falso positivo che la tesi vieta. Con ≥2 candidati ci asteniamo (⇒ Unresolved):
    // un arco mancante batte un arco bugiardo, la fiducia nel grafo vale più della
    // copertura. NIENTE `.first()`/`.find()` globale (P0-1) né scelta arbitraria tra
    // omonimi (misurato su semver: l'euristica risolveva `BuildMetadata::as_str` a
    // `Prerelease::as_str`). Stessa disciplina anti-omonimi del Passo 1.5.
    let bare = last_segment(target).unwrap_or(target);
    if let Some(candidates) = ctx.new_by_name.get(bare) {
        let same_module: Vec<EntityId> = candidates
            .iter()
            .filter(|(id, _)| {
                ctx.new_by_id_lang
                    .get(id)
                    .is_some_and(|t_lang| language_matches(ctx.language, t_lang))
            })
            .filter(|(_, qname)| qname.starts_with(ctx.module_prefix))
            .map(|(id, _)| *id)
            .collect();
        match same_module.as_slice() {
            [unico] => return Ok(Some((*unico, ResolutionStrategy::SameModule))),
            // ≥2 omonimi nel modulo: ambiguo senza inferenza di tipo. Astieniti, e
            // non interrogare nemmeno il DB (aggiungerebbe solo altri omonimi).
            [_, _, ..] => return Ok(None),
            [] => {}
        }
    }
    // Batch privo del nome in questo modulo: stesso vincolo di unicità sul DB (utile
    // nel re-index incrementale, dove i fratelli sono già nello storage).
    let suffix = format!("::{bare}");
    let db_hits = ctx.storage.find_entities_by_name_pattern(bare).await?;
    let same_module_db: Vec<&Entity> = db_hits
        .iter()
        .filter(|e| e.qualified_name == bare || e.qualified_name.ends_with(&suffix))
        .filter(|e| {
            let t_lang = e
                .metadata
                .get("language")
                .cloned()
                .unwrap_or_else(|| detect_language(&e.location.file_path));
            language_matches(ctx.language, &t_lang)
        })
        .filter(|e| e.qualified_name.starts_with(ctx.module_prefix))
        .collect();
    if let [unico] = same_module_db.as_slice() {
        return Ok(Some((unico.id, ResolutionStrategy::SameModule)));
    }

    // 4 — Nessun match (o omonimi ambigui): Unresolved. Niente fallback globale sul
    // DB, che aggancerebbe un omonimo in un altro modulo/crate.
    Ok(None)
}

/// Cerca l'**unica** entità interna (batch corrente + DB) che vive sotto
/// `crate_prefix` (es. `crates::codeos-storage::`), nel linguaggio del chiamante, il
/// cui `qualified_name` termina con `::{tail}` (es. `::SqliteStorage::in_memory`).
///
/// È il cuore della risoluzione *re-export aware* (P0-2): l'import e il crate ci
/// dicono *dove* cercare, ma non il modulo interno; il suffisso `Tipo::metodo` lo
/// individua senza indovinare il path. L'unicità è il guard anti-arco-bugiardo: se
/// zero o più d'una entità combaciano restituiamo `None`, perché un `Tipo::metodo`
/// ambiguo non deve mai diventare un arco arbitrario ("un arco mancante è meglio di
/// uno che mente").
async fn unique_internal_match(
    crate_prefix: &str,
    tail: &str,
    ctx: &ResolutionContext<'_>,
) -> anyhow::Result<Option<EntityId>> {
    if tail.is_empty() {
        return Ok(None);
    }
    let suffix = format!("::{tail}");
    let mut hits: HashSet<EntityId> = HashSet::new();

    // Batch corrente: entità appena create, non ancora nel DB.
    for (qname, id) in ctx.new_by_qname.iter() {
        if qname.starts_with(crate_prefix) && qname.ends_with(&suffix) {
            if let Some(t_lang) = ctx.new_by_id_lang.get(id) {
                if language_matches(ctx.language, t_lang) {
                    hits.insert(*id);
                }
            }
        }
    }
    // DB persistito: pattern `%tail%` (selettivo: include già `Tipo::metodo`), poi
    // filtrato per crate + suffisso esatto + lingua.
    for e in ctx.storage.find_entities_by_name_pattern(tail).await? {
        if e.qualified_name.starts_with(crate_prefix) && e.qualified_name.ends_with(&suffix) {
            let t_lang = e
                .metadata
                .get("language")
                .cloned()
                .unwrap_or_else(|| detect_language(&e.location.file_path));
            if language_matches(ctx.language, &t_lang) {
                hits.insert(e.id);
            }
        }
    }

    if hits.len() == 1 {
        Ok(hits.into_iter().next())
    } else {
        Ok(None)
    }
}

/// Prefisso che delimita il **crate del chiamante** a partire dal suo
/// `module_prefix`, per limitare la ricerca crate-local (Stadi 2.1/2.5) senza
/// sconfinare in altri crate dello stesso DB.
/// - Monorepo `crates::<name>::…` → `Some("crates::<name>::")` (il crate).
/// - Layout generico con una radice sorgenti `…::src::…` → `Some("…::src::")`,
///   così una call `modulo::fn` risolve a un modulo *fratello* sotto lo stesso
///   `src` (es. semver `…::semver::src::lib` cerca solo dentro `…::semver::src::`,
///   trovando `…::semver::src::eval::matches_comparator`). Il prefisso include il
///   nome-cartella del crate che precede `src`, quindi due crate fratelli
///   (`…::foo::src`, `…::bar::src`) non si agganciano a vicenda.
/// - Nessuno dei due ⇒ `None`: non sappiamo dove finisce il crate, quindi non
///   risolviamo crate-local (conservativo, anti-falso-positivo).
fn caller_crate_prefix(module_prefix: &str) -> Option<String> {
    let segs: Vec<&str> = module_prefix.split("::").collect();
    if segs.first() == Some(&"crates") {
        let crate_name = segs.get(1).filter(|s| !s.is_empty())?;
        return Some(format!("crates::{crate_name}::"));
    }
    // Tronca al primo `src` (il più esterno → il crate root dei sorgenti).
    let src_pos = segs.iter().position(|s| *s == "src")?;
    Some(format!("{}::", segs[..=src_pos].join("::")))
}

fn target_candidates(target: &str, module_prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let normalized = target.replace(['.', '/'], "::");
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
                if rest.split("::").next().is_some_and(starts_lowercase) {
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
        let t_lang = entity
            .metadata
            .get("language")
            .cloned()
            .unwrap_or_else(|| detect_language(&entity.location.file_path));
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
        if let Some(id) = lookup_exact(
            &current,
            src_language,
            new_by_qname,
            new_by_id_lang,
            storage,
        )
        .await?
        {
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
        "go" => "go".to_string(),
        "java" => "java".to_string(),
        _ => "unknown".to_string(),
    }
}

/// Classifica un file come codice di **test** o di **produzione** dalle convenzioni
/// di percorso del suo linguaggio. Euristica pura sul path (nessun I/O): serve al
/// Guardian per **escludere dal mining gli archi nati nei test** — un test importa e
/// chiama liberamente attraverso i layer (è il suo mestiere), e non descrive
/// l'architettura del prodotto.
///
/// Deliberatamente **conservativa**: nel dubbio risponde `"prod"`. Un falso "prod"
/// lascia l'arco nel grafo (al più rumore), un falso "test" lo nasconderebbe — e la
/// filosofia di CodeOS preferisce un arco di troppo a uno nascosto a sproposito.
fn classify_source_kind(file_path: &str, language: &str) -> &'static str {
    // Separatori normalizzati e tutto minuscolo: i match sotto sono
    // case-insensitive e indipendenti dall'OS.
    let path = file_path.replace('\\', "/").to_lowercase();
    let file_name = path.rsplit('/').next().unwrap_or(path.as_str());

    // Segnale universale: una directory `tests/` o `__tests__/` ospita test in
    // quasi tutti gli ecosistemi (inclusi i test d'integrazione Rust e i moduli
    // `mod tests;` sotto `src/tests/`).
    if path.contains("/tests/") || path.contains("/__tests__/") {
        return "test";
    }

    let is_test = match language {
        "go" => file_name.ends_with("_test.go"),
        "java" | "kotlin" => path.contains("/src/test/"),
        "python" => file_name.starts_with("test_") || file_name.ends_with("_test.py"),
        "typescript" | "javascript" => file_name.contains(".test.") || file_name.contains(".spec."),
        // Rust: i test d'integrazione vivono in `tests/` (già coperto sopra); gli
        // unit test inline `#[cfg(test)]` non sono distinguibili dal path e li
        // lasciamo "prod" finché un parser non li marca via metadata.
        _ => false,
    };

    if is_test {
        "test"
    } else {
        "prod"
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

/// Identità stabile di un arco risolto → la sua nascita. La coppia di
/// `qualified_name` (più il `kind`) sopravvive al re-index; gli `EntityId` no.
type EdgeBornMap = HashMap<(String, RelationKind, String), (String, i64)>;

/// Nascita raccolta da `collect_removals` nel re-index: nodi (`qname → (commit,
/// ts)`) e archi risolti (vedi [`EdgeBornMap`]).
type BornHarvest = (HashMap<String, (String, i64)>, EdgeBornMap);

/// Estrae la nascita (`born_commit`, `born_ts`) dai metadata di un'entità, se
/// presente e ben formata. Serve a CONSERVARE la nascita nel re-index: un
/// `born_ts` non numerico (corruzione/manomissione) è ignorato (`None`) invece di
/// propagare un dato fasullo — meglio ri-timbrare che fidarsi di un istante rotto.
fn born_stamp(metadata: &HashMap<String, String>) -> Option<(String, i64)> {
    let commit = metadata.get("born_commit")?;
    let ts: i64 = metadata.get("born_ts")?.parse().ok()?;
    Some((commit.clone(), ts))
}

/// Normalizza un `target_qualified_name` grezzo emesso dal parser. I simboli del
/// codice non contengono mai whitespace: rimuoviamo spazi e newline interni —
/// introdotti quando il testo del nodo abbraccia più righe (`obj\n    .metodo`). È
/// la prima difesa contro i target sporchi che producevano layer ed archi finti.
///
/// Restituisce `None` se dopo la pulizia il target è vuoto o non contiene **alcun**
/// carattere da identificatore (lettera, cifra, `_`): non è un simbolo risolvibile,
/// e va scartato del tutto invece di inquinare il grafo con un arco senza senso.
fn sanitize_target(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() {
        return None;
    }
    if !cleaned.chars().any(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(cleaned)
}

/// Restituisce l'`EntityId` dell'entità sintetica per la dipendenza esterna
/// `root` (es. `tokio`), creandola se non esiste ancora — né nel batch corrente
/// (via `cache`) né nel DB (via `get_entity_by_qname`). Le entità esterne hanno
/// `file_path` `<external>` e non vengono mai rimosse dalla re-indicizzazione.
async fn external_entity_id(
    root: &str,
    language: &str,
    cache: &mut HashMap<String, EntityId>,
    delta: &mut GraphDelta,
    storage: &dyn GraphStorage,
) -> anyhow::Result<EntityId> {
    let qname = format!("external::{root}");
    if let Some(id) = cache.get(&qname) {
        return Ok(*id);
    }
    if let Some(existing) = storage.get_entity_by_qname(&qname).await? {
        cache.insert(qname, existing.id);
        return Ok(existing.id);
    }

    let id = EntityId::new();
    let mut metadata = HashMap::new();
    metadata.insert("language".to_string(), language.to_string());
    metadata.insert("external".to_string(), "true".to_string());
    metadata.insert("dependency_root".to_string(), root.to_string());
    // Alias di `dependency_root` con la chiave uniforme `package`: lo stesso nome
    // che gli archi esterni portano nei loro metadata (P1-b).
    metadata.insert("package".to_string(), root.to_string());
    delta.added_entities.push(Entity {
        id,
        kind: codeos_types::EntityKind::ExternalDependency,
        qualified_name: qname.clone(),
        location: codeos_types::SourceLocation {
            file_path: "<external>".to_string(),
            start_line: 0,
            start_column: 0,
            end_line: 0,
            end_column: 0,
        },
        metadata,
    });
    cache.insert(qname, id);
    Ok(id)
}

/// Decide se `target` è una dipendenza esterna e, in tal caso, ne restituisce la
/// "radice" (il crate/pacchetto da usare come entità sintetica).
///
/// Conservativo per costruzione: in dubbio preferisce `None` (⇒ `Unresolved`)
/// piuttosto che etichettare come esterno un target interno non ancora risolto o
/// una call locale mancata. La discriminazione è per-linguaggio.
fn external_dependency_root(
    target: &str,
    language: &str,
    namespace: &HashMap<String, String>,
    is_import: bool,
) -> Option<String> {
    match language {
        "rust" => external_root_rust(target, is_import),
        "python" => external_root_python(target, namespace, is_import),
        // Per il web trattiamo come esterni solo gli import di pacchetti (bare
        // specifier); le call non risolte restano Unresolved.
        "typescript" | "javascript" if is_import => external_root_web(target),
        _ => None,
    }
}

/// Rust: `tokio::sync::mpsc` → `tokio`. Esclude le keyword di path relative al
/// progetto (`crate`/`self`/`super`/`Self`) e i crate interni (`codeos_*`).
fn external_root_rust(target: &str, is_import: bool) -> Option<String> {
    let root = first_segment(target)?;
    if matches!(root, "crate" | "self" | "super" | "Self") {
        return None;
    }
    if is_internal_crate_name(root) {
        return None;
    }
    // I crate esterni iniziano in minuscolo (convenzione Rust). Un identificatore
    // CamelCase senza path è un tipo locale non risolto, non una dipendenza.
    if !starts_lowercase(root) {
        return None;
    }
    // Serve un path (`tokio::...`) o un import esplicito: una bareword minuscola
    // proveniente da una call (`foo()`) è quasi sempre una funzione locale
    // mancata, non un crate esterno.
    if target.contains("::") || is_import {
        Some(root.to_string())
    } else {
        None
    }
}

/// Python: per un import (`import os`, `from requests import get`) la radice è il
/// primo segmento. Per una call/uso è esterna solo se il primo segmento è un nome
/// importato nel file (`os.getcwd()` con `import os`). Gli import relativi
/// (`.mod`) sono interni al progetto.
fn external_root_python(
    target: &str,
    namespace: &HashMap<String, String>,
    is_import: bool,
) -> Option<String> {
    if target.starts_with('.') {
        return None;
    }
    let root = first_segment(target)?;
    if is_import {
        return Some(root.to_string());
    }
    if namespace.contains_key(root) {
        Some(root.to_string())
    } else {
        None
    }
}

/// Web (TS/JS): un bare specifier (`react`, `@scope/pkg/sub`) è un pacchetto
/// esterno; un import relativo/assoluto (`./x`, `/abs`) è un modulo interno.
fn external_root_web(target: &str) -> Option<String> {
    if target.starts_with('.') || target.starts_with('/') {
        return None;
    }
    npm_package_root(target)
}

/// `@scope/pkg/sub` → `@scope/pkg`; `pkg/sub` → `pkg`.
fn npm_package_root(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if let Some(scoped) = spec.strip_prefix('@') {
        let mut parts = scoped.split('/');
        let scope = parts.next().filter(|s| !s.is_empty())?;
        let pkg = parts.next().filter(|s| !s.is_empty())?;
        Some(format!("@{scope}/{pkg}"))
    } else {
        let root = spec.split('/').next().filter(|s| !s.is_empty())?;
        Some(root.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codeos_parser::{
        GoParser, JavaParser, LanguageParser, PythonParser, RustParser, TypeScriptParser,
    };
    use codeos_storage::SqliteStorage;
    use codeos_types::EntityKind;
    use std::path::Path;

    async fn parse(path: &str, src: &str) -> ParsedFileResult {
        PythonParser::new().parse_file(Path::new(path), src).await
    }

    async fn parse_rust(path: &str, src: &str) -> ParsedFileResult {
        RustParser::new().parse_file(Path::new(path), src).await
    }

    async fn parse_go(path: &str, src: &str) -> ParsedFileResult {
        GoParser::new().parse_file(Path::new(path), src).await
    }

    async fn parse_java(path: &str, src: &str) -> ParsedFileResult {
        JavaParser::new().parse_file(Path::new(path), src).await
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

    /// Trova l'arco `Calls` uscente da `source_id` e ne restituisce
    /// `(target_id, resolution_strategy)`. Panica se assente.
    fn call_from(delta: &GraphDelta, source_id: EntityId) -> (EntityId, &str) {
        let rel = delta
            .added_relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls && r.source_id == source_id)
            .expect("arco Calls atteso assente");
        (
            rel.target_id,
            rel.metadata
                .get("resolution_strategy")
                .map(String::as_str)
                .unwrap_or(""),
        )
    }

    #[tokio::test]
    async fn resolves_reexported_type_method_via_import() {
        // P0-2: il tipo vive in un sotto-modulo (`src::sqlite`), l'import nomina solo
        // crate+tipo (`codeos_storage::SqliteStorage`). La call `SqliteStorage::in_memory`
        // dev'essere risolta al metodo reale tramite suffisso unico nel crate.
        let parsed = vec![
            parse_rust(
                "/repo/crates/codeos-storage/src/sqlite.rs",
                "pub struct SqliteStorage;\nimpl SqliteStorage {\n    pub fn in_memory() {}\n}\n",
            )
            .await,
            parse_rust(
                "/repo/crates/codeos-core/src/lib.rs",
                "use codeos_storage::SqliteStorage;\npub fn make() {\n    SqliteStorage::in_memory();\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();
        let make = find(&delta, "crates::codeos-core::src::lib::make");
        let method = find(
            &delta,
            "crates::codeos-storage::src::sqlite::SqliteStorage::in_memory",
        );

        let (target_id, strategy) = call_from(&delta, make.id);
        assert_eq!(target_id, method.id, "la call deve puntare al metodo reale");
        assert_eq!(strategy, "import");
    }

    #[tokio::test]
    async fn resolves_crate_local_qualified_call_without_import() {
        // P0-2: `Decision::from_new` chiamato nello stesso crate dove `Decision` è
        // definito, SENZA `use`. Risolto via suffisso unico nel crate del chiamante.
        let parsed = vec![
            parse_rust(
                "/repo/crates/codeos-paleo/src/fossil.rs",
                "pub struct Decision;\nimpl Decision {\n    pub fn from_new() {}\n}\n",
            )
            .await,
            parse_rust(
                "/repo/crates/codeos-paleo/src/lib.rs",
                "pub fn build() {\n    Decision::from_new();\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();
        let build = find(&delta, "crates::codeos-paleo::src::lib::build");
        let method = find(
            &delta,
            "crates::codeos-paleo::src::fossil::Decision::from_new",
        );

        let (target_id, strategy) = call_from(&delta, build.id);
        assert_eq!(target_id, method.id);
        assert_eq!(strategy, "same_module");
    }

    #[tokio::test]
    async fn ambiguous_qualified_call_stays_unresolved() {
        // Anti-arco-bugiardo: due `Repo::open` nello STESSO crate (moduli diversi)
        // rendono `Repo::open` ambiguo ⇒ nessun arco arbitrario, resta Unresolved.
        let parsed = vec![
            parse_rust(
                "/repo/crates/codeos-storage/src/a.rs",
                "pub struct Repo;\nimpl Repo {\n    pub fn open() {}\n}\n",
            )
            .await,
            parse_rust(
                "/repo/crates/codeos-storage/src/b.rs",
                "pub struct Repo;\nimpl Repo {\n    pub fn open() {}\n}\n",
            )
            .await,
            parse_rust(
                "/repo/crates/codeos-core/src/lib.rs",
                "use codeos_storage::Repo;\npub fn go() {\n    Repo::open();\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();
        let go = find(&delta, "crates::codeos-core::src::lib::go");

        assert!(
            !delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::Calls && r.source_id == go.id),
            "una call ambigua non deve produrre alcun arco risolto"
        );
        assert!(
            delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::Unresolved
                    && r.source_id == go.id
                    && r.metadata.get("unresolved_target").map(String::as_str) == Some("Repo::open")
            }),
            "la call ambigua deve restare Unresolved col target grezzo"
        );
    }

    #[tokio::test]
    async fn same_module_homonym_method_call_stays_unresolved() {
        // Anti-arco-bugiardo (misurato su semver): `BuildMetadata::as_str` e
        // `Prerelease::as_str` vivono entrambi nel modulo `lib`. Una call per nome
        // semplice (`self.inner.val()` → bare `val`) non vede il tipo del receiver:
        // con DUE `val` omonimi nello stesso modulo l'euristica `same_module` NON
        // deve sceglierne uno (prima lo faceva: `BuildMetadata::as_str` →
        // `Prerelease::as_str`). Un arco mancante batte un arco che indovina.
        let parsed = vec![
            parse_rust(
                "/repo/crates/codeos-x/src/lib.rs",
                "pub struct A {\n    inner: B,\n}\npub struct B;\nimpl A {\n    pub fn val(&self) -> u8 {\n        self.inner.val()\n    }\n}\nimpl B {\n    pub fn val(&self) -> u8 {\n        0\n    }\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();
        let a_val = find(&delta, "crates::codeos-x::src::lib::A::val");
        let b_val = find(&delta, "crates::codeos-x::src::lib::B::val");

        // Nessun arco Calls verso un `val` omonimo (né B::val né A::val stesso): è
        // indecidibile senza inferenza di tipo sul receiver.
        let bogus = delta.added_relations.iter().any(|r| {
            r.kind == RelationKind::Calls
                && r.source_id == a_val.id
                && (r.target_id == b_val.id || r.target_id == a_val.id)
        });
        assert!(
            !bogus,
            "una call per nome semplice tra omonimi dello stesso modulo non deve risolvere"
        );
        // E l'astensione è onesta: resta un Unresolved col target grezzo `val`.
        assert!(
            delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::Unresolved
                    && r.source_id == a_val.id
                    && r.metadata
                        .get("unresolved_target")
                        .map(String::as_str)
                        .is_some_and(|t| t.ends_with("val"))
            }),
            "la call omonima ambigua deve restare Unresolved"
        );
    }

    #[tokio::test]
    async fn external_qualified_call_is_not_misresolved() {
        // `HashMap::new` (std) non deve mai agganciarsi a un'entità interna: il tipo
        // non è nel grafo, quindi resta Unresolved (niente arco verso un omonimo).
        let parsed = vec![
            parse_rust(
                "/repo/crates/codeos-core/src/lib.rs",
                "use std::collections::HashMap;\npub fn go() {\n    HashMap::new();\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();
        let go = find(&delta, "crates::codeos-core::src::lib::go");

        assert!(
            !delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::Calls && r.source_id == go.id),
            "HashMap::new non deve risolvere a un'entità interna"
        );
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
    async fn external_import_resolves_to_synthetic_external_entity() {
        // `import os` + `os.getcwd()`: l'import e la call risolvono entrambi a un
        // unico nodo sintetico `external::os`, niente Unresolved con target nullo.
        let src = "import os\n\ndef f():\n    os.getcwd()\n";
        let parsed = parse("m.py", src).await;
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        let delta = resolver.resolve(&[parsed], &storage).await.unwrap();

        let ext = find(&delta, "external::os");
        assert_eq!(ext.kind, EntityKind::ExternalDependency);
        assert_eq!(
            ext.metadata.get("external").map(String::as_str),
            Some("true")
        );

        // L'import `os` è un arco Imports verso il nodo esterno (target non nullo).
        assert!(
            delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::Imports && r.target_id == ext.id && !r.target_id.is_nil()
            }),
            "l'import di os deve agganciarsi a external::os"
        );
        // Nessuna relazione Unresolved residua: tutto ha trovato un target.
        assert!(
            !delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::Unresolved),
            "import e call esterni non devono più produrre Unresolved"
        );
        // L'unico nodo esterno è condiviso (cache): non si duplica per la call.
        let externals = delta
            .added_entities
            .iter()
            .filter(|e| e.kind == EntityKind::ExternalDependency)
            .count();
        assert_eq!(externals, 1);
    }

    #[tokio::test]
    async fn external_rust_crate_becomes_synthetic_dependency() {
        // `use tokio::sync::mpsc;` da un crate interno → external::tokio.
        let parsed = parse_rust(
            "/repo/crates/codeos-core/src/lib.rs",
            "use tokio::sync::mpsc;\npub fn route() {}\n",
        )
        .await;
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&[parsed], &storage).await.unwrap();

        let ext = find(&delta, "external::tokio");
        assert_eq!(ext.kind, EntityKind::ExternalDependency);
        // P1-b: l'entità esterna porta il pacchetto sotto la chiave uniforme.
        assert_eq!(
            ext.metadata.get("package").map(String::as_str),
            Some("tokio")
        );
        let source = find(&delta, "crates::codeos-core::src::lib");
        let import = delta
            .added_relations
            .iter()
            .find(|r| {
                r.kind == RelationKind::Imports && r.source_id == source.id && r.target_id == ext.id
            })
            .expect("l'import di tokio deve collegare il modulo a external::tokio");
        // P1-b: anche l'arco esterno porta lo stesso `package`.
        assert_eq!(
            import.metadata.get("package").map(String::as_str),
            Some("tokio")
        );
    }

    #[tokio::test]
    async fn internal_crate_and_local_calls_are_not_externalized() {
        // Un import di crate interno non risolto e una call locale mancante NON
        // devono diventare dipendenze esterne: restano Unresolved (conservativo).
        let parsed = parse_rust(
            "/repo/crates/codeos-core/src/lib.rs",
            "use codeos_types::Foo;\npub fn route() {\n    missing_local();\n}\n",
        )
        .await;
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&[parsed], &storage).await.unwrap();

        assert!(
            !delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name.starts_with("external::codeos")),
            "i crate interni non devono diventare dipendenze esterne"
        );
        assert!(
            !delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name == "external::missing_local"),
            "una call locale mancata non è una dipendenza esterna"
        );
    }

    #[tokio::test]
    async fn call_through_local_module_is_not_externalized() {
        // Anti-bugia (a2), misurato su semver: una call il cui path-root è un modulo
        // NOSTRO non va esternalizzata (il resolver coniava un `external::eval`
        // inesistente per `eval::matches_comparator`). Qui la funzione chiamata NON
        // esiste nel modulo `eval` (`eval::missing_fn`): non c'è nessun fratello da
        // risolvere, quindi il caso esercita esattamente il fallback esterno — e la
        // guardia (a2) lo fa cadere a Unresolved (onesto) invece di inventare
        // `external::eval`. Il caso risolvibile è coperto da
        // `call_through_local_module_resolves_to_sibling`.
        //
        // NB: layout senza prefisso `crates::` (project_root=None) → `caller_crate_prefix`
        // usa la radice `src`, come nel layout fisico di semver (`private::tmp::…`).
        let parsed = vec![
            parse_rust(
                "/myproj/src/eval.rs",
                "pub fn matches_comparator() -> bool {\n    true\n}\n",
            )
            .await,
            parse_rust(
                "/myproj/src/lib.rs",
                "pub fn matches() -> bool {\n    eval::missing_fn()\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();

        // `eval` è un modulo locale: nessuna entità esterna inventata.
        assert!(
            !delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name == "external::eval"),
            "una call attraverso un modulo locale non deve coniare external::eval"
        );

        // E l'astensione è onesta: la call resta Unresolved col target grezzo che
        // nomina il modulo locale (non risolta, ma nemmeno bugiarda).
        let caller = find(&delta, "myproj::src::lib::matches");
        assert!(
            delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::Unresolved
                    && r.source_id == caller.id
                    && r.metadata
                        .get("unresolved_target")
                        .map(String::as_str)
                        .is_some_and(|t| t.contains("eval") && t.ends_with("missing_fn"))
            }),
            "la call verso una fn inesistente del modulo locale deve restare Unresolved, non external"
        );
    }

    #[tokio::test]
    async fn call_through_local_module_resolves_to_sibling() {
        // Recall (a2-recall), misurato su semver: il corpo di `Comparator::matches` è
        // `eval::matches_comparator(...)`, dove `eval` è un modulo fratello
        // (`src/eval.rs`) e `matches_comparator` esiste ed è UNICO nel crate. Lo
        // Stadio 2.5 (`caller_crate_prefix` esteso alla radice `src` nei layout
        // non-`crates::`) lo risolve all'unica entità interna `…::eval::matches_comparator`,
        // senza indovinare: l'unicità è il guard anti-falso-positivo (≥2 omonimi ⇒
        // astensione, come negli Stadi 2.1/2.5 e nella canonicalizzazione).
        let parsed = vec![
            parse_rust(
                "/myproj/src/eval.rs",
                "pub fn matches_comparator() -> bool {\n    true\n}\n",
            )
            .await,
            parse_rust(
                "/myproj/src/lib.rs",
                "pub fn matches() -> bool {\n    eval::matches_comparator()\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();

        let caller = find(&delta, "myproj::src::lib::matches");
        let sibling = find(&delta, "myproj::src::eval::matches_comparator");

        // La call risolve al fratello GIUSTO (non a un omonimo, non a external).
        assert!(
            delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::Calls
                    && r.source_id == caller.id
                    && r.target_id == sibling.id
            }),
            "la call verso un modulo fratello unico deve risolvere all'entità locale"
        );
        // Nessun fantasma esterno e nessun Unresolved residuo per quel target.
        assert!(
            !delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name == "external::eval"),
            "una call risolta al fratello non deve coniare external::eval"
        );
        assert!(
            !delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::Unresolved
                    && r.source_id == caller.id
                    && r.metadata
                        .get("unresolved_target")
                        .map(String::as_str)
                        .is_some_and(|t| t.ends_with("matches_comparator"))
            }),
            "la call risolta non deve lasciare un Unresolved residuo"
        );
    }

    #[test]
    fn sanitize_target_strips_whitespace_and_rejects_non_symbols() {
        // Whitespace interno (catturato da un'espressione multi-riga) rimosso.
        assert_eq!(
            sanitize_target("obj\n    .metodo").as_deref(),
            Some("obj.metodo")
        );
        assert_eq!(sanitize_target("  Foo::bar  ").as_deref(), Some("Foo::bar"));
        // Vuoto o privo di identificatori ⇒ non è un simbolo: scartato.
        assert_eq!(sanitize_target("   "), None);
        assert_eq!(sanitize_target("::"), None);
        assert_eq!(sanitize_target(""), None);
    }

    #[tokio::test]
    async fn cross_module_homonym_is_not_falsely_linked() {
        // Regressione P0-1: `v.is_empty()` in un crate NON deve agganciarsi a
        // `GraphDelta::is_empty` di un ALTRO crate solo perché condividono il nome.
        // È esattamente il falso positivo che il fallback globale per nome produceva
        // (`handle_import CALLS GraphDelta::is_empty` indicizzando CodeOS stesso).
        let parsed = vec![
            parse_rust(
                "/repo/crates/codeos-types/src/lib.rs",
                "pub struct GraphDelta;\nimpl GraphDelta {\n    pub fn is_empty(&self) -> bool {\n        true\n    }\n}\n",
            )
            .await,
            parse_rust(
                "/repo/crates/codeos-graph/src/resolver.rs",
                "pub fn handle_import() {\n    let v: Vec<u8> = Vec::new();\n    v.is_empty();\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();

        let handle = find(&delta, "crates::codeos-graph::src::resolver::handle_import");
        let is_empty = find(
            &delta,
            "crates::codeos-types::src::lib::GraphDelta::is_empty",
        );

        // NESSUN arco Calls cross-crate da handle_import all'omonimo is_empty.
        assert!(
            !delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::Calls
                    && r.source_id == handle.id
                    && r.target_id == is_empty.id
            }),
            "il nome semplice non deve agganciare un omonimo di un altro crate"
        );
    }

    #[tokio::test]
    async fn resolves_go_intramodule_call_and_method_receiver() {
        // End-to-end Go: prova che `detect_language` riconosca `.go` (altrimenti il
        // resolver userebbe "unknown" e il language-match fallirebbe), così la call
        // intra-modulo risolve e il metodo appartiene al tipo ricevente.
        let src = "package m\n\ntype Server struct{}\n\nfunc (s *Server) Start() {\n    boot()\n}\n\nfunc boot() {}\n";
        let parsed = parse_go("m.go", src).await;
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        let delta = resolver.resolve(&[parsed], &storage).await.unwrap();

        let server = find(&delta, "m::Server");
        assert_eq!(server.kind, EntityKind::Struct);
        let start = find(&delta, "m::Server::Start");
        assert_eq!(start.kind, EntityKind::Method);
        let boot = find(&delta, "m::boot");

        // Il metodo Start appartiene a Server (BelongsTo da parent_local_id).
        assert!(
            delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::BelongsTo
                    && r.source_id == start.id
                    && r.target_id == server.id),
            "Start deve appartenere a Server"
        );
        // La call `boot()` dentro Start risolve alla funzione locale `boot`.
        assert!(
            delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::Calls
                    && r.source_id == start.id
                    && r.target_id == boot.id),
            "la call intra-modulo a boot deve risolvere (non Unresolved)"
        );
    }

    #[tokio::test]
    async fn resolves_java_intraclass_call_and_heritage() {
        // End-to-end Java: prova che `detect_language` riconosca `.java` (altrimenti
        // il resolver userebbe "unknown" e il language-match fallirebbe), così la
        // call intra-classe risolve, il metodo appartiene alla classe e `extends`
        // aggancia la superclasse locale.
        let src = r#"package com.example;

class BaseCache {}

class Cache extends BaseCache {
    String get(String key) {
        return lookup(key);
    }

    String lookup(String key) {
        return key;
    }
}
"#;
        let parsed = parse_java("app.java", src).await;
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        let delta = resolver.resolve(&[parsed], &storage).await.unwrap();

        let cache = find(&delta, "app::Cache");
        assert_eq!(cache.kind, EntityKind::Class);
        let base = find(&delta, "app::BaseCache");
        let get = find(&delta, "app::Cache::get");
        assert_eq!(get.kind, EntityKind::Method);
        let lookup = find(&delta, "app::Cache::lookup");

        // `get` appartiene a Cache (BelongsTo da parent_local_id).
        assert!(
            delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::BelongsTo
                    && r.source_id == get.id
                    && r.target_id == cache.id),
            "get deve appartenere a Cache"
        );
        // La call `lookup(key)` dentro `get` risolve al metodo locale `lookup`.
        assert!(
            delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::Calls
                    && r.source_id == get.id
                    && r.target_id == lookup.id),
            "la call intra-classe a lookup deve risolvere (non Unresolved)"
        );
        // `extends BaseCache` aggancia la superclasse locale (non Unresolved).
        assert!(
            delta
                .added_relations
                .iter()
                .any(|r| r.kind == RelationKind::Extends
                    && r.source_id == cache.id
                    && r.target_id == base.id),
            "extends deve agganciare BaseCache"
        );
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

    #[tokio::test]
    async fn stamps_birth_when_commit_context_is_known() {
        // Grafo temporale (vision step 2): con un commit noto, ogni nodo creato
        // nasce timbrato con quel commit+istante.
        let cc = CommitContext {
            commit: "deadbeef".to_string(),
            ts: 1_700_000_000,
        };
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None).with_commit_context(Some(cc.clone()));

        let delta = resolver
            .resolve(
                &[parse("m.py", "class Foo:\n    def bar(self):\n        pass\n").await],
                &storage,
            )
            .await
            .unwrap();

        // È la prima volta che vediamo questi nodi: nascono tutti al commit corrente.
        for qname in ["m", "m::Foo", "m::Foo::bar"] {
            let e = find(&delta, qname);
            assert_eq!(
                e.metadata.get("born_commit").map(String::as_str),
                Some("deadbeef"),
                "{qname}: born_commit assente/errato"
            );
            assert_eq!(
                e.metadata.get("born_ts").map(String::as_str),
                Some("1700000000"),
                "{qname}: born_ts assente/errato"
            );
        }
    }

    #[tokio::test]
    async fn reindex_preserves_birth_does_not_reset_to_current_commit() {
        // Anti-rumore temporale (trap #3): re-indicizzare un file NON deve far
        // sembrare "nato adesso" un nodo che esiste da commit precedenti. La nascita
        // si conserva attraverso il delete+recreate del re-index, agganciata
        // all'identità (`qualified_name`).
        let storage = SqliteStorage::in_memory().unwrap();
        let src = "class Foo:\n    def bar(self):\n        pass\n";

        // Indicizzazione al commit "aaa".
        let r1 = GraphResolver::new(None).with_commit_context(Some(CommitContext {
            commit: "aaa".to_string(),
            ts: 1000,
        }));
        let d1 = r1
            .resolve(&[parse("m.py", src).await], &storage)
            .await
            .unwrap();
        storage.apply_delta(d1).await.unwrap();

        // Re-indicizzazione dello STESSO file a un commit DIVERSO "bbb".
        let r2 = GraphResolver::new(None).with_commit_context(Some(CommitContext {
            commit: "bbb".to_string(),
            ts: 2000,
        }));
        let d2 = r2
            .resolve(&[parse("m.py", src).await], &storage)
            .await
            .unwrap();
        storage.apply_delta(d2).await.unwrap();

        // L'entità sopravvissuta conserva la nascita ORIGINALE (non "bbb"/2000).
        // Non-vacuo: una logica che timbra sempre il commit corrente fallirebbe qui.
        let foo = storage
            .get_entity_by_qname("m::Foo")
            .await
            .unwrap()
            .expect("Foo deve esistere dopo il re-index");
        assert_eq!(
            foo.metadata.get("born_commit").map(String::as_str),
            Some("aaa"),
            "born_commit resettato al commit corrente: rumore temporale!"
        );
        assert_eq!(
            foo.metadata.get("born_ts").map(String::as_str),
            Some("1000"),
            "born_ts resettato al commit corrente: rumore temporale!"
        );
    }

    #[tokio::test]
    async fn no_commit_context_stamps_no_birth() {
        // Retro-compatibilità + anti-falso-positivo sul tempo: senza commit noto NON
        // si inventa alcuna nascita. I percorsi e i test esistenti restano identici.
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None); // nessun with_commit_context

        let delta = resolver
            .resolve(&[parse("m.py", "class Foo:\n    pass\n").await], &storage)
            .await
            .unwrap();

        for e in &delta.added_entities {
            assert!(
                !e.metadata.contains_key("born_commit"),
                "{}: born_commit timbrato senza commit noto",
                e.qualified_name
            );
            assert!(
                !e.metadata.contains_key("born_ts"),
                "{}: born_ts timbrato senza commit noto",
                e.qualified_name
            );
        }
    }

    #[tokio::test]
    async fn stamps_birth_and_identity_on_resolved_edges() {
        // Grafo temporale (vision step 2), archi: una call RISOLTA nasce timbrata
        // col commit corrente e porta l'identità stabile (`source_qname`/`target_qname`).
        let cc = CommitContext {
            commit: "edge0001".to_string(),
            ts: 1_700_000_001,
        };
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None).with_commit_context(Some(cc));

        let src = "def helper():\n    pass\n\ndef top_level():\n    helper()\n";
        let delta = resolver
            .resolve(&[parse("m.py", src).await], &storage)
            .await
            .unwrap();

        let call = delta
            .added_relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls)
            .expect("la call risolta a 'helper' è assente");
        assert_eq!(
            call.metadata.get("source_qname").map(String::as_str),
            Some("m::top_level"),
            "source_qname dell'arco assente/errato"
        );
        assert_eq!(
            call.metadata.get("target_qname").map(String::as_str),
            Some("m::helper"),
            "target_qname dell'arco assente/errato"
        );
        assert_eq!(
            call.metadata.get("born_commit").map(String::as_str),
            Some("edge0001"),
            "born_commit dell'arco assente/errato"
        );
        assert_eq!(
            call.metadata.get("born_ts").map(String::as_str),
            Some("1700000001"),
            "born_ts dell'arco assente/errato"
        );
    }

    #[tokio::test]
    async fn reindex_preserves_resolved_edge_birth() {
        // Anti-rumore temporale (trap #3) sugli ARCHI: re-indicizzare un file non
        // deve far sembrare "nato adesso" un arco che esiste da commit precedenti.
        // La nascita si conserva attraverso il delete+recreate, agganciata
        // all'identità stabile `(source_qname, kind, target_qname)`.
        let storage = SqliteStorage::in_memory().unwrap();
        let src = "def helper():\n    pass\n\ndef top_level():\n    helper()\n";

        let r1 = GraphResolver::new(None).with_commit_context(Some(CommitContext {
            commit: "aaa".to_string(),
            ts: 1000,
        }));
        let d1 = r1
            .resolve(&[parse("m.py", src).await], &storage)
            .await
            .unwrap();
        storage.apply_delta(d1).await.unwrap();

        // Re-index dello STESSO file a un commit DIVERSO "bbb".
        let r2 = GraphResolver::new(None).with_commit_context(Some(CommitContext {
            commit: "bbb".to_string(),
            ts: 2000,
        }));
        let d2 = r2
            .resolve(&[parse("m.py", src).await], &storage)
            .await
            .unwrap();
        storage.apply_delta(d2).await.unwrap();

        let calls = storage
            .query_relations(RelationFilter {
                kind: Some(RelationKind::Calls),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(calls.len(), 1, "atteso un solo arco Calls dopo il re-index");
        // Non-vacuo: una logica che timbra sempre il commit corrente darebbe "bbb"/2000.
        assert_eq!(
            calls[0].metadata.get("born_commit").map(String::as_str),
            Some("aaa"),
            "born_commit dell'arco resettato al commit corrente: rumore temporale!"
        );
        assert_eq!(
            calls[0].metadata.get("born_ts").map(String::as_str),
            Some("1000"),
            "born_ts dell'arco resettato al commit corrente: rumore temporale!"
        );
    }

    #[tokio::test]
    async fn unresolved_edge_carries_no_birth_nor_identity() {
        // Trappola #3 sul tempo: un arco che NON risolve (target nil, nessuna
        // identità) non deve ricevere una nascita — sarebbe un timestamp sul rumore.
        let cc = CommitContext {
            commit: "edge0002".to_string(),
            ts: 1_700_000_002,
        };
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None).with_commit_context(Some(cc));

        // `self.thing.mystery_method()` arriva come bare `mystery_method`, che non
        // esiste nel modulo: il resolver si astiene (⇒ Unresolved), niente arco bugiardo.
        let src = "class C:\n    def run(self):\n        self.thing.mystery_method()\n";
        let delta = resolver
            .resolve(&[parse("m.py", src).await], &storage)
            .await
            .unwrap();

        let unresolved = delta
            .added_relations
            .iter()
            .find(|r| r.kind == RelationKind::Unresolved)
            .expect("atteso un arco Unresolved per 'mystery_method'");
        assert!(
            !unresolved.metadata.contains_key("born_commit")
                && !unresolved.metadata.contains_key("born_ts"),
            "un arco Unresolved non deve avere una nascita (rumore temporale)"
        );
        assert!(
            !unresolved.metadata.contains_key("source_qname")
                && !unresolved.metadata.contains_key("target_qname"),
            "un arco Unresolved non ha un'identità stabile da timbrare"
        );
    }

    #[tokio::test]
    async fn no_commit_context_stamps_no_edge_birth() {
        // Senza commit noto non si inventa la nascita nemmeno sugli archi (retro-
        // compatibilità + anti-falso-positivo sul tempo). L'identità (`*_qname`),
        // indipendente dal tempo, resta comunque presente.
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None); // nessun with_commit_context
        let src = "def helper():\n    pass\n\ndef top_level():\n    helper()\n";
        let delta = resolver
            .resolve(&[parse("m.py", src).await], &storage)
            .await
            .unwrap();

        let call = delta
            .added_relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls)
            .expect("la call risolta a 'helper' è assente");
        assert!(
            !call.metadata.contains_key("born_commit") && !call.metadata.contains_key("born_ts"),
            "arco timbrato con una nascita senza commit noto"
        );
        assert_eq!(
            call.metadata.get("source_qname").map(String::as_str),
            Some("m::top_level"),
            "l'identità dell'arco non dipende dal tempo: deve esserci comunque"
        );
        assert_eq!(
            call.metadata.get("target_qname").map(String::as_str),
            Some("m::helper")
        );
    }

    #[test]
    fn classify_source_kind_by_path_convention() {
        // Directory `tests/` / `__tests__/`: segnale universale.
        assert_eq!(
            classify_source_kind("crates/foo/tests/it.rs", "rust"),
            "test"
        );
        assert_eq!(
            classify_source_kind("src/app/__tests__/x.ts", "typescript"),
            "test"
        );
        // Go: suffisso `_test.go`.
        assert_eq!(classify_source_kind("pkg/server_test.go", "go"), "test");
        assert_eq!(classify_source_kind("pkg/server.go", "go"), "prod");
        // Java/Kotlin: albero `src/test/`.
        assert_eq!(
            classify_source_kind("mod/src/test/java/AppTest.java", "java"),
            "test"
        );
        assert_eq!(
            classify_source_kind("mod/src/main/java/App.java", "java"),
            "prod"
        );
        // Python: `test_*.py` o `*_test.py`.
        assert_eq!(classify_source_kind("test_service.py", "python"), "test");
        assert_eq!(classify_source_kind("service_test.py", "python"), "test");
        assert_eq!(classify_source_kind("service.py", "python"), "prod");
        // TS/JS: `.test.` o `.spec.` nel nome.
        assert_eq!(
            classify_source_kind("a/b/comp.test.ts", "typescript"),
            "test"
        );
        assert_eq!(
            classify_source_kind("a/b/comp.spec.tsx", "typescript"),
            "test"
        );
        assert_eq!(classify_source_kind("a/b/comp.ts", "typescript"), "prod");
        // Rust: gli unit test inline `#[cfg(test)]` non sono distinguibili dal
        // path → "prod" (conservativo).
        assert_eq!(classify_source_kind("src/lib.rs", "rust"), "prod");
    }

    #[tokio::test]
    async fn stamps_source_kind_on_entities_and_relations() {
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(None);

        // Stessa forma (helper + chiamante nello stesso modulo) in due file: uno di
        // produzione, uno di test (convenzione Python `test_*.py`).
        let body = "def helper():\n    pass\n\ndef top():\n    helper()\n";
        let prod = parse("app/m.py", body).await;
        let test = parse("tests/test_m.py", body).await;

        let delta = resolver.resolve(&[prod, test], &storage).await.unwrap();

        // Entità: ereditano il source_kind del proprio file.
        let prod_top = find(&delta, "app::m::top");
        assert_eq!(
            prod_top.metadata.get("source_kind").map(String::as_str),
            Some("prod")
        );
        let test_top = find(&delta, "tests::test_m::top");
        assert_eq!(
            test_top.metadata.get("source_kind").map(String::as_str),
            Some("test")
        );

        // Archi Calls: ognuno porta il source_kind del file SORGENTE (P0-1: la
        // risoluzione per nome semplice è same-module, niente confusione fra i due
        // `helper` omonimi).
        let prod_call = delta
            .added_relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls && r.source_id == prod_top.id)
            .expect("call prod assente");
        assert_eq!(
            prod_call.metadata.get("source_kind").map(String::as_str),
            Some("prod")
        );
        let test_call = delta
            .added_relations
            .iter()
            .find(|r| r.kind == RelationKind::Calls && r.source_id == test_top.id)
            .expect("call test assente");
        assert_eq!(
            test_call.metadata.get("source_kind").map(String::as_str),
            Some("test")
        );
    }

    #[tokio::test]
    async fn type_fragments_collapse_into_one_canonical() {
        // Anti-frammentazione (Passo 1.5): lo stesso tipo con `impl` sparsi su più
        // file non deve diventare N entità. `Version` è definito in lib.rs e
        // ri-implementato in display.rs (che lo importa con `use crate::Version`):
        // il placeholder `impl_target` di display.rs va fuso nell'UNICA definizione
        // canonica, e i suoi metodi diventano figli del canonico.
        let parsed = vec![
            parse_rust(
                "/repo/src/lib.rs",
                "pub struct Version;\nimpl Version {\n    pub fn new() {}\n}\n",
            )
            .await,
            parse_rust(
                "/repo/src/display.rs",
                "use crate::Version;\nimpl Version {\n    pub fn show(&self) {}\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();

        // Una sola entità-tipo `Version`: il canonico di lib.rs.
        let canonical = find(&delta, "src::lib::Version");
        assert_eq!(canonical.kind, EntityKind::Struct);
        let version_entities = delta
            .added_entities
            .iter()
            .filter(|e| e.qualified_name.ends_with("::Version"))
            .count();
        assert_eq!(
            version_entities, 1,
            "il placeholder display::Version dev'essere stato fuso nel canonico"
        );
        // Il frammento per-file è sparito.
        assert!(
            !delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name == "src::display::Version"),
            "nessun frammento per-file deve sopravvivere"
        );

        // Il metodo definito nell'impl sparso (display.rs) ora appartiene al canonico.
        let show = find(&delta, "src::display::Version::show");
        assert!(
            delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::BelongsTo
                    && r.source_id == show.id
                    && r.target_id == canonical.id
            }),
            "il metodo dell'impl sparso dev'essere figlio del tipo canonico"
        );
        // E il metodo della definizione vera resta figlio dello stesso canonico.
        let new = find(&delta, "src::lib::Version::new");
        assert!(
            delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::BelongsTo
                    && r.source_id == new.id
                    && r.target_id == canonical.id
            }),
            "anche il metodo della definizione vera resta figlio del canonico"
        );
        // Nessun placeholder lascia un BelongsTo orfano verso il suo modulo.
        assert!(
            !delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::BelongsTo && r.source_id == show.id && {
                    let m = find(&delta, "src::display");
                    r.target_id == m.id
                }
            }),
            "il BelongsTo del metodo non deve puntare al modulo del frammento"
        );
    }

    #[tokio::test]
    async fn homonym_types_block_canonicalization() {
        // Guardia anti-merge-bugiardo: due tipi `Error` distinti (moduli diversi)
        // rendono ambiguo a quale appartenga un `impl Error` di un terzo file. Con
        // ≥2 candidati canonici NON si fonde: un merge mancante batte uno bugiardo.
        let parsed = vec![
            parse_rust("/repo/src/a.rs", "pub struct Error;\n").await,
            parse_rust("/repo/src/b.rs", "pub struct Error;\n").await,
            parse_rust(
                "/repo/src/c.rs",
                "impl Error {\n    pub fn code(&self) {}\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();

        // Le due definizioni reali restano distinte...
        find(&delta, "src::a::Error");
        find(&delta, "src::b::Error");
        // ...e il placeholder ambiguo di c.rs NON viene fuso (resta dov'è).
        assert!(
            delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name == "src::c::Error"),
            "con omonimi ambigui il placeholder non va fuso"
        );
    }

    #[tokio::test]
    async fn external_imported_type_blocks_canonicalization() {
        // Guardia import-esterno: se il file che fa `impl Error` ha importato `Error`
        // da un CRATE ESTERNO (`use other_crate::Error`), quell'impl riguarda il tipo
        // esterno, non l'omonimo locale: non va fuso nel `struct Error` del progetto.
        let parsed = vec![
            parse_rust("/repo/src/lib.rs", "pub struct Error;\n").await,
            parse_rust(
                "/repo/src/b.rs",
                "use other_crate::Error;\nimpl Error {\n    pub fn foo(&self) {}\n}\n",
            )
            .await,
        ];
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        let delta = resolver.resolve(&parsed, &storage).await.unwrap();

        // La definizione locale resta...
        find(&delta, "src::lib::Error");
        // ...e il placeholder di b.rs NON viene fuso (l'impl è sul tipo esterno).
        assert!(
            delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name == "src::b::Error"),
            "un impl su un tipo importato da crate esterno non va fuso nell'omonimo locale"
        );
    }

    #[tokio::test]
    async fn type_fragment_merges_with_db_canonical_on_single_file_reindex() {
        // Re-index di un SINGOLO file (follow-up del Fix #3): la definizione canonica
        // non è nel batch — vive in un altro file, già indicizzato e persistito. Senza
        // il lookup su storage il placeholder `impl_target` ri-frammenterebbe il tipo a
        // ogni re-index incrementale (il bug che questo test blinda). Scenario: prima
        // si indicizza tutto (lib.rs definisce `Version`, display.rs lo ri-implementa),
        // si persiste, poi si re-indicizza il SOLO display.rs.
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        // 1. Indicizzazione full-project + persistenza nello storage.
        let full = vec![
            parse_rust(
                "/repo/src/lib.rs",
                "pub struct Version;\nimpl Version {\n    pub fn new() {}\n}\n",
            )
            .await,
            parse_rust(
                "/repo/src/display.rs",
                "use crate::Version;\nimpl Version {\n    pub fn show(&self) {}\n}\n",
            )
            .await,
        ];
        let delta_full = resolver.resolve(&full, &storage).await.unwrap();
        storage.apply_delta(delta_full).await.unwrap();

        // Il canonico persistito è uno solo: `src::lib::Version`.
        let canonical = storage
            .get_entity_by_qname("src::lib::Version")
            .await
            .unwrap()
            .expect("il tipo canonico dev'essere persistito dopo l'indice full");

        // 2. Re-index del SOLO display.rs: la def canonica resta nel DB, non nel batch.
        let reindex = vec![
            parse_rust(
                "/repo/src/display.rs",
                "use crate::Version;\nimpl Version {\n    pub fn show(&self) {}\n}\n",
            )
            .await,
        ];
        let delta = resolver.resolve(&reindex, &storage).await.unwrap();

        // Nessun frammento per-file ricompare: il placeholder è stato fuso col canonico
        // trovato nello storage (la via batch qui non vedrebbe alcuna definizione).
        assert!(
            !delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name == "src::display::Version"),
            "il re-index di un singolo file NON deve ri-frammentare il tipo: \
             il placeholder va fuso col canonico nello storage"
        );

        // Il metodo ri-creato appartiene al canonico DB, non a un frammento locale.
        let show = find(&delta, "src::display::Version::show");
        let show_id = show.id;
        assert!(
            delta.added_relations.iter().any(|r| {
                r.kind == RelationKind::BelongsTo
                    && r.source_id == show_id
                    && r.target_id == canonical.id
            }),
            "il metodo dell'impl re-indicizzato dev'essere figlio del tipo canonico DB"
        );

        // 3. Integrità end-to-end: applicato il delta del re-index, il grafo resta
        //    consistente — il canonico DB sopravvive (non era nel file re-indicizzato),
        //    il frammento non esiste, e l'arco del metodo punta a un'entità reale.
        storage.apply_delta(delta).await.unwrap();
        assert!(
            storage
                .get_entity_by_qname("src::lib::Version")
                .await
                .unwrap()
                .is_some(),
            "il tipo canonico (altro file) non dev'essere toccato dal re-index"
        );
        assert!(
            storage
                .get_entity_by_qname("src::display::Version")
                .await
                .unwrap()
                .is_none(),
            "nessun frammento per-file deve persistere nel grafo dopo il re-index"
        );
        let belongs = storage
            .query_relations(RelationFilter {
                source_id: Some(show_id),
                kind: Some(RelationKind::BelongsTo),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            belongs.iter().any(|r| r.target_id == canonical.id),
            "dopo l'apply, l'arco BelongsTo del metodo punta al canonico DB (nessun arco pendente)"
        );
    }

    #[tokio::test]
    async fn db_canonical_lookup_abstains_on_homonyms() {
        // Guardia anti-merge-bugiardo per la NUOVA via DB (re-index singolo file):
        // se nello storage esistono ≥2 definizioni omonime, a quale apparterrebbe un
        // `impl` di un terzo file è ambiguo ⇒ non si fonde (un merge mancante batte uno
        // bugiardo). È l'analogo di `homonym_types_block_canonicalization` ma quando i
        // candidati vivono nel DB, non nel batch.
        let storage = SqliteStorage::in_memory().unwrap();
        let resolver = GraphResolver::new(Some("/repo".to_string()));

        // 1. Persisti due `Item` distinti (moduli diversi dello stesso crate).
        let full = vec![
            parse_rust("/repo/src/a.rs", "pub struct Item;\n").await,
            parse_rust("/repo/src/b.rs", "pub struct Item;\n").await,
        ];
        let delta_full = resolver.resolve(&full, &storage).await.unwrap();
        storage.apply_delta(delta_full).await.unwrap();

        // 2. Re-index del solo c.rs con un `impl Item`: i canonici sono nel DB, ≥2.
        let reindex = vec![
            parse_rust(
                "/repo/src/c.rs",
                "impl Item {\n    pub fn code(&self) {}\n}\n",
            )
            .await,
        ];
        let delta = resolver.resolve(&reindex, &storage).await.unwrap();

        // Con omonimi ambigui nel DB il placeholder NON viene fuso: resta dov'è.
        assert!(
            delta
                .added_entities
                .iter()
                .any(|e| e.qualified_name == "src::c::Item"),
            "con ≥2 canonici omonimi nel DB il lookup dev'astenersi (niente merge indovinato)"
        );
    }
}
