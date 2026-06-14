//! Il **Minatore di Decisioni**: estrae il *perché esplicito* dai messaggi di commit.
//!
//! # L'idea
//!
//! I [`fossil`](crate::fossil) datano la **nascita** di un confine (il commit più
//! vecchio che ha co-toccato due layer). Restano fuori le decisioni che un autore
//! ha **scritto a parole** nel messaggio di commit: «ho usato SQLite *perché*…»,
//! «BREAKING CHANGE: …», «DECISION: …», un riferimento a un ADR. Quel *perché* è
//! già nel repository — ma sepolto nella storia, mai promosso a memoria di intento.
//!
//! Il minatore lo riemerge nel ledger. È la tesi **anti-falso-positivo** applicata
//! alla cattura del perché, e qui è non-negoziabile:
//!
//! - **Mai inventare.** Il razionale estratto è una **copia verbatim** delle parole
//!   dell'autore (al più normalizzata negli spazi), mai una parafrasi. Il rischio
//!   peggiore è riportare il testo giusto; non è mai un perché immaginato.
//! - **Citare la fonte.** Ogni decisione minata porta l'**hash del commit**: è
//!   verificabile, esattamente come una [`Evidence::Commit`](crate::history). Una
//!   decisione minata può solo diventare una *proposta* da confermare a mano.
//! - **Astenersi.** La stragrande maggioranza dei commit NON è una decisione («fix
//!   typo», «wip», «bump deps»). Su questi il minatore **non produce nulla** invece
//!   di forzare un perché: un arco mancante è meglio di uno che mente. Il tasso di
//!   astensione è un dato da *misurare*, non un fallimento da nascondere.
//!
//! # Cos'è un segnale di intento esplicito
//!
//! Solo due famiglie, in ordine di forza:
//!
//! 1. **Forte** — un marcatore strutturale deliberato: una riga etichettata
//!    (`DECISION:`, `WHY:`, `RATIONALE:`, `MOTIVAZIONE:`, `PERCHÉ:`), il footer
//!    `BREAKING CHANGE:` dei conventional-commit, o un riferimento a un ADR
//!    (`ADR-014`). L'autore ha *scelto* di registrare un perché.
//! 2. **Causale** — il corpo del commit contiene un connettivo causale
//!    (`because`, `in order to`, `to avoid`, `perché`, `per evitare`, …): l'autore
//!    ha spiegato un *perché* in prosa. Più debole (più rumore), quindi marcata
//!    come tale così l'umano la tria.
//!
//! Tutto il resto è astensione.

use std::path::Path;
use std::process::Command;

/// Un messaggio di commit completo — la materia prima del minatore. A differenza
/// di [`Commit`](crate::history::Commit) (che porta solo il soggetto `%s`, perché
/// ai fossili basta quello), qui serve anche il **corpo** `%b`: è lì che vive il
/// perché in prosa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMessage {
    /// Hash del commit (`%H`): la citazione verificabile della decisione minata.
    pub hash: String,
    /// La riga di soggetto (`%s`): diventa il titolo della decisione.
    pub subject: String,
    /// Il corpo del messaggio (`%b`), eventualmente multilinea e vuoto.
    pub body: String,
}

/// Quanto è forte il segnale di intento che ha fatto scattare l'estrazione.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentConfidence {
    /// Marcatore strutturale esplicito (`DECISION:`/`BREAKING CHANGE:`/`ADR-…`).
    Strong,
    /// Connettivo causale nel corpo/soggetto (`because`, `perché`, …).
    Causal,
}

impl IntentConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            IntentConfidence::Strong => "forte",
            IntentConfidence::Causal => "causale",
        }
    }
}

/// La **fonte** verificabile di una decisione minata: dove vive il perché.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionSource {
    /// Un commit git, per hash: il perché era nel messaggio.
    Commit(String),
    /// Un documento (ADR/RFC/design doc), per path: il perché era scritto lì.
    Document(String),
}

