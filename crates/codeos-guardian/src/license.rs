//! Scanner delle LICENZE: dipendenze (v1) + sorgenti (v2). Onesto.
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
//! Onestà di scope (v1, dipendenze):
//! - la licenza viene SOLO da metadati locali del pacchetto (Cargo.toml del
//!   registry cache per Rust; package.json dentro node_modules per JS/TS);
//! - licenza non trovata ⇒ `None` («sconosciuta»), mai indovinata.
//!
//! Onestà di scope (v2, sorgenti — [`scan_source_notices`]):
//! - tag SPDX e intestazioni di copyright riportati VERBATIM con path:riga
//!   (la prima intestazione per file: censimento per-file, dichiarato);
//! - i file LICENSE/COPYING si classificano SOLO per frase distintiva esatta
//!   (famiglie note); testo non riconosciuto ⇒ NON CLASSIFICATA (astensione)
//!   — niente motore di matching legale alla Black Duck, dichiarato;
//! - un copyright NON è una licenza: mai una violazione di policy da solo.
//!
//! Per entrambe: niente rete; si riportano FATTI (nome, licenza dichiarata,
//! dove, chi la vieta), MAI conclusioni legali («incompatibile» è terreno
//! per avvocati).

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
// v2 — scansione NEI SORGENTI: SPDX, intestazioni di copyright, file LICENSE
// vendored. Stessa disciplina della v1: FATTI verbatim (path:riga + testo),
// classificazione SOLO per frase distintiva esatta, astensione altrimenti.
// ---------------------------------------------------------------------------

/// Un avviso di licenza/copyright trovato NEI SORGENTI del progetto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceNotice {
    /// Path RELATIVO alla root del repo.
    pub path: String,
    /// Riga 1-based dell'avviso; 0 = avviso a livello di FILE (es. LICENSE).
    pub line: u32,
    /// "spdx" | "copyright" | "license-file".
    pub kind: String,
    /// Il contenuto: l'espressione SPDX verbatim, la riga di copyright
    /// (trim, troncata onestamente), o la famiglia di licenza classificata di
    /// un file LICENSE. VUOTA per un license-file = NON CLASSIFICATA
    /// (astensione: il file esiste — un fatto — ma il testo non combacia con
    /// nessuna frase distintiva nota; mai indovinato).
    pub text: String,
}

/// Esito della scansione dei sorgenti: avvisi (ordinati per path, riga) e il
/// conteggio di quelli TAGLIATI dal tetto — troncamento dichiarato, mai muto.
#[derive(Debug, Clone, Default)]
pub struct SourceScan {
    pub notices: Vec<SourceNotice>,
    pub truncated: u32,
}

/// Tetto sul numero di avvisi riportati (un repo dove OGNI file ha
/// un'intestazione produrrebbe migliaia di voci; il residuo si DICHIARA).
const MAX_SOURCE_NOTICES: usize = 2000;
/// Oltre questa dimensione il file si salta (stessa soglia della guardia
/// per-file del parser: i multi-MiB sono generati/blob, non intestazioni).
const MAX_SOURCE_FILE_BYTES: u64 = 3 * 1024 * 1024;
/// Le righe di copyright si riportano troncate a questa lunghezza (in CHAR,
/// mai a metà di un multibyte).
const MAX_NOTICE_TEXT_CHARS: usize = 160;

/// Scansiona i SORGENTI del repo alla ricerca di:
/// - tag `SPDX-License-Identifier:` (l'espressione, verbatim);
/// - intestazioni di copyright (la PRIMA riga per file: censimento per-file,
///   non per-occorrenza — dichiarato);
/// - file LICENSE/LICENCE/COPYING/NOTICE (vendored o di progetto), con la
///   famiglia di licenza riconosciuta SOLO per frase distintiva esatta.
///
/// Salta `.git`, `target`, `node_modules` (terze parti già censite dai
/// metadati), `.codeos`, i file binari (NUL nei primi 8 KiB) e quelli oltre
/// i 3 MiB. Solo filesystem locale, niente rete. Ordine deterministico.
pub fn scan_source_notices(repo_root: &Path) -> SourceScan {
    let mut notices: Vec<SourceNotice> = Vec::new();
    let mut stack = vec![repo_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            let name = child
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if child.is_dir() {
                if matches!(
                    name.as_str(),
                    ".git" | "target" | "node_modules" | ".codeos"
                ) {
                    continue;
                }
                stack.push(child);
                continue;
            }
            if std::fs::metadata(&child)
                .map(|m| m.len() > MAX_SOURCE_FILE_BYTES)
                .unwrap_or(true)
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(&child) else {
                continue;
            };
            // Binario = NUL nei primi 8 KiB: non è testo, niente intestazioni.
            if bytes.iter().take(8192).any(|b| *b == 0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let rel = child
                .strip_prefix(repo_root)
                .unwrap_or(&child)
                .to_string_lossy()
                .to_string();
            if is_license_file_name(&name) {
                notices.push(SourceNotice {
                    path: rel,
                    line: 0,
                    kind: "license-file".to_string(),
                    text: classify_license_text(&text).unwrap_or_default(),
                });
                continue;
            }
            collect_header_notices(&rel, &text, &mut notices);
        }
    }
    notices.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    let truncated = notices.len().saturating_sub(MAX_SOURCE_NOTICES) as u32;
    notices.truncate(MAX_SOURCE_NOTICES);
    SourceScan { notices, truncated }
}

