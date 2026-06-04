//! La sorgente della storia dei commit.
//!
//! L'analisi di astensione ha bisogno di una sola cosa da ogni commit: **quali
//! file ha toccato**. Astraendola dietro [`CommitHistory`] manteniamo la
//! matematica testabile in isolamento (con [`InMemoryHistory`]) e isoliamo l'unico
//! pezzo di I/O — la lettura di `git` — in [`GitLog`].

use std::path::{Path, PathBuf};
use std::process::Command;

/// Un commit ridotto all'essenziale per le due analisi del Paleontologo:
/// - il **Campo di Astensione** usa solo `changed_files` (quali file ha toccato);
/// - i **Fossili di Decisione** usano anche `hash`, `timestamp` e `subject` per
///   datare la *nascita* di un confine e recuperarne l'**intento** (il messaggio
///   con cui lo sviluppatore lo ha disegnato).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Hash del commit (`%H`). Vuoto se la sorgente non lo fornisce.
    pub hash: String,
    /// Istante del commit (committer date, secondi Unix `%ct`). 0 se assente.
    pub timestamp: i64,
    /// Riga di soggetto del messaggio (`%s`): l'intento dichiarato dall'autore.
    pub subject: String,
    /// I path dei file che ha toccato (relativi alla radice del repository).
    pub changed_files: Vec<String>,
}

impl Commit {
    /// Costruttore minimale: solo i file. `hash`/`subject` vuoti, `timestamp` 0.
    /// Sufficiente per il Campo di Astensione, che conta solo le co-occorrenze.
    pub fn new(changed_files: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            hash: String::new(),
            timestamp: 0,
            subject: String::new(),
            changed_files: changed_files.into_iter().map(Into::into).collect(),
        }
    }

    /// Costruttore completo: include i metadati richiesti dai Fossili di Decisione
    /// (hash, istante, messaggio di soggetto).
    pub fn with_meta(
        hash: impl Into<String>,
        timestamp: i64,
        subject: impl Into<String>,
        changed_files: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            hash: hash.into(),
            timestamp,
            subject: subject.into(),
            changed_files: changed_files.into_iter().map(Into::into).collect(),
        }
    }
}

/// Una sorgente di commit. `Send + Sync` perché la consumiamo da contesti async
/// (il Guardian) dietro a un `Arc`.
pub trait CommitHistory: Send + Sync {
    /// Tutti i commit rilevanti, dal più recente al più vecchio (l'ordine non
    /// conta per il conteggio delle occasioni).
    fn commits(&self) -> anyhow::Result<Vec<Commit>>;
}

/// Marcatore di inizio commit nell'output di `git log`. Sceglie una stringa che
/// non può comparire come prefisso di un path di file, così distinguere l'header
/// del commit dalle righe dei file è banale e robusto.
const COMMIT_MARKER: &str = ">>>>codeos-commit:";

/// Separatore di campo dentro l'header di un commit (`hash`, `timestamp`,
/// `subject`). È il byte ASCII *unit separator* (`0x1f`): non compare mai in un
/// hash, in un timestamp numerico o in una riga di soggetto, quindi spaccare
/// l'header è banale e non ambiguo.
const FIELD_SEP: char = '\u{1f}';

/// Legge la storia invocando il binario `git` di sistema.
///
/// DECISION: nessuna dipendenza nativa `libgit2`/`git2` (che richiederebbe link a
/// C e gonfierebbe i tempi di build). `git` è già un prerequisito di qualsiasi
/// codebase su cui CodeOS gira; lo shell-out costa zero e tiene il crate leggero.
/// Usiamo `--no-merges` (i merge non sono decisioni di astensione: non scrivono
/// codice nuovo) e `--name-only` per i path.
pub struct GitLog {
    repo_root: PathBuf,
    /// Limite opzionale al numero di commit (i più recenti). `None` = tutta la storia.
    max_commits: Option<usize>,
}

impl GitLog {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            max_commits: None,
        }
    }

    /// Limita l'analisi agli ultimi `n` commit (storia recente = preferenze attuali del team).
    pub fn with_max_commits(mut self, n: usize) -> Self {
        self.max_commits = Some(n);
        self
    }
}

