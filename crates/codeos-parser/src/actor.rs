//! `ParserActor`: legge i file dal disco, li analizza e pubblica i risultati.

use std::path::{Path, PathBuf};

use anyhow::Context;
use codeos_types::bus::{CodeOsEvent, Command};
use codeos_types::ParsedFileResult;
use tokio::sync::{broadcast, mpsc};

use crate::cpp::CppParser;
use crate::go::GoParser;
use crate::java_lang::JavaParser;
use crate::python::PythonParser;
use crate::rust_lang::RustParser;
use crate::traits::LanguageParser;
use crate::typescript::TypeScriptParser;
use crate::workspace::WorkspaceModel;

/// Tetto di dimensione (byte) per file indicizzato. Oltre questa soglia un file è
/// quasi certamente generato a macchina / vendored / minificato (binding FFI,
/// bundle, fixture): parsarlo costa tempo super-lineare e ha fatto STALLARE
/// l'indicizzazione nel collaudo (windows-rs, file da 3,2–4,2 MB → 0 entità dopo
/// oltre 240 s). Lo saltiamo con un avviso esplicito, non blocchiamo l'indicizzazione:
/// un file mancante e NOMINATO nei log è meglio di un'indicizzazione che non finisce.
/// Soglia generosa: il più grande sorgente scritto a mano osservato (checker.ts di
/// tsc, ~2,9 MB) resta sotto. (Il crash da profondità è gestito a parte da `stacker`.)
const MAX_FILE_BYTES: u64 = 3 * 1024 * 1024;

/// Attore che indicizza i file.
///
/// Per ogni comando di indicizzazione: legge il sorgente, sceglie il parser per
/// estensione, produce i [`ParsedFileResult`] e pubblica un
/// [`CodeOsEvent::FilesIndexed`] sul bus. Non conosce il grafo (invariante 1.4)
/// né gli altri attori (invariante 1.3): riceve comandi su un `mpsc` e pubblica
/// eventi su un `broadcast`.
pub struct ParserActor {
    parsers: Vec<Box<dyn LanguageParser>>,
    events: broadcast::Sender<CodeOsEvent>,
}

impl ParserActor {
    /// Crea l'attore col set di parser predefinito.
    pub fn new(events: broadcast::Sender<CodeOsEvent>) -> Self {
        Self {
            parsers: vec![
                Box::new(PythonParser::new()),
                Box::new(RustParser::new()),
                Box::new(TypeScriptParser::new()),
                Box::new(GoParser::new()),
                Box::new(JavaParser::new()),
                Box::new(CppParser::new()),
            ],
            events,
        }
    }