/// Confronta gli avvisi nei sorgenti con la policy del ledger. Si controllano
/// SOLO gli avvisi che dichiarano una LICENZA (tag SPDX e file LICENSE
/// classificati): un'intestazione di copyright NON è una licenza — resta un
/// fatto informativo, mai una violazione. Stesso match contenitivo
/// case-insensitive di [`check_policy`].
pub fn check_source_policy(
    notices: &[SourceNotice],
    denied: &[(String, String)],
) -> Vec<LicenseViolation> {
    let mut out = Vec::new();
    for n in notices {
        let is_license_claim = n.kind == "spdx" || n.kind == "license-file";
        if !is_license_claim || n.text.is_empty() {
            continue; // copyright, o license-file non classificato: astensione.
        }
        let expr_lower = n.text.to_lowercase();
        for (id, title) in denied {
            if !id.is_empty() && expr_lower.contains(&id.to_lowercase()) {
                let place = if n.line > 0 {
                    format!("{}:{}", n.path, n.line)
                } else {
                    n.path.clone()
                };
                out.push(LicenseViolation {
                    dependency: format!("{place} (sorgente)"),
                    license: n.text.clone(),
                    denied: id.clone(),
                    decision_title: title.clone(),
                });
            }
        }
    }
    out
}

/// Un nome file che DICHIARA di essere una licenza: LICENSE, LICENCE,
/// COPYING, NOTICE — nudo, con variante (`LICENSE-APACHE`, `LICENSE_MIT`) o
/// con estensione DOCUMENTALE (`NOTICE.txt`, `COPYING.LESSER`). Il suffisso è
/// ad ALLOWLIST: un file di CODICE che si chiama così (es. `license.rs` — il
/// falso positivo trovato dal collaudo su codeos-3 stesso: questo modulo
/// conteneva le frasi distintive e si auto-classificava AGPL) è sorgente, non
/// una licenza. Conservativo: un nome esotico si scansiona come sorgente
/// (miss onesto), mai si classifica per nome.
fn is_license_file_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    ["LICENSE", "LICENCE", "COPYING", "NOTICE"].iter().any(|p| {
        let Some(rest) = upper.strip_prefix(p) else {
            return false;
        };
        rest.is_empty()
            || rest.starts_with('-')
            || rest.starts_with('_')
            || matches!(rest, ".TXT" | ".MD" | ".RST" | ".HTML" | ".LESSER")
    })
}

/// Classifica il TESTO di un file di licenza per FRASE DISTINTIVA esatta —
/// l'unico modo onesto senza un motore di matching legale: o la frase c'è,
/// o ci si astiene (`None`). Le famiglie GNU si controllano dalla più
/// specifica (AGPL/LGPL prima di GPL: il loro testo CONTIENE «General
/// Public License»). Il risultato è una FAMIGLIA (es. "BSD"), non un parere.
fn classify_license_text(text: &str) -> Option<String> {
    let t = text.to_lowercase();
    if t.contains("gnu affero general public license") {
        return Some("AGPL-3.0".to_string());
    }
    if t.contains("gnu lesser general public license") {
        return Some("LGPL".to_string());
    }
    if t.contains("gnu general public license") {
        if t.contains("version 3") {
            return Some("GPL-3.0".to_string());
        }
        if t.contains("version 2") {
            return Some("GPL-2.0".to_string());
        }
        return Some("GPL".to_string());
    }
    if t.contains("apache license") && t.contains("version 2.0") {
        return Some("Apache-2.0".to_string());
    }
    if t.contains("mozilla public license") && t.contains("2.0") {
        return Some("MPL-2.0".to_string());
    }
    if t.contains("permission is hereby granted, free of charge") {
        return Some("MIT".to_string());
    }
    if t.contains("permission to use, copy, modify, and/or distribute") {
        return Some("ISC".to_string());
    }
    if t.contains("redistribution and use in source and binary forms") {
        return Some("BSD".to_string());
    }
    None
}