impl CommitHistory for GitLog {
    fn commits(&self) -> anyhow::Result<Vec<Commit>> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo_root)
            .arg("log")
            .arg("--no-merges")
            .arg("--name-only")
            .arg(format!(
                "--pretty=format:{COMMIT_MARKER}%H{FIELD_SEP}%ct{FIELD_SEP}%s"
            ));
        if let Some(n) = self.max_commits {
            cmd.arg(format!("-n{n}"));
        }

        let output = cmd.output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("`git log` fallito ({}): {}", output.status, stderr.trim());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut commits = parse_git_log(&stdout);

        // Rileva la radice effettiva del repository git programmando `git rev-parse --show-toplevel`.
        // Se fallisce, usiamo `self.repo_root` come fallback. Questo risolve il mismatch
        // di percorso se la cartella di lavoro è una sottocartella del repository git.
        let git_root = match Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
        {
            Ok(out) if out.status.success() => {
                let path_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
                PathBuf::from(path_str)
            }
            _ => self.repo_root.clone(),
        };

        // `git log --name-only` riporta i path RELATIVI alla radice del repo, ma il
        // grafo indicizza i file con path ASSOLUTI. Assolutizziamo ogni path unendolo alla git_root.
        for commit in &mut commits {
            for file in &mut commit.changed_files {
                *file = git_root.join(&*file).to_string_lossy().into_owned();
            }
        }
        Ok(commits)
    }
}

/// Trasforma l'output di
/// `git log --name-only --pretty=format:{MARKER}%H{SEP}%ct{SEP}%s` in commit.
/// Ogni riga col marcatore apre un nuovo commit (header = hash, timestamp,
/// soggetto separati da [`FIELD_SEP`]); le righe non vuote successive sono i suoi
/// file. Funzione pura: il parsing è testabile senza git.
fn parse_git_log(stdout: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    let mut current: Option<Commit> = None;
    for line in stdout.lines() {
        if let Some(header) = line.strip_prefix(COMMIT_MARKER) {
            if let Some(c) = current.take() {
                commits.push(c);
            }
            // Header: `hash{SEP}timestamp{SEP}subject`. Formati legacy senza i
            // separatori degradano con grazia (hash = tutto, timestamp 0).
            let mut fields = header.splitn(3, FIELD_SEP);
            let hash = fields.next().unwrap_or("").to_string();
            let timestamp = fields
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let subject = fields.next().unwrap_or("").to_string();
            current = Some(Commit {
                hash,
                timestamp,
                subject,
                changed_files: Vec::new(),
            });
        } else if !line.trim().is_empty() {
            if let Some(c) = current.as_mut() {
                c.changed_files.push(line.to_string());
            }
        }
    }
    if let Some(c) = current.take() {
        commits.push(c);
    }
    commits
}

/// Legge il commit di `HEAD` del repository in `repo_root`, se esiste.
///
/// Serve a **timbrare la nascita** dei nodi del grafo temporale (vision step 2):
/// chi indicizza chiede a questo lettore «su quale commit sto guardando il
/// codice?» e passa l'hash+istante al `GraphResolver`.
///
/// **Best-effort e onesto.** Restituisce `None` — *mai* un hash o un istante
/// inventato — se:
/// - `repo_root` non è un repository git;
/// - `git` non è installato (lo spawn fallisce);
/// - `HEAD` non esiste ancora (repo appena inizializzato, zero commit).
///
/// È la tesi anti-falso-positivo applicata al tempo: **meglio una nascita mancante
/// che una che mente**. Un fallimento qui non deve mai rompere l'indicizzazione —
/// il timbro temporale è metadato opzionale, non un prerequisito. Per questo NON
/// è un `Result`: l'unico esito possibile è «ho un commit» o «non ce l'ho».
///
/// Niente `--no-merges` (a differenza di [`GitLog::commits`]): vogliamo *esattamente*
/// `HEAD`, anche se è un commit di merge. Niente `--name-only`: per il timbro non
/// servono i file toccati, quindi `changed_files` resta vuoto.
pub fn head_commit(repo_root: impl AsRef<Path>) -> Option<Commit> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root.as_ref())
        .arg("log")
        .arg("-1")
        .arg(format!(
            "--pretty=format:{COMMIT_MARKER}%H{FIELD_SEP}%ct{FIELD_SEP}%s"
        ))
        .output()
        .ok()?;
    if !output.status.success() {
        // `not a git repository`, `HEAD` non nato, ecc. → nessun timbro, non un errore.
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // `-1` produce esattamente un commit (o zero righe se HEAD non esiste).
    // Riusiamo il parser puro così la forma dell'header è una sola in tutto il crate.
    parse_git_log(&stdout).into_iter().next()
}

