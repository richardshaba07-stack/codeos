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
        // Mappa id→source_kind ("test"/"prod"): nel Passo 3 la consultiamo per
        // timbrare ogni arco di dipendenza con la natura del file SORGENTE, così il
        // Guardian può escludere dal mining gli archi nati nei test (vedi
        // `cross_layer`).
        let mut new_by_id_source_kind: HashMap<EntityId, String> = HashMap::new();

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
            self.collect_removals(&file.file_path, storage, &mut delta)
                .await?;

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
        );
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
/// Limite noto (follow-up): il merge è *batch-local*. Nel re-index di un singolo
/// file la definizione canonica può non essere nel batch; servirà un lookup su
/// storage. L'indicizzazione full-project (caso normale) passa tutti i file in un
/// solo batch, quindi qui funziona.
///
/// Ritorna il numero di placeholder fusi.
fn canonicalize_type_fragments(
    delta: &mut GraphDelta,
    new_by_qname: &mut HashMap<String, EntityId>,
    new_by_name: &mut HashMap<String, Vec<(EntityId, String)>>,
    aux: &[EntityAux],
    file_ctxs: &[FileContext],
) -> usize {
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
        let Some(cands) = canon_by_name.get(a.name.as_str()) else {
            continue; // 0 definizioni locali omonime: tipo esterno/sconosciuto
        };
        if cands.len() != 1 {
            continue; // ≥2 omonimi nel progetto: ambiguo, non fondere
        }
        let canonical = cands[0];
        if canonical == a.id {
            continue;
        }
        let ctx = &file_ctxs[a.file_idx];
        if let Some(target) = ctx.namespace.get(&a.name) {
            if external_dependency_root(target, &ctx.language, &ctx.namespace, true).is_some() {
                continue; // il nome è importato da un crate esterno
            }
        }
        merge.insert(a.id, canonical);
    }
    if merge.is_empty() {
        return 0;
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
    merge.len()
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

/// Prefisso di crate del chiamante a partire dal suo `module_prefix`.
/// `crates::codeos-paleo::src::fossil` → `Some("crates::codeos-paleo::")`.
/// `None` se il file non vive sotto `crates::<crate>::` (non possiamo limitare la
/// ricerca a un crate, quindi non risolviamo crate-local).
fn caller_crate_prefix(module_prefix: &str) -> Option<String> {
    let mut parts = module_prefix.split("::");
    if parts.next()? != "crates" {
        return None;
    }
    let crate_name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("crates::{crate_name}::"))
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
        // Anti-bugia (a2), misurato su semver: il corpo di `Comparator::matches` è
        // `eval::matches_comparator(...)`, dove `eval` è il modulo LOCALE di semver
        // (`src/eval.rs`, e `eval::matches_comparator` ESISTE come entità). Il
        // resolver coniava un `external::eval` e ci puntava la call: una dipendenza
        // esterna INVENTATA (bugia) e insieme una mancata risoluzione del fratello.
        // Ora una call il cui path-root è un modulo nostro NON si esternalizza: resta
        // Unresolved (onesto). La risoluzione vera al fratello cross-module è un
        // recall-win separato (layout non-`crates::`), qui basta non mentire.
        //
        // NB: layout senza prefisso `crates::` (project_root=None) → lo Stadio 2.5
        // (`caller_crate_prefix`) si astiene, quindi la call cade fino al fallback
        // esterno, esattamente come nel layout fisico di semver (`private::tmp::…`).
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
                        .is_some_and(|t| t.contains("eval") && t.ends_with("matches_comparator"))
            }),
            "la call verso il modulo locale deve restare Unresolved, non external"
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
}