    /// Consuma i comandi finché il canale resta aperto.
    pub async fn run(self, mut commands: mpsc::Receiver<Command>) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::IndexFiles { files, reply_to } => {
                    let results = self.index_files(&files).await;
                    self.publish(results);
                    // DECISION: il Parser non conosce gli `EntityId` (li crea il
                    // GraphResolver nel Blocco 3): rispondiamo con vec vuoto. Le
                    // entità reali arriveranno ai sottoscrittori via `GraphUpdated`.
                    let _ = reply_to.send(Ok(Vec::new())).await;
                }
                Command::ReIndexFile {
                    file_path,
                    reply_to,
                } => {
                    let results = self.index_files(std::slice::from_ref(&file_path)).await;
                    self.publish(results);
                    let _ = reply_to.send(Ok(())).await;
                }
                Command::IndexProject {
                    project_root,
                    reply_to,
                } => {
                    let files = self.collect_source_files(&project_root);
                    tracing::info!(root = %project_root, count = files.len(), "IndexProject");
                    let results = self.index_files(&files).await;
                    self.publish(results);
                    let _ = reply_to.send(Ok(())).await;
                }
                Command::RemoveFiles { files, reply_to } => {
                    // La rimozione effettiva dal grafo spetta al GraphActor
                    // (Blocco 3, "Passo 0 — Pulizia"): qui non c'è nulla da parsare.
                    tracing::warn!(
                        count = files.len(),
                        "RemoveFiles: rimozione dal grafo nel Blocco 3"
                    );
                    let _ = reply_to.send(Ok(())).await;
                }
                other => tracing::warn!(?other, "parser actor: comando non di sua competenza"),
            }
        }
        tracing::debug!("parser actor: canale comandi chiuso, esco");
    }

    async fn index_files(&self, files: &[String]) -> Vec<ParsedFileResult> {
        let mut results = Vec::with_capacity(files.len());
        // Un solo modello di workspace per batch: la cache evita di rileggere lo
        // stesso manifest una volta per file (P1-c).
        let mut workspace = WorkspaceModel::new();
        for file in files {
            match self.index_one(file).await {
                Ok(Some(mut result)) => {
                    stamp_package(&mut result, &mut workspace);
                    results.push(result);
                }
                Ok(None) => tracing::debug!(%file, "nessun parser per l'estensione, salto"),
                Err(err) => tracing::warn!(%file, error = %err, "indicizzazione fallita"),
            }
        }
        results
    }

    async fn index_one(&self, file: &str) -> anyhow::Result<Option<ParsedFileResult>> {
        let path = Path::new(file);
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let Some(parser) = self.parsers.iter().find(|p| p.can_parse(extension)) else {
            return Ok(None);
        };
        // Guardia anti-stallo: salta i file enormi (generati/vendored/minificati)
        // prima di leggerli e parsarli. `Ok(None)` = saltato pulito, non un errore.
        if let Ok(meta) = tokio::fs::metadata(path).await {
            if meta.len() > MAX_FILE_BYTES {
                tracing::warn!(
                    %file,
                    bytes = meta.len(),
                    limit = MAX_FILE_BYTES,
                    "file oltre il limite di dimensione: saltato (probabile generato/vendored)"
                );
                return Ok(None);
            }
        }
        let source = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("lettura di {file} fallita"))?;
        Ok(Some(parser.parse_file(path, &source).await))
    }

    fn publish(&self, results: Vec<ParsedFileResult>) {
        if results.is_empty() {
            return;
        }
        // L'assenza di sottoscrittori non è un errore.
        let _ = self.events.send(CodeOsEvent::FilesIndexed { results });
    }

    /// Cammina ricorsivamente la directory del progetto raccogliendo i file con
    /// un'estensione gestita da un parser.
    fn collect_source_files(&self, root: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if !is_ignored_dir(&path) {
                        stack.push(path);
                    }
                } else if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
                    if self.parsers.iter().any(|p| p.can_parse(extension)) {
                        found.push(path.to_string_lossy().to_string());
                    }
                }
            }
        }
        filter_generated(found, Path::new(root))
    }
}

/// Toglie dalla lista i file marcati `linguist-generated` nel `.gitattributes` della
/// root: il fix PRECISO ai file generati a macchina (binding FFI, bundle), che
/// inquinano il grafo e fanno stallare l'indicizzazione. È deterministico e a zero
/// falsi positivi (a differenza di un'euristica sull'entropia): salta solo ciò che il
/// repo stesso dichiara generato. Complementare al backstop per-dimensione
/// (`MAX_FILE_BYTES`), che resta per i generati NON dichiarati. No-op senza
/// `.gitattributes` o senza pattern generati.
fn filter_generated(files: Vec<String>, root: &Path) -> Vec<String> {
    let patterns = load_generated_patterns(root);
    if patterns.is_empty() {
        return files;
    }
    files
        .into_iter()
        .filter(|f| {
            let rel = Path::new(f)
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| f.clone());
            let generated = patterns.iter().any(|p| gitattr_matches(p, &rel));
            if generated {
                tracing::info!(file = %f, "saltato: marcato linguist-generated in .gitattributes");
            }
            !generated
        })
        .collect()
}

/// Estrae dal `.gitattributes` della root i pattern marcati `linguist-generated`
/// (settato; non `-linguist-generated` né `=false`). Vuoto se il file non c'è.
fn load_generated_patterns(root: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(root.join(".gitattributes")) else {
        return Vec::new();
    };
    let mut patterns = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(pattern) = tokens.next() else {
            continue;
        };
        let generated = tokens.any(|a| a == "linguist-generated" || a == "linguist-generated=true");
        if generated {
            patterns.push(pattern.to_string());
        }
    }
    patterns
}

