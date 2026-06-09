//! Prova il lettore `GitLog` end-to-end contro un repository git **vero**, creato
//! al volo in una cartella temporanea. Se `git` non è disponibile (CI minimale),
//! il test si salta da solo invece di fallire.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use codeos_paleo::{occasions, CommitHistory, GitLog};

/// Esegue un comando git nella cartella `dir`. `None` se git manca o il comando
/// fallisce (usato per saltare il test in ambienti senza git).
fn git(dir: &Path, args: &[&str]) -> Option<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .ok()?;
    status.success().then_some(())
}

#[test]
fn measures_abstention_over_a_real_git_history() {
    let tmp = tempfile::tempdir().expect("cartella temporanea");
    let root = tmp.path().canonicalize().expect("canonicalize root");

    // Ambiente senza git: salta, non fallire.
    if git(&root, &["init", "-q"]).is_none() {
        eprintln!("git non disponibile o init fallito: test saltato");
        return;
    }
    // Identità + niente firma: il repo è effimero e isolato.
    git(&root, &["config", "user.email", "test@codeos.dev"]).expect("git config email");
    git(&root, &["config", "user.name", "CodeOS Test"]).expect("git config name");
    let _ = git(&root, &["config", "commit.gpgsign", "false"]);

    std::fs::create_dir_all(root.join("app/api")).unwrap();
    std::fs::create_dir_all(root.join("app/core")).unwrap();
    std::fs::write(root.join("app/api/h.py"), "x = 1\n").unwrap();
    std::fs::write(root.join("app/core/s.py"), "y = 1\n").unwrap();

    // Commit 1: tocca ENTRAMBI i layer ⇒ una occasione di astensione.
    git(&root, &["add", "-A"]).expect("git add");
    git(&root, &["commit", "-q", "-m", "co-touch api+core"]).expect("git commit 1");

    // Commit 2: tocca solo api ⇒ NON è un'occasione.
    std::fs::write(root.join("app/api/h.py"), "x = 2\n").unwrap();
    git(&root, &["add", "-A"]).expect("git add");
    git(&root, &["commit", "-q", "-m", "api only"]).expect("git commit 2");

    let commits = GitLog::new(root.as_path())
        .commits()
        .expect("lettura git log");
    assert_eq!(commits.len(), 2, "due commit attesi: {commits:?}");

    // `GitLog` assolutizza i path rispetto alla radice del repo (per combaciare
    // con i `file_path` assoluti del grafo): le chiavi di `file_layers` devono
    // perciò essere assolute, esattamente come i `changed_files` dei commit.
    let api_path = root.join("app/api/h.py").to_string_lossy().into_owned();
    let core_path = root.join("app/core/s.py").to_string_lossy().into_owned();
    assert!(
        commits.iter().any(|c| c.changed_files.contains(&api_path)),
        "i changed_files devono essere assoluti: {commits:?}"
    );

    let mut file_layers: HashMap<String, HashSet<String>> = HashMap::new();
    file_layers
        .entry(api_path)
        .or_default()
        .insert("app::api".into());
    file_layers
        .entry(core_path)
        .or_default()
        .insert("app::core".into());

    // Una sola occasione: solo il primo commit ha co-toccato i due layer.
    assert_eq!(
        occasions("app::api", "app::core", &file_layers, &commits),
        1
    );
}