impl DecisionSource {
    /// Un riferimento breve e leggibile per l'output a terminale (hash corto /
    /// nome del file).
    pub fn short(&self) -> String {
        match self {
            DecisionSource::Commit(h) => {
                let s = if h.len() >= 9 { &h[..9] } else { h };
                format!("commit {s}")
            }
            DecisionSource::Document(p) => {
                let name = p.rsplit('/').next().unwrap_or(p);
                format!("ADR {name}")
            }
        }
    }

    /// La chiave stabile per la deduplica (hash pieno / path intero).
    pub fn key(&self) -> &str {
        match self {
            DecisionSource::Commit(h) => h,
            DecisionSource::Document(p) => p,
        }
    }
}

/// Una decisione estratta da una fonte verificabile: il perché **verbatim**
/// dell'autore, con la prova che lo sostiene. È volutamente disaccoppiata da
/// `codeos-memory`: il minatore resta puro, e chi la consuma (la CLI) la trasforma
/// in una *proposta* da confermare, citando la fonte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedDecision {
    /// Da dove viene il perché — la prova citabile (commit o documento).
    pub source: DecisionSource,
    /// Il titolo della decisione (il soggetto del commit / l'H1 dell'ADR).
    pub title: String,
    /// Il testo di intento dell'autore, **copiato verbatim** (mai parafrasato).
    pub rationale: String,
    /// Quale segnale ha innescato l'estrazione (tracciabilità: l'umano vede *perché*
    /// CodeOS ha proposto questa, es. `DECISION`, `BREAKING CHANGE`, `because`, `ADR`).
    pub marker: String,
    /// La forza del segnale.
    pub confidence: IntentConfidence,
}

/// Etichette che marcano un razionale dichiarato (confronto case-insensitive sul
/// prefisso prima del primo `:`). Includono il footer `BREAKING CHANGE` dei
/// conventional-commit e la forma italiana.
const LABELS: &[&str] = &[
    "DECISION",
    "RATIONALE",
    "REASON",
    "WHY",
    "MOTIVATION",
    "MOTIVAZIONE",
    "PERCHÉ",
    "PERCHE'",
    "PERCHE",
    "BREAKING CHANGE",
    "BREAKING-CHANGE",
];

/// Connettivi causali (case-insensitive, EN+IT). `since` è escluso di proposito:
/// è ambiguo (temporale vs causale) e porterebbe rumore.
const CONNECTIVES: &[&str] = &[
    // English
    "because",
    "in order to",
    "so that",
    "to avoid",
    "to prevent",
    "to ensure",
    "otherwise",
    "rather than",
    "instead of",
    "the reason",
    // Italiano
    "perché",
    "perche",
    "poiché",
    "poiche",
    "siccome",
    "dato che",
    "in modo da",
    "per evitare",
    "per non",
    "altrimenti",
    "anziché",
    "anziche",
    "invece di",
    "il motivo",
];

/// Prefissi di righe-trailer che NON sono un perché (firme, co-autori, link a
/// issue, la boilerplate di un revert). Vengono ignorati nella ricerca causale,
/// così un commit il cui «corpo» è solo `Co-authored-by:` si astiene.
const TRAILER_PREFIXES: &[&str] = &[
    "signed-off-by",
    "co-authored-by",
    "reviewed-by",
    "acked-by",
    "tested-by",
    "fixes",
    "closes",
    "refs",
    "cc",
];

/// Estrae le decisioni esplicite da una sequenza di messaggi di commit. **Puro**:
/// nessun I/O, così la logica anti-FP è testabile senza un repo git. Restituisce
/// solo i commit con un segnale di intento; tutti gli altri sono astensioni
/// (assenti dal risultato), per costruzione.
pub fn mine(messages: &[CommitMessage]) -> Vec<MinedDecision> {
    messages
        .iter()
        .filter_map(|m| {
            extract_intent(&m.subject, &m.body).map(|(rationale, marker, confidence)| {
                MinedDecision {
                    source: DecisionSource::Commit(m.hash.clone()),
                    title: m.subject.trim().to_string(),
                    rationale,
                    marker,
                    confidence,
                }
            })
        })
        .collect()
}