/// Estrae da UN file sorgente i tag SPDX (tutti) e la PRIMA intestazione di
/// copyright. Il filtro copyright è conservativo: la parola «copyright» da
/// sola non basta (comparirebbe in prosa), serve anche «(c)», «©» o un anno —
/// la forma delle intestazioni reali.
fn collect_header_notices(rel_path: &str, text: &str, out: &mut Vec<SourceNotice>) {
    let mut copyright_found = false;
    for (i, line) in text.lines().enumerate() {
        let lineno = (i + 1) as u32;
        if let Some(pos) = line.find("SPDX-License-Identifier:") {
            let expr = line[pos + "SPDX-License-Identifier:".len()..]
                .trim()
                .trim_end_matches("*/")
                .trim_end_matches("-->")
                .trim()
                .to_string();
            // Un'espressione SPDX inizia alfanumerica ("MIT", "GPL-3.0…"):
            // il marcatore citato DENTRO il codice (es. `find("SPDX-…:")`,
            // proprio in questo modulo) lascia un residuo di punteggiatura —
            // scartarlo è solo-restrittivo, mai un avviso spazzatura.
            if expr
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            {
                out.push(SourceNotice {
                    path: rel_path.to_string(),
                    line: lineno,
                    kind: "spdx".to_string(),
                    text: expr,
                });
            }
        }
        if !copyright_found && looks_like_copyright(line) {
            copyright_found = true;
            out.push(SourceNotice {
                path: rel_path.to_string(),
                line: lineno,
                kind: "copyright".to_string(),
                text: truncate_chars(line.trim(), MAX_NOTICE_TEXT_CHARS),
            });
        }
    }
}

/// Riconoscitore conservativo di una riga di copyright reale.
fn looks_like_copyright(line: &str) -> bool {
    let lower = line.to_lowercase();
    if !lower.contains("copyright") {
        return false;
    }
    if lower.contains("(c)") || line.contains('©') {
        return true;
    }
    // Un anno a 4 cifre che inizia per 19/20 (le intestazioni datate).
    let b = lower.as_bytes();
    b.windows(4).any(|w| {
        (w.starts_with(b"19") || w.starts_with(b"20")) && w.iter().all(|c| c.is_ascii_digit())
    })
}