/// Sorgente in memoria: la storia è data esplicitamente. Serve ai test (e a
/// chiunque voglia alimentare l'analisi da un'altra origine, es. una API di host git).
pub struct InMemoryHistory {
    commits: Vec<Commit>,
}

impl InMemoryHistory {
    pub fn new(commits: Vec<Commit>) -> Self {
        Self { commits }
    }
}

impl CommitHistory for InMemoryHistory {
    fn commits(&self) -> anyhow::Result<Vec<Commit>> {
        Ok(self.commits.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commits_and_their_files() {
        let raw = "\
>>>>codeos-commit:aaa111
app/api/handler.py
app/core/service.py

>>>>codeos-commit:bbb222
app/api/handler.py
";
        let commits = parse_git_log(raw);
        assert_eq!(commits.len(), 2);
        assert_eq!(
            commits[0].changed_files,
            vec!["app/api/handler.py", "app/core/service.py"]
        );
        assert_eq!(commits[1].changed_files, vec!["app/api/handler.py"]);
    }

    #[test]
    fn ignores_leading_and_blank_lines() {
        // Un commit senza file (es. commit vuoto) non rompe il parsing.
        let raw = ">>>>codeos-commit:aaa\n\n>>>>codeos-commit:bbb\nfile.py\n";
        let commits = parse_git_log(raw);
        assert_eq!(commits.len(), 2);
        assert!(commits[0].changed_files.is_empty());
        assert_eq!(commits[1].changed_files, vec!["file.py"]);
    }

    #[test]
    fn empty_log_yields_no_commits() {
        assert!(parse_git_log("").is_empty());
    }

    /// Esegue un comando git nel repo di test, fallendo con un messaggio chiaro
    /// se git non è disponibile o il comando non riesce (così un ambiente senza
    /// git dà un errore leggibile invece di un panic opaco).
    fn run_git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
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

    /// Inizializza un repo git ermetico e isolato dalla config globale dell'utente
    /// (identità locale, niente firma gpg): un commit di test non deve dipendere
    /// dall'ambiente né fallire per una firma mancante.
    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q"]);
        run_git(dir, &["config", "user.email", "test@codeos.local"]);
        run_git(dir, &["config", "user.name", "CodeOS Test"]);
        run_git(dir, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn head_commit_reads_real_repo() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "ciao").unwrap();
        run_git(tmp.path(), &["add", "a.txt"]);
        run_git(tmp.path(), &["commit", "-q", "-m", "primo commit"]);

        let head = head_commit(tmp.path()).expect("HEAD esiste dopo un commit");
        // Hash reale (40 hex per sha1), istante reale (>0), soggetto reale.
        assert_eq!(head.hash.len(), 40, "hash completo, non inventato");
        assert!(head.hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(head.timestamp > 0, "istante reale del commit, non 0/finto");
        assert_eq!(head.subject, "primo commit");
    }

    #[test]
    fn head_commit_is_none_outside_a_repo() {
        // Cartella che esiste ma NON è un repo git → nessun timbro, non un errore.
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            head_commit(tmp.path()).is_none(),
            "fuori da un repo non si inventa una nascita"
        );
    }

    #[test]
    fn head_commit_is_none_on_unborn_head() {
        // Repo inizializzato ma SENZA commit: HEAD non esiste ancora.
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        assert!(
            head_commit(tmp.path()).is_none(),
            "HEAD non nato ⇒ None, mai un hash finto"
        );
    }

    #[test]
    fn parses_commit_metadata_for_fossils() {
        // Header reale: hash{SEP}timestamp{SEP}subject. Il soggetto può contenere
        // spazi e due-punti; solo il separatore di unità lo delimita.
        let raw = format!(
            "{COMMIT_MARKER}abc123{FIELD_SEP}1700000000{FIELD_SEP}introduce api layer over core\n\
             app/api/h.py\napp/core/s.py\n"
        );
        let commits = parse_git_log(&raw);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].hash, "abc123");
        assert_eq!(commits[0].timestamp, 1_700_000_000);
        assert_eq!(commits[0].subject, "introduce api layer over core");
        assert_eq!(
            commits[0].changed_files,
            vec!["app/api/h.py", "app/core/s.py"]
        );
    }
}