/// Il cuore anti-FP: dato soggetto e corpo, ritorna `Some((rationale, marker,
/// confidence))` **solo** se c'è un segnale di intento esplicito, `None` altrimenti.
/// L'ordine è per forza decrescente: etichetta → ADR → causale.
fn extract_intent(subject: &str, body: &str) -> Option<(String, String, IntentConfidence)> {
    // I merge non sono decisioni di intento (difensivo: il lettore usa già
    // `--no-merges`).
    if subject.trim_start().to_lowercase().starts_with("merge ") {
        return None;
    }

    // Soggetto come prima «riga», poi le righe del corpo.
    let lines: Vec<&str> = std::iter::once(subject).chain(body.lines()).collect();

    // 1) Etichetta esplicita (la più forte): `DECISION: …`, `BREAKING CHANGE: …`.
    for (i, line) in lines.iter().enumerate() {
        if let Some((marker, after)) = match_label(line) {
            let rationale = capture_from(after, &lines[i + 1..]);
            if !rationale.is_empty() {
                return Some((rationale, marker, IntentConfidence::Strong));
            }
        }
    }

    // 2) Riferimento a un ADR: l'intento vive nel documento citato, l'hash lo
    //    àncora. La riga intera (verbatim) è il razionale.
    for line in &lines {
        if contains_adr_ref(line) {
            let rationale = line.trim().to_string();
            if !rationale.is_empty() {
                return Some((rationale, "ADR".to_string(), IntentConfidence::Strong));
            }
        }
    }

    // 3) Connettivo causale nel corpo (poi nel soggetto): l'autore ha spiegato un
    //    perché in prosa. Più debole, marcato come tale. Si cattura il PARAGRAFO
    //    intero che contiene il connettivo (non la singola riga: i corpi sono spesso
    //    a-capo a ~72 colonne, e una riga sola sarebbe un frammento a metà frase).
    for para in paragraphs_of(body) {
        if let Some(conn) = first_connective(&para) {
            return Some((para, conn, IntentConfidence::Causal));
        }
    }
    let subject_norm = subject.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(conn) = first_connective(&subject_norm) {
        return Some((subject_norm, conn, IntentConfidence::Causal));
    }

    None
}

/// Spezza il corpo in **paragrafi** (blocchi separati da righe vuote), scartando
/// trailer e boilerplate di revert e normalizzando gli spazi. Un paragrafo è
/// l'unità naturale di un pensiero: catturarlo intero dà un perché completo invece
/// di un frammento di riga.
fn paragraphs_of(body: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let flush = |buf: &mut Vec<&str>, out: &mut Vec<String>| {
        if !buf.is_empty() {
            let joined = buf.join(" ");
            let norm = joined.split_whitespace().collect::<Vec<_>>().join(" ");
            if !norm.is_empty() {
                out.push(norm);
            }
            buf.clear();
        }
    };
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            flush(&mut current, &mut paragraphs);
        } else if !is_trailer(t) && !is_revert_boilerplate(t) {
            current.push(t);
        }
    }
    flush(&mut current, &mut paragraphs);
    paragraphs
}

/// Se `line` è una riga etichettata (`<LABEL>: …`), ritorna `(marker_canonico,
/// testo_dopo_i_due_punti)`. Il confronto è case-insensitive e tollera un bullet
/// iniziale (`- `, `* `, `• `) e un suffisso non alfabetico dopo l'etichetta
/// (`DECISION (D102): …`).
fn match_label(line: &str) -> Option<(String, &str)> {
    let trimmed = strip_bullet(line.trim_start());
    let (prefix, after) = trimmed.split_once(':')?;
    let prefix_upper = prefix.trim().to_uppercase();
    for label in LABELS {
        // Combacia se il prefisso È l'etichetta, o l'etichetta seguita da un
        // carattere non alfanumerico (es. «DECISION (D102)»).
        let matches = prefix_upper == *label
            || (prefix_upper.starts_with(label)
                && prefix_upper[label.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_alphanumeric()));
        if matches {
            let marker = match *label {
                "BREAKING-CHANGE" => "BREAKING CHANGE".to_string(),
                other => other.to_string(),
            };
            return Some((marker, after));
        }
    }
    None
}

