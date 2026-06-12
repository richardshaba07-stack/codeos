//! Scanner delle LICENZE delle dipendenze (v1, onesto).
//!
//! Il bisogno (dell'utente, e vero «al 1000%» per un agente): un agente che
//! aggiunge dipendenze non ha oggi alcun gate di licenza — una dipendenza
//! GPL/AGPL infilata in un codebase proprietario è un rischio legale reale.
//!
//! Il differenziale di CodeOS rispetto a uno scanner di manifest: il grafo ha
//! già i nodi `ExternalDependency` e gli ARCHI — non solo «dipendi da X», ma
//! *chi* usa X. E la POLICY di licenza è intento architetturale: una decisione
//! del ledger (`codeos decide --tags "license-deny:GPL-3.0"`) diventa il gate.
//!
//! Onestà di scope (v1):
//! - la licenza viene SOLO da metadati locali del pacchetto (Cargo.toml del
//!   registry cache per Rust; package.json dentro node_modules per JS/TS);
//! - niente rete, niente classificazione del testo dei file LICENSE (la parte
//!   dura di Black Duck), niente vendored-scanning: dichiarati FUORI scope;
//! - licenza non trovata ⇒ `None` («sconosciuta»), mai indovinata;
//! - si riportano FATTI (nome, versione, licenza dichiarata, chi la vieta),
//!   MAI conclusioni legali («incompatibile» è terreno per avvocati).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// La licenza dichiarata di UNA dipendenza diretta del progetto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyLicense {
    /// Nome del pacchetto come appare nel manifest.
    pub name: String,
    /// Ecosistema: "cargo" | "npm" | "pip" | "go".
    pub ecosystem: String,
    /// Espressione di licenza DICHIARATA dal pacchetto (es. "MIT OR Apache-2.0").
    /// `None` = sconosciuta: il metadato locale non c'è — astensione, non "MIT
    /// per sentito dire".
    pub license: Option<String>,
    /// Da dove viene il dato (per la verificabilità): path del metadato letto,
    /// o il manifest del progetto se la licenza è sconosciuta.
    pub source: String,
}

/// Una violazione di POLICY: una dipendenza la cui licenza dichiarata contiene
/// un identificatore vietato da una decisione del ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseViolation {
    pub dependency: String,
    pub license: String,
    /// L'identificatore vietato che combacia (es. "GPL-3.0").
    pub denied: String,
    /// Il titolo della decisione del ledger che lo vieta (il PERCHÉ è lì).
    pub decision_title: String,
}