/// Match SEMPLIFICATO di un pattern stile `.gitattributes`/`.gitignore` contro un
/// path relativo alla root. Copre i casi comuni dei file generati (`*.ext`, `dir/**`,
/// `**/name`, `path/to/file`, basename nudo); pattern più esotici possono non
/// combaciare e quei file ricadono sul backstop per-dimensione — onesto, non
/// completo, e senza dipendenze nuove.
fn gitattr_matches(pattern: &str, rel_path: &str) -> bool {
    let pat = pattern.trim_start_matches('/');
    // `*.ext` / `*suffisso` ⇒ suffisso, ovunque.
    if let Some(suffix) = pat.strip_prefix('*') {
        if !suffix.contains('*') && !suffix.contains('/') {
            return rel_path.ends_with(suffix);
        }
    }
    // `prefix/**` o `prefix/*` ⇒ tutto sotto prefix.
    if let Some(prefix) = pat.strip_suffix("/**").or_else(|| pat.strip_suffix("/*")) {
        return rel_path == prefix || rel_path.starts_with(&format!("{prefix}/"));
    }
    // `**/name` ⇒ basename o sottopath.
    if let Some(tail) = pat.strip_prefix("**/") {
        return rel_path == tail || rel_path.ends_with(&format!("/{tail}"));
    }
    // Senza `/`: combacia il basename ovunque. Altrimenti path esatto.
    if !pat.contains('/') {
        return rel_path.rsplit('/').next() == Some(pat);
    }
    rel_path == pat
}

/// Timbra il pacchetto di appartenenza (`package`) sui metadata di ogni entità del
/// file, leggendolo dal manifest più vicino sul disco (P1-c). Se il file non è
/// posseduto da alcun manifest noto, non aggiunge nulla: il Guardian ricadrà
/// sull'euristica di profondità del path. Non sovrascrive un `package` che il
/// parser avesse già dedotto.
fn stamp_package(result: &mut ParsedFileResult, workspace: &mut WorkspaceModel) {
    let Some(package) = workspace.package_for_file(Path::new(&result.file_path)) else {
        return;
    };
    for entity in &mut result.entities {
        entity
            .metadata
            .entry("package".to_string())
            .or_insert_with(|| package.clone());
    }
}

