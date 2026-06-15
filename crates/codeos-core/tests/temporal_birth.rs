//! Fixture E2E del grafo temporale (vision step 2): prova che il *cablaggio* del
//! `GraphActor` nel sistema reale timbra la nascita dei nodi con l'HEAD **vero**
//! del repo indicizzato.
//!
//! Gli unit test del grafo iniettano un `CommitProvider` costante: provano lo
//! stamping, ma NON esercitano la *colla* di `codeos-core` — la closure che
//! cattura la root del repo e chiama `codeos_paleo::head_commit`. Questo test la
//! esercita end-to-end, attraverso il Dispatcher, con un repo git ermetico:
//! l'unica prova che la nascita timbrata nel sistema vivo combaci con l'HEAD reale
//! del repo, e non con un valore inventato. È il pendant temporale di
//! `anti_false_positives.rs` (che gira di proposito con `repo_root = None`).

use std::path::Path;
use std::process::Command as Git;
use std::sync::Arc;

use codeos_core::spawn_with_storage_decisions_and_repo;
use codeos_memory::InMemoryDecisionStore;
use codeos_storage::{GraphStorage, SqliteStorage};
use codeos_types::bus::{CodeOsEvent, Command};

/// Esegue un comando git nel repo di test, fallendo con un messaggio chiaro se
/// git non è disponibile o il comando non riesce (un ambiente senza git dà un
/// errore leggibile invece di un panic opaco).
fn run_git(dir: &Path, args: &[&str]) {
    let out = Git::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git deve essere disponibile per questo test");
    assert!(
        out.status.success(),
        "git {args:?} fallito: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Cattura l'output (trimmato) di un git che stampa un solo valore, fallendo chiaro.
fn git_capture(dir: &Path, args: &[&str]) -> String {
    let out = Git::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git deve essere disponibile per questo test");
    assert!(
        out.status.success(),
        "git {args:?} fallito: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Inizializza un repo git ermetico e isolato dalla config globale dell'utente
/// (identità locale, niente firma gpg): un commit di test non deve dipendere
/// dall'ambiente né fallire per una firma mancante.
fn init_repo(dir: &Path) {
    run_git(dir, &["init", "-q"]);
    run_git(dir, &["config", "user.email", "test@codeos.local"]);
    run_git(dir, &["config", "user.name", "CodeOS Test"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
}

#[tokio::test]
async fn live_indexing_stamps_birth_with_the_real_repo_head() {
    // --- 1. Repo git ermetico con un sorgente Rust committato -------------------
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("codeos_temporal_e2e_{nanos}"));
    tokio::fs::create_dir_all(root.join("src")).await.unwrap();
    // Un nome di funzione distintivo: nessuna collisione di sotto-stringa col path
    // temporaneo né con altre entità.
    tokio::fs::write(root.join("src/lib.rs"), "pub fn temporal_birthmark() {}\n")
        .await
        .unwrap();

    init_repo(&root);
    run_git(&root, &["add", "."]);
    run_git(&root, &["commit", "-q", "-m", "primo commit"]);

    // L'HEAD reale del repo: è il valore con cui la nascita DEVE combaciare.
    let real_head = git_capture(&root, &["rev-parse", "HEAD"]);
    assert_eq!(real_head.len(), 40, "HEAD reale a 40 hex, non finto");
    let real_ts: i64 = git_capture(&root, &["show", "-s", "--format=%ct", "HEAD"])
        .parse()
        .expect("committer date di HEAD è un intero");

    // --- 2. Avvia il sistema PUNTANDO al repo (repo_root = Some) -----------------
    // È la differenza con anti_false_positives.rs (che usa None): qui vogliamo che
    // il compositore inietti il provider git reale nel GraphActor.
    let storage: Arc<dyn GraphStorage> = Arc::new(SqliteStorage::in_memory().unwrap());
    let decisions = Arc::new(InMemoryDecisionStore::new());
    let system =
        spawn_with_storage_decisions_and_repo(storage.clone(), decisions, Some(root.clone()));

    let mut events = system.events.subscribe();

    // --- 3. Indicizza il progetto attraverso il Dispatcher reale ----------------
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

    // Un solo GraphUpdated: IndexProject pubblica un unico FilesIndexed col batch.
    loop {
        match events
            .recv()
            .await
            .expect("bus chiuso prima di GraphUpdated")
        {
            CodeOsEvent::GraphUpdated { .. } => break,
            _ => continue,
        }
    }

    // --- 4. L'entità reale nasce timbrata con l'HEAD VERO del repo ---------------
    let mut hits = storage
        .find_entities_by_name_pattern("temporal_birthmark")
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "atteso esattamente 1 `temporal_birthmark`, trovati {}: {:?}",
        hits.len(),
        hits.iter().map(|e| &e.qualified_name).collect::<Vec<_>>()
    );
    let entity = hits.remove(0);

    assert_eq!(
        entity.metadata.get("born_commit").map(String::as_str),
        Some(real_head.as_str()),
        "la nascita nel sistema vivo deve combaciare con l'HEAD reale del repo, \
         non con un valore inventato"
    );
    assert_eq!(
        entity
            .metadata
            .get("born_ts")
            .and_then(|s| s.parse::<i64>().ok()),
        Some(real_ts),
        "born_ts deve essere l'istante reale del commit di HEAD, non 0/finto"
    );

    let _ = tokio::fs::remove_dir_all(&root).await;
}
