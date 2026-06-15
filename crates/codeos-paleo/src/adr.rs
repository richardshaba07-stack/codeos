//! Ingestione degli **ADR** (Architecture Decision Records).
//!
//! Gli ADR sono l'artefatto standard con cui i team registrano le decisioni
//! architetturali a parole: un file Markdown per decisione, con titolo, stato,
//! contesto e la decisione vera e propria. È il *perché* già scritto — ma vive in
//! `docs/adr/` accanto al codice, non nel ledger di CodeOS.
//!
//! Questo modulo lo riemerge, con la stessa disciplina anti-FP del
//! [`miner`](crate::miner):
//!
//! - **Verbatim**: il razionale è il testo della sezione *Decision* (o *Context*)
//!   copiato, mai parafrasato.
//! - **Cita la fonte**: ogni decisione minata porta il path dell'ADR
//!   ([`DecisionSource::Document`]), verificabile.
//! - **Astiene**: un file che non ha la forma di un ADR (niente titolo H1, nessuna
//!   sezione *Decision*/*Context*), un template, o un ADR *superseded/rejected*
//!   (non è più la verità corrente) non produce nulla.
//!
//! Gli ADR sono decisioni esplicite *per definizione*: qui non c'è inferenza né
//! rischio-flood, solo lettura fedele di ciò che un umano ha già deliberato.

use std::path::Path;

use crate::miner::{DecisionSource, IntentConfidence, MinedDecision};

/// Un ADR letto da disco: il path (per la citazione) e il contenuto grezzo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdrDoc {
    /// Path relativo alla radice del repo (la citazione verificabile).
    pub path: String,
    /// Il contenuto Markdown del file.
    pub content: String,
}

/// Le directory convenzionali dove vivono gli ADR (Nygard, MADR, adr-tools).
const ADR_DIRS: &[&str] = &[
    "docs/adr",
    "docs/adrs",
    "doc/adr",
    "docs/architecture/decisions",
    "docs/decisions",
    "architecture/decisions",
    "adr",
    "adrs",
];

/// Legge gli ADR dalle directory convenzionali sotto `repo_root`. **Best-effort**:
/// le directory assenti e i file illeggibili si saltano in silenzio (non è un
/// errore non avere ADR). Ordinato per path, così l'output è deterministico.
pub fn read_adrs(repo_root: impl AsRef<Path>) -> Vec<AdrDoc> {
    let root = repo_root.as_ref();
    let mut docs = Vec::new();
    for rel in ADR_DIRS {
        let dir = root.join(rel);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            // Salta gli «indice»/template che non sono decisioni.
            if name == "readme.md" || name == "index.md" || name.contains("template") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                docs.push(AdrDoc {
                    path: format!("{rel}/{}", path.file_name().unwrap().to_string_lossy()),
                    content,
                });
            }
        }
    }
    docs.sort_by(|a, b| a.path.cmp(&b.path));
    docs
}

/// `true` se `path` vive in una directory ADR convenzionale (confronto su
/// separatori normalizzati). Serve a distinguere un commit che *implementa* una
/// decisione citando un ADR (segnale valido) da uno che *edita* un file ADR
/// (manutenzione: l'ADR è già ingerito dalla fonte autoritativa, quindi il segnale
/// a livello di commit è solo rumore — finding misurato su adr-tools).
pub fn is_adr_path(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    ADR_DIRS
        .iter()
        .any(|dir| norm.contains(&format!("/{dir}/")) || norm.starts_with(&format!("{dir}/")))
}

/// Estrae le decisioni dagli ADR. **Puro**: nessun I/O, testabile senza filesystem.
/// Salta i documenti che non hanno la forma di un ADR e gli ADR non più correnti.
pub fn mine_adrs(docs: &[AdrDoc]) -> Vec<MinedDecision> {
    docs.iter().filter_map(parse_adr).collect()
}

