//! Fixture E2E anti-falsi-positivi (roadmap P7-24).
//!
//! Indicizza un mini-progetto realistico **attraverso il sistema completo**
//! (Dispatcher → ParserActor → GraphResolver → Storage → GuardianActor) e prova
//! che le tre correzioni del blocco P0 reggano *insieme*, end-to-end — non solo
//! isolate nei rispettivi unit test:
//!
//!  - **P0-1** (resolver): due moduli con un metodo **omonimo** (`is_empty`) non
//!    vengono collegati per sbaglio quando uno chiama `v.is_empty()` su un tipo
//!    locale. Il falso positivo storico era `handle_import CALLS
//!    GraphDelta::is_empty` indicizzando CodeOS stesso.
//!  - **P0-2** (guardian): una dipendenza esterna (`tokio`) viene tracciata come
//!    nodo sintetico ma **non** entra nella mappa dei layer, quindi non genera
//!    invarianti fantasma tipo "external non deve dipendere da …".
//!  - **P0-3** (parser): l'output di build (`out/`) e le dipendenze installate
//!    (`node_modules/`) non vengono **mai** indicizzati.
//!
//! È anche il controllo positivo di **P0-1b**: gli archi risolti portano nei
//! metadata la confidenza di risoluzione (`high` per l'esterno, `medium` per il
//! match per nome semplice nello stesso modulo).

use std::path::Path;
use std::sync::Arc;

use codeos_core::spawn_with_storage_decisions_and_repo;
use codeos_memory::InMemoryDecisionStore;
use codeos_storage::{GraphStorage, RelationFilter, SqliteStorage};
use codeos_types::bus::{CodeOsEvent, Command};
use codeos_types::{EntityKind, RelationKind};

/// Scrive un file creando al volo le directory genitrici.
async fn write_file(path: &Path, contents: &str) {
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(path, contents).await.unwrap();
}