/// Scansiona i manifest del progetto e risolve le licenze dai metadati LOCALI.
/// Pura rispetto alla rete; legge solo filesystem. L'ordine è deterministico
/// (per ecosistema, poi nome).
pub fn scan_licenses(repo_root: &Path) -> Vec<DependencyLicense> {
    let mut out: BTreeMap<(String, String), DependencyLicense> = BTreeMap::new();

    // --- Rust: Cargo.toml (root + workspace members di primo livello) ---
    // I MEMBRI del workspace (crates/<nome>) non sono terze parti: esistono come
    // directory locali e si saltano — mostrarli «sconosciuti» sarebbe rumore.
    let is_workspace_member = |name: &str| {
        repo_root
            .join("crates")
            .join(name)
            .join("Cargo.toml")
            .exists()
            || repo_root.join(name).join("Cargo.toml").exists()
    };
    for manifest in find_cargo_manifests(repo_root) {
        if let Ok(text) = std::fs::read_to_string(&manifest) {
            for dep in cargo_dependency_names(&text) {
                if is_workspace_member(&dep) {
                    continue;
                }
                let license = cargo_registry_license(&dep);
                let source = license
                    .as_ref()
                    .map(|(_, p)| p.clone())
                    .unwrap_or_else(|| manifest.to_string_lossy().to_string());
                out.entry(("cargo".to_string(), dep.clone()))
                    .or_insert(DependencyLicense {
                        name: dep,
                        ecosystem: "cargo".to_string(),
                        license: license.map(|(l, _)| l),
                        source,
                    });
            }
        }
    }

    // --- JS/TS: package.json + node_modules/<pkg>/package.json ---
    let pkg_json = repo_root.join("package.json");
    if let Ok(text) = std::fs::read_to_string(&pkg_json) {
        for dep in npm_dependency_names(&text) {
            let meta = repo_root
                .join("node_modules")
                .join(&dep)
                .join("package.json");
            let license = std::fs::read_to_string(&meta)
                .ok()
                .and_then(|t| json_string_field(&t, "license"));
            let source = if license.is_some() {
                meta.to_string_lossy().to_string()
            } else {
                pkg_json.to_string_lossy().to_string()
            };
            out.entry(("npm".to_string(), dep.clone()))
                .or_insert(DependencyLicense {
                    name: dep,
                    ecosystem: "npm".to_string(),
                    license,
                    source,
                });
        }
    }

    // --- Python: requirements.txt — solo i NOMI; la licenza resta sconosciuta
    //     in v1 (i pacchetti potrebbero non essere installati). Astensione. ---
    let req = repo_root.join("requirements.txt");
    if let Ok(text) = std::fs::read_to_string(&req) {
        for dep in pip_dependency_names(&text) {
            out.entry(("pip".to_string(), dep.clone()))
                .or_insert(DependencyLicense {
                    name: dep,
                    ecosystem: "pip".to_string(),
                    license: None,
                    source: req.to_string_lossy().to_string(),
                });
        }
    }

    out.into_values().collect()
}

/// Confronta le licenze con la POLICY del ledger: gli identificatori vietati
/// arrivano dai tag `license-deny:<ID>` delle decisioni correnti. Match
/// CONTENITIVO e case-insensitive sull'espressione dichiarata («MIT OR
/// GPL-3.0» contiene «GPL-3.0» ⇒ segnalato: è un FATTO che l'espressione lo
/// contenga; la valutazione dell'OR spetta a un umano).
pub fn check_policy(
    licenses: &[DependencyLicense],
    denied: &[(String, String)], // (id vietato, titolo della decisione)
) -> Vec<LicenseViolation> {
    let mut out = Vec::new();
    for dep in licenses {
        let Some(expr) = &dep.license else { continue };
        let expr_lower = expr.to_lowercase();
        for (id, title) in denied {
            if !id.is_empty() && expr_lower.contains(&id.to_lowercase()) {
                out.push(LicenseViolation {
                    dependency: format!("{} ({})", dep.name, dep.ecosystem),
                    license: expr.clone(),
                    denied: id.clone(),
                    decision_title: title.clone(),
                });
            }
        }
    }
    out
}

/// Estrae gli identificatori vietati dai tag `license-deny:<ID>` di una lista
/// di tag di decisione.
pub fn denied_ids_from_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .filter_map(|t| t.strip_prefix("license-deny:"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Parsing dei manifest — minimale e onesto (niente parser TOML/JSON completi:
// per nomi di dipendenze e un campo stringa bastano scansioni di riga robuste;
// un manifest esotico non riconosciuto produce MENO voci, mai voci inventate).
// ---------------------------------------------------------------------------

fn find_cargo_manifests(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let top = root.join("Cargo.toml");
    if top.exists() {
        out.push(top);
    }
    // Workspace members di primo e secondo livello (crates/*/Cargo.toml).
    for sub in ["crates", "."] {
        if let Ok(entries) = std::fs::read_dir(root.join(sub)) {
            for e in entries.flatten() {
                let m = e.path().join("Cargo.toml");
                if m.exists() && !out.contains(&m) {
                    out.push(m);
                }
            }
        }
    }
    out
}

/// Nomi delle dipendenze dalle sezioni `[dependencies]`/`[dev-dependencies]`/
/// `[build-dependencies]` di un Cargo.toml. Ignora `path = `-only (membri del
/// workspace, non pacchetti terzi) e le righe `workspace = true` SENZA registry…
/// no: `{ workspace = true }` eredita dal root — il root le elenca comunque,
/// quindi qui le TENIAMO (il dedup è a monte).
fn cargo_dependency_names(toml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_deps = false;
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = matches!(
                line,
                "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]"
            ) || line.starts_with("[dependencies.")
                || line.starts_with("[dev-dependencies.")
                || line.starts_with("[build-dependencies.");
            // Forma `[dependencies.foo]`: il nome è nell'header stesso.
            if let Some(rest) = line
                .strip_prefix("[dependencies.")
                .or_else(|| line.strip_prefix("[dev-dependencies."))
                .or_else(|| line.strip_prefix("[build-dependencies."))
            {
                let name = rest.trim_end_matches(']').trim().to_string();
                if !name.is_empty() {
                    out.push(name);
                }
            }
            continue;
        }
        if !in_deps || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, rhs)) = line.split_once('=') {
            let name = name.trim().trim_matches('"').to_string();
            // `foo = { path = "../foo" }` senza version: membro locale, non terzo.
            let local_only = rhs.contains("path") && !rhs.contains("version");
            if !name.is_empty() && !local_only {
                out.push(name);
            }
        }
    }
    out
}