/// Tronca a N CARATTERI (mai a metà di un multibyte — la lezione del crash
/// UTF-8 della compress, commit 7071ae6).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
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

    // --- v2: scansione nei sorgenti ---

    #[test]
    fn source_scan_extracts_spdx_and_first_copyright_with_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "// SPDX-License-Identifier: MIT OR Apache-2.0\n\
             // Copyright (c) 2021 Example Corp\n\
             // Copyright (c) 2022 Second Holder — IGNORATA (censimento per-file)\n\
             fn main() {}\n",
        )
        .unwrap();
        let scan = scan_source_notices(dir.path());
        assert_eq!(scan.truncated, 0);
        let spdx: Vec<_> = scan.notices.iter().filter(|n| n.kind == "spdx").collect();
        assert_eq!(spdx.len(), 1);
        assert_eq!(spdx[0].line, 1);
        assert_eq!(spdx[0].text, "MIT OR Apache-2.0"); // verbatim
        let copy: Vec<_> = scan
            .notices
            .iter()
            .filter(|n| n.kind == "copyright")
            .collect();
        assert_eq!(copy.len(), 1, "solo la PRIMA riga di copyright per file");
        assert_eq!(copy[0].line, 2);
        assert!(copy[0].text.contains("Example Corp"));
    }

    #[test]
    fn license_files_classify_only_by_distinctive_phrase() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("LICENSE"),
            "MIT License\n\nPermission is hereby granted, free of charge, to any person…",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("vendored")).unwrap();
        std::fs::write(
            dir.path().join("vendored").join("COPYING"),
            "GNU GENERAL PUBLIC LICENSE\nVersion 3, 29 June 2007\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("LICENSE-CUSTOM"),
            "Licenza artigianale: fai quel che vuoi.\n",
        )
        .unwrap();
        let scan = scan_source_notices(dir.path());
        let by_path = |p: &str| {
            scan.notices
                .iter()
                .find(|n| n.path == p)
                .unwrap_or_else(|| panic!("manca l'avviso per {p}"))
        };
        assert_eq!(by_path("LICENSE").text, "MIT");
        assert_eq!(by_path("vendored/COPYING").text, "GPL-3.0");
        // Testo ignoto: il FILE si riporta (fatto), la classificazione si
        // ASTIENE (text vuota) — mai indovinata.
        assert_eq!(by_path("LICENSE-CUSTOM").text, "");
        assert!(scan.notices.iter().all(|n| n.kind == "license-file"));
    }

    #[test]
    fn binary_files_and_third_party_dirs_stay_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"\x00\x01Copyright (c) 2020").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/pkg/index.js"),
            "// Copyright (c) 2020 Pkg Authors\n",
        )
        .unwrap();
        // La parola in PROSA senza (c)/©/anno non è un'intestazione.
        std::fs::write(
            dir.path().join("doc.md"),
            "Il copyright è un istituto giuridico.\n",
        )
        .unwrap();
        let scan = scan_source_notices(dir.path());
        assert!(
            scan.notices.is_empty(),
            "trovati avvisi inattesi: {:?}",
            scan.notices
        );
    }

    #[test]
    fn source_policy_flags_license_claims_never_bare_copyright() {
        let notices = vec![
            SourceNotice {
                path: "vendored/gpl.c".into(),
                line: 1,
                kind: "spdx".into(),
                text: "GPL-3.0-only".into(),
            },
            SourceNotice {
                path: "vendored/gpl.c".into(),
                line: 2,
                kind: "copyright".into(),
                text: "Copyright (c) 2020 GPL-3.0 Fan Club".into(), // mai una violazione
            },
            SourceNotice {
                path: "vendored/COPYING".into(),
                line: 0,
                kind: "license-file".into(),
                text: "GPL-3.0".into(),
            },
            SourceNotice {
                path: "LICENSE-CUSTOM".into(),
                line: 0,
                kind: "license-file".into(),
                text: String::new(), // non classificato: astensione
            },
        ];
        let denied = vec![("GPL-3.0".to_string(), "niente GPL".to_string())];
        let v = check_source_policy(&notices, &denied);
        let places: Vec<&str> = v.iter().map(|x| x.dependency.as_str()).collect();
        assert_eq!(
            places,
            vec!["vendored/gpl.c:1 (sorgente)", "vendored/COPYING (sorgente)"]
        );
    }

    #[test]
    fn gnu_family_classifies_most_specific_first() {
        assert_eq!(
            classify_license_text("GNU AFFERO GENERAL PUBLIC LICENSE Version 3"),
            Some("AGPL-3.0".to_string())
        );
        assert_eq!(
            classify_license_text("GNU LESSER GENERAL PUBLIC LICENSE Version 2.1"),
            Some("LGPL".to_string())
        );
        assert_eq!(classify_license_text("testo qualunque"), None);
    }

    /// Regressione dal collaudo e2e su codeos-3 STESSO: questo modulo
    /// (`license.rs`) veniva classificato «file di licenza AGPL-3.0» perché il
    /// nome matcha il prefisso LICENSE e il sorgente CONTIENE le frasi
    /// distintive (sono le stringhe di matching!). Un file di CODICE non è mai
    /// un file di licenza; e il marcatore SPDX citato dentro il codice non
    /// deve produrre un avviso spazzatura.
    #[test]
    fn a_code_file_named_license_is_not_a_license_file_nor_garbage_spdx() {
        assert!(!is_license_file_name("license.rs"));
        assert!(!is_license_file_name("Licenses.tsx"));
        // I nomi reali restano dentro.
        assert!(is_license_file_name("LICENSE"));
        assert!(is_license_file_name("LICENSE-APACHE"));
        assert!(is_license_file_name("COPYING.LESSER"));
        assert!(is_license_file_name("NOTICE.txt"));

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("license.rs"),
            "fn f(line: &str) { line.find(\"SPDX-License-Identifier:\"); }\n\
             // il testo contiene: gnu affero general public license\n",
        )
        .unwrap();
        let scan = scan_source_notices(dir.path());
        assert!(
            scan.notices.is_empty(),
            "un file di codice non deve produrre né license-file né SPDX spazzatura: {:?}",
            scan.notices
        );
    }
}
