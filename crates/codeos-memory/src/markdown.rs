//! [`MarkdownDecisionStore`]: una decisione = un file Markdown.
//!
//! La forma canonica del briefing: la memoria storica deve essere **ispezionabile
//! e modificabile a mano**, versionabile in git accanto al codice. Ogni decisione
//! è un file con front-matter (i campi machine-readable) e corpo in prosa (il
//! *perché*, leggibile da un umano).
//!
//! Il formato è volutamente minimale e parsato in casa (nessuna dipendenza YAML):
//! il front-matter è una sequenza di `chiave: valore` su riga singola; i campi
//! multilinea (`context`, `rationale`) vivono nel corpo, sotto heading dedicati.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use codeos_types::{EntityId, RelationKind};
use uuid::Uuid;

use crate::decision::{Decision, DecisionKind};
use crate::evidence::Evidence;
use crate::store::DecisionStore;

const FRONT_MATTER_DELIM: &str = "---\n";
const SECTION_CONTEXT: &str = "Contesto";
const SECTION_RATIONALE: &str = "Razionale";
const SECTION_EVIDENCE: &str = "Evidenza";

/// Store su file system: ogni [`Decision`] è un `.md` nella directory data.
pub struct MarkdownDecisionStore {
    dir: PathBuf,
}

impl MarkdownDecisionStore {
    /// Apre (creandola se serve) la directory che ospita le decisioni.
    pub async fn new(dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let dir = dir.into();
        tokio::fs::create_dir_all(&dir).await.with_context(|| {
            format!(
                "creazione della directory decisioni '{}' fallita",
                dir.display()
            )
        })?;
        Ok(Self { dir })
    }

    /// Nome file leggibile: `{slug-del-titolo}-{id}.md`. Lo slug rende la
    /// directory navigabile; l'id garantisce l'unicità.
    fn file_name(decision: &Decision) -> String {
        format!("{}-{}.md", slug(&decision.title), decision.id)
    }
}

#[async_trait]
impl DecisionStore for MarkdownDecisionStore {
    async fn record(&self, decision: &Decision) -> anyhow::Result<()> {
        let path = self.dir.join(Self::file_name(decision));
        tokio::fs::write(&path, render(decision))
            .await
            .with_context(|| format!("scrittura della decisione '{}' fallita", path.display()))?;
        Ok(())
    }

    async fn all(&self) -> anyhow::Result<Vec<Decision>> {
        let mut out = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.dir).await.with_context(|| {
            format!(
                "lettura della directory decisioni '{}' fallita",
                self.dir.display()
            )
        })?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !is_markdown(&path) {
                continue;
            }
            let content = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("lettura di '{}' fallita", path.display()))?;
            match parse(&content) {
                Ok(decision) => out.push(decision),
                // Un file corrotto/modificato a mano in modo invalido non deve far
                // crashare la lettura dell'intera memoria: lo segnaliamo e proseguiamo.
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "decisione Markdown illeggibile, salto");
                }
            }
        }

        out.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        Ok(out)
    }
}

fn is_markdown(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
}

/// Serializza una decisione nel formato Markdown.
fn render(d: &Decision) -> String {
    let mut s = String::new();
    s.push_str(FRONT_MATTER_DELIM);
    s.push_str(&format!("id: {}\n", d.id));
    s.push_str(&format!("kind: {}\n", d.kind.as_str()));
    s.push_str(&format!("author: {}\n", one_line(&d.author)));
    s.push_str(&format!("title: {}\n", one_line(&d.title)));
    s.push_str(&format!("timestamp: {}\n", one_line(&d.timestamp)));
    s.push_str(&format!(
        "related_entities: {}\n",
        join_ids(&d.related_entity_ids)
    ));
    s.push_str(&format!(
        "related_decisions: {}\n",
        join_ids(&d.related_decision_ids)
    ));
    s.push_str(&format!("supersedes: {}\n", join_ids(&d.supersedes)));
    s.push_str(&format!("deprecates: {}\n", join_ids(&d.deprecates)));
    s.push_str(&format!("tags: {}\n", d.tags.join(", ")));
    s.push_str(FRONT_MATTER_DELIM);
    s.push('\n');

    s.push_str(&format!("# {}\n\n", one_line(&d.title)));
    s.push_str(&format!("## {SECTION_CONTEXT}\n\n"));
    s.push_str(d.context.trim());
    s.push_str(&format!("\n\n## {SECTION_RATIONALE}\n\n"));
    s.push_str(d.rationale.trim());
    s.push('\n');

    // L'evidenza vive nel corpo, come lista leggibile a mano: è il *perché*
    // verificabile, e nel briefing dev'essere ispezionabile da un umano. Sezione
    // omessa quando vuota (decisione di autorità umana), così i file restano puliti.
    if !d.evidence.is_empty() {
        s.push_str(&format!("\n## {SECTION_EVIDENCE}\n\n"));
        for e in &d.evidence {
            s.push_str(&format!("- {}\n", render_evidence_line(e)));
        }
    }
    s
}