/// Cerca la licenza di un crate nella cache locale del registry
/// (`~/.cargo/registry/src/<index>/<name>-<ver>/Cargo.toml`, campo `license`).
/// `None` se il crate non è in cache o non dichiara `license` — astensione.
fn cargo_registry_license(name: &str) -> Option<(String, String)> {
    let home = std::env::var("HOME").ok()?;
    let src = PathBuf::from(home).join(".cargo/registry/src");
    for index in std::fs::read_dir(&src).ok()?.flatten() {
        let dir = index.path();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Prendi la versione "maggiore" in cache (ordinamento lessicografico:
        // approssimazione onesta, dichiarata — la licenza cambia raramente).
        let mut best: Option<PathBuf> = None;
        let prefix = format!("{name}-");
        for e in entries.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            if let Some(ver) = fname.strip_prefix(&prefix) {
                // Il resto dev'essere una versione (inizia con cifra): evita che
                // `serde-` matchi `serde-json-…`… i nomi crate non hanno trattini
                // ambigui qui perché il prefix include il trattino finale.
                if ver.chars().next().is_some_and(|c| c.is_ascii_digit())
                    && best.as_ref().is_none_or(|b| {
                        e.path().file_name().unwrap_or_default() > b.file_name().unwrap_or_default()
                    })
                {
                    best = Some(e.path());
                }
            }
        }
        if let Some(dir) = best {
            let manifest = dir.join("Cargo.toml");
            if let Ok(text) = std::fs::read_to_string(&manifest) {
                for line in text.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("license") {
                        let rest = rest.trim_start();
                        if let Some(v) = rest.strip_prefix('=') {
                            let v = v.trim().trim_matches('"').to_string();
                            if !v.is_empty() {
                                return Some((v, manifest.to_string_lossy().to_string()));
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Nomi dei pacchetti da `dependencies`/`devDependencies` di un package.json.
fn npm_dependency_names(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["\"dependencies\"", "\"devDependencies\""] {
        if let Some(start) = json.find(key) {
            if let Some(open) = json[start..].find('{') {
                let body = &json[start + open + 1..];
                if let Some(close) = body.find('}') {
                    // Le coppie si separano per VIRGOLA, non per riga: i
                    // package.json compatti tengono più dipendenze sulla stessa.
                    for entry in body[..close].split(',') {
                        if let Some((name, _)) = entry.split_once(':') {
                            let name = name.trim().trim_matches('"').to_string();
                            if !name.is_empty() {
                                out.push(name);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Campo stringa top-level di un JSON (es. `"license": "MIT"`). Minimale:
/// basta per i package.json reali; forme esotiche (oggetto `{type,url}`
/// legacy) producono `None` — astensione, non parsing creativo.
fn json_string_field(json: &str, field: &str) -> Option<String> {
    let key = format!("\"{field}\"");
    let start = json.find(&key)?;
    let rest = &json[start + key.len()..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let after = after.strip_prefix('"')?;
    let end = after.find('"')?;
    let v = &after[..end];
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Nomi dei pacchetti da un requirements.txt (righe `nome==ver`, `nome>=…`,
/// `nome`). Commenti, opzioni (`-r`, `--hash`) e URL sono saltati.
fn pip_dependency_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        let name: String = line
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_names_cover_inline_and_section_forms() {
        let toml = r#"
[package]
name = "x"

[dependencies]
serde = "1"
tokio = { version = "1", features = ["full"] }
local-helper = { path = "../helper" }

[dependencies.uuid]
version = "1"

[dev-dependencies]
tempfile = "3"
"#;
        let names = cargo_dependency_names(toml);
        assert!(names.contains(&"serde".to_string()));
        assert!(names.contains(&"tokio".to_string()));
        assert!(names.contains(&"uuid".to_string()));
        assert!(names.contains(&"tempfile".to_string()));
        // path-only = membro locale, non un pacchetto terzo.
        assert!(!names.contains(&"local-helper".to_string()));
    }

    #[test]
    fn npm_names_and_license_field() {
        let json = r#"{
  "name": "app",
  "dependencies": { "react": "^18.0.0", "left-pad": "1.0.0" },
  "devDependencies": { "vitest": "^1.0.0" }
}"#;
        let names = npm_dependency_names(json);
        assert_eq!(names, vec!["react", "left-pad", "vitest"]);
        assert_eq!(
            json_string_field(r#"{"name":"x","license":"MIT"}"#, "license"),
            Some("MIT".to_string())
        );
        // Forma legacy a oggetto: astensione, non parsing creativo.
        assert_eq!(
            json_string_field(r#"{"license":{"type":"ISC"}}"#, "license"),
            None
        );
    }

    #[test]
    fn pip_names_skip_comments_and_options() {
        let txt = "# deps\nrequests==2.31\nnumpy>=1.20\n-r other.txt\ntorch\n";
        assert_eq!(
            pip_dependency_names(txt),
            vec!["requests", "numpy", "torch"]
        );
    }

    #[test]
    fn policy_match_is_containment_and_cites_the_decision() {
        let licenses = vec![
            DependencyLicense {
                name: "good".into(),
                ecosystem: "npm".into(),
                license: Some("MIT".into()),
                source: "node_modules/good/package.json".into(),
            },
            DependencyLicense {
                name: "viral".into(),
                ecosystem: "npm".into(),
                license: Some("GPL-3.0-only".into()),
                source: "node_modules/viral/package.json".into(),
            },
            DependencyLicense {
                name: "dual".into(),
                ecosystem: "cargo".into(),
                license: Some("MIT OR GPL-3.0".into()),
                source: "x".into(),
            },
            DependencyLicense {
                name: "unknown".into(),
                ecosystem: "pip".into(),
                license: None, // sconosciuta: MAI una violazione inventata
                source: "requirements.txt".into(),
            },
        ];
        let denied = vec![("GPL-3.0".to_string(), "niente GPL nel prodotto".to_string())];
        let v = check_policy(&licenses, &denied);
        let names: Vec<&str> = v.iter().map(|x| x.dependency.as_str()).collect();
        assert_eq!(names, vec!["viral (npm)", "dual (cargo)"]);
        assert!(v
            .iter()
            .all(|x| x.decision_title == "niente GPL nel prodotto"));
    }

    #[test]
    fn denied_ids_come_only_from_the_machine_tag() {
        let tags = vec![
            "licenses".to_string(),
            "license-deny:GPL-3.0".to_string(),
            "license-deny: AGPL-3.0 ".to_string(),
            "license-deny:".to_string(),
        ];
        assert_eq!(denied_ids_from_tags(&tags), vec!["GPL-3.0", "AGPL-3.0"]);
    }
}