#[tokio::test]
async fn indexing_a_realistic_project_produces_no_false_positives() {
    // --- 1. Costruisci il mini-progetto su disco --------------------------------
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("codeos_e2e_{nanos}"));

    // `types_like`: definisce GraphDelta::is_empty — l'omonimo "vittima".
    write_file(
        &root.join("crates/types_like/src/lib.rs"),
        "pub struct GraphDelta;\n\
         impl GraphDelta {\n\
         \x20   pub fn is_empty(&self) -> bool {\n\
         \x20       true\n\
         \x20   }\n\
         }\n",
    )
    .await;

    // `graph_like`: importa tokio (esterno), chiama un helper locale (deve
    // risolvere) e chiama `v.is_empty()` su una Vec locale (NON deve agganciare
    // l'omonimo di `types_like`).
    write_file(
        &root.join("crates/graph_like/src/resolver.rs"),
        "use tokio::sync::mpsc;\n\n\
         pub fn handle_import() {\n\
         \x20   helper();\n\
         \x20   let v: Vec<u8> = Vec::new();\n\
         \x20   v.is_empty();\n\
         }\n\n\
         fn helper() {}\n",
    )
    .await;

    // Output di build e dipendenze installate: la camminata deve saltarli del
    // tutto. Sono `.js` (li gestirebbe il parser TS), quindi se NON fossero
    // ignorati produrrebbero le entità `generatedJunk` / `leftPad`.
    write_file(
        &root.join("crates/graph_like/out/generated.js"),
        "export function generatedJunk() { return 42; }\n",
    )
    .await;
    write_file(
        &root.join("node_modules/left-pad/index.js"),
        "module.exports = function leftPad() {};\n",
    )
    .await;

    // --- 2. Avvia il sistema con uno storage ispezionabile ----------------------
    // Passandolo noi, possiamo interrogare il grafo dopo l'indicizzazione.
    // repo_root = None: niente git (il temp dir non è un repo), così il test resta
    // deterministico; i qualified_name conterranno il prefisso del path temporaneo,
    // irrilevante per le asserzioni (cerchiamo per sotto-stringa).
    let storage: Arc<dyn GraphStorage> = Arc::new(SqliteStorage::in_memory().unwrap());
    let decisions = Arc::new(InMemoryDecisionStore::new());
    let system = spawn_with_storage_decisions_and_repo(storage.clone(), decisions, None);

    let mut events = system.events.subscribe();

    // --- 3. Indicizza l'intero progetto (esercita la camminata che salta out/) --
    let (reply_to, mut reply_rx) = tokio::sync::mpsc::channel(1);
    system
        .commands
        .send(Command::IndexProject {
            project_root: root.to_string_lossy().to_string(),
            reply_to,
        })
        .await
        .expect("front door chiusa");
    reply_rx
        .recv()
        .await
        .expect("nessuna risposta dal parser")
        .expect("IndexProject fallito");

    // Attendi che il GraphActor abbia applicato il delta (un solo GraphUpdated:
    // IndexProject pubblica un unico FilesIndexed col batch completo).
    loop {
        match events.recv().await.expect("bus chiuso prima di GraphUpdated") {
            CodeOsEvent::GraphUpdated { .. } => break,
            _ => continue,
        }
    }

    // --- 4. P0-3: l'output di build NON è stato indicizzato ----------------------
    assert!(
        storage
            .find_entities_by_name_pattern("generatedJunk")
            .await
            .unwrap()
            .is_empty(),
        "out/ non deve essere indicizzato"
    );
    assert!(
        storage
            .find_entities_by_name_pattern("leftPad")
            .await
            .unwrap()
            .is_empty(),
        "node_modules/ non deve essere indicizzato"
    );

    // Controllo positivo: i sorgenti veri SÌ. Senza questo, il test passerebbe
    // anche se non avessimo indicizzato nulla.
    let handle = exactly_one(&storage, "handle_import").await;
    let helper = exactly_one(&storage, "::helper").await;
    let victim = exactly_one(&storage, "GraphDelta::is_empty").await;

    // --- 5. P0-1: l'omonimo cross-modulo NON è collegato ------------------------
    let from_handle = storage
        .query_relations(RelationFilter {
            source_id: Some(handle.id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !from_handle.iter().any(|r| r.target_id == victim.id),
        "il nome semplice `v.is_empty()` non deve agganciare l'omonimo di un altro modulo"
    );

    // --- 6. P0-1b: la call intra-modulo a `helper` risolve, con confidenza media -
    let to_helper = from_handle
        .iter()
        .find(|r| r.kind == RelationKind::Calls && r.target_id == helper.id)
        .expect("la call intra-modulo a helper deve risolvere (non Unresolved)");
    assert_eq!(
        to_helper
            .metadata
            .get("resolution_confidence")
            .map(String::as_str),
        Some("medium"),
        "una risoluzione per nome semplice nello stesso modulo è confidenza media"
    );

    // --- 7. P0-2: `tokio` è tracciato come dipendenza esterna, agganciata high ---
    let tokio_ext = storage
        .get_entity_by_qname("external::tokio")
        .await
        .unwrap()
        .expect("external::tokio deve esistere (l'esterno si traccia, non si butta)");
    assert_eq!(tokio_ext.kind, EntityKind::ExternalDependency);

    let into_tokio = storage
        .query_relations(RelationFilter {
            target_id: Some(tokio_ext.id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !into_tokio.is_empty(),
        "l'import di tokio deve agganciarsi a external::tokio"
    );
    assert!(
        into_tokio.iter().all(|r| r.kind == RelationKind::Imports),
        "gli archi verso un crate esterno sono import"
    );
    assert!(
        into_tokio
            .iter()
            .any(|r| r.metadata.get("resolution_confidence").map(String::as_str) == Some("high")),
        "una dipendenza esterna è agganciata con confidenza alta"
    );

    // --- 8. P0-2: il referto NON contiene invarianti fantasma sull'esterno ------
    let (rep_tx, mut rep_rx) = tokio::sync::mpsc::channel(1);
    system
        .commands
        .send(Command::ArchitectureReport { reply_to: rep_tx })
        .await
        .expect("front door chiusa");
    let report = rep_rx
        .recv()
        .await
        .expect("nessuna risposta dal guardian")
        .expect("referto fallito");

    for inv in &report.invariants {
        assert!(
            !inv.upstream.contains("external") && !inv.downstream.contains("external"),
            "nessun invariante deve nascere da un layer esterno: {} → {}",
            inv.downstream,
            inv.upstream
        );
    }
    for gap in &report.gaps {
        assert!(
            !gap.upstream.contains("external") && !gap.downstream.contains("external"),
            "nessuna lacuna deve riferirsi a un layer esterno: {} → {}",
            gap.downstream,
            gap.upstream
        );
    }

    let _ = tokio::fs::remove_dir_all(&root).await;
}

/// Recupera l'unica entità il cui `qualified_name` contiene `needle`, fallendo
/// con un messaggio chiaro se ce ne sono zero o più d'una.
async fn exactly_one(storage: &Arc<dyn GraphStorage>, needle: &str) -> codeos_types::Entity {
    let mut hits = storage.find_entities_by_name_pattern(needle).await.unwrap();
    assert_eq!(
        hits.len(),
        1,
        "atteso esattamente 1 match per '{needle}', trovati {}: {:?}",
        hits.len(),
        hits.iter().map(|e| &e.qualified_name).collect::<Vec<_>>()
    );
    hits.remove(0)
}