/// Costruisce il razionale da `head` (il testo dopo i due punti) più le righe di
/// continuazione successive, fermandosi alla prima riga vuota, a un'altra etichetta
/// o a un trailer. Verbatim, solo gli spazi sono normalizzati.
fn capture_from(head: &str, rest: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let h = head.trim();
    if !h.is_empty() {
        parts.push(h.to_string());
    }
    for line in rest {
        let t = line.trim();
        if t.is_empty() || match_label(line).is_some() || is_trailer(t) {
            break;
        }
        parts.push(t.to_string());
    }
    parts.join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `true` se la riga cita un ADR: `ADR-<num>` o `ADR <num>` (case-insensitive).
fn contains_adr_ref(line: &str) -> bool {
    let lower = line.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("adr") {
        let start = i + pos;
        let after = start + 3;
        // Il carattere dopo «adr» dev'essere un separatore, poi una cifra.
        let sep = bytes.get(after).copied();
        if matches!(sep, Some(b'-') | Some(b' ') | Some(b'_') | Some(b'#')) {
            if let Some(&next) = bytes.get(after + 1) {
                if next.is_ascii_digit() {
                    return true;
                }
            }
        }
        i = after;
    }
    false
}

/// Il primo connettivo causale presente in `line` (case-insensitive), come stringa
/// di marcatore, o `None`.
fn first_connective(line: &str) -> Option<String> {
    let lower = line.to_lowercase();
    CONNECTIVES
        .iter()
        .find(|c| lower.contains(**c))
        .map(|c| c.to_string())
}

fn is_trailer(line: &str) -> bool {
    let lower = line.trim_start().to_lowercase();
    let prefix = lower.split_once(':').map(|(p, _)| p).unwrap_or(&lower);
    TRAILER_PREFIXES.contains(&prefix.trim())
}

fn is_revert_boilerplate(line: &str) -> bool {
    line.trim_start()
        .to_lowercase()
        .starts_with("this reverts commit")
}

fn strip_bullet(line: &str) -> &str {
    for b in ["- ", "* ", "• "] {
        if let Some(rest) = line.strip_prefix(b) {
            return rest;
        }
    }
    line
}

// --- Lettura da git (l'unico I/O) -----------------------------------------------

/// Marcatore d'inizio record nell'output di `git log`. Distinto da quello dei
/// fossili (`>>>>codeos-commit:`) per non confondere i due flussi.
const MSG_MARKER: &str = ">>>>codeos-msg:";
/// Separatore di campo dentro l'header (`hash`, `subject`): l'*unit separator*
/// ASCII (`0x1f`), che non compare mai in un hash o in una riga di soggetto.
const FIELD_SEP: char = '\u{1f}';

/// Legge i messaggi di commit (soggetto **e** corpo) dal repository, dal più
/// recente. `--no-merges` (i merge non sono decisioni). `None` = tutta la storia.
///
/// **Best-effort onesto**: propaga un errore se `git` fallisce, ma non inventa
/// nulla. Non riusa [`GitLog`](crate::history::GitLog): a quello serve `--name-only`
/// (i file), a questo il corpo `%b` — mescolarli renderebbe il parsing ambiguo.
pub fn read_commit_messages(
    repo_root: impl AsRef<Path>,
    max: Option<usize>,
) -> anyhow::Result<Vec<CommitMessage>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo_root.as_ref())
        .arg("log")
        .arg("--no-merges")
        .arg(format!("--pretty=format:{MSG_MARKER}%H{FIELD_SEP}%s{FIELD_SEP}%b"));
    if let Some(n) = max {
        cmd.arg(format!("-n{n}"));
    }
    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`git log` fallito ({}): {}", output.status, stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_commit_messages(&stdout))
}