/// Directory che non contengono mai codice sorgente *di prodotto*: VCS, cache,
/// ambienti virtuali e — soprattutto — output di build/generati. Indicizzarle
/// inquina il grafo con artefatti (es. `vscode-extension/out/extension.js`) che
/// creerebbero layer fantasma e falsi invarianti. Lista conservativa: solo nomi
/// che per convenzione consolidata sono rigenerabili, mai sorgente scritto a mano.
fn is_ignored_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(
            // Version control e config di CodeOS.
            ".git" | ".codeos"
            // Build output / artefatti (Rust, Node, generici).
                | "target"
                | "out"
                | "dist"
                | "build"
                | "coverage"
            // Cache di tool e bundler.
                | ".next"
                | ".turbo"
                | ".cache"
                | ".mypy_cache"
                | ".pytest_cache"
            // Dipendenze installate (mai sorgente del progetto).
                | "node_modules"
                | "vendor"
                | "__pycache__"
                | ".venv"
                | "venv"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{broadcast, mpsc};

    fn temp_py_path() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("codeos_parser_{nanos}.py"))
    }

    #[tokio::test]
    async fn index_files_reads_disk_and_publishes_parsed_results() {
        let path = temp_py_path();
        tokio::fs::write(&path, "class Foo:\n    def bar(self):\n        pass\n")
            .await
            .unwrap();

        let (events_tx, mut events_rx) = broadcast::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let actor = ParserActor::new(events_tx);
        let handle = tokio::spawn(actor.run(cmd_rx));

        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        cmd_tx
            .send(Command::IndexFiles {
                files: vec![path.to_string_lossy().to_string()],
                reply_to: reply_tx,
            })
            .await
            .unwrap();

        let reply = reply_rx.recv().await.expect("nessuna risposta");
        assert!(reply.is_ok());

        let event = events_rx.recv().await.expect("nessun evento");
        let CodeOsEvent::FilesIndexed { results } = event else {
            panic!("evento inatteso");
        };
        assert_eq!(results.len(), 1);
        let names: Vec<&str> = results[0]
            .entities
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(names.contains(&"Foo"), "names = {names:?}");
        assert!(names.contains(&"bar"), "names = {names:?}");

        drop(cmd_tx); // chiude il loop dell'attore
        let _ = handle.await;
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn oversized_file_is_skipped_not_parsed() {
        // Regressione dello STALLO (collaudo windows-rs): un file enorme
        // (generato/vendored) viene SALTATO prima di parsarlo. Controprova: un file
        // piccolo viene parsato normalmente — la guardia è selettiva, non globale.
        let actor = ParserActor::new(broadcast::channel(16).0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let big = std::env::temp_dir().join(format!("codeos_big_{stamp}.py"));
        let small = std::env::temp_dir().join(format!("codeos_small_{stamp}.py"));

        let huge = format!("# {}\nx = 1\n", "a".repeat(MAX_FILE_BYTES as usize + 16));
        tokio::fs::write(&big, &huge).await.unwrap();
        let skipped = actor.index_one(&big.to_string_lossy()).await.unwrap();
        assert!(
            skipped.is_none(),
            "un file oltre {MAX_FILE_BYTES} byte va saltato, non parsato"
        );

        tokio::fs::write(&small, "def f():\n    pass\n")
            .await
            .unwrap();
        let parsed = actor.index_one(&small.to_string_lossy()).await.unwrap();
        assert!(parsed.is_some(), "un file piccolo dev'essere parsato");

        let _ = tokio::fs::remove_file(&big).await;
        let _ = tokio::fs::remove_file(&small).await;
    }

    #[test]
    fn gitattr_matches_common_generated_patterns() {
        assert!(gitattr_matches("*.gen.rs", "src/api.gen.rs"));
        assert!(gitattr_matches("generated/**", "generated/x/y.rs"));
        assert!(gitattr_matches("generated/**", "generated"));
        assert!(gitattr_matches("**/mod.rs", "a/b/mod.rs"));
        assert!(gitattr_matches("crates/foo/bar.rs", "crates/foo/bar.rs"));
        assert!(gitattr_matches("bindings.rs", "deep/bindings.rs")); // basename nudo
                                                                     // Non devono combaciare:
        assert!(!gitattr_matches("*.gen.rs", "src/api.rs"));
        assert!(!gitattr_matches("generated/**", "src/x.rs"));
        assert!(!gitattr_matches("**/mod.rs", "a/lib.rs"));
    }

    #[tokio::test]
    async fn gitattributes_generated_files_are_skipped() {
        // Il fix preciso: i file dichiarati `linguist-generated` nel `.gitattributes`
        // vengono saltati a priori; i sorgenti normali restano.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codeos_gitattr_{stamp}"));
        let sub = root.join("generated");
        tokio::fs::create_dir_all(&sub).await.unwrap();
        tokio::fs::write(
            root.join(".gitattributes"),
            "# generati\ngenerated/** linguist-generated\n*.gen.rs linguist-generated=true\n",
        )
        .await
        .unwrap();
        tokio::fs::write(sub.join("bindings.rs"), "pub fn x() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("api.gen.rs"), "pub fn y() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("main.rs"), "pub fn z() {}\n")
            .await
            .unwrap();

        let actor = ParserActor::new(broadcast::channel(16).0);
        let files = actor.collect_source_files(&root.to_string_lossy());
        let names: Vec<String> = files
            .iter()
            .filter_map(|f| f.rsplit('/').next().map(String::from))
            .collect();

        assert!(
            names.iter().any(|n| n == "main.rs"),
            "il sorgente normale resta: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "bindings.rs"),
            "generated/** saltato: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "api.gen.rs"),
            "*.gen.rs saltato: {names:?}"
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn index_project_collects_rust_and_typescript_sources() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codeos_parser_project_{nanos}"));
        let src = root.join("src");
        let ui = root.join("ui");
        tokio::fs::create_dir_all(&src).await.unwrap();
        tokio::fs::create_dir_all(&ui).await.unwrap();
        tokio::fs::write(src.join("lib.rs"), "pub fn boot() {}\n")
            .await
            .unwrap();
        tokio::fs::write(ui.join("panel.ts"), "export function render() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("README.md"), "# ignored\n")
            .await
            .unwrap();

        let actor = ParserActor::new(broadcast::channel(16).0);
        let files = actor.collect_source_files(&root.to_string_lossy());
        let endings: Vec<&str> = files.iter().filter_map(|f| f.rsplit('/').next()).collect();
        assert!(endings.contains(&"lib.rs"), "files = {files:?}");
        assert!(endings.contains(&"panel.ts"), "files = {files:?}");
        assert!(!endings.contains(&"README.md"), "files = {files:?}");

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn index_project_skips_build_output_and_dependencies() {
        // Regressione P0-3: un `.ts`/`.js` dentro out/, dist/ o node_modules NON
        // deve finire nel grafo (sono artefatti rigenerabili, non sorgente).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codeos_parser_ignored_{nanos}"));
        let src = root.join("src");
        let out = root.join("out");
        let dist = root.join("dist");
        let deps = root.join("node_modules").join("left-pad");
        for dir in [&src, &out, &dist, &deps] {
            tokio::fs::create_dir_all(dir).await.unwrap();
        }
        tokio::fs::write(src.join("real.ts"), "export const x = 1;\n")
            .await
            .unwrap();
        tokio::fs::write(out.join("real.js"), "exports.x = 1;\n")
            .await
            .unwrap();
        tokio::fs::write(dist.join("bundle.js"), "module.exports={};\n")
            .await
            .unwrap();
        tokio::fs::write(deps.join("index.js"), "module.exports=1;\n")
            .await
            .unwrap();

        let actor = ParserActor::new(broadcast::channel(16).0);
        let files = actor.collect_source_files(&root.to_string_lossy());
        let endings: Vec<&str> = files.iter().filter_map(|f| f.rsplit('/').next()).collect();

        assert!(
            endings.contains(&"real.ts"),
            "il sorgente vero manca: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.contains("/out/")
                || f.contains("/dist/")
                || f.contains("/node_modules/")),
            "build output o dipendenze non devono essere indicizzati: {files:?}"
        );

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn index_stamps_package_from_the_nearest_cargo_manifest() {
        // Regressione P1-c: le entità di un file dentro un crate devono ereditare il
        // nome del pacchetto letto dal Cargo.toml più vicino, non l'euristica path.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codeos_parser_pkg_{nanos}"));
        let crate_src = root.join("crates").join("alpha").join("src");
        tokio::fs::create_dir_all(&crate_src).await.unwrap();
        tokio::fs::write(
            root.join("crates").join("alpha").join("Cargo.toml"),
            "[package]\nname = \"alpha-core\"\n",
        )
        .await
        .unwrap();
        let file = crate_src.join("lib.rs");
        tokio::fs::write(&file, "pub fn boot() {}\n").await.unwrap();

        let actor = ParserActor::new(broadcast::channel(16).0);
        let results = actor
            .index_files(&[file.to_string_lossy().to_string()])
            .await;

        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .entities
                .iter()
                .all(|e| e.metadata.get("package").map(String::as_str) == Some("alpha-core")),
            "ogni entità deve portare package=alpha-core: {:?}",
            results[0].entities
        );

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn index_without_manifest_leaves_package_unset() {
        // Senza manifest sopra il file, nessun `package`: il Guardian ricadrà
        // sull'euristica di profondità del path.
        let path = temp_py_path();
        tokio::fs::write(&path, "class Foo:\n    pass\n")
            .await
            .unwrap();

        let actor = ParserActor::new(broadcast::channel(16).0);
        let results = actor
            .index_files(&[path.to_string_lossy().to_string()])
            .await;

        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .entities
                .iter()
                .all(|e| !e.metadata.contains_key("package")),
            "senza manifest non deve esserci alcun package"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn unknown_extension_yields_no_event() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeos_parser_{nanos}.txt"));
        tokio::fs::write(&path, "non è python").await.unwrap();

        let (events_tx, mut events_rx) = broadcast::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let handle = tokio::spawn(ParserActor::new(events_tx).run(cmd_rx));

        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        cmd_tx
            .send(Command::IndexFiles {
                files: vec![path.to_string_lossy().to_string()],
                reply_to: reply_tx,
            })
            .await
            .unwrap();
        reply_rx.recv().await.expect("nessuna risposta").unwrap();

        // Nessun parser per .txt => nessun evento. Usiamo try_recv per non bloccare.
        assert!(events_rx.try_recv().is_err());

        drop(cmd_tx);
        let _ = handle.await;
        let _ = tokio::fs::remove_file(&path).await;
    }
}