/// Una citazione su riga singola, in forma `tag: payload` (l'arco usa
/// ` --Kind--> ` come separatore: i delimitatori con spazio non compaiono nei
/// qualified name, quindi il round-trip è robusto).
fn render_evidence_line(e: &Evidence) -> String {
    match e {
        Evidence::Commit(h) => format!("commit: {}", one_line(h)),
        Evidence::Edge {
            source,
            kind,
            target,
        } => format!(
            "edge: {} --{}--> {}",
            one_line(source),
            kind.as_str(),
            one_line(target)
        ),
        Evidence::Entity(q) => format!("entity: {}", one_line(q)),
        Evidence::Test(t) => format!("test: {}", one_line(t)),
        Evidence::PriorDecision(id) => format!("decision: {id}"),
        Evidence::Document(src) => format!("doc: {}", one_line(src)),
    }
}

/// Legge la sezione `## Evidenza` in una lista di [`Evidence`]. Le righe
/// malformate vengono **saltate** (non si inventa evidenza da testo corrotto):
/// una citazione illeggibile non deve far perdere l'intera decisione.
fn parse_evidence(body: &str) -> Vec<Evidence> {
    section(body, SECTION_EVIDENCE)
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("- "))
        .filter_map(parse_evidence_line)
        .collect()
}

fn parse_evidence_line(line: &str) -> Option<Evidence> {
    let (tag, payload) = line.split_once(':')?;
    let payload = payload.trim();
    match tag.trim() {
        "commit" => Some(Evidence::Commit(payload.to_string())),
        "entity" => Some(Evidence::Entity(payload.to_string())),
        "test" => Some(Evidence::Test(payload.to_string())),
        "decision" => parse_id(payload).ok().map(Evidence::PriorDecision),
        "doc" => Some(Evidence::Document(payload.to_string())),
        "edge" => {
            let (source, rest) = payload.split_once(" --")?;
            let (kind, target) = rest.split_once("--> ")?;
            Some(Evidence::Edge {
                source: source.trim().to_string(),
                kind: RelationKind::from_str_lenient(kind.trim()),
                target: target.trim().to_string(),
            })
        }
        _ => None,
    }
}

/// Deserializza una decisione dal formato Markdown.
fn parse(content: &str) -> anyhow::Result<Decision> {
    let rest = content
        .strip_prefix(FRONT_MATTER_DELIM)
        .ok_or_else(|| anyhow!("front-matter mancante (atteso un blocco '---' iniziale)"))?;
    let term = format!("\n{FRONT_MATTER_DELIM}");
    let end = rest
        .find(&term)
        .ok_or_else(|| anyhow!("delimitatore di chiusura del front-matter mancante"))?;
    let front_matter = &rest[..end];
    let body = &rest[end + term.len()..];

    let mut fields = std::collections::HashMap::new();
    for line in front_matter.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Split sul PRIMO ':' soltanto: i valori (timestamp, "ai:Guardian") possono
        // contenerne altri.
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("riga di front-matter non valida: '{line}'"))?;
        fields.insert(key.trim().to_string(), value.trim().to_string());
    }

    let get = |key: &str| fields.get(key).cloned().unwrap_or_default();

    Ok(Decision {
        id: parse_id(&get("id")).context("campo 'id' non valido")?,
        kind: DecisionKind::from_str_lenient(&get("kind")),
        author: get("author"),
        title: get("title"),
        context: section(body, SECTION_CONTEXT),
        rationale: section(body, SECTION_RATIONALE),
        related_entity_ids: parse_ids(&get("related_entities"))
            .context("campo 'related_entities' non valido")?,
        related_decision_ids: parse_ids(&get("related_decisions"))
            .context("campo 'related_decisions' non valido")?,
        supersedes: parse_ids(&get("supersedes")).context("campo 'supersedes' non valido")?,
        deprecates: parse_ids(&get("deprecates")).context("campo 'deprecates' non valido")?,
        evidence: parse_evidence(body),
        tags: parse_list(&get("tags")),
        timestamp: get("timestamp"),
    })
}