/// Trasforma l'output di `git log --pretty=format:{MARKER}%H{SEP}%s{SEP}%b` in
/// messaggi. Ogni riga col marcatore apre un record; le righe successive sono la
/// continuazione del corpo. **Puro**: il parsing è testabile senza git.
fn parse_commit_messages(stdout: &str) -> Vec<CommitMessage> {
    let mut records: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(MSG_MARKER) {
            if let Some(r) = current.take() {
                records.push(r);
            }
            current = Some(rest.to_string());
        } else if let Some(buf) = current.as_mut() {
            buf.push('\n');
            buf.push_str(line);
        }
    }
    if let Some(r) = current.take() {
        records.push(r);
    }

    records
        .into_iter()
        .filter_map(|rec| {
            // `hash{SEP}subject{SEP}body…`. Il corpo (rimanente) può essere
            // multilinea e vuoto; soggetto e hash non contengono mai il separatore.
            let mut fields = rec.splitn(3, FIELD_SEP);
            let hash = fields.next()?.trim().to_string();
            if hash.is_empty() {
                return None;
            }
            let subject = fields.next().unwrap_or("").to_string();
            let body = fields.next().unwrap_or("").trim_end().to_string();
            Some(CommitMessage {
                hash,
                subject,
                body,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(hash: &str, subject: &str, body: &str) -> CommitMessage {
        CommitMessage {
            hash: hash.to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
        }
    }

    /// Una riga etichettata produce un razionale FORTE, copiato verbatim.
    #[test]
    fn mines_a_labeled_rationale_verbatim() {
        let m = msg(
            "abc123",
            "Passa a SQLite",
            "DECISION: niente server DB, deve girare offline su un laptop.\n\nAltro testo.",
        );
        let out = mine(&[m]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, DecisionSource::Commit("abc123".to_string()));
        assert_eq!(out[0].title, "Passa a SQLite");
        assert_eq!(out[0].marker, "DECISION");
        assert_eq!(out[0].confidence, IntentConfidence::Strong);
        assert_eq!(
            out[0].rationale,
            "niente server DB, deve girare offline su un laptop."
        );
    }

    /// Il footer `BREAKING CHANGE` dei conventional-commit è un segnale forte.
    #[test]
    fn mines_a_breaking_change_footer() {
        let m = msg(
            "def456",
            "feat: nuovo formato del config",
            "BREAKING CHANGE: il vecchio campo `port` è rimosso, usare `addr`.",
        );
        let out = mine(&[m]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].marker, "BREAKING CHANGE");
        assert_eq!(out[0].confidence, IntentConfidence::Strong);
        assert!(out[0].rationale.contains("`port`"));
    }

    /// Un riferimento a un ADR cita la decisione: forte, riga verbatim.
    #[test]
    fn mines_an_adr_reference() {
        let m = msg("a11", "Introduce caching layer (ADR-014)", "");
        let out = mine(&[m]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].marker, "ADR");
        assert_eq!(out[0].confidence, IntentConfidence::Strong);
        assert!(out[0].rationale.contains("ADR-014"));
    }

    /// Un connettivo causale in italiano nel corpo → razionale CAUSALE, verbatim.
    #[test]
    fn mines_a_causal_body_italian() {
        let m = msg(
            "ca5",
            "Cache dei risultati del parser",
            "Memorizzo l'AST per evitare di riparsare ogni file a ogni query.",
        );
        let out = mine(&[m]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence, IntentConfidence::Causal);
        assert_eq!(out[0].marker, "per evitare");
        assert!(out[0].rationale.contains("per evitare di riparsare"));
    }

    /// Un corpo causale a-capo su più righe (come `git log` lo emette a ~72 colonne)
    /// dev'essere catturato come PARAGRAFO intero, non come frammento di riga.
    #[test]
    fn captures_the_whole_causal_paragraph_not_a_line_fragment() {
        let m = msg(
            "wrap1",
            "Time-box dell'indicizzazione",
            "Mettiamo un tetto di 420s sull'indicizzazione per evitare\nche un monorepo \
             gigante faccia girare il resolver all'infinito\ne blocchi la richiesta gRPC.\n\n\
             Co-authored-by: X <x@y.io>",
        );
        let out = mine(&[m]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence, IntentConfidence::Causal);
        // L'intera frase, ricomposta dalle tre righe; il trailer è escluso.
        assert!(out[0].rationale.contains("per evitare che un monorepo gigante"));
        assert!(out[0].rationale.contains("blocchi la richiesta gRPC"));
        assert!(!out[0].rationale.contains("Co-authored-by"));
    }

    /// Idem in inglese («because»).
    #[test]
    fn mines_a_causal_body_english() {
        let m = msg(
            "ca6",
            "Pin tokio version",
            "We pin tokio because newer minors broke the runtime in CI.",
        );
        let out = mine(&[m]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence, IntentConfidence::Causal);
        assert_eq!(out[0].marker, "because");
    }

    /// Il cuore anti-FP: i commit terse non producono NULLA (astensione).
    #[test]
    fn abstains_on_terse_commits() {
        let terse = vec![
            msg("1", "fix typo", ""),
            msg("2", "wip", ""),
            msg("3", "bump deps", ""),
            msg("4", "Update README", ""),
            msg("5", "formatting", ""),
        ];
        assert!(
            mine(&terse).is_empty(),
            "nessun segnale esplicito ⇒ nessuna decisione inventata"
        );
    }

    /// Un corpo fatto SOLO di trailer (co-autori, firme) non è un perché.
    #[test]
    fn abstains_when_body_is_only_trailers() {
        let m = msg(
            "tr1",
            "Refactor del modulo auth",
            "Co-authored-by: Tizio <t@x.io>\nSigned-off-by: Caio <c@x.io>",
        );
        assert!(mine(&[m]).is_empty());
    }

    /// I merge non sono decisioni di intento.
    #[test]
    fn abstains_on_merge_commits() {
        let m = msg("mg1", "Merge branch 'feature/x' into main", "because reasons");
        assert!(mine(&[m]).is_empty());
    }

    /// Garanzia di non-invenzione: il razionale estratto è SEMPRE un sottoinsieme
    /// (a meno di spazi) del testo originale del commit. Mai una parafrasi.
    #[test]
    fn rationale_is_always_verbatim() {
        let cases = vec![
            msg("v1", "x", "WHY: scelta dettata dal vincolo di memoria a 64MB"),
            msg("v2", "y", "Lo facciamo perché il vecchio path era O(n^2)"),
            msg("v3", "Implementa retry (ADR-7)", ""),
        ];
        for m in &cases {
            for d in mine(std::slice::from_ref(m)) {
                let haystack = normalize(&format!("{} {}", m.subject, m.body));
                let needle = normalize(&d.rationale);
                assert!(
                    haystack.contains(&needle),
                    "razionale non verbatim:\n  estratto: {needle}\n  origine:  {haystack}"
                );
            }
        }
    }

    fn normalize(s: &str) -> String {
        s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Il parser regge corpi multilinea e corpi vuoti, e non confonde i record.
    #[test]
    fn parses_multiline_and_empty_bodies() {
        // Record separati da newline, come l'output reale di `git log` (l'header di
        // ogni commit è all'inizio di riga; il corpo vuoto di h2 emette comunque una
        // riga prima del marcatore successivo).
        let raw = format!(
            "{MSG_MARKER}h1{FIELD_SEP}primo soggetto{FIELD_SEP}riga uno\nriga due\n\
             {MSG_MARKER}h2{FIELD_SEP}secondo soggetto{FIELD_SEP}\n\
             {MSG_MARKER}h3{FIELD_SEP}terzo{FIELD_SEP}corpo breve"
        );
        let msgs = parse_commit_messages(&raw);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].hash, "h1");
        assert_eq!(msgs[0].subject, "primo soggetto");
        assert_eq!(msgs[0].body, "riga uno\nriga due");
        assert_eq!(msgs[1].hash, "h2");
        assert_eq!(msgs[1].subject, "secondo soggetto");
        assert_eq!(msgs[1].body, "");
        assert_eq!(msgs[2].body, "corpo breve");
    }

    /// `DECISION (D102): …` — l'etichetta con un suffisso non alfabetico combacia.
    #[test]
    fn label_tolerates_a_parenthetical_suffix() {
        let m = msg("p1", "Soggetto", "DECISION (D102): isolare il dominio dall'infra");
        let out = mine(&[m]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].marker, "DECISION");
        assert!(out[0].rationale.contains("isolare il dominio"));
    }
}
