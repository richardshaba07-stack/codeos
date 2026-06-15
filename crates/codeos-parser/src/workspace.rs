//! `WorkspaceModel`: mappa un file al **pacchetto** che lo possiede leggendo i
//! manifest sul disco (`Cargo.toml`, `package.json`, `go.mod`).
//!
//! Perché serve (P1-c): il Guardian deriva i "layer" dal `qualified_name`, che a
//! sua volta nasce dal path. L'euristica di profondità fissa
//! ([`crate`-depth][crate::workspace]) funziona quando i pacchetti vivono tutti
//! allo stesso livello (`crates/<nome>/src/…`), ma in un monorepo con annidamento
//! **non uniforme** (`packages/gruppo/sub/pkg/…`) accorpa pacchetti distinti nello
//! stesso layer, generando confini finti. Ancorando il layer al **confine reale
//! del pacchetto** — la directory che contiene il manifest — i layer diventano
//! onesti a prescindere dalla profondità.
//!
//! Filosofia conservativa di CodeOS: se nessun manifest possiede il file (linguaggi
//! senza manifest, o un `Cargo.toml` di solo workspace senza `[package]`),
//! ritorniamo `None` e il Guardian ricade sull'euristica. Non inventiamo confini
//! che non sappiamo leggere.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cache `directory → pacchetto` per non rileggere lo stesso manifest una volta per
/// file. Vive quanto un singolo `IndexProject`/`IndexFiles`: i manifest cambiano di
/// rado e una ri-indicizzazione completa riparte pulita.
#[derive(Default)]
pub struct WorkspaceModel {
    /// Risultato (anche negativo) di [`package_at_dir`] per directory già visitata.
    by_dir: HashMap<PathBuf, Option<String>>,
}

impl WorkspaceModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Il pacchetto che possiede `file`, cercato risalendo gli antenati fino al
    /// primo manifest riconosciuto. `None` se nessun antenato è un pacchetto.
    ///
    /// `skip(1)`: il primo `ancestor` è il path del file stesso; partiamo dalla sua
    /// directory.
    pub fn package_for_file(&mut self, file: &Path) -> Option<String> {
        for dir in file.ancestors().skip(1) {
            if let Some(found) = self.cached_package_at_dir(dir) {
                return Some(found);
            }
        }
        None
    }

    fn cached_package_at_dir(&mut self, dir: &Path) -> Option<String> {
        if let Some(cached) = self.by_dir.get(dir) {
            return cached.clone();
        }
        let computed = package_at_dir(dir);
        self.by_dir.insert(dir.to_path_buf(), computed.clone());
        computed
    }
}

/// Il pacchetto dichiarato da un manifest **direttamente** in `dir`, se c'è.
/// Prova i formati in ordine; il primo manifest con un nome vince.
fn package_at_dir(dir: &Path) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) {
        if let Some(name) = toml_name_in_section(&text, "package") {
            return Some(name);
        }
        // `Cargo.toml` senza `[package]` = root di un workspace virtuale: non è un
        // pacchetto. I crate stanno SOTTO, mai sopra: continuiamo a salire (in
        // pratica non troveremo altro e tornerà `None`).
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
        if let Some(name) = json_top_level_name(&text) {
            return Some(name);
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("go.mod")) {
        if let Some(module) = go_mod_module(&text) {
            return Some(module);
        }
    }
    None
}

/// Estrae `name = "…"` dalla sezione `[section]` di un TOML, con un mini-parser a
/// righe (niente dipendenza TOML: ci serve un solo campo). Riconosce sia gli apici
/// doppi che singoli; ignora commenti e altre sezioni. Match **esatto** sul nome
/// della sezione: `[package.metadata]` non è `[package]`.
fn toml_name_in_section(text: &str, section: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            let header = header.trim_end_matches(']').trim();
            in_section = header == section;
            continue;
        }
        if in_section {
            if let Some(rest) = line.strip_prefix("name") {
                if let Some(value) = rest.trim_start().strip_prefix('=') {
                    let v = value.trim().trim_matches(|c| c == '"' || c == '\'');
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Estrae il campo top-level `"name"` da un `package.json`. Usa `serde_json` perché
/// il JSON scritto a mano dagli utenti (oggetti annidati, escape, unicode) non si
/// parsa a vista in modo affidabile, e un `name` dentro `dependencies` non deve mai
/// essere scambiato per quello del pacchetto.
fn json_top_level_name(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    value.get("name")?.as_str().map(str::to_string)
}

/// Estrae il path del modulo dalla direttiva `module …` di un `go.mod`. Il modulo
/// (es. `github.com/acme/svc`) è l'identità del pacchetto Go. Richiede uno spazio
/// dopo `module` per non confondersi con un identificatore che inizia per "module".
fn go_mod_module(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("module") {
            if rest.starts_with(char::is_whitespace) {
                let module = rest.trim();
                if !module.is_empty() {
                    return Some(module.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_package_name() {
        let toml = "[package]\nname = \"codeos-core\"\nversion = \"0.1.0\"\n\n[dependencies]\nname = \"not-this\"\n";
        assert_eq!(
            toml_name_in_section(toml, "package").as_deref(),
            Some("codeos-core")
        );
    }

    #[test]
    fn cargo_workspace_root_has_no_package_name() {
        // Un Cargo.toml di solo workspace non possiede file: niente [package].
        let toml = "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n";
        assert_eq!(toml_name_in_section(toml, "package"), None);
    }

    #[test]
    fn package_metadata_subsection_is_not_package() {
        // `[package.metadata]` non deve essere scambiata per `[package]`.
        let toml = "[package.metadata.docs]\nname = \"sneaky\"\n";
        assert_eq!(toml_name_in_section(toml, "package"), None);
    }

    #[test]
    fn parses_json_top_level_name_only() {
        let json = r#"{ "name": "@acme/ui", "dependencies": { "name": "decoy" } }"#;
        assert_eq!(json_top_level_name(json).as_deref(), Some("@acme/ui"));
    }

    #[test]
    fn invalid_json_yields_none() {
        assert_eq!(json_top_level_name("{ not json"), None);
    }

    #[test]
    fn parses_go_module_path() {
        let go_mod = "module github.com/acme/svc\n\ngo 1.21\n";
        assert_eq!(
            go_mod_module(go_mod).as_deref(),
            Some("github.com/acme/svc")
        );
        // Non deve agganciarsi a un identificatore che inizia per "module".
        assert_eq!(go_mod_module("moduleX = 1\n"), None);
    }

    #[test]
    fn package_for_file_walks_up_to_the_nearest_manifest() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codeos_ws_{nanos}"));
        let crate_dir = root.join("crates").join("alpha");
        let deep = crate_dir.join("src").join("inner");
        std::fs::create_dir_all(&deep).unwrap();
        // Root di workspace (senza [package]) + crate con [package].
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=[\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"alpha\"\n",
        )
        .unwrap();

        let mut model = WorkspaceModel::new();
        // Un file profondo dentro il crate risale fino a `crates/alpha` → "alpha".
        let file = deep.join("mod.rs");
        assert_eq!(model.package_for_file(&file).as_deref(), Some("alpha"));
        // La cache restituisce lo stesso risultato (secondo accesso).
        assert_eq!(model.package_for_file(&file).as_deref(), Some("alpha"));

        // Un file senza alcun manifest sopra di sé (fuori da crates/) → None.
        let orphan = root.join("scripts").join("tool.rs");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, "fn main() {}\n").unwrap();
        assert_eq!(model.package_for_file(&orphan), None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