fn parse_adr(doc: &AdrDoc) -> Option<MinedDecision> {
    // Serve un titolo H1, altrimenti non è un ADR ben formato.
    let title = adr_title(&doc.content)?;
    if title.to_lowercase().contains("template") {
        return None;
    }

    // Stato: se l'ADR è stato SUPERATO o RESPINTO non è più la verità corrente
    // → astensione onesta (non si registra una decisione morta come viva).
    if let Some(status) = section(&doc.content, "status") {
        let s = status.to_lowercase();
        if s.contains("superseded") || s.contains("rejected") || s.contains("superato") {
            return None;
        }
    }

    // Il razionale: la sezione *Decision* (la scelta), o in mancanza *Context*.
    let rationale = section(&doc.content, "decision")
        .or_else(|| section(&doc.content, "context"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    Some(MinedDecision {
        source: DecisionSource::Document(doc.path.clone()),
        title,
        rationale,
        marker: "ADR".to_string(),
        confidence: IntentConfidence::Strong,
    })
}

/// Il titolo dall'H1 (`# …`), ripulito dalla numerazione (`1. `, `0001: `,
/// `ADR-7 — `). `None` se non c'è un H1.
fn adr_title(content: &str) -> Option<String> {
    for line in content.lines() {
        let t = line.trim();
        // Un H1 esatto: un solo '#' seguito da spazio.
        if let Some(rest) = t.strip_prefix("# ") {
            let title = strip_adr_numbering(rest.trim());
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    None
}

/// Toglie una numerazione iniziale tipo `1.`, `0001:`, `ADR 7 -`, lasciando il
/// titolo leggibile. Conservativo: se dopo lo strip non resta nulla, torna l'input.
fn strip_adr_numbering(title: &str) -> String {
    let mut s = title.trim();
    // Prefisso «ADR» opzionale.
    if s.len() >= 3 && s[..3].eq_ignore_ascii_case("adr") {
        s = s[3..].trim_start_matches(['-', ' ', '\u{2014}', '#', ':']).trim_start();
    }
    // Cifre iniziali (il numero dell'ADR).
    let after_digits = s.trim_start_matches(|c: char| c.is_ascii_digit());
    // Se c'erano cifre, mangia anche il separatore che segue (`.`/`:`/`-`/`—`).
    if after_digits.len() != s.len() {
        let cleaned = after_digits
            .trim_start_matches(['.', ':', ')', '-', ' ', '\u{2014}'])
            .trim();
        if !cleaned.is_empty() {
            return cleaned.to_string();
        }
    }
    s.trim().to_string()
}

/// Estrae il testo della sezione la cui heading (`##`, `###`, …) **contiene** la
/// parola chiave (case-insensitive), fino alla prossima heading. Robusto alle
/// varianti MADR (`## Context and Problem Statement`, `## Decision Outcome`).
fn section(content: &str, keyword: &str) -> Option<String> {
    let kw = keyword.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if is_heading(line) {
            let heading_text = line.trim_start_matches('#').trim().to_lowercase();
            if heading_text.contains(&kw) {
                // Raccoglie fino alla prossima heading.
                let mut body = Vec::new();
                let mut j = i + 1;
                while j < lines.len() && !is_heading(lines[j].trim()) {
                    body.push(lines[j]);
                    j += 1;
                }
                let text = body.join("\n").trim().to_string();
                return (!text.is_empty()).then_some(text);
            }
        }
        i += 1;
    }
    None
}

fn is_heading(line: &str) -> bool {
    line.starts_with("# ")
        || line.starts_with("## ")
        || line.starts_with("### ")
        || line.starts_with("#### ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adr(path: &str, content: &str) -> AdrDoc {
        AdrDoc {
            path: path.to_string(),
            content: content.to_string(),
        }
    }

    /// Un ADR Nygard ben formato → decisione FORTE, razionale = sezione Decision,
    /// fonte = il path, titolo ripulito dalla numerazione.
    #[test]
    fn mines_a_well_formed_nygard_adr() {
        let doc = adr(
            "docs/adr/0001-use-sqlite.md",
            "# 1. Use SQLite for storage\n\n## Status\n\nAccepted\n\n## Context\n\n\
             We need an embedded store.\n\n## Decision\n\nWe will use SQLite because it \
             runs in-process with zero ops.\n\n## Consequences\n\nNo server to manage.",
        );
        let out = mine_adrs(&[doc]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Use SQLite for storage");
        assert_eq!(out[0].marker, "ADR");
        assert_eq!(out[0].confidence, IntentConfidence::Strong);
        assert_eq!(
            out[0].source,
            DecisionSource::Document("docs/adr/0001-use-sqlite.md".to_string())
        );
        assert!(out[0].rationale.contains("runs in-process"));
        // Il razionale è la DECISION, non le Consequences.
        assert!(!out[0].rationale.contains("No server to manage"));
    }

    /// Un ADR SUPERSEDED non è più la verità corrente → astensione.
    #[test]
    fn abstains_on_a_superseded_adr() {
        let doc = adr(
            "docs/adr/0002-old.md",
            "# 2. Old choice\n\n## Status\n\nSuperseded by ADR-5\n\n## Decision\n\nWe used X.",
        );
        assert!(mine_adrs(&[doc]).is_empty());
    }

    /// Un file senza forma di ADR (niente H1, niente Decision/Context) → astensione.
    #[test]
    fn abstains_on_a_non_adr_file() {
        let doc = adr("docs/adr/notes.md", "Some loose notes.\n- a\n- b\n");
        assert!(mine_adrs(&[doc]).is_empty());
    }

    /// Senza una sezione Decision si ripiega sul Context (è comunque un perché).
    #[test]
    fn falls_back_to_context_when_no_decision_section() {
        let doc = adr(
            "docs/adr/0003-x.md",
            "# 3. Something\n\n## Status\n\nProposed\n\n## Context\n\nThe reason we consider this.",
        );
        let out = mine_adrs(&[doc]);
        assert_eq!(out.len(), 1);
        assert!(out[0].rationale.contains("The reason we consider this"));
    }

    /// MADR: heading più lunghe (`## Decision Outcome`) devono combaciare.
    #[test]
    fn matches_madr_style_long_headings() {
        let doc = adr(
            "docs/decisions/0004.md",
            "# ADR-4 — Pick gRPC\n\n## Context and Problem Statement\n\nNeed RPC.\n\n\
             ## Decision Outcome\n\nChosen: gRPC, because of streaming.",
        );
        let out = mine_adrs(&[doc]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Pick gRPC");
        assert!(out[0].rationale.contains("because of streaming"));
    }

    /// Un template non è una decisione.
    #[test]
    fn abstains_on_a_template() {
        let doc = adr(
            "docs/adr/template.md",
            "# ADR Template\n\n## Status\n\n## Decision\n\n{decision goes here}",
        );
        assert!(mine_adrs(&[doc]).is_empty());
    }

    #[test]
    fn recognizes_adr_paths() {
        assert!(is_adr_path("/abs/repo/doc/adr/0002-x.md"));
        assert!(is_adr_path("docs/adr/0001-y.md"));
        assert!(is_adr_path("/r/docs/architecture/decisions/0003.md"));
        // Un file qualunque NON è un ADR.
        assert!(!is_adr_path("/abs/repo/src/billing/charge.rs"));
        assert!(!is_adr_path("README.md"));
    }

    #[test]
    fn strips_various_numbering_styles() {
        assert_eq!(strip_adr_numbering("1. Use SQLite"), "Use SQLite");
        assert_eq!(strip_adr_numbering("0001: Use SQLite"), "Use SQLite");
        assert_eq!(strip_adr_numbering("ADR-7 — Use SQLite"), "Use SQLite");
        assert_eq!(strip_adr_numbering("Use SQLite"), "Use SQLite");
    }
}