/// Estrae il testo sotto `## {heading}`, fino al prossimo `## ` o alla fine.
fn section(body: &str, heading: &str) -> String {
    let marker = format!("## {heading}");
    let Some(pos) = body.find(&marker) else {
        return String::new();
    };
    let after = &body[pos + marker.len()..];
    let end = after.find("\n## ").unwrap_or(after.len());
    after[..end].trim().to_string()
}

/// Comprime ogni sequenza di whitespace verticale in uno spazio: garantisce che
/// un valore di front-matter resti su una sola riga.
fn one_line(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn join_ids(ids: &[EntityId]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_ids(raw: &str) -> anyhow::Result<Vec<EntityId>> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_id)
        .collect()
}

fn parse_id(raw: &str) -> anyhow::Result<EntityId> {
    Uuid::parse_str(raw.trim())
        .map(EntityId)
        .with_context(|| format!("UUID non valido: '{raw}'"))
}

fn parse_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Slug ASCII per il nome file: minuscole, non-alfanumerici → `-`, max 40 char.
fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in title.chars() {
        if out.len() >= 40 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "decision".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("codeos_mem_{tag}_{nanos}"))
    }

    fn sample(title: &str) -> Decision {
        Decision {
            id: EntityId::new(),
            kind: DecisionKind::ArchitectureRule,
            author: "ai:ArchitectureGuardian".to_string(),
            title: title.to_string(),
            context: "Il login deve restare lato server.\nMotivi di sicurezza.".to_string(),
            rationale: "Evita di esporre i segreti al client.".to_string(),
            related_entity_ids: vec![EntityId::new(), EntityId::new()],
            related_decision_ids: vec![EntityId::new()],
            supersedes: vec![EntityId::new()],
            deprecates: vec![EntityId::new()],
            evidence: vec![
                Evidence::Commit("a1b2c3d4".to_string()),
                // Qualified name con `::` e `<>`: mette alla prova i separatori.
                Evidence::Edge {
                    source: "crate::auth::login::handle".to_string(),
                    kind: RelationKind::Calls,
                    target: "crate::session::Store::<T>::insert".to_string(),
                },
                Evidence::Entity("crate::auth::login".to_string()),
                Evidence::PriorDecision(EntityId::new()),
                Evidence::Document("docs/adr/0001-login-lato-server.md".to_string()),
            ],
            tags: vec!["sicurezza".to_string(), "login".to_string()],
            timestamp: "2026-05-31T10:00:00+00:00".to_string(),
        }
    }

    #[test]
    fn render_then_parse_round_trips_all_fields() {
        let original = sample("Login lato server");
        let parsed = parse(&render(&original)).expect("parse fallito");

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.kind, original.kind);
        assert_eq!(parsed.author, original.author); // contiene ':' → deve sopravvivere
        assert_eq!(parsed.title, original.title);
        assert_eq!(parsed.rationale, original.rationale);
        assert_eq!(parsed.related_entity_ids, original.related_entity_ids);
        assert_eq!(parsed.related_decision_ids, original.related_decision_ids);
        assert_eq!(parsed.supersedes, original.supersedes);
        assert_eq!(parsed.deprecates, original.deprecates);
        assert_eq!(parsed.evidence, original.evidence); // arco con `::`/`<>` incluso
        assert_eq!(parsed.tags, original.tags);
        assert_eq!(parsed.timestamp, original.timestamp); // contiene ':' → idem
                                                          // Il contesto multilinea è normalizzato (trim) ma preservato nel contenuto.
        assert!(parsed.context.contains("lato server"));
        assert!(parsed.context.contains("sicurezza"));
    }

    #[test]
    fn slug_is_filesystem_friendly() {
        assert_eq!(slug("Login lato server!"), "login-lato-server");
        assert_eq!(slug("   "), "decision");
    }

    #[test]
    fn parse_rejects_a_file_without_front_matter() {
        assert!(parse("# Solo un titolo\n\nnessun front-matter").is_err());
    }

    #[tokio::test]
    async fn writes_and_reads_back_from_disk() {
        let dir = unique_dir("disk");
        let store = MarkdownDecisionStore::new(&dir).await.unwrap();

        let decision = sample("Usare SQLite per la v1");
        store.record(&decision).await.unwrap();

        // Il file è davvero lì e ha l'aspetto giusto.
        let listing = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(listing, 1);

        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, decision.id);
        assert_eq!(all[0].title, decision.title);

        // related_to filtra per entità agganciata.
        let target = decision.related_entity_ids[0];
        let hits = store.related_to(&[target]).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(store
            .related_to(&[EntityId::new()])
            .await
            .unwrap()
            .is_empty());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
