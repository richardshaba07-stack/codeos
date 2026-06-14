//! [`Guardian`]: il lato storage del sistema immunitario.
//!
//! Legge il grafo persistito tramite il trait [`GraphStorage`], costruisce la
//! mappa entità → layer e delega a [`crate::invariant`] sia la scoperta delle
//! regole sia il rilevamento delle violazioni.
//!
//! DECISION: per ottenere la mappa entità → layer **non** estendiamo il trait
//! `GraphStorage` con un `get_all_entities()`. Ci servono solo le entità che
//! partecipano a una relazione (le uniche che contano per il layering): le
//! ricaviamo dagli estremi delle relazioni e le carichiamo con
//! `get_entity_by_id`. Così la feature resta confinata in questo crate e il
//! trait di persistenza non cambia.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Risolve la root del repository git in modo INDIPENDENTE dalla working dir del
/// client/server: prima `CODEOS_REPO` (la sorgente di verità con cui il server è
/// configurato), poi la risalita dell'albero fino a una directory con `.git`,
/// infine la CWD come ultima spiaggia. Risolve il limite operativo per cui `mri`
/// falliva se il server non girava ESATTAMENTE nella root del repo (collaudo).
fn resolve_repo_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    repo_root_from(std::env::var("CODEOS_REPO").ok(), &cwd)
}

/// Tetto di elementi per lista nella risposta di `guard --after`. Nel collaudo
/// DeepSpeed la risposta era 6,5 MB (dump di ~72k relazioni candidate) e sforava il
/// limite gRPC di 4 MB → `OutOfRange`, comando fallito. Tronchiamo le liste lunghe
/// con un conteggio del residuo — mai in silenzio, l'onestà del passo Compress.
const MAX_GUARD_ITEMS: usize = 3000;

/// Il **profilo temporale del rischio** di UN invariante (Guardian 2.0): da quanto il
/// confine non è esercitato. Un invariante CONFIDENTE (alta confidenza Wilson) ma
/// STANTIO (ultimo esercizio molto vecchio rispetto a HEAD) è "battle-tested ma forse
/// non più attivamente mantenuto" — la dimensione TEMPORALE che ESTENDE il Campo di
/// Astensione senza toccare il lower bound di Wilson (trap #2): il confine resta
/// confidente, ma il tempo qualifica quanto è "fresco".
#[derive(Debug, Clone, PartialEq)]
pub struct RuleStaleness {
    pub upstream: String,
    pub downstream: String,
    /// La confidenza (Wilson) dell'invariante — NON modificata dal tempo.
    pub confidence: f32,
    /// Timestamp Unix dell'ULTIMA occasione (ultima volta esercitato il confine).
    pub last_exercised_unix: i64,
    /// Secondi tra l'ultimo esercizio e il commit più recente (HEAD). Più alto ⇒ più
    /// stantio: l'esposizione c'è stata, ma non di recente.
    pub staleness_secs: i64,
}

/// Tronca `items` a `max` aggiungendo una riga di nota col residuo (mai nascosto),
/// per non sforare il limite di trasporto gRPC di 4 MB.
fn truncate_with_note(mut items: Vec<String>, max: usize, what: &str) -> Vec<String> {
    let extra = items.len().saturating_sub(max);
    if extra > 0 {
        items.truncate(max);
        items.push(format!(
            "(+{extra} {what} non mostrati: troncati per il limite di trasporto gRPC di 4 MB)"
        ));
    }
    items
}

/// Versione PURA (testabile senza toccare l'ambiente) di [`resolve_repo_root`].
fn repo_root_from(env_repo: Option<String>, cwd: &Path) -> PathBuf {
    if let Some(repo) = env_repo {
        if !repo.trim().is_empty() {
            return PathBuf::from(repo);
        }
    }
    let mut dir = cwd;
    loop {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd.to_path_buf(),
        }
    }
}

/// `true` se `ref_name` esiste nel repo (branch/tag/commit). Sonda economica via
/// `git rev-parse --verify --quiet`.
fn git_ref_exists(repo_dir: &Path, ref_name: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{ref_name}^{{commit}}"))
        .output()
        .map(|o| o.status.success())
        .is_ok_and(|ok| ok)
}

/// Il branch di DEFAULT del repo, rilevato (mai indovinato): prima il default
/// dichiarato dal remote (`origin/HEAD`), poi i nomi convenzionali che ESISTONO
/// (`main`, `master`). `None` se nessuno esiste — il chiamante decide come
/// fallire, onestamente.
fn default_base_ref(repo_dir: &Path) -> Option<String> {
    if let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .output()
    {
        if out.status.success() {
            let full = String::from_utf8_lossy(&out.stdout);
            if let Some(name) = full.trim().rsplit('/').next() {
                if !name.is_empty() && git_ref_exists(repo_dir, name) {
                    return Some(name.to_string());
                }
            }
        }
    }
    ["main", "master"]
        .into_iter()
        .find(|c| git_ref_exists(repo_dir, c))
        .map(str::to_string)
}

/// Righe AGGIUNTE (lato `head`) di un file, dal diff unificato a contesto 0.
/// `git diff -U0 base..head -- file` → parsing degli header di hunk `@@ -a,b +c,d @@`:
/// il lato `+c,d` dà l'intervallo `[c, c+d-1]` di righe nuove a `head`. Serve a `pr_mri`
/// per distinguere il codice che il PR ha davvero AGGIUNTO/cambiato dal resto del file.
/// `None` se git non è eseguibile o il diff fallisce → il chiamante fa fallback onesto
/// a tutte le entità del file (meglio sovra-riportare che dichiarare 0 a torto).
fn added_line_ranges(
    repo_dir: &Path,
    base: &str,
    head: &str,
    rel_file: &str,
) -> Option<Vec<(u32, u32)>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .arg("diff")
        .arg("-U0")
        .arg(format!("{base}..{head}"))
        .arg("--")
        .arg(rel_file)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_added_ranges(&String::from_utf8_lossy(&output.stdout)))
}

/// Parsing PURO degli header di hunk di un diff unificato → intervalli `[start, end]`
/// delle righe aggiunte sul lato `+`. Testabile senza git.
fn parse_added_ranges(diff_text: &str) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    for line in diff_text.lines() {
        // Header di hunk: "@@ -a,b +c,d @@ …". Estrae il lato "+c,d" (`d` assente ⇒ 1).
        let Some(after) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some(plus) = after.split('+').nth(1) else {
            continue;
        };
        let spec = plus.split([' ', '@']).next().unwrap_or("");
        let mut parts = spec.split(',');
        let Some(start) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let count = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);
        if count == 0 {
            continue; // hunk di pura cancellazione: 0 righe aggiunte a `head`
        }
        ranges.push((start, start + count - 1));
    }
    ranges
}

use codeos_memory::{Decision, DecisionKind, DecisionStore, Evidence, Proposal};
use codeos_paleo::{
    excavate, occasion_window, occasions, Abstention, CommitHistory, DecisionFossil, Z_95,
};
use codeos_storage::{GraphStorage, RelationFilter};
use codeos_types::bus::{ArchitectureViolation, NewDecision};
use codeos_types::{Entity, EntityId, EntityKind, Relation};

use crate::invariant::{
    boundary_entities, layer_of_entity, mine_layering_candidates, mine_layering_rules,
    support_edges, violations_for, LayerConfig, LayerKey, LayeringCandidate, LayeringRule,
};
use crate::meta::{mine_missing_invariants, MetaConfig, MissingInvariant};

/// Prefisso del tag che identifica in modo stabile l'invariante (a prescindere
/// dall'`id`, rigenerato a ogni mining): è la chiave del ciclo di vita di una
/// regola in memoria. Forma: `layering-invariant:{upstream}|{downstream}`.
const RULE_TAG_PREFIX: &str = "layering-invariant:";

/// Quanti commit al massimo nella STORIA del confine di `why` (i più recenti):
/// abbastanza per leggere l'evoluzione, non un `git log` intero.
const MAX_STORY_COMMITS: usize = 5;

/// Quante decisioni al massimo nel context pack per l'agente: un riassunto, non il
/// ledger (che resta consultabile per intero con `why`). Le UMANE hanno priorità.
const MAX_PACK_DECISIONS: usize = 4;

/// Una voce del "registro" degli invarianti attualmente in vigore, **derivata dal
/// ledger** (le decisioni correnti), non da un tag di stato scritto a mano.
struct ActiveInvariant {
    /// La `Decision` che ha promosso l'invariante (la referenzieremo nel ritiro).
    promotion_id: EntityId,
    /// Le entità di confine già agganciate alla promozione: le riportiamo sul
    /// ritiro, così la stessa query che mostrava la regola ne mostra anche la fine.
    related_entity_ids: Vec<EntityId>,
}

/// Il custode dell'architettura, agganciato a uno storage del grafo.
pub struct Guardian {
    storage: Arc<dyn GraphStorage>,
    /// Memoria storica opzionale: se presente, gli invarianti scoperti vengono
    /// **promossi** a `Decision` (kind `ArchitectureRule`) interrogabili dal Query.
    decisions: Option<Arc<dyn DecisionStore>>,
    /// Storia git opzionale: se presente, abilita la confidenza **calibrata sul
    /// tempo** (il Campo di Astensione) in [`mine_rules_calibrated`](Guardian::mine_rules_calibrated).
    history: Option<Arc<dyn CommitHistory>>,
    config: LayerConfig,
    /// Configurazione dello *spazio negativo del secondo ordine*
    /// ([`missing_invariants`](Guardian::missing_invariants)).
    meta_config: MetaConfig,
    /// Regole di layering **dichiarate a mano** dall'umano (`.codeos/config.yaml`).
    /// Si fondono con quelle scoperte: valgono per decreto (confidenza 1.0) anche
    /// dove il grafo non ha evidenza strutturale. Vuoto = nessuna config.
    declared: Vec<LayeringRule>,
}

impl Guardian {
    /// Crea un Guardian con la configurazione di layering di default.
    pub fn new(storage: Arc<dyn GraphStorage>) -> Self {
        Self {
            storage,
            decisions: None,
            history: None,
            config: LayerConfig::default(),
            meta_config: MetaConfig::default(),
            declared: Vec::new(),
        }
    }

    /// Crea un Guardian con una configurazione esplicita.
    pub fn with_config(storage: Arc<dyn GraphStorage>, config: LayerConfig) -> Self {
        Self {
            storage,
            decisions: None,
            history: None,
            config,
            meta_config: MetaConfig::default(),
            declared: Vec::new(),
        }
    }

    /// Aggancia il Memory Engine: gli invarianti scoperti da [`learn`](Self::learn)
    /// diventano memoria storica persistente.
    pub fn with_memory(storage: Arc<dyn GraphStorage>, decisions: Arc<dyn DecisionStore>) -> Self {
        Self {
            storage,
            decisions: Some(decisions),
            history: None,
            config: LayerConfig::default(),
            meta_config: MetaConfig::default(),
            declared: Vec::new(),
        }
    }

    /// Aggancia le regole di layering **dichiarate** dall'umano (tipicamente caricate
    /// da `.codeos/config.yaml` via [`crate::load_declared_rules`]). Chainable su
    /// qualsiasi costruttore. Le regole dichiarate si fondono con quelle scoperte e
    /// vengono fatte rispettare anche senza supporto strutturale nel grafo.
    pub fn with_declared_rules(mut self, declared: Vec<LayeringRule>) -> Self {
        self.declared = declared;
        self
    }

    /// Fonde le regole **dichiarate** con quelle passate (scoperte), senza duplicare
    /// una coppia `(upstream, downstream)` già presente: se l'umano dichiara un
    /// confine che il grafo ha già dissotterrato, vince la versione scoperta (porta
    /// supporto ed evidenza). Le dichiarate aggiungono solo i confini *mancanti*.
    fn merge_declared(&self, mut rules: Vec<LayeringRule>) -> Vec<LayeringRule> {
        if self.declared.is_empty() {
            return rules;
        }
        let existing: HashSet<(LayerKey, LayerKey)> = rules
            .iter()
            .map(|r| (r.upstream.clone(), r.downstream.clone()))
            .collect();
        for rule in &self.declared {
            if !existing.contains(&(rule.upstream.clone(), rule.downstream.clone())) {
                rules.push(rule.clone());
            }
        }
        rules
    }

    /// Aggancia una sorgente di storia git (il Paleontologo). Abilita la
    /// confidenza calibrata sul tempo in
    /// [`mine_rules_calibrated`](Self::mine_rules_calibrated). Chainable su
    /// qualsiasi costruttore: `Guardian::new(storage).with_commit_history(git)`.
    pub fn with_commit_history(mut self, history: Arc<dyn CommitHistory>) -> Self {
        self.history = Some(history);
        self
    }

    /// `true` se una storia git è agganciata: la confidenza degli invarianti è
    /// allora ricalibrata dal Campo di Astensione, non solo strutturale. Permette a
    /// chi costruisce il referto di etichettare ogni confidenza come *calibrata*.
    pub fn has_history(&self) -> bool {
        self.history.is_some()
    }

    /// La **qualità del grafo** sottostante (roadmap P2-7): i contatori che dicono
    /// quanto fidarsi del referto (relazioni risolte vs irrisolte vs a bassa
    /// confidenza, entità esterne). Pura lettura: delega allo storage.
    pub async fn graph_quality(&self) -> anyhow::Result<codeos_types::bus::GraphQualityInfo> {
        self.storage.graph_quality().await
    }

    /// Scopre le regole di layering dall'**intero grafo persistito**.
    pub async fn mine_rules(&self) -> anyhow::Result<Vec<LayeringRule>> {
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let layer_map = self.build_layer_map(&relations).await?;
        let mined = mine_layering_rules(&relations, &layer_map, &self.config);
        Ok(self.merge_declared(mined))
    }

    /// **Lo stadio 1 del flusso (candidato → proposta → decisione).** Le asimmetrie
    /// che si stanno *formando* nell'intero grafo: stesso spazio negativo puro di un
    /// invariante, ma con supporto ancora nella banda
    /// `[DEFAULT_MIN_CANDIDATE_SUPPORT, min_support)`.
    ///
    /// **Sola lettura, derivata, mai persistita.** A differenza di
    /// [`learn`](Self::learn), non tocca il `DecisionStore`: un candidato è un
    /// segnale grezzo, non storia confermata, e il ledger custodisce solo ciò che è
    /// stato deciso. Le regole **dichiarate a mano** non vi compaiono: valgono già
    /// per decreto, non sono confini *in formazione*. È lo specchio anticipatore di
    /// [`mine_rules`](Self::mine_rules) — mostra cosa potrebbe diventare un
    /// invariante prima che lo diventi, senza fossilizzarlo (trap #3).
    pub async fn candidates(&self) -> anyhow::Result<Vec<LayeringCandidate>> {
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let layer_map = self.build_layer_map(&relations).await?;
        Ok(mine_layering_candidates(
            &relations,
            &layer_map,
            &self.config,
        ))
    }

    /// Come [`mine_rules`](Self::mine_rules), ma **calibra** la confidenza di ogni
    /// regola sul *negativo del tempo* — il **Campo di Astensione**.
    ///
    /// La confidenza strutturale euristica (`1 - 1/(support+1)`) non distingue un
    /// invariante sopravvissuto a mille occasioni di essere violato da una
    /// coincidenza di un grafo giovane. Qui la rimpiazziamo col **lower bound di
    /// Wilson** sul tasso di astensione: per ogni regola contiamo le *occasioni*
    /// (commit che hanno co-toccato i due layer, in cui qualcuno avrebbe potuto
    /// invertire la freccia) e, poiché una regola appena scoperta ha zero archi nel
    /// verso proibito, ogni occasione è un'**astensione**. Più esposizione senza
    /// violazione ⇒ più confidenza.
    ///
    /// Degrada con grazia: senza storia git agganciata
    /// ([`with_commit_history`](Self::with_commit_history)), o per le regole i cui
    /// layer non sono mai stati co-toccati (zero occasioni), la confidenza resta
    /// quella strutturale. L'assenza di evidenza temporale **non abbassa** una
    /// regola: la conferma solo quando l'evidenza c'è.
    pub async fn mine_rules_calibrated(&self) -> anyhow::Result<Vec<LayeringRule>> {
        let mut rules = self.mine_rules().await?;
        let Some(history) = self.history.clone() else {
            return Ok(rules);
        };

        // `git` è I/O bloccante: leggiamo la storia fuori dal runtime async.
        let commits = tokio::task::spawn_blocking(move || history.commits()).await??;

        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let file_layers = self.build_file_layers(&relations).await?;

        for rule in &mut rules {
            // Le regole dichiarate valgono per decreto: niente calibrazione sul
            // tempo, la confidenza resta 1.0 voluta dall'umano.
            if rule.origin == codeos_types::bus::RuleOrigin::Declared {
                continue;
            }
            let occ = occasions(&rule.upstream.0, &rule.downstream.0, &file_layers, &commits);
            if occ == 0 {
                continue; // nessuna evidenza temporale: tieni la confidenza strutturale.
            }
            // Regola appena scoperta ⇒ direzione proibita assente nel grafo ⇒ zero
            // violazioni nel tempo: ogni occasione è stata un'astensione.
            rule.confidence = Abstention::new(occ, 0).wilson_lower_bound(Z_95) as f32;
        }
        Ok(rules)
    }

    /// **Rischio temporale (Guardian 2.0):** per ogni invariante calibrato (non
    /// dichiarato), da quanto il confine non è esercitato — il profilo temporale del
    /// Campo di Astensione ([`RuleStaleness`]). Fa emergere gli invarianti
    /// *confidenti ma stantii*: alta confidenza Wilson, ma ultimo esercizio molto
    /// vecchio rispetto a HEAD.
    ///
    /// ESTENDE, non sostituisce (trap #2): la confidenza di Wilson resta intatta, il
    /// tempo la *qualifica*. Vuoto senza storia git, o per i confini i cui layer non
    /// si sono mai co-toccati (nessun'occasione ⇒ niente da datare). Ordinato dal più
    /// stantio.
    pub async fn invariant_staleness(&self) -> anyhow::Result<Vec<RuleStaleness>> {
        let rules = self.mine_rules_calibrated().await?;
        let Some(history) = self.history.clone() else {
            return Ok(Vec::new());
        };
        let commits = tokio::task::spawn_blocking(move || history.commits()).await??;
        // HEAD = il commit più recente (la storia è newest-first). Senza commit non c'è
        // un "adesso" rispetto a cui misurare la staleness.
        let Some(now) = commits.first().map(|c| c.timestamp) else {
            return Ok(Vec::new());
        };
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let file_layers = self.build_file_layers(&relations).await?;

        let mut out = Vec::new();
        for rule in &rules {
            // Le regole dichiarate valgono per decreto: niente analisi temporale.
            if rule.origin == codeos_types::bus::RuleOrigin::Declared {
                continue;
            }
            if let Some(window) =
                occasion_window(&rule.upstream.0, &rule.downstream.0, &file_layers, &commits)
            {
                out.push(RuleStaleness {
                    upstream: rule.upstream.0.clone(),
                    downstream: rule.downstream.0.clone(),
                    confidence: rule.confidence,
                    last_exercised_unix: window.last_ts,
                    staleness_secs: window.staleness_secs(now),
                });
            }
        }
        // Il più stantio in cima: è quello su cui il "confidente ma forse non più
        // mantenuto" pesa di più.
        out.sort_by(|a, b| {
            b.staleness_secs
                .cmp(&a.staleness_secs)
                .then_with(|| a.upstream.cmp(&b.upstream))
        });
        Ok(out)
    }

    /// **Fossili di Decisione.** Per ogni regola scoperta, scava nella storia git
    /// l'istante di *cristallizzazione* del confine: il commit più vecchio che ha
    /// co-toccato i due layer, con il diff strutturale di nascita e l'**intento**
    /// (il messaggio del commit, le parole di chi ha disegnato il confine).
    ///
    /// È il complemento del Campo di Astensione: là misuriamo *quanto* un
    /// invariante è battle-tested, qui recuperiamo *quando* e *perché* è nato —
    /// l'invariante dedotto dallo spazio negativo viene ancorato a un intento umano
    /// reale, datato e citabile.
    ///
    /// Vuoto senza storia git agganciata ([`with_commit_history`](Self::with_commit_history))
    /// o per le regole i cui layer non si sono mai co-toccati. Ordinato per istante
    /// di nascita (i confini più antichi per primi).
    pub async fn fossils(&self) -> anyhow::Result<Vec<DecisionFossil>> {
        let rules = self.mine_rules().await?;
        let by_key = self.excavate_fossils(&rules).await?;
        let mut out: Vec<DecisionFossil> = rules
            .iter()
            .filter_map(|rule| by_key.get(&rule_key(rule)).cloned())
            .collect();

        // Relativizzazione dei path di born_structure (roadmap P2-8)
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let file_layers = self.build_file_layers(&relations).await?;
        let common_prefix = find_common_prefix(file_layers.keys());
        for f in &mut out {
            f.make_paths_relative(&common_prefix);
        }

        out.sort_by(|a, b| {
            a.born_at_unix
                .cmp(&b.born_at_unix)
                .then_with(|| a.upstream.cmp(&b.upstream))
        });
        Ok(out)
    }

    /// Scava i fossili per un insieme di regole, indicizzati per [`rule_key`].
    /// Mappa vuota (no-op) senza storia git o senza regole. Centralizza la lettura
    /// bloccante di git (in `spawn_blocking`) e il ponte file → layer, così sia
    /// [`fossils`](Self::fossils) sia [`learn`](Self::learn) la condividono.
    async fn excavate_fossils(
        &self,
        rules: &[LayeringRule],
    ) -> anyhow::Result<HashMap<String, DecisionFossil>> {
        let Some(history) = self.history.clone() else {
            return Ok(HashMap::new());
        };
        if rules.is_empty() {
            return Ok(HashMap::new());
        }

        let commits = tokio::task::spawn_blocking(move || history.commits()).await??;
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let file_layers = self.build_file_layers(&relations).await?;

        let mut out = HashMap::new();
        for rule in rules {
            if let Some(fossil) =
                excavate(&rule.upstream.0, &rule.downstream.0, &file_layers, &commits)
            {
                out.insert(rule_key(rule), fossil);
            }
        }
        Ok(out)
    }

    /// Scava il Fossile di Decisione per UN confine ARBITRARIO (`upstream`↔`downstream`),
    /// non solo per gli invarianti scoperti: trova nella storia git il commit più
    /// vecchio che ha co-toccato i due layer, col suo intento (il messaggio). È ciò
    /// che de-inerta `why` — un agente con grep dovrebbe rifare quest'archeologia ogni
    /// volta; qui è pre-calcolata e ancorata al confine. `None` senza storia git o se i
    /// due layer non si sono MAI co-toccati (nessun confine ⇒ niente da spiegare, mai
    /// inventato).
    async fn excavate_boundary(
        &self,
        upstream: &str,
        downstream: &str,
    ) -> anyhow::Result<Option<DecisionFossil>> {
        let Some(history) = self.history.clone() else {
            return Ok(None);
        };
        let commits = tokio::task::spawn_blocking(move || history.commits()).await??;
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let file_layers = self.build_file_layers(&relations).await?;
        Ok(excavate(upstream, downstream, &file_layers, &commits))
    }

    /// La STORIA del confine (Crono-Semantic Mining su `why`): i commit più recenti
    /// che hanno co-toccato entrambi i lati, formattati «hash · data · soggetto».
    /// La nascita (il fossile) dice quando il confine è APPARSO — spesso un commit
    /// iniziale poco informativo; la storia dice come è stato ESERCITATO nel tempo,
    /// con l'intento dichiarato (verbatim) di ciascun commit. Vuota senza storia o
    /// senza occasioni: mai inventata (trap #1: niente razionali sintetizzati).
    async fn boundary_story_lines(
        &self,
        upstream: &str,
        downstream: &str,
    ) -> anyhow::Result<Vec<String>> {
        let Some(history) = self.history.clone() else {
            return Ok(Vec::new());
        };
        let commits = tokio::task::spawn_blocking(move || history.commits()).await??;
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let file_layers = self.build_file_layers(&relations).await?;
        let story = codeos_paleo::boundary_story(
            upstream,
            downstream,
            &file_layers,
            &commits,
            MAX_STORY_COMMITS,
        );
        Ok(story
            .iter()
            .map(|o| {
                let short: String = o.hash.chars().take(12).collect();
                let date = chrono::DateTime::from_timestamp(o.timestamp, 0)
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                format!("[{short}] {date} «{}»", o.subject)
            })
            .collect())
    }

    /// **Spazio negativo del secondo ordine.** Punta l'algoritmo dello spazio
    /// negativo un livello più in alto, sulla griglia *layer × layer*, e segnala gli
    /// invarianti che *mancano dove dovrebbero esserci*: un layer-fondazione,
    /// rispettato a senso unico da molti altri layer, che ha però **un'unica
    /// eccezione** accoppiata bidirezionalmente. Quel buco nella convenzione è quasi
    /// sempre debito tecnico o un bug latente — il candidato perfetto da far rivedere
    /// a un umano (o a un LLM).
    ///
    /// È diagnostica, non muta la memoria: l'assenza di un invariante è un
    /// *sospetto*, non un fatto da promuovere a `Decision`. Ordinato per quanto è
    /// isolata l'eccezione (`foundation_support` decrescente).
    pub async fn missing_invariants(&self) -> anyhow::Result<Vec<MissingInvariant>> {
        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let layer_map = self.build_layer_map(&relations).await?;
        let rules = mine_layering_rules(&relations, &layer_map, &self.config);
        Ok(mine_missing_invariants(
            &relations,
            &layer_map,
            &rules,
            &self.meta_config,
        ))
    }

    /// **Il ciclo di apprendimento.** Riconcilia la memoria storica con la verità
    /// architetturale corrente del grafo:
    ///
    /// - **promuove** gli invarianti appena scoperti a `Decision` (kind
    ///   `ArchitectureRule`), così il *perché* — finora implicito nello spazio
    ///   negativo — diventa esplicito e interrogabile dal Query;
    /// - **ritira** gli invarianti diventati *stale*: una regola un tempo valida che
    ///   il grafo non sostiene più (ora ci sono dipendenze in entrambi i versi).
    ///
    /// Tutto è **additivo**: non si cancella nulla (invariante della memoria). Un
    /// ritiro è una nuova `Decision` che referenzia la promozione via
    /// `related_decision_ids` — la storia resta leggibile, ma lo stato corrente è
    /// quello che conta. È **idempotente**: a parità di grafo, una seconda passata
    /// non produce nulla. No-op se nessuna memoria è agganciata. Restituisce gli
    /// `EntityId` delle decisioni create (promozioni + ritiri).
    pub async fn learn(&self) -> anyhow::Result<Vec<EntityId>> {
        let Some(store) = &self.decisions else {
            return Ok(Vec::new());
        };

        let relations = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        let layer_map = self.build_layer_map(&relations).await?;
        let rules = mine_layering_rules(&relations, &layer_map, &self.config);
        let current_keys: HashSet<String> = rules.iter().map(rule_key).collect();

        // Fossili di Decisione: se c'è una storia git, datiamo ogni confine alla
        // sua nascita per ancorare la promozione a un intento umano reale. Vuoto
        // (e a costo zero) senza storia: la promozione resta com'era.
        let fossils = self.excavate_fossils(&rules).await?;

        // Il registro degli invarianti ATTIVI è DERIVATO dal ledger, non ricostruito
        // ripercorrendo tag di stato scritti a mano: `current_decisions()` sono già le
        // decisioni `Accepted`, e una promozione che un ritiro ha `deprecated` diventa
        // `Deprecated` → cade da sé fuori da questo insieme (la stessa "verità di oggi"
        // che il Query Engine consuma). Tra le correnti, una regola in vigore è una
        // `ArchitectureRule` (il ritiro è una `Decision`, kind diverso) che porta la
        // propria chiave nel tag `RULE_TAG_PREFIX`. **Una sola fonte di verità:** lo
        // stato lo dice il ledger (supersedes/deprecates), mai un tag duplicato che
        // potrebbe divergere — coerente col principio di Slice A "lo stato si deriva,
        // non si memorizza".
        let mut active: HashMap<String, ActiveInvariant> = HashMap::new();
        for decision in store.current_decisions().await? {
            if decision.kind != DecisionKind::ArchitectureRule {
                continue;
            }
            let Some(key) = decision
                .tags
                .iter()
                .find(|t| t.starts_with(RULE_TAG_PREFIX))
                .cloned()
            else {
                continue;
            };
            active.insert(
                key,
                ActiveInvariant {
                    promotion_id: decision.id,
                    related_entity_ids: decision.related_entity_ids,
                },
            );
        }

        let mut changes = Vec::new();

        // (A) Promozione: gli invarianti scoperti ora e non ancora in registro.
        for rule in &rules {
            if active.contains_key(&rule_key(rule)) {
                continue;
            }
            let related = boundary_entities(rule, &relations, &layer_map);
            let fossil = fossils.get(&rule_key(rule));
            let evidence = promotion_evidence(rule, &relations, &layer_map, fossil);
            let decision = promotion_decision(rule, related, evidence, fossil);
            store.record(&decision).await?;
            tracing::info!(
                upstream = %rule.upstream,
                downstream = %rule.downstream,
                support = rule.support,
                "invariante di layering promosso a memoria storica"
            );
            changes.push(decision.id);
        }

        // (B) Ritiro: gli invarianti attivi che il grafo non sostiene più. Ordinati
        // per chiave così l'output (e i timestamp dei file Markdown) è deterministico.
        let mut stale: Vec<(String, ActiveInvariant)> = active
            .into_iter()
            .filter(|(key, _)| !current_keys.contains(key))
            .collect();
        stale.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, entry) in stale {
            let Some((upstream, downstream)) = parse_rule_key(&key) else {
                continue;
            };
            let decision = retraction_decision(
                upstream,
                downstream,
                entry.promotion_id,
                entry.related_entity_ids,
            );
            store.record(&decision).await?;
            tracing::info!(
                upstream,
                downstream,
                "invariante di layering ritirato: asimmetria non più valida"
            );
            changes.push(decision.id);
        }

        Ok(changes)
    }

    /// Verifica delle relazioni **candidate** (ipotetiche, o appena aggiunte)
    /// contro le regole scoperte dal grafo.
    ///
    /// Le candidate vengono **escluse** dall'insieme da cui si scoprono le regole:
    /// così una dipendenza vietata che fosse già stata persistita non finisce per
    /// "giustificare sé stessa" annullando lo spazio negativo che la rende
    /// illecita.
    pub async fn check(
        &self,
        candidates: &[Relation],
    ) -> anyhow::Result<Vec<ArchitectureViolation>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let candidate_ids: HashSet<EntityId> = candidates.iter().map(|r| r.id).collect();
        let mut established = self
            .storage
            .query_relations(RelationFilter::default())
            .await?;
        established.retain(|r| !candidate_ids.contains(&r.id));

        // La mappa dei layer deve coprire gli estremi sia delle relazioni
        // stabilite sia delle candidate.
        let mut endpoints = established.clone();
        endpoints.extend_from_slice(candidates);
        let layer_map = self.build_layer_map(&endpoints).await?;

        let mined = mine_layering_rules(&established, &layer_map, &self.config);
        let rules = self.merge_declared(mined);
        let mut violations = violations_for(candidates, &layer_map, &rules);

        // Arricchisci ogni violazione con la posizione dell'entità SORGENTE: è lì
        // che vive la dipendenza proibita, ed è dove un editor pianterà la
        // diagnostica. La funzione pura `violations_for` non vede le entità; qui
        // sì, via storage. Se l'entità non c'è, la posizione resta `None`.
        for violation in &mut violations {
            if let Some(entity) = self.storage.get_entity_by_id(&violation.source_id).await? {
                violation.location = Some(entity.location);
            }
        }
        Ok(violations)
    }

    /// Mappa ogni entità che compare come estremo di una relazione al suo layer.
    /// Gli `EntityId` nulli (target di `Unresolved`) e le entità non trovate sono
    /// semplicemente assenti dalla mappa: chi le interroga le ignora.
    ///
    /// Le **dipendenze esterne** (`tokio`, `std`, `react`…) sono volutamente
    /// **escluse** (P0-2): non sono layer *della tua* architettura, quindi un arco
    /// `crates::codeos-rpc → external::std` non deve diventare l'invariante assurdo
    /// "external::std non deve dipendere da codeos-rpc". Restano comunque nel grafo
    /// (interrogabili via query/impatto): qui le togliamo solo dal *ragionamento sui
    /// layer*. Essendo assenti dalla mappa, ogni arco che le tocca è ignorato in
    /// blocco da [`crate::invariant::cross_layer`] (mining, gap e violazioni).
    async fn build_layer_map(
        &self,
        relations: &[Relation],
    ) -> anyhow::Result<HashMap<EntityId, LayerKey>> {
        let mut ids: HashSet<EntityId> = HashSet::new();
        for rel in relations {
            if !rel.source_id.is_nil() {
                ids.insert(rel.source_id);
            }
            if !rel.target_id.is_nil() {
                ids.insert(rel.target_id);
            }
        }

        let mut map = HashMap::with_capacity(ids.len());
        for id in ids {
            if let Some(entity) = self.storage.get_entity_by_id(&id).await? {
                if entity.kind == EntityKind::ExternalDependency {
                    continue;
                }
                map.insert(id, layer_of_entity(&entity, self.config.layer_depth));
            }
        }
        Ok(map)
    }

    /// Mappa ogni **file** (il `file_path` dell'entità) ai layer che vi sono
    /// definiti. È il ponte tra il mondo del grafo (i layer derivano dai
    /// `qualified_name`) e il mondo di git (i commit toccano *file*): senza, non
    /// potremmo contare le occasioni di astensione. Un file può ospitare più layer
    /// (raro ma gestito con un set).
    async fn build_file_layers(
        &self,
        relations: &[Relation],
    ) -> anyhow::Result<HashMap<String, HashSet<String>>> {
        let mut ids: HashSet<EntityId> = HashSet::new();
        for rel in relations {
            if !rel.source_id.is_nil() {
                ids.insert(rel.source_id);
            }
            if !rel.target_id.is_nil() {
                ids.insert(rel.target_id);
            }
        }

        let mut map: HashMap<String, HashSet<String>> = HashMap::new();
        for id in ids {
            if let Some(entity) = self.storage.get_entity_by_id(&id).await? {
                // Coerente con build_layer_map: le esterne non sono layer e il loro
                // file fittizio `<external>` non è mai toccato da un commit git.
                if entity.kind == EntityKind::ExternalDependency {
                    continue;
                }
                let layer = layer_of_entity(&entity, self.config.layer_depth).0;
                map.entry(entity.location.file_path)
                    .or_default()
                    .insert(layer);
            }
        }
        Ok(map)
    }

    /// Rileva se la storia del repository è insufficiente per tracciare i confini in modo affidabile.
    pub async fn check_history_adequacy(&self, fossils: &[DecisionFossil]) -> anyhow::Result<bool> {
        let Some(history) = &self.history else {
            return Ok(false);
        };
        let history = history.clone();
        let commits = tokio::task::spawn_blocking(move || history.commits()).await??;
        Ok(codeos_paleo::is_history_insufficient(&commits, fossils))
    }

    /// Seleziona le entità bersaglio per un goal/query in linguaggio
    /// quasi-naturale. Estrae keyword (≥3 caratteri) e le cerca per sottostringa,
    /// ma SCARTA quelle non discriminative: una parola presente in più di metà dei
    /// qualified_name — i prefissi universali del namespace come "codeos", "src",
    /// "crates" — non localizza nulla, e trattarla come bersaglio gonfierebbe
    /// l'impatto all'intero grafo (un falso positivo). Le entità sono deduplicate
    /// per id. Condiviso da `guard_before` e `get_context_pack`.
    async fn select_target_entities(&self, goal: &str) -> Vec<Entity> {
        // Specificità IDF-like (stesso principio del query engine): una keyword RARA
        // (matcha poche entità) è un segnale forte; una comune è rumore. Peso ∝ 1/match.
        const SPEC_SCALE: u64 = 1_000_000;
        // Cap del pacchetto agent-facing: per un contesto destinato a un'AI la PRECISIONE
        // batte il recall — meglio le 24 entità più specifiche che un elenco di omonimi.
        const MAX_CONTEXT_ENTITIES: usize = 24;

        let mut keywords: Vec<String> = goal
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| s.len() >= 3)
            .map(|s| s.to_lowercase())
            .collect();
        // Il goal INTERO (normalizzato) come keyword aggiuntiva: un match sull'intera
        // frase (es. `select_top`) è molto più MIRATO dei singoli frammenti (`select`,
        // `top`) — quindi prende la specificità più alta e domina gli omonimi parziali.
        let whole = goal.trim().to_lowercase();
        if whole.len() >= 3 && !keywords.contains(&whole) {
            keywords.push(whole);
        }

        let total_entities = self
            .storage
            .graph_quality()
            .await
            .map(|q| q.total_entities)
            .unwrap_or(0);

        let mut by_id: HashMap<EntityId, Entity> = HashMap::new();
        let mut specificity: HashMap<EntityId, u64> = HashMap::new();
        for kw in &keywords {
            let Ok(ents) = self.storage.find_entities_by_name_pattern(kw).await else {
                continue;
            };
            // Soglia 50%: tiene i nomi di crate reali (pochi punti percentuali) e
            // scarta i prefissi universali che matcherebbero quasi tutto il grafo.
            if total_entities > 0 && (ents.len() as u64) * 2 > total_entities {
                continue;
            }
            if ents.is_empty() {
                continue;
            }
            let weight = (SPEC_SCALE / ents.len() as u64).max(1);
            for ent in ents {
                *specificity.entry(ent.id).or_insert(0) += weight;
                by_id.entry(ent.id).or_insert(ent);
            }
        }

        // Ordina per specificità decrescente (a parità, nome — deterministico) e cappa:
        // così il context pack è FOCALIZZATO sull'entità cercata, non annacquato dagli
        // omonimi di una keyword comune (prima il goal "select_top" tirava dentro
        // select_target_entities, split_top_level_commas, is_stopword…).
        let mut ranked: Vec<Entity> = by_id.into_values().collect();
        ranked.sort_by(|a, b| {
            let sa = specificity.get(&a.id).copied().unwrap_or(0);
            let sb = specificity.get(&b.id).copied().unwrap_or(0);
            sb.cmp(&sa)
                .then_with(|| a.qualified_name.cmp(&b.qualified_name))
        });
        ranked.truncate(MAX_CONTEXT_ENTITIES);
        ranked
    }

    pub async fn guard_before(
        &self,
        goal: &str,
    ) -> anyhow::Result<codeos_types::bus::GuardBeforeResponse> {
        let target_entities = self.select_target_entities(goal).await;

        let mut target_files: Vec<String> = target_entities
            .iter()
            .map(|e| e.location.file_path.clone())
            .collect();
        target_files.sort();
        target_files.dedup();

        let rules = self.mine_rules_calibrated().await.unwrap_or_default();
        let mut target_layers = HashSet::new();
        for ent in &target_entities {
            let layer = layer_of_entity(ent, self.config.layer_depth);
            target_layers.insert(layer.0);
        }

        let mut boundaries = Vec::new();
        for rule in &rules {
            if target_layers.contains(&rule.upstream.0)
                || target_layers.contains(&rule.downstream.0)
            {
                boundaries.push(format!(
                    "'{}' non deve dipendere da '{}' (confidenza: {:.2})",
                    rule.downstream.0, rule.upstream.0, rule.confidence
                ));
            }
        }

        // Blast radius = numero di ENTITÀ distinte che dipendono dal bersaglio
        // (le `source` delle relazioni entranti), non il numero di archi. Un'entità
        // che dipende dal bersaglio via più relazioni è un solo impatto. Il vecchio
        // codice deduplicava per id di relazione: contava gli archi e poteva
        // superare il totale delle entità del grafo — un numero che mente.
        let mut blast_radius = 0;
        let mut dependent_entities = HashSet::new();
        for ent in &target_entities {
            if let Ok(rels) = self
                .storage
                .query_relations(RelationFilter {
                    target_id: Some(ent.id),
                    ..Default::default()
                })
                .await
            {
                for rel in rels {
                    if dependent_entities.insert(rel.source_id) {
                        blast_radius += 1;
                    }
                }
            }
        }

        // `target_entities` vuoto NON vuol dire «modifica sicura»: vuol dire che
        // non siamo riusciti a localizzare il goal nel grafo (nessun simbolo
        // corrispondente, oppure solo parole troppo generiche, scartate perché
        // matchavano mezzo grafo). Dirlo a voce alta è meglio di un blast radius 0
        // rassicurante che mente — un arco mancante è meglio di uno che inganna.
        let unlocalized = target_entities.is_empty();
        let safe_path = if unlocalized {
            "Goal non localizzato: nessun simbolo corrispondente nel grafo \
             (o solo parole troppo generiche, scartate). Blast radius 0 qui NON \
             significa «modifica sicura», significa «non lo so». Riprova nominando \
             un modulo o una funzione specifici."
                .to_string()
        } else {
            format!(
                "Per modificare i file in sicurezza, mantieni separati i layer: {}",
                target_layers.into_iter().collect::<Vec<_>>().join(", ")
            )
        };

        let mut context_pack = format!(
            "# AI Architecture Firewall - Guard Before\n\n**Goal:** \"{}\"\n\n",
            goal
        );
        if unlocalized {
            context_pack.push_str(
                "> ⚠️ **Goal non localizzato**: nessun simbolo nel grafo corrisponde \
                 a questo goal (o le parole erano troppo generiche e sono state \
                 scartate). Blast radius 0 qui significa «non lo so», non «modifica \
                 sicura». Rinomina il goal con un modulo o una funzione concreti.\n\n",
            );
        }
        context_pack.push_str("## Target Files a rischio:\n");
        for f in &target_files {
            context_pack.push_str(&format!("- {}\n", f));
        }
        context_pack.push_str("\n## Confini architetturali da preservare:\n");
        for b in &boundaries {
            context_pack.push_str(&format!("- {}\n", b));
        }
        context_pack.push_str(&format!(
            "\n**Raggio d'impatto (Blast Radius):** {} entità dipendenti.\n",
            blast_radius
        ));

        Ok(codeos_types::bus::GuardBeforeResponse {
            target_files,
            boundaries,
            blast_radius,
            safe_path,
            context_pack,
        })
    }

    pub async fn guard_after(&self) -> anyhow::Result<codeos_types::bus::GuardAfterResponse> {
        let mut latest_relations = Vec::new();
        if let Some(history) = &self.history {
            if let Ok(commits) = history.commits() {
                if let Some(latest_commit) = commits.first() {
                    for file_path in &latest_commit.changed_files {
                        if let Ok(entities) = self.storage.get_entities_by_file(file_path).await {
                            for ent in entities {
                                if let Ok(rels) = self
                                    .storage
                                    .query_relations(RelationFilter {
                                        source_id: Some(ent.id),
                                        ..Default::default()
                                    })
                                    .await
                                {
                                    latest_relations.extend(rels);
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut candidate_relations = latest_relations;
        if candidate_relations.is_empty() {
            if let Ok(all_rels) = self
                .storage
                .query_relations(RelationFilter::default())
                .await
            {
                candidate_relations = all_rels;
            }
        }

        let mut violations = self.check(&candidate_relations).await.unwrap_or_default();

        let mut new_relations = Vec::new();
        for rel in &candidate_relations {
            if let (Ok(Some(src)), Ok(Some(tgt))) = (
                self.storage.get_entity_by_id(&rel.source_id).await,
                self.storage.get_entity_by_id(&rel.target_id).await,
            ) {
                new_relations.push(format!(
                    "'{}' -> '{}' ({:?})",
                    src.qualified_name, tgt.qualified_name, rel.kind
                ));
            }
        }

        let mut proposed_fixes = Vec::new();
        for vio in &violations {
            if let Some(loc) = &vio.location {
                proposed_fixes.push(format!(
                    "Riferimento illegale in {}:{}. Dettaglio: {}",
                    loc.file_path, loc.start_line, vio.message
                ));
            } else {
                proposed_fixes.push(format!("Riferimento illegale. Dettaglio: {}", vio.message));
            }
        }

        // Troncamento onesto per non sforare il limite gRPC di 4 MB (collaudo
        // DeepSpeed: 6,5 MB). Le violazioni (il segnale) e i fix in parallelo; il dump
        // informativo delle relazioni candidate separatamente. Il residuo è sempre
        // CONTATO, mai nascosto.
        let vio_extra = violations.len().saturating_sub(MAX_GUARD_ITEMS);
        violations.truncate(MAX_GUARD_ITEMS);
        proposed_fixes.truncate(MAX_GUARD_ITEMS);
        if vio_extra > 0 {
            proposed_fixes.push(format!(
                "(+{vio_extra} violazioni non mostrate: troncate per il limite di trasporto gRPC di 4 MB)"
            ));
        }
        let new_relations = truncate_with_note(new_relations, MAX_GUARD_ITEMS, "relazioni");

        Ok(codeos_types::bus::GuardAfterResponse {
            new_relations,
            violations,
            proposed_fixes,
        })
    }

    pub async fn get_context_pack(
        &self,
        goal: &str,
        for_ai: bool,
    ) -> anyhow::Result<codeos_types::bus::GetContextPackResponse> {
        let target_entities = self.select_target_entities(goal).await;
        // Goal non localizzato: nessuna entità del grafo corrisponde. blast_count
        // sarà 0 e il rischio scivolerebbe a "low" — una falsa rassicurazione
        // servita dritta a un'AI. "low" qui significherebbe «non valutabile», non
        // «sicuro»: meglio dichiararlo «unknown» e dirlo a chiare lettere.
        let unlocalized = target_entities.is_empty();

        // Preserva l'ORDINE di specificità (da `select_target_entities`): l'entità più
        // mirata — e il suo file — vanno in CIMA, così l'AI legge per prima la cosa più
        // rilevante. Dedup che CONSERVA l'ordine (un sort alfabetico distruggerebbe il
        // ranking, riportando il rumore in testa).
        let mut files_to_read: Vec<String> = Vec::new();
        for e in &target_entities {
            let f = e.location.file_path.clone();
            if !files_to_read.contains(&f) {
                files_to_read.push(f);
            }
        }

        let mut relevant_entities: Vec<String> = Vec::new();
        for e in &target_entities {
            let q = e.qualified_name.clone();
            if !relevant_entities.contains(&q) {
                relevant_entities.push(q);
            }
        }

        let mut key_dependencies = Vec::new();
        let selected_ids: HashSet<EntityId> = target_entities.iter().map(|e| e.id).collect();
        for ent in &target_entities {
            if let Ok(rels) = self
                .storage
                .query_relations(RelationFilter {
                    source_id: Some(ent.id),
                    ..Default::default()
                })
                .await
            {
                for rel in rels {
                    if selected_ids.contains(&rel.target_id) {
                        if let (Ok(Some(src)), Ok(Some(tgt))) = (
                            self.storage.get_entity_by_id(&rel.source_id).await,
                            self.storage.get_entity_by_id(&rel.target_id).await,
                        ) {
                            key_dependencies.push(format!(
                                "{} -> {} ({:?})",
                                src.qualified_name, tgt.qualified_name, rel.kind
                            ));
                        }
                    }
                }
            }
        }

        let rules = self.mine_rules_calibrated().await.unwrap_or_default();
        let mut target_layers = HashSet::new();
        for ent in &target_entities {
            let layer = layer_of_entity(ent, self.config.layer_depth);
            target_layers.insert(layer.0);
        }

        // Il *perché* per l'agente: le decisioni del ledger agganciate al goal — per
        // ID (invarianti auto-promossi) o per tag=SEGMENTO `::` esatto del qualname
        // (le decisioni UMANE registrate con `decide`, taggate coi nomi). Stesso
        // matching anti-flood del Query Engine; UMANE prime (il non-derivabile), cap
        // stretto: il pack è un riassunto, il ledger completo resta su `why`. Senza
        // questo blocco il pack non portava MAI l'intento: l'agente vedeva la
        // struttura ma non il perché — proprio lo strato che distingue CodeOS.
        let mut pack_decisions: Vec<String> = Vec::new();
        if let Some(store) = &self.decisions {
            if let Ok(current) = store.current_decisions().await {
                // Le decisioni UMANE pertinenti — punto UNICO di verità condiviso
                // col context pack di codeos-query (codeos_memory::select_human_decisions).
                // SOLO `Decision` (gli invarianti auto-derivati sono già nelle
                // BOUNDARIES: ripeterli nel WHY costa token all'agente).
                let kept = codeos_memory::select_human_decisions(
                    current,
                    &target_entities,
                    MAX_PACK_DECISIONS,
                );
                pack_decisions = kept
                    .iter()
                    .map(|d| {
                        let rationale = d.rationale.trim();
                        if rationale.is_empty() {
                            format!("{} (autore: {})", d.title, d.author)
                        } else {
                            format!("{}: {} (autore: {})", d.title, rationale, d.author)
                        }
                    })
                    .collect();
            }
        }

        let mut boundaries_to_preserve = Vec::new();
        for rule in &rules {
            if target_layers.contains(&rule.upstream.0)
                || target_layers.contains(&rule.downstream.0)
            {
                boundaries_to_preserve.push(format!(
                    "'{}' non deve dipendere da '{}' (confidenza: {:.2})",
                    rule.downstream.0, rule.upstream.0, rule.confidence
                ));
            }
        }

        let mut local_patterns = Vec::new();
        for rule in rules.iter().take(3) {
            local_patterns.push(format!(
                "Convenzione: '{}' dipende da '{}' a senso unico.",
                rule.downstream.0, rule.upstream.0
            ));
        }
        if local_patterns.is_empty() {
            local_patterns.push("Nessun pattern strutturale specifico rilevato.".to_string());
        }

        let mut suggested_tests = Vec::new();
        for ent in &target_entities {
            if let Ok(rels) = self
                .storage
                .query_relations(RelationFilter {
                    target_id: Some(ent.id),
                    kind: Some(codeos_types::RelationKind::Tests),
                    ..Default::default()
                })
                .await
            {
                for rel in rels {
                    if let Ok(Some(test_ent)) = self.storage.get_entity_by_id(&rel.source_id).await
                    {
                        suggested_tests.push(format!(
                            "Esegui il test: {} ({})",
                            test_ent.qualified_name, test_ent.location.file_path
                        ));
                    }
                }
            }
        }
        if suggested_tests.is_empty() {
            suggested_tests
                .push("Scrivi nuovi test unitari per coprire le modifiche apportate.".to_string());
        }

        let mut blast_count = 0;
        for ent in &target_entities {
            if let Ok(rels) = self
                .storage
                .query_relations(RelationFilter {
                    target_id: Some(ent.id),
                    ..Default::default()
                })
                .await
            {
                blast_count += rels.len();
            }
        }
        let estimated_risk = if unlocalized {
            "unknown".to_string()
        } else if blast_count > 8 {
            "high".to_string()
        } else if blast_count > 2 {
            "medium".to_string()
        } else {
            "low".to_string()
        };

        let goal_interpretation = if unlocalized {
            format!(
                "Goal non localizzato: nessuna entità del grafo corrisponde a \"{}\" \
                 (simbolo inesistente o termini troppo generici, scartati). Il rischio \
                 «unknown» significa «non valutabile», non «sicuro»: rinomina il goal \
                 con un modulo o una funzione concreti.",
                goal
            )
        } else {
            format!(
                "Analisi e preparazione del contesto per raggiungere il goal: \"{}\"",
                goal
            )
        };

        // `--for ai` produce un formato COMPATTO (chiave:valore, niente prosa/emoji/
        // intestazioni numerate): meno token e più parsabile per un agente. Senza il
        // flag, il formato Markdown ricco e leggibile da umano. Prima il flag era
        // ignorato (output identico) — collaudo: `--for ai` no-op, ora differenziato.
        let markdown = if for_ai {
            let mut m = format!("GOAL: {}\nRISK: {}\n", goal, estimated_risk.to_uppercase());
            if unlocalized {
                m.push_str(
                    "UNLOCALIZED: nessuna entità del grafo corrisponde (rischio non valutabile, non «sicuro»)\n",
                );
            }
            m.push_str(&format!("GOAL_INTERP: {goal_interpretation}\n"));
            // Il perché PRIMA della struttura (come nel contesto di `query`): è il
            // contenuto non-derivabile, e un agente lo deve leggere per primo.
            if !pack_decisions.is_empty() {
                m.push_str(&format!("WHY: {}\n", pack_decisions.join(" | ")));
            }
            m.push_str(&format!("FILES: {}\n", files_to_read.join(", ")));
            m.push_str(&format!("ENTITIES: {}\n", relevant_entities.join(", ")));
            m.push_str(&format!("DEPS: {}\n", key_dependencies.join("; ")));
            m.push_str(&format!(
                "BOUNDARIES: {}\n",
                boundaries_to_preserve.join("; ")
            ));
            m.push_str(&format!("PATTERNS: {}\n", local_patterns.join("; ")));
            m.push_str(&format!("TESTS: {}\n", suggested_tests.join("; ")));
            m
        } else {
            let mut markdown = format!("# AI Context Pack - goal: \"{}\"\n\n", goal);
            markdown.push_str(&format!(
                "**Stima del rischio:** {}\n\n",
                estimated_risk.to_uppercase()
            ));
            if unlocalized {
                markdown.push_str(
                    "> ⚠️ **Goal non localizzato**: nessuna entità del grafo corrisponde a \
                     questo goal. Rischio «unknown» = «non valutabile», non «sicuro». Le \
                     sezioni qui sotto sono vuote perché non c'è nulla da ancorare: rinomina \
                     il goal con un modulo o una funzione concreti.\n\n",
                );
            }
            markdown.push_str("## 1. Interpretazione del Goal\n");
            markdown.push_str(&format!("{}\n\n", goal_interpretation));
            // Il perché in alto, come nel contesto di `query`: il non-derivabile
            // non si seppellisce in coda. Sezione assente se il ledger non ha nulla
            // di rilevante (niente rumore).
            if !pack_decisions.is_empty() {
                markdown.push_str("## Perché è fatto così (decisioni dal ledger)\n");
                for d in &pack_decisions {
                    markdown.push_str(&format!("- {}\n", d));
                }
                markdown.push('\n');
            }
            markdown.push_str("## 2. File da Leggere / Modificare\n");
            for f in &files_to_read {
                markdown.push_str(&format!("- {}\n", f));
            }
            markdown.push_str("\n## 3. Entità Rilevanti nel Contesto\n");
            for e in &relevant_entities {
                markdown.push_str(&format!("- {}\n", e));
            }
            markdown.push_str("\n## 4. Dipendenze Chiave\n");
            for dep in &key_dependencies {
                markdown.push_str(&format!("- {}\n", dep));
            }
            markdown.push_str("\n## 5. Confini da Preservare\n");
            for b in &boundaries_to_preserve {
                markdown.push_str(&format!("- {}\n", b));
            }
            markdown.push_str("\n## 6. Pattern Locali\n");
            for p in &local_patterns {
                markdown.push_str(&format!("- {}\n", p));
            }
            markdown.push_str("\n## 7. Test Suggeriti\n");
            for t in &suggested_tests {
                markdown.push_str(&format!("- {}\n", t));
            }
            markdown
        };

        Ok(codeos_types::bus::GetContextPackResponse {
            goal_interpretation,
            files_to_read,
            relevant_entities,
            key_dependencies,
            boundaries_to_preserve,
            local_patterns,
            suggested_tests,
            estimated_risk,
            formatted_markdown: markdown,
            decisions: pack_decisions,
        })
    }

    pub async fn pr_mri(
        &self,
        base: &str,
        head: &str,
    ) -> anyhow::Result<codeos_types::bus::PrMriResponse> {
        // Root del repo risolta in modo indipendente dalla CWD (CODEOS_REPO →
        // risalita a `.git` → CWD): `mri` non richiede più che il server giri
        // esattamente nella root del repo git.
        let repo_dir = resolve_repo_root();
        // Base VUOTA = «usa il default del repo». "main" è solo una convenzione: i
        // repo storici usano `master` (l'unico vero bug della campagna dei 50
        // progetti: `mri` senza --base falliva exit-128 su anyhow, branch master).
        // Il rilevamento avviene SOLO senza base esplicita: un --base sbagliato
        // scritto dall'utente deve fallire onestamente, mai essere "corretto" di
        // nascosto (maschererebbe i typo).
        let base = if base.trim().is_empty() {
            default_base_ref(&repo_dir).ok_or_else(|| {
                anyhow::anyhow!(
                    "PrMri: nessuna base indicata e nessun branch di default \
                     rilevabile (origin/HEAD, main, master assenti). Indica \
                     --base <ref> esplicitamente."
                )
            })?
        } else {
            base.to_string()
        };
        let base = base.as_str();
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(&repo_dir)
            .arg("diff")
            .arg("--name-only")
            .arg(format!("{}..{}", base, head));
        // Se il diff non si può ottenere (git assente, ref inesistenti, dir non-git)
        // NON dobbiamo proseguire: 0 file → 0 violazioni → rischio "low" sarebbe un
        // referto pulito su un'analisi mai avvenuta. Un ref sbagliato non è un PR
        // sano. Un diff vuoto ma RIUSCITO (base == head) resta invece legittimo.
        let output = cmd.output().map_err(|e| {
            anyhow::anyhow!("PrMri: impossibile eseguire 'git diff {base}..{head}': {e}")
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let details = if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" Dettagli git: {}", stderr.trim())
            };
            anyhow::bail!(
                "PrMri: 'git diff {base}..{head}' fallito ({}). Verifica che i ref \
                 esistano e che la working dir del server sia un repo git.{details}",
                output.status
            );
        }

        // Teniamo sia il path RELATIVO (per il diff riga-a-riga) sia l'ASSOLUTO (con cui
        // il grafo indicizza i file). `files` (assoluti) resta per gli hotspot storici.
        let mut file_pairs: Vec<(String, String)> = Vec::new();
        let mut files = Vec::new();
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let rel = line.trim();
            if rel.is_empty() {
                continue;
            }
            let abs = match repo_dir.join(rel).canonicalize() {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => repo_dir.join(rel).to_string_lossy().to_string(),
            };
            files.push(abs.clone());
            file_pairs.push((rel.to_string(), abs));
        }

        // Entità TOCCATE dal PR: quelle il cui span di righe interseca le righe AGGIUNTE
        // a `head` (mappa diff→nodi AST). Senza questo filtro `pr_mri` elencava OGNI
        // dipendenza dei file toccati come "nuova" — incluse le PREESISTENTI (falso
        // positivo segnalato dai test reali: «mri sovra-riporta il delta»). Se il diff
        // per-file non è interpretabile (rename, git assente), fallback ONESTO: tutte le
        // entità del file (meglio sovra-riportare che mentire dichiarando 0 a torto).
        let mut target_entities = Vec::new();
        for (rel, abs) in &file_pairs {
            let Ok(entities) = self.storage.get_entities_by_file(abs).await else {
                continue;
            };
            let ranges = added_line_ranges(&repo_dir, base, head, rel);
            for e in entities {
                // Le entità a span d'intero-file (Module/Project) NON sono localizzabili
                // sul diff: il loro span copre QUALSIASI riga aggiunta, e le loro
                // "dipendenze" sono gli import di modulo in blocco — preesistenti e nuovi
                // indistinguibili senza la riga del singolo import (che il grafo non
                // memorizza: `Relation` non ha location). Includerle significava
                // dichiarare "nuovi" import VECCHI: esattamente il falso positivo dei
                // test reali. Le saltiamo e leggiamo il delta dalle UNITÀ toccate
                // (funzioni/metodi/tipi); una dipendenza di modulo che conta davvero
                // riemerge attraverso la funzione che la USA.
                if matches!(e.kind, EntityKind::Module | EntityKind::Project) {
                    continue;
                }
                match &ranges {
                    Some(rs) => {
                        let (s, t) = (e.location.start_line, e.location.end_line);
                        if rs.iter().any(|&(a, b)| s <= b && t >= a) {
                            target_entities.push(e);
                        }
                    }
                    // Diff non interpretabile (rename, git assente): fallback onesto a
                    // tutte le unità del file (meglio sovra-riportare che mentire con 0).
                    None => target_entities.push(e),
                }
            }
        }

        let mut relations = Vec::new();
        for ent in &target_entities {
            if let Ok(rels) = self
                .storage
                .query_relations(RelationFilter {
                    source_id: Some(ent.id),
                    ..Default::default()
                })
                .await
            {
                relations.extend(rels);
            }
        }

        let mut new_dependencies = Vec::new();
        for rel in &relations {
            if let (Ok(Some(src)), Ok(Some(tgt))) = (
                self.storage.get_entity_by_id(&rel.source_id).await,
                self.storage.get_entity_by_id(&rel.target_id).await,
            ) {
                new_dependencies.push(format!(
                    "'{}' -> '{}'",
                    src.qualified_name, tgt.qualified_name
                ));
            }
        }
        new_dependencies.sort();
        new_dependencies.dedup();

        let mut violated_boundaries = Vec::new();
        let violations = self.check(&relations).await.unwrap_or_default();
        for vio in &violations {
            violated_boundaries.push(vio.message.clone());
        }

        let mut new_external_dependencies = Vec::new();
        for rel in &relations {
            if let Ok(Some(tgt)) = self.storage.get_entity_by_id(&rel.target_id).await {
                if tgt.kind == codeos_types::EntityKind::ExternalDependency {
                    new_external_dependencies.push(tgt.qualified_name.clone());
                }
            }
        }
        new_external_dependencies.sort();
        new_external_dependencies.dedup();

        let mut historical_hotspots = Vec::new();
        if let Some(history) = &self.history {
            if let Ok(commits) = history.commits() {
                let mut file_counts = HashMap::new();
                for commit in &commits {
                    for file in &commit.changed_files {
                        *file_counts.entry(file.clone()).or_insert(0) += 1;
                    }
                }
                for file in &files {
                    if let Some(&count) = file_counts.get(file) {
                        if count > 2 {
                            historical_hotspots
                                .push(format!("{} (modificato {} volte)", file, count));
                        }
                    }
                }
            }
        }

        let mut impacted_tests = Vec::new();
        for ent in &target_entities {
            if let Ok(rels) = self
                .storage
                .query_relations(RelationFilter {
                    target_id: Some(ent.id),
                    kind: Some(codeos_types::RelationKind::Tests),
                    ..Default::default()
                })
                .await
            {
                for r in rels {
                    if let Ok(Some(test_ent)) = self.storage.get_entity_by_id(&r.source_id).await {
                        impacted_tests.push(test_ent.qualified_name.clone());
                    }
                }
            }
        }
        impacted_tests.sort();
        impacted_tests.dedup();

        let blast_radius_change = (violations.len() + new_external_dependencies.len()) as i32;

        let risk_score = if !violated_boundaries.is_empty() {
            "high".to_string()
        } else if relations.len() > 10 {
            "medium".to_string()
        } else {
            "low".to_string()
        };

        let summary = format!(
            "PR MRI scansionato tra {} e {}. {} dipendenze dal codice modificato, {} violazioni architetturali.",
            base, head, new_dependencies.len(), violated_boundaries.len()
        );

        Ok(codeos_types::bus::PrMriResponse {
            new_dependencies,
            violated_boundaries,
            blast_radius_change,
            historical_hotspots,
            new_external_dependencies,
            impacted_tests,
            risk_score,
            summary,
        })
    }

    /// Scansione LICENZE delle dipendenze + policy dal ledger di intento.
    /// Le licenze vengono dai metadati LOCALI (registry cache cargo, node_modules);
    /// assente = «sconosciuta», mai indovinata. La policy sono le decisioni correnti
    /// con tag `license-deny:<ID>`: il razionale del divieto vive nel ledger, il
    /// report lo CITA (fatti, mai conclusioni legali). La v2 scansiona anche i
    /// SORGENTI (SPDX, copyright, file LICENSE vendored): gli avvisi che dichiarano
    /// una licenza passano per la STESSA policy (un copyright nudo mai — non è
    /// una licenza).
    pub async fn license_report(&self) -> anyhow::Result<codeos_types::bus::LicenseReportResponse> {
        let repo = resolve_repo_root();
        let scanned = crate::license::scan_licenses(&repo);
        let source_scan = crate::license::scan_source_notices(&repo);

        let mut denied: Vec<(String, String)> = Vec::new();
        if let Some(store) = &self.decisions {
            if let Ok(current) = store.current_decisions().await {
                for d in &current {
                    for id in crate::license::denied_ids_from_tags(&d.tags) {
                        denied.push((id, d.title.clone()));
                    }
                }
            }
        }
        let mut violations = crate::license::check_policy(&scanned, &denied);
        violations.extend(crate::license::check_source_policy(
            &source_scan.notices,
            &denied,
        ));

        Ok(codeos_types::bus::LicenseReportResponse {
            dependencies: scanned
                .into_iter()
                .map(|d| codeos_types::bus::DependencyLicenseInfo {
                    name: d.name,
                    ecosystem: d.ecosystem,
                    license: d.license.unwrap_or_default(),
                    source: d.source,
                })
                .collect(),
            violations: violations
                .into_iter()
                .map(|v| codeos_types::bus::LicenseViolationInfo {
                    dependency: v.dependency,
                    license: v.license,
                    denied: v.denied,
                    decision_title: v.decision_title,
                })
                .collect(),
            denied_count: denied.len() as u32,
            source_notices: source_scan
                .notices
                .into_iter()
                .map(|n| codeos_types::bus::SourceNoticeInfo {
                    path: n.path,
                    line: n.line,
                    kind: n.kind,
                    text: n.text,
                })
                .collect(),
            notices_truncated: source_scan.truncated,
        })
    }

    pub async fn why(&self, expr: &str) -> anyhow::Result<codeos_types::bus::WhyResponse> {
        let parts: Vec<&str> = if expr.contains('|') {
            expr.split('|').collect()
        } else if expr.contains("->") {
            expr.split("->").collect()
        } else {
            expr.split_whitespace().collect()
        };

        let upstream = parts.first().unwrap_or(&"").trim().to_string();
        let downstream = parts.get(1).unwrap_or(&"").trim().to_string();

        // Spiegare un confine richiede DUE estremi. Senza, non c'è una relazione da
        // raccontare: niente fossili, niente decisioni. Col matching per sottostringa
        // un estremo vuoto è veleno — `contains("")` è sempre vero, quindi `why "foo"`
        // (senza separatore) trascinava dentro OGNI decisione con un tag qualsiasi.
        // Meglio chiedere l'input giusto che mentire con "decisioni correlate" inventate.
        let well_formed = !upstream.is_empty() && !downstream.is_empty();

        let mut born_commit = String::new();
        let mut born_date = String::new();
        let mut intent = String::new();
        let mut co_changed_files = Vec::new();
        let mut history_insufficient = false;
        let mut fossil_found = false;
        let mut markdown_decisions = Vec::new();

        if well_formed {
            let fossils = self.fossils().await.unwrap_or_default();
            let matching_fossil = fossils.into_iter().find(|f| {
                (f.upstream == upstream && f.downstream == downstream)
                    || (f.upstream == downstream && f.downstream == upstream)
            });

            // Se nessun INVARIANTE scoperto combacia, scava il confine ARBITRARIO
            // direttamente dalla storia: `why` spiega QUALSIASI confine nato in git
            // (il commit che l'ha creato + l'intento), non solo gli invarianti — è ciò
            // che lo rende NON inerte sui confini reali che l'utente chiede.
            let fossil = match matching_fossil {
                Some(f) => Some(f),
                None => self
                    .excavate_boundary(&upstream, &downstream)
                    .await
                    .unwrap_or(None),
            };
            if let Some(f) = fossil {
                fossil_found = true;
                born_commit = f.born_at.clone();
                born_date = chrono::DateTime::from_timestamp(f.born_at_unix, 0)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                intent = f.intent.clone();
                co_changed_files = f.born_structure.clone();
                history_insufficient = self.check_history_adequacy(&[f]).await.unwrap_or(false);
            }

            if let Some(store) = &self.decisions {
                if let Ok(all_decisions) = store.all().await {
                    for dec in all_decisions {
                        // Una decisione spiega il confine A↔B solo se cita ENTRAMBI gli
                        // estremi (nel titolo O nei tag). La vecchia condizione accettava
                        // UNO QUALSIASI dei due nei tag (OR) → un `why "A|B"` veniva
                        // inondato da OGNI invariante che toccava A *oppure* B,
                        // seppellendo la decisione umana specifica (il moat). Richiedere
                        // ENTRAMBI isola il confine giusto.
                        let mentions = |needle: &str| {
                            dec.title.contains(needle)
                                || dec.tags.iter().any(|t| t.contains(needle))
                        };
                        if mentions(&upstream) && mentions(&downstream) {
                            markdown_decisions.push(format!(
                                "### {}\n\n**Autore:** {}\n\n**Razionale:** {}\n\n**Contesto:** {}",
                                dec.title, dec.author, dec.rationale, dec.context
                            ));
                        }
                    }
                }
            }
        }

        // L'explanation non deve MAI affermare una nascita che non abbiamo trovato.
        // Senza fossile il vecchio codice stampava "è nato nel commit  in data ."
        // (campi vuoti): un confine inventato. Ora i tre casi sono distinti.
        let explanation = if !well_formed {
            "Per spiegare un confine servono due estremi: usa why \"a|b\" (oppure \
             \"a->b\"). Con un solo nome non c'è una relazione da raccontare."
                .to_string()
        } else if fossil_found {
            format!(
                "Il confine architetturale tra '{}' e '{}' è nato nel commit {} in data {}.",
                upstream, downstream, born_commit, born_date
            )
        } else {
            format!(
                "Nessun confine registrato tra '{}' e '{}': non risulta un fossile \
                 nella storia analizzata. Non lo invento — un confine assente è meglio \
                 di uno inventato.",
                upstream, downstream
            )
        };

        // La STORIA del confine: solo per espressioni ben formate (servono due
        // estremi). Errori di mining non devono far fallire il `why` (la nascita e
        // le decisioni restano valide): degradano a storia vuota, onestamente.
        let boundary_story = if well_formed {
            self.boundary_story_lines(&upstream, &downstream)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(codeos_types::bus::WhyResponse {
            born_commit,
            born_date,
            intent,
            co_changed_files,
            markdown_decisions,
            explanation,
            history_insufficient,
            boundary_story,
        })
    }

    pub async fn simulate(
        &self,
        expr: &str,
    ) -> anyhow::Result<codeos_types::bus::SimulateResponse> {
        let mut source = String::new();
        let mut target = String::new();
        if expr.to_lowercase().contains("move") && expr.to_lowercase().contains("to") {
            let parts: Vec<&str> = expr.split_whitespace().collect();
            if let Some(pos) = parts.iter().position(|&w| w.to_lowercase() == "move") {
                if let Some(to_pos) = parts.iter().position(|&w| w.to_lowercase() == "to") {
                    if to_pos > pos + 1 && parts.len() > to_pos + 1 {
                        source = parts[pos + 1..to_pos].join(" ");
                        target = parts[to_pos + 1..].join(" ");
                    }
                }
            }
        }

        let mut dependencies_to_rewrite = Vec::new();
        let mut changed_boundaries = Vec::new();
        let mut risks = Vec::new();
        let mut suggested_tests = Vec::new();
        let mut recommendation_plan = Vec::new();

        if source.is_empty() || target.is_empty() {
            dependencies_to_rewrite.push("Specifica un'espressione nel formato 'move <sorgente> to <destinazione>' per simulare.".to_string());
            risks.push("Espressione non riconosciuta.".to_string());
        } else if source == target {
            // Spostare qualcosa su sé stesso è un no-op: niente confini fusi, niente
            // piano in 4 passi. Dirlo è meglio che inventare un refactor inesistente.
            risks.push(format!(
                "Sorgente e destinazione coincidono ('{}'): non c'è nulla da spostare.",
                source
            ));
        } else {
            let matched = self
                .storage
                .find_entities_by_name_pattern(&source)
                .await
                .unwrap_or_default();

            if matched.is_empty() {
                // Sorgente assente dal grafo. La vecchia versione emetteva comunque
                // rischi e un piano in 4 passi con dependencies_to_rewrite vuoto — che
                // un'AI legge come «niente dipende da questo, spostamento facile». Ma
                // vuoto qui significa «non l'ho trovata», non «spostamento sicuro».
                risks.push(format!(
                    "Nessuna entità corrisponde a '{}' nel grafo: non posso simularne lo \
                     spostamento. Una lista di dipendenze vuota qui significa «sorgente \
                     sconosciuta», non «spostamento sicuro».",
                    source
                ));
            } else {
                for ent in matched {
                    if let Ok(outgoing) = self
                        .storage
                        .query_relations(RelationFilter {
                            source_id: Some(ent.id),
                            ..Default::default()
                        })
                        .await
                    {
                        for r in outgoing {
                            if let Ok(Some(tgt)) = self.storage.get_entity_by_id(&r.target_id).await
                            {
                                dependencies_to_rewrite.push(format!(
                                    "Modifica chiamata da '{}' a '{}'",
                                    ent.qualified_name, tgt.qualified_name
                                ));
                            }
                        }
                    }
                    if let Ok(incoming) = self
                        .storage
                        .query_relations(RelationFilter {
                            target_id: Some(ent.id),
                            ..Default::default()
                        })
                        .await
                    {
                        for r in incoming {
                            if let Ok(Some(src)) = self.storage.get_entity_by_id(&r.source_id).await
                            {
                                dependencies_to_rewrite.push(format!(
                                    "Aggiorna chiamata da '{}' a '{}' (nuova destinazione: {})",
                                    src.qualified_name, ent.qualified_name, target
                                ));
                            }
                        }
                    }
                }

                risks.push(format!(
                    "Lo spostamento di '{}' potrebbe rompere l'incapsulamento del layer.",
                    source
                ));
                changed_boundaries.push(format!(
                    "Il confine di '{}' verrà fuso con '{}'.",
                    source, target
                ));
                recommendation_plan.push(format!("1. Crea il modulo di destinazione '{}'", target));
                recommendation_plan.push(format!(
                    "2. Sposta le classi/funzioni da '{}' a '{}'",
                    source, target
                ));
                recommendation_plan
                    .push("3. Aggiorna i relativi import nel resto del progetto".to_string());
                recommendation_plan.push("4. Esegui i test di regressione".to_string());
                suggested_tests.push(format!("Esegui tutti i test nel modulo '{}'", target));
            }
        }

        Ok(codeos_types::bus::SimulateResponse {
            dependencies_to_rewrite,
            changed_boundaries,
            risks,
            suggested_tests,
            recommendation_plan,
        })
    }
}

/// Trova il prefisso comune più lungo fra una serie di stringhe (es. cartella radice).
fn find_common_prefix<'a>(mut paths: impl Iterator<Item = &'a String>) -> String {
    let Some(first) = paths.next() else {
        return String::new();
    };
    let mut common = first.clone();
    for path in paths {
        let mut new_common = String::new();
        for (c1, c2) in common.chars().zip(path.chars()) {
            if c1 == c2 {
                new_common.push(c1);
            } else {
                break;
            }
        }
        common = new_common;
    }
    if let Some(idx) = common.rfind('/') {
        common.truncate(idx + 1);
    } else if let Some(idx) = common.rfind('\\') {
        common.truncate(idx + 1);
    }
    common
}

/// La chiave stabile di un invariante: identifica la coppia ordinata
/// (upstream, downstream) a prescindere dall'`id` (rigenerato a ogni mining).
fn rule_key(rule: &LayeringRule) -> String {
    format!("{RULE_TAG_PREFIX}{}|{}", rule.upstream, rule.downstream)
}

/// Inverso di [`rule_key`]: ricava `(upstream, downstream)` dalla chiave. `None`
/// se la stringa non ha la forma attesa (decisione manipolata a mano).
fn parse_rule_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix(RULE_TAG_PREFIX)?.split_once('|')
}

/// Trasforma una regola scoperta in una `Decision` pronta per la memoria storica.
/// Il *perché* (finora implicito nello spazio negativo) viene reso in prosa, e le
/// entità di confine vengono agganciate perché il Query lo ritrovi.
///
/// Se è disponibile un [`DecisionFossil`] (storia git agganciata), il *contesto*
/// viene **ancorato alla nascita del confine**: il commit che lo ha disegnato, il
/// suo intento (il messaggio) e il diff strutturale di cristallizzazione. Un tag
/// `born-at:{hash}` rende il fossile filtrabile. L'arricchimento NON tocca la
/// chiave di dedup ([`rule_key`]): `learn` resta idempotente.
fn promotion_decision(
    rule: &LayeringRule,
    related_entity_ids: Vec<EntityId>,
    evidence: Vec<Evidence>,
    fossil: Option<&DecisionFossil>,
) -> Decision {
    let mut context = format!(
        "Scoperto automaticamente dal Guardian osservando lo spazio negativo del \
         grafo: {support} dipendenze vanno '{downstream}' → '{upstream}' e zero nel \
         verso opposto. L'asimmetria è trattata come invariante architetturale.",
        support = rule.support,
        downstream = rule.downstream,
        upstream = rule.upstream,
    );
    let mut tags = vec![
        "layering".to_string(),
        "invariante".to_string(),
        rule_key(rule),
    ];

    // Fossile di Decisione: àncora la regola alla sua origine storica.
    if let Some(fossil) = fossil.filter(|f| !f.born_at.is_empty()) {
        let short: String = fossil.born_at.chars().take(12).collect();
        let intent = if fossil.intent.is_empty() {
            "(nessun messaggio di commit)"
        } else {
            fossil.intent.as_str()
        };
        let structure = if fossil.born_structure.is_empty() {
            "(nessun file tracciato)".to_string()
        } else {
            fossil.born_structure.join(", ")
        };
        context.push_str(&format!(
            " Origine storica (Fossile di Decisione): il confine è nato nel commit \
             {short} — «{intent}». Diff di cristallizzazione: {structure}."
        ));
        tags.push(format!("born-at:{short}"));
    }

    let new = NewDecision {
        author: "ai:ArchitectureGuardian".to_string(),
        title: format!(
            "Invariante di layering: '{}' non deve dipendere da '{}'",
            rule.upstream, rule.downstream
        ),
        context,
        rationale: format!(
            "'{upstream}' è un layer di base da cui '{downstream}' dipende, mai il \
             contrario: un arco '{upstream}' → '{downstream}' invertirebbe la freccia \
             architetturale stabilita. Confidenza euristica {confidence:.2} (support \
             {support}); regola scoperta dai dati, non configurata a mano.",
            upstream = rule.upstream,
            downstream = rule.downstream,
            confidence = rule.confidence,
            support = rule.support,
        ),
        related_entity_ids,
        related_decision_ids: Vec::new(),
        // Una promozione nasce nuova: non rimpiazza né ritira altre decisioni.
        supersedes: Vec::new(),
        deprecates: Vec::new(),
        tags,
    };
    // Trappola #1 resa esecutiva: se il grafo (e la storia) provano qualcosa di
    // citabile, la promozione passa per il cancello `Proposal` — che per costruzione
    // non esiste senza evidenza — e quella evidenza viaggia DENTRO la Decision (il
    // perché resta verificabile a posteriori). Senza nulla da citare (archi privi di
    // identità stabile e niente git) non si finge: si ricade su `from_new`, una
    // Decision onesta a evidenza vuota, identica a una scritta a mano.
    if evidence.is_empty() {
        Decision::from_new(new, DecisionKind::ArchitectureRule)
    } else {
        Proposal::new(new, DecisionKind::ArchitectureRule, evidence)
            .expect("evidenza non vuota garantita dal ramo sopra")
            .confirm()
    }
}

/// Le evidenze che il grafo (e la storia git) **già provano** per questa regola:
/// ogni arco di supporto osservato, citato per identità stabile
/// (`Evidence::Edge`), più — se c'è un fossile — il commit che ha disegnato il
/// confine (`Evidence::Commit`). È la materia prima del cancello [`Proposal`]: la
/// promozione non porterà rationale inventato, solo ciò che il grafo dimostra.
///
/// Gli archi senza `source_qname`/`target_qname` nei metadati (strutturali, o
/// risolti prima della Slice 2) vengono **saltati** dal `filter_map`: non si
/// inventa un'identità che l'arco non porta. Costo zero di query: i nomi sono già
/// nei metadati dell'arco.
fn promotion_evidence(
    rule: &LayeringRule,
    relations: &[Relation],
    layer_map: &HashMap<EntityId, LayerKey>,
    fossil: Option<&DecisionFossil>,
) -> Vec<Evidence> {
    let mut evidence: Vec<Evidence> = support_edges(rule, relations, layer_map)
        .into_iter()
        .filter_map(|rel| {
            let source = rel.metadata.get("source_qname")?.clone();
            let target = rel.metadata.get("target_qname")?.clone();
            Some(Evidence::Edge {
                source,
                kind: rel.kind,
                target,
            })
        })
        .collect();
    if let Some(fossil) = fossil.filter(|f| !f.born_at.is_empty()) {
        evidence.push(Evidence::Commit(fossil.born_at.clone()));
    }
    evidence
}

/// Registra il **ritiro** di un invariante diventato stale. Non è una regola in
/// vigore (kind `Decision`, non `ArchitectureRule`) ma una nota storica: referenzia
/// la promozione originale e riusa le sue entità di confine, così la stessa query
/// che mostrava la regola ne mostra anche la fine.
fn retraction_decision(
    upstream: &str,
    downstream: &str,
    promotion_id: EntityId,
    related_entity_ids: Vec<EntityId>,
) -> Decision {
    let new = NewDecision {
        author: "ai:ArchitectureGuardian".to_string(),
        title: format!(
            "Invariante di layering ritirato: '{upstream}' → '{downstream}' ora esiste nel grafo"
        ),
        context: format!(
            "L'invariante che vietava '{upstream}' → '{downstream}' non è più sostenuto \
             dallo spazio negativo: il grafo ora contiene dipendenze in ENTRAMBI i versi \
             tra i due layer."
        ),
        rationale: format!(
            "Ritirato automaticamente dal Guardian: la condizione che l'aveva fatto \
             emergere (zero archi '{upstream}' → '{downstream}') non è più vera. La \
             promozione originale resta in memoria come storia (vedi \
             related_decision_ids): la memoria non si riscrive, si stratifica."
        ),
        related_entity_ids,
        related_decision_ids: vec![promotion_id],
        // Il ritiro vive nel ledger come un `deprecates` sulla promozione: da lì si
        // DERIVA il suo stato `Deprecated`, unica fonte di verità. Niente tag
        // `status:retired` parallelo — lo stato si deriva, non si memorizza (Slice A):
        // un consumatore del Memory Engine vede l'invariante decaduto senza conoscere
        // i tag del Guardian, e non resta una seconda etichetta che possa divergere.
        supersedes: Vec::new(),
        deprecates: vec![promotion_id],
        tags: vec![
            "layering".to_string(),
            "invariante".to_string(),
            format!("{RULE_TAG_PREFIX}{upstream}|{downstream}"),
        ],
    };
    Decision::from_new(new, DecisionKind::Decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    use codeos_storage::SqliteStorage;
    use codeos_types::{Entity, EntityKind, GraphDelta, RelationKind, SourceLocation};

    #[test]
    fn parse_added_ranges_extracts_the_plus_side_of_hunks() {
        // Diff con due hunk: una riga aggiunta a 10, e 3 righe (12..14) a fronte di una
        // cancellazione (-5,2). `pr_mri` usa SOLO il lato `+` per sapere cosa il PR ha
        // aggiunto a `head` — così non scambia le dipendenze preesistenti per "nuove".
        let diff = "diff --git a/f.rs b/f.rs\n\
                    --- a/f.rs\n\
                    +++ b/f.rs\n\
                    @@ -9,0 +10 @@ fn x() {\n\
                    +    let new = dep();\n\
                    @@ -5,2 +12,3 @@ impl T {\n\
                    +    a();\n+    b();\n+    c();\n";
        let ranges = parse_added_ranges(diff);
        assert_eq!(ranges, vec![(10, 10), (12, 14)], "ranges = {ranges:?}");
    }

    #[test]
    fn parse_added_ranges_ignores_pure_deletions() {
        // Hunk di pura cancellazione (+N,0): 0 righe aggiunte ⇒ nessun intervallo, quindi
        // nessuna entità "toccata" da attribuire al PR per quel punto.
        let diff = "@@ -10,3 +9,0 @@\n-    gone_a();\n-    gone_b();\n-    gone_c();\n";
        assert!(parse_added_ranges(diff).is_empty());
    }

    #[test]
    fn repo_root_prefers_codeos_repo_then_walks_up_to_git() {
        // 1. CODEOS_REPO ha priorità ed è INDIPENDENTE dalla CWD (il bug di `mri`).
        assert_eq!(
            repo_root_from(Some("/srv/myrepo".to_string()), Path::new("/tmp/altrove")),
            PathBuf::from("/srv/myrepo")
        );
        // 2/3. Senza env (o whitespace), risale dall'albero fino alla dir con `.git`.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("codeos_reporoot_{stamp}"));
        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(
            repo_root_from(None, &nested),
            root,
            "senza env, risale fino al .git"
        );
        assert_eq!(
            repo_root_from(Some("   ".to_string()), &nested),
            root,
            "CODEOS_REPO whitespace ignorato, risale al root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn truncate_with_note_caps_long_lists_and_counts_the_rest() {
        // Sotto il tetto: invariato (nessuna nota).
        let short: Vec<String> = (0..3).map(|i| i.to_string()).collect();
        assert_eq!(truncate_with_note(short.clone(), 5, "relazioni"), short);
        // Sopra il tetto: troncato a `max` + 1 riga di nota col residuo.
        let long: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let capped = truncate_with_note(long, 4, "relazioni");
        assert_eq!(capped.len(), 5, "4 elementi + 1 nota");
        assert!(
            capped[4].contains("+6") && capped[4].contains("4 MB"),
            "la nota conta il residuo (10-4=6) e cita il limite: {:?}",
            capped[4]
        );
    }

    fn entity(qname: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            qualified_name: qname.to_string(),
            location: SourceLocation {
                file_path: format!("{}.py", qname.replace("::", "/")),
                start_line: 1,
                start_column: 0,
                end_line: 2,
                end_column: 0,
            },
            metadata: Map::new(),
        }
    }

    fn relation(kind: RelationKind, source: EntityId, target: EntityId) -> Relation {
        Relation {
            id: EntityId::new(),
            kind,
            source_id: source,
            target_id: target,
            metadata: Map::new(),
        }
    }

    /// Semina un grafo a due layer con tre dipendenze `app::api` → `app::core`.
    /// Ritorna (storage, entità api, entità core).
    async fn seeded_two_layer_graph() -> (Arc<dyn GraphStorage>, Vec<Entity>, Vec<Entity>) {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::api::handler_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::core::service_{i}::do_it")))
            .collect();

        let relations = (0..3)
            .map(|i| relation(RelationKind::Calls, api[i].id, core[i].id))
            .collect();

        storage
            .apply_delta(GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: relations,
                ..Default::default()
            })
            .await
            .unwrap();

        (storage, api, core)
    }

    #[tokio::test]
    async fn discovers_the_layering_rule_from_the_graph() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);

        let rules = guardian.mine_rules().await.unwrap();
        assert_eq!(rules.len(), 1, "regole = {rules:?}");
        assert_eq!(rules[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("app::api".to_string()));
    }

    #[tokio::test]
    async fn surfaces_a_forming_boundary_as_a_candidate() {
        // Due archi api → core (zero nel verso opposto): asimmetria pura ma sotto la
        // soglia di default (3). `mine_rules` non lo vede ancora come invariante, ma
        // `candidates` lo fa emergere come confine *in formazione* (stadio 1 del
        // flusso) — derivato dallo storage, senza scrivere nulla nel ledger.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..2)
            .map(|i| entity(&format!("app::api::handler_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..2)
            .map(|i| entity(&format!("app::core::service_{i}::do_it")))
            .collect();
        let relations = (0..2)
            .map(|i| relation(RelationKind::Calls, api[i].id, core[i].id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: relations,
                ..Default::default()
            })
            .await
            .unwrap();

        let guardian = Guardian::new(storage);

        assert!(
            guardian.mine_rules().await.unwrap().is_empty(),
            "due archi non sono ancora un invariante"
        );

        let candidates = guardian.candidates().await.unwrap();
        assert_eq!(candidates.len(), 1, "candidati = {candidates:?}");
        assert_eq!(candidates[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(candidates[0].downstream, LayerKey("app::api".to_string()));
        assert_eq!(candidates[0].support, 2);
        assert_eq!(candidates[0].needed, 1);
    }

    #[tokio::test]
    async fn external_dependencies_never_become_layering_rules() {
        // Regressione P0-2: un layer interno che importa `tokio` 3 volte è
        // un'asimmetria a senso unico PERFETTA — esattamente la forma che il miner
        // promuoverebbe a invariante. Ma `external::tokio` non è un layer della
        // nostra architettura: non deve generare la regola assurda "external::tokio
        // non deve dipendere da app::api". Deve restare però nel grafo (query/impatto).
        let (storage, api, _core) = seeded_two_layer_graph().await;

        let tokio = Entity {
            id: EntityId::new(),
            kind: EntityKind::ExternalDependency,
            qualified_name: "external::tokio".to_string(),
            location: SourceLocation {
                file_path: "<external>".to_string(),
                start_line: 0,
                start_column: 0,
                end_line: 0,
                end_column: 0,
            },
            metadata: Map::new(),
        };
        let ext_edges: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Imports, api[i].id, tokio.id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: vec![tokio.clone()],
                added_relations: ext_edges,
                ..Default::default()
            })
            .await
            .unwrap();

        let guardian = Guardian::new(storage.clone());
        let rules = guardian.mine_rules().await.unwrap();

        // Solo l'invariante reale (app::core ← app::api). Nessuna regola che nomini
        // un layer esterno, in nessuno dei due versi.
        assert_eq!(
            rules.len(),
            1,
            "atteso solo l'invariante interno: {rules:?}"
        );
        assert!(
            !rules.iter().any(|r| {
                r.upstream.0.starts_with("external") || r.downstream.0.starts_with("external")
            }),
            "nessun invariante deve nominare una dipendenza esterna: {rules:?}"
        );

        // La dipendenza esterna resta interrogabile nel grafo (non è stata rimossa).
        assert!(
            storage
                .get_entity_by_qname("external::tokio")
                .await
                .unwrap()
                .is_some(),
            "external::tokio deve restare nel grafo per query e impatto"
        );
    }

    #[tokio::test]
    async fn flags_a_proposed_reversed_dependency() {
        let (storage, api, core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);

        // Edge proposto core → api: il layer di base che dipende dal layer alto.
        let proposed = relation(RelationKind::Calls, core[0].id, api[0].id);
        let violations = guardian
            .check(std::slice::from_ref(&proposed))
            .await
            .unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].relation_id, proposed.id);
        assert!(violations[0].message.contains("app::core"));

        // Edge proposto nella direzione lecita: nessuna violazione.
        let ok = relation(RelationKind::Calls, api[1].id, core[1].id);
        assert!(guardian.check(&[ok]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn promotes_the_discovered_invariant_to_persistent_memory() {
        use codeos_memory::InMemoryDecisionStore;

        let (storage, api, core) = seeded_two_layer_graph().await;
        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian = Guardian::with_memory(storage, store.clone());

        let created = guardian.learn().await.unwrap();
        assert_eq!(created.len(), 1, "un invariante atteso");

        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 1);
        let decision = &all[0];
        assert_eq!(decision.kind, DecisionKind::ArchitectureRule);
        assert_eq!(decision.author, "ai:ArchitectureGuardian");
        assert!(decision.title.contains("app::core"));
        assert!(decision.title.contains("app::api"));
        // Le entità di confine sono agganciate ⇒ il Query ritroverà il perché
        // interrogando una qualsiasi delle entità coinvolte.
        assert!(decision.related_entity_ids.contains(&api[0].id));
        assert!(decision.related_entity_ids.contains(&core[0].id));
        // Archi nudi (senza identità stabile) e niente git ⇒ nessuna evidenza da
        // citare: si ricade su `from_new`, evidenza vuota. Onesto, non si inventa.
        assert!(
            decision.evidence.is_empty(),
            "senza qname né fossile l'evidenza dev'essere vuota: {:?}",
            decision.evidence
        );

        // Idempotente: una seconda passata non duplica l'invariante.
        let again = guardian.learn().await.unwrap();
        assert!(again.is_empty(), "la seconda learn non deve creare nulla");
        assert_eq!(store.all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn learn_is_a_noop_without_memory() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        assert!(guardian.learn().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn promotion_cites_its_support_edges_as_evidence() {
        use codeos_memory::{Evidence, InMemoryDecisionStore};

        // Come `seeded_two_layer_graph`, ma gli archi portano l'identità stabile
        // (`source_qname`/`target_qname`) come in produzione dopo la Slice 2. È
        // l'unica differenza che serve perché il Guardian possa citarli — a costo
        // zero di query: i nomi sono già nei metadati dell'arco.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::api::handler_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::core::service_{i}::do_it")))
            .collect();
        let relations: Vec<Relation> = (0..3)
            .map(|i| {
                let mut r = relation(RelationKind::Calls, api[i].id, core[i].id);
                r.metadata
                    .insert("source_qname".to_string(), api[i].qualified_name.clone());
                r.metadata
                    .insert("target_qname".to_string(), core[i].qualified_name.clone());
                r
            })
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: relations,
                ..Default::default()
            })
            .await
            .unwrap();

        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian = Guardian::with_memory(storage, store.clone());
        assert_eq!(guardian.learn().await.unwrap().len(), 1);

        let all = store.all().await.unwrap();
        let promo = &all[0];
        // L'invariante promosso PORTA l'evidenza nel ledger: i 3 archi di supporto,
        // citati per identità stabile. Niente rationale campato in aria — solo ciò
        // che il grafo prova (trappola #1: la Proposal non esiste senza evidenza).
        assert_eq!(promo.evidence.len(), 3, "evidenza = {:?}", promo.evidence);
        for ev in &promo.evidence {
            let Evidence::Edge {
                source,
                kind,
                target,
            } = ev
            else {
                panic!("attesa solo Evidence::Edge, trovato {ev:?}");
            };
            assert_eq!(*kind, RelationKind::Calls);
            assert!(
                source.starts_with("app::api::handler_"),
                "source = {source}"
            );
            assert!(
                target.starts_with("app::core::service_"),
                "target = {target}"
            );
        }
    }

    #[tokio::test]
    async fn retires_a_stale_invariant_when_the_asymmetry_breaks() {
        use codeos_memory::{DecisionStatus, InMemoryDecisionStore};

        let (storage, api, core) = seeded_two_layer_graph().await;
        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian = Guardian::with_memory(storage.clone(), store.clone());

        // L'invariante emerge e viene promosso.
        assert_eq!(guardian.learn().await.unwrap().len(), 1);

        // Il codice evolve: 3 archi core → api (il verso prima proibito). Ora la
        // dipendenza è bidirezionale e l'asimmetria che giustificava la regola sparisce.
        let reverse: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Calls, core[i].id, api[i].id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_relations: reverse,
                ..Default::default()
            })
            .await
            .unwrap();

        // learn() deve RITIRARE l'invariante, senza cancellare la promozione.
        let changes = guardian.learn().await.unwrap();
        assert_eq!(changes.len(), 1, "atteso esattamente un ritiro");

        let all = store.all().await.unwrap();
        assert_eq!(
            all.len(),
            2,
            "promozione + ritiro coesistono (storia additiva)"
        );
        let promotion = all
            .iter()
            .find(|d| d.kind == DecisionKind::ArchitectureRule)
            .expect("promozione mancante");
        // Il ritiro è l'unica `Decision` (kind diverso dalla promozione, che è una
        // `ArchitectureRule`): lo riconosciamo dallo stato del ledger, non da un tag.
        let retraction = all
            .iter()
            .find(|d| d.kind == DecisionKind::Decision)
            .expect("ritiro mancante");
        // Il ritiro referenzia la promozione e ne eredita le entità di confine.
        assert!(retraction.related_decision_ids.contains(&promotion.id));
        assert_eq!(retraction.related_entity_ids, promotion.related_entity_ids);

        // Il ritiro cabla il ledger: la promozione è `Deprecated` (stato derivato,
        // non scritto su di essa) e sparisce dalle decisioni correnti, mentre il
        // ritiro stesso resta corrente. È il primo produttore reale dello stato.
        assert_eq!(retraction.deprecates, vec![promotion.id]);
        let ledger = store.ledger().await.unwrap();
        let promotion_status = ledger
            .iter()
            .find(|(d, _)| d.id == promotion.id)
            .map(|(_, s)| *s);
        assert_eq!(promotion_status, Some(DecisionStatus::Deprecated));
        let current = store.current_decisions().await.unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, retraction.id);

        // Idempotente: a grafo invariato, un'altra learn non aggiunge nulla.
        assert!(guardian.learn().await.unwrap().is_empty());
        assert_eq!(store.all().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn re_promotes_after_a_retired_invariant_re_emerges() {
        use codeos_memory::InMemoryDecisionStore;

        let (storage, api, core) = seeded_two_layer_graph().await;
        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian = Guardian::with_memory(storage.clone(), store.clone());

        guardian.learn().await.unwrap(); // promozione

        // Rompi l'asimmetria, poi ritira.
        let reverse: Vec<Relation> = (0..3)
            .map(|i| relation(RelationKind::Calls, core[i].id, api[i].id))
            .collect();
        let reverse_ids: Vec<EntityId> = reverse.iter().map(|r| r.id).collect();
        storage
            .apply_delta(GraphDelta {
                added_relations: reverse,
                ..Default::default()
            })
            .await
            .unwrap();
        guardian.learn().await.unwrap(); // ritiro

        // Il codice torna sano: rimuoviamo gli archi proibiti. L'asimmetria si ripristina.
        storage
            .apply_delta(GraphDelta {
                removed_relation_ids: reverse_ids,
                ..Default::default()
            })
            .await
            .unwrap();

        // learn() deve RI-promuovere l'invariante (il ritiro non lo blocca per sempre).
        let changes = guardian.learn().await.unwrap();
        assert_eq!(changes.len(), 1, "l'invariante riemerso va ri-promosso");

        // Storia: promozione, ritiro, ri-promozione — tre strati, due regole attive nel tempo.
        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 3);
        let promotions = all
            .iter()
            .filter(|d| d.kind == DecisionKind::ArchitectureRule)
            .count();
        assert_eq!(promotions, 2, "due promozioni nella storia");
    }

    // ---------------------------------------------------------------------------
    // Regole DICHIARATE: il decreto umano che vale anche senza evidenza nel grafo.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn declared_rule_is_enforced_without_any_structural_support() {
        use crate::declared::declared_layering_rules;

        // Grafo con DUE soli archi api → core: sotto la soglia di min_support (3),
        // quindi NESSUN invariante verrebbe scoperto dallo spazio negativo.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..2)
            .map(|i| entity(&format!("app::api::h_{i}::run")))
            .collect();
        let core: Vec<Entity> = (0..2)
            .map(|i| entity(&format!("app::core::s_{i}::do_it")))
            .collect();
        let good: Vec<Relation> = (0..2)
            .map(|i| relation(RelationKind::Calls, api[i].id, core[i].id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: api.iter().chain(core.iter()).cloned().collect(),
                added_relations: good,
                ..Default::default()
            })
            .await
            .unwrap();

        // Senza config: niente regole (supporto insufficiente).
        let plain = Guardian::new(storage.clone());
        assert!(plain.mine_rules().await.unwrap().is_empty());

        // Con la regola DICHIARATA "app::api non deve dipendere da app::core".
        let declared = declared_layering_rules(
            "architecture:\n  rules:\n    - type: layer_dependency\n      from: [\"app::api\"]\n      to: [\"app::core\"]\n",
        );
        let guardian = Guardian::new(storage).with_declared_rules(declared);

        // Ora la regola esiste (per decreto) e compare nel mining, etichettata.
        let rules = guardian.mine_rules().await.unwrap();
        assert_eq!(rules.len(), 1, "regole = {rules:?}");
        assert_eq!(rules[0].origin, codeos_types::bus::RuleOrigin::Declared);
        assert_eq!(rules[0].upstream, LayerKey("app::api".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("app::core".to_string()));

        // E viene fatta rispettare: un arco api → core inverte la freccia dichiarata.
        let bad = relation(RelationKind::Calls, api[0].id, core[0].id);
        let violations = guardian.check(std::slice::from_ref(&bad)).await.unwrap();
        assert_eq!(violations.len(), 1, "violazioni = {violations:?}");
        assert_eq!(violations[0].relation_id, bad.id);
    }

    #[tokio::test]
    async fn discovered_rule_wins_over_a_duplicate_declaration() {
        use crate::declared::declared_layering_rules;

        // Il grafo scopre app::api → app::core (support 3). La stessa coppia è anche
        // dichiarata: non deve duplicarsi, e deve restare la versione SCOPERTA (che
        // porta supporto ed evidenza).
        // Lo spazio negativo scopre upstream=app::core, downstream=app::api
        // ("core non deve dipendere da api"). Dichiariamo lo STESSO confine:
        // from=app::core, to=app::api.
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let declared = declared_layering_rules(
            "architecture:\n  rules:\n    - type: layer_dependency\n      from: [\"app::core\"]\n      to: [\"app::api\"]\n",
        );
        let guardian = Guardian::new(storage).with_declared_rules(declared);

        let rules = guardian.mine_rules().await.unwrap();
        assert_eq!(rules.len(), 1, "nessun duplicato: {rules:?}");
        assert_eq!(rules[0].origin, codeos_types::bus::RuleOrigin::Discovered);
        assert_eq!(rules[0].support, 3);
    }

    #[tokio::test]
    async fn check_excludes_candidates_so_a_persisted_bad_edge_self_reports() {
        let (storage, api, core) = seeded_two_layer_graph().await;

        // La dipendenza proibita viene effettivamente PERSISTITA (come se un LLM
        // l'avesse già scritta). Senza l'esclusione delle candidate, lo spazio
        // negativo sarebbe sporcato e non rileveremmo nulla.
        let bad = relation(RelationKind::Calls, core[0].id, api[0].id);
        storage
            .apply_delta(GraphDelta {
                added_relations: vec![bad.clone()],
                ..Default::default()
            })
            .await
            .unwrap();

        let guardian = Guardian::new(storage);
        let violations = guardian.check(std::slice::from_ref(&bad)).await.unwrap();
        assert_eq!(violations.len(), 1, "violazioni = {violations:?}");
        assert_eq!(violations[0].relation_id, bad.id);
    }

    // ---------------------------------------------------------------------------
    // Campo di Astensione: la confidenza calibrata sul *negativo del tempo*.
    // ---------------------------------------------------------------------------

    /// La confidenza strutturale di una regola con support 3: `1 - 1/(3+1)`.
    const STRUCTURAL_CONF_SUPPORT_3: f32 = 0.75;

    #[tokio::test]
    async fn mature_history_raises_confidence_above_the_structural_baseline() {
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, api, core) = seeded_two_layer_graph().await;

        // Baseline: senza tempo, è solo l'euristica strutturale (0.75).
        let baseline = Guardian::new(storage.clone()).mine_rules().await.unwrap();
        assert_eq!(baseline.len(), 1);
        assert!((baseline[0].confidence - STRUCTURAL_CONF_SUPPORT_3).abs() < 1e-6);

        // Storia matura: 30 commit co-toccano api e core SENZA mai invertire la
        // freccia — 30 astensioni. La regola ha pagato 30 occasioni di violazione.
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();
        let mature: Vec<Commit> = (0..30)
            .map(|_| Commit::new([api_file.clone(), core_file.clone()]))
            .collect();

        let guardian =
            Guardian::new(storage).with_commit_history(Arc::new(InMemoryHistory::new(mature)));
        let rules = guardian.mine_rules_calibrated().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert!(
            rules[0].confidence > baseline[0].confidence,
            "30 astensioni devono ALZARE la confidenza: {} vs baseline {}",
            rules[0].confidence,
            baseline[0].confidence
        );
        assert!(rules[0].confidence > 0.85, "conf = {}", rules[0].confidence);
    }

    #[tokio::test]
    async fn scarce_history_lowers_confidence_below_the_structural_baseline() {
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();

        // Solo 2 occasioni: l'asimmetria strutturale (3 archi) potrebbe essere una
        // coincidenza di un grafo giovane. Il tempo lo rivela e la confidenza scende
        // SOTTO il baseline strutturale — cosa che l'euristica non sapeva fare.
        let scarce = vec![
            Commit::new([api_file.clone(), core_file.clone()]),
            Commit::new([api_file, core_file]),
        ];
        let guardian =
            Guardian::new(storage).with_commit_history(Arc::new(InMemoryHistory::new(scarce)));
        let rules = guardian.mine_rules_calibrated().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert!(
            rules[0].confidence < STRUCTURAL_CONF_SUPPORT_3,
            "poca esposizione deve ABBASSARE la confidenza: {}",
            rules[0].confidence
        );
    }

    #[tokio::test]
    async fn without_history_confidence_stays_structural() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        // Nessuna storia agganciata: mine_rules_calibrated degrada a mine_rules.
        let rules = guardian.mine_rules_calibrated().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert!((rules[0].confidence - STRUCTURAL_CONF_SUPPORT_3).abs() < 1e-6);
    }

    #[tokio::test]
    async fn history_without_co_changes_keeps_structural_confidence() {
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, _api, _core) = seeded_two_layer_graph().await;
        // Commit che toccano file ESTRANEI ai layer noti: zero occasioni. L'assenza
        // di evidenza temporale non deve abbassare la regola, solo lasciarla com'è.
        let unrelated = vec![
            Commit::new(["docs/readme.md"]),
            Commit::new(["ci/pipeline.yml"]),
        ];
        let guardian =
            Guardian::new(storage).with_commit_history(Arc::new(InMemoryHistory::new(unrelated)));
        let rules = guardian.mine_rules_calibrated().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert!(
            (rules[0].confidence - STRUCTURAL_CONF_SUPPORT_3).abs() < 1e-6,
            "conf = {}",
            rules[0].confidence
        );
    }

    // ---------------------------------------------------------------------------
    // Fossili di Decisione: la nascita del confine (l'asse dell'INTENTO).
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn fossils_recovers_the_birth_of_the_boundary() {
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();

        // Newest-first (come git log): l'ultimo commit è la nascita del confine.
        let history = InMemoryHistory::new(vec![
            Commit::with_meta(
                "newer0",
                200,
                "later refactor",
                [api_file.clone(), core_file.clone()],
            ),
            Commit::with_meta(
                "old00birth",
                100,
                "draw the boundary",
                [api_file.clone(), core_file.clone()],
            ),
        ]);
        let guardian = Guardian::new(storage).with_commit_history(Arc::new(history));

        let fossils = guardian.fossils().await.unwrap();
        assert_eq!(fossils.len(), 1, "un fossile atteso: {fossils:?}");
        let f = &fossils[0];
        assert_eq!(f.born_at, "old00birth");
        assert_eq!(f.born_at_unix, 100);
        assert_eq!(f.intent, "draw the boundary");
        assert_eq!(f.upstream, "app::core");
        assert_eq!(f.downstream, "app::api");
        let common_prefix = find_common_prefix([&api_file, &core_file].into_iter());
        let api_rel = api_file
            .strip_prefix(&common_prefix)
            .unwrap_or(&api_file)
            .to_string();
        let core_rel = core_file
            .strip_prefix(&common_prefix)
            .unwrap_or(&core_file)
            .to_string();
        assert!(f.born_structure.contains(&api_rel));
        assert!(f.born_structure.contains(&core_rel));
    }

    #[tokio::test]
    async fn fossils_is_empty_without_history() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        assert!(guardian.fossils().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn why_tells_the_boundary_story_most_recent_first_verbatim() {
        // Crono-Semantic Mining: `why` non si ferma alla NASCITA (spesso un commit
        // iniziale poco eloquente) — racconta la STORIA del confine: i commit che
        // l'hanno esercitato, dal più recente, con l'intento VERBATIM dell'autore.
        use codeos_paleo::{Commit, InMemoryHistory};
        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();
        let history = InMemoryHistory::new(vec![
            Commit::with_meta(
                "birthC0mmit",
                100,
                "draw the boundary",
                [api_file.clone(), core_file.clone()],
            ),
            // Tocca solo un lato: NON è un'occasione, non entra nella storia.
            Commit::with_meta("apionly", 150, "api refactor", [api_file.clone()]),
            Commit::with_meta(
                "reinf0rce",
                200,
                "reinforce the api-core contract",
                [core_file, api_file],
            ),
        ]);
        let guardian = Guardian::new(storage).with_commit_history(Arc::new(history));

        let resp = guardian.why("app::api|app::core").await.unwrap();
        assert_eq!(
            resp.boundary_story.len(),
            2,
            "solo le occasioni vere (co-toccano ENTRAMBI i lati): {:?}",
            resp.boundary_story
        );
        assert!(
            resp.boundary_story[0].contains("reinf0rce")
                && resp.boundary_story[0].contains("reinforce the api-core contract"),
            "il più recente prima, con hash e intento verbatim: {:?}",
            resp.boundary_story
        );
        assert!(
            resp.boundary_story[1].contains("birthC0mmit"),
            "la nascita chiude la storia: {:?}",
            resp.boundary_story
        );

        // Senza storia git: storia vuota, mai inventata.
        let (storage2, _a, _c) = seeded_two_layer_graph().await;
        let bare = Guardian::new(storage2);
        let resp2 = bare.why("app::api|app::core").await.unwrap();
        assert!(resp2.boundary_story.is_empty());
    }

    #[tokio::test]
    async fn excavate_boundary_recovers_an_arbitrary_boundary_from_history() {
        // Il MOAT: `why` non è più inerte sui confini arbitrari. `excavate_boundary`
        // scava il confine api↔core dalla storia — il commit che l'ha disegnato + il
        // suo intento — anche quando NON lo si interroga come invariante scoperto.
        use codeos_paleo::{Commit, InMemoryHistory};
        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();
        let history = InMemoryHistory::new(vec![Commit::with_meta(
            "birthC0mmit",
            100,
            "draw the boundary",
            [api_file, core_file],
        )]);
        let guardian = Guardian::new(storage).with_commit_history(Arc::new(history));

        let fossil = guardian
            .excavate_boundary("app::api", "app::core")
            .await
            .unwrap()
            .expect("il confine è co-toccato dal commit: deve esserci un fossile");
        assert_eq!(fossil.born_at, "birthC0mmit");
        assert_eq!(fossil.intent, "draw the boundary");
    }

    #[tokio::test]
    async fn excavate_boundary_is_none_without_history_or_co_touch() {
        let (storage, _a, _c) = seeded_two_layer_graph().await;
        // Senza storia git: niente da datare, None onesto (non un confine inventato).
        let guardian = Guardian::new(storage);
        assert!(guardian
            .excavate_boundary("app::api", "app::core")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn invariant_staleness_measures_time_since_last_exercise() {
        // Rischio temporale (Guardian 2.0). Il confine api↔core è esercitato
        // (co-toccato) al commit di nascita a ts 100; il commit più recente (HEAD, ts
        // 500) tocca SOLO api ⇒ NON è un'occasione. Staleness = 500 - 100 = 400: alta
        // confidenza, ma non esercitato di recente.
        use codeos_paleo::{Commit, InMemoryHistory};
        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();
        let history = InMemoryHistory::new(vec![
            Commit::with_meta("recentHEAD", 500, "touch api only", [api_file.clone()]),
            Commit::with_meta("birth", 100, "draw boundary", [api_file, core_file]),
        ]);
        let guardian = Guardian::new(storage).with_commit_history(Arc::new(history));

        let stale = guardian.invariant_staleness().await.unwrap();
        let entry = stale
            .iter()
            .find(|s| {
                (s.upstream == "app::core" && s.downstream == "app::api")
                    || (s.upstream == "app::api" && s.downstream == "app::core")
            })
            .expect("il confine api↔core deve avere un profilo temporale");
        assert_eq!(
            entry.last_exercised_unix, 100,
            "ultimo esercizio = il commit di nascita (HEAD tocca solo api, non è occasione)"
        );
        assert_eq!(entry.staleness_secs, 400, "now(500) - last(100) = 400");
        assert!(
            entry.confidence > 0.0,
            "la confidenza Wilson resta intatta, il tempo la qualifica soltanto"
        );
    }

    #[tokio::test]
    async fn invariant_staleness_is_empty_without_history() {
        let (storage, _a, _c) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        assert!(guardian.invariant_staleness().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn context_pack_for_ai_uses_a_compact_format() {
        // Il flag `--for ai` non è più un no-op (collaudo): produce un formato COMPATTO
        // chiave:valore, diverso dal Markdown ricco per umano.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let guardian = Guardian::new(storage);
        let human = guardian.get_context_pack("qualcosa", false).await.unwrap();
        let ai = guardian.get_context_pack("qualcosa", true).await.unwrap();

        assert_ne!(
            human.formatted_markdown, ai.formatted_markdown,
            "i due formati devono differire: il flag non è più un no-op"
        );
        assert!(
            human.formatted_markdown.contains("## 1."),
            "il formato umano è Markdown ricco con intestazioni numerate"
        );
        assert!(
            ai.formatted_markdown.contains("GOAL:") && ai.formatted_markdown.contains("FILES:"),
            "il formato AI è compatto chiave:valore"
        );
        assert!(
            !ai.formatted_markdown.contains("## 1."),
            "il formato AI non ha intestazioni Markdown"
        );
    }

    #[tokio::test]
    async fn context_pack_carries_the_human_why_from_the_ledger() {
        use codeos_memory::{Decision, DecisionKind, DecisionStore, InMemoryDecisionStore};
        use codeos_types::bus::NewDecision;

        // Il giro del moat sull'ULTIMA superficie agent-facing: una decisione UMANA
        // registrata con `decide --tags <modulo>` deve comparire nel context pack
        // (riga WHY nel formato ai, sezione «Perché» nel Markdown) quando il goal
        // tocca quel modulo — e NON comparire su un goal estraneo (niente rumore).
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let store = Arc::new(InMemoryDecisionStore::new());
        store
            .record(&Decision::from_new(
                NewDecision {
                    author: "human:Richard".into(),
                    title: "api non chiama il database direttamente".into(),
                    context: String::new(),
                    rationale: "tutto passa dal layer core".into(),
                    related_entity_ids: vec![],
                    related_decision_ids: vec![],
                    supersedes: vec![],
                    deprecates: vec![],
                    tags: vec!["api".into()],
                },
                DecisionKind::Decision,
            ))
            .await
            .unwrap();
        let guardian = Guardian::with_memory(storage, store);

        // Goal che tocca il modulo taggato: il perché entra, in ENTRAMBI i formati.
        let ai = guardian
            .get_context_pack("handler api", true)
            .await
            .unwrap();
        assert!(
            ai.decisions
                .iter()
                .any(|d| d.contains("api non chiama il database")),
            "la decisione umana deve essere nel pack: {:?}",
            ai.decisions
        );
        assert!(
            ai.formatted_markdown.contains("WHY:")
                && ai.formatted_markdown.contains("api non chiama il database"),
            "il formato ai porta la riga WHY:\n{}",
            ai.formatted_markdown
        );
        let md = guardian
            .get_context_pack("handler api", false)
            .await
            .unwrap();
        assert!(
            md.formatted_markdown.contains("Perché è fatto così")
                && md.formatted_markdown.contains("api non chiama il database"),
            "il Markdown porta la sezione del perché:\n{}",
            md.formatted_markdown
        );

        // Goal estraneo (il tag "api" non è un segmento dei suoi qualname): niente
        // perché — il matching per segmento non inonda.
        let other = guardian
            .get_context_pack("core service", true)
            .await
            .unwrap();
        assert!(
            !other
                .decisions
                .iter()
                .any(|d| d.contains("api non chiama il database")),
            "su un goal estraneo la decisione NON deve comparire: {:?}",
            other.decisions
        );
    }

    /// Guard di REGRESSIONE end-to-end della PRECISIONE della retrieval (istituziona-
    /// lizza i risultati misurati a mano in eval/moat-benchmark/scaled/RETRIEVAL_PRECISION.md
    /// e blinda il fix anti-flood `7dac120` sull'intero percorso del pack). Tre moduli,
    /// qualified_name CON un segmento `src` (il vettore del flood): un tag `src`
    /// aggancerebbe OGNI entità. Il pack deve: (a) far emergere la decisione del modulo
    /// giusto (recall), (b) NON far comparire mai il tag strutturale `src` (niente
    /// flood), (c) non sconfinare in un altro modulo (niente leak). Senza il fix questo
    /// test fallisce: la decisione `src` comparirebbe su ogni goal.
    #[tokio::test]
    async fn context_pack_retrieval_is_precise_and_a_structural_tag_never_floods() {
        use codeos_memory::{Decision, DecisionKind, DecisionStore, InMemoryDecisionStore};
        use codeos_types::bus::NewDecision;

        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let ents: Vec<Entity> = [
            "shop::src::billing::charge::run",
            "shop::src::billing::refund::run",
            "shop::src::auth::login::run",
            "shop::src::auth::logout::run",
            "shop::src::core::db::run",
            "shop::src::core::cache::run",
        ]
        .iter()
        .map(|q| entity(q))
        .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: ents,
                ..Default::default()
            })
            .await
            .unwrap();

        let store = Arc::new(InMemoryDecisionStore::new());
        let mk = |title: &str, tags: &[&str]| {
            Decision::from_new(
                NewDecision {
                    author: "human:test".into(),
                    title: title.into(),
                    context: String::new(),
                    rationale: "perché".into(),
                    related_entity_ids: vec![],
                    related_decision_ids: vec![],
                    supersedes: vec![],
                    deprecates: vec![],
                    tags: tags.iter().map(|s| s.to_string()).collect(),
                },
                DecisionKind::Decision,
            )
        };
        store
            .record(&mk("addebito idempotente mai due volte", &["billing"]))
            .await
            .unwrap();
        store
            .record(&mk("nota strutturale che non deve floodare", &["src"]))
            .await
            .unwrap();
        store
            .record(&mk("sessione validata lato server", &["auth"]))
            .await
            .unwrap();

        let guardian = Guardian::with_memory(storage, store);

        // Goal sul modulo billing.
        let billing = guardian.get_context_pack("billing charge", true).await.unwrap();
        let b = billing.decisions.join(" | ");
        assert!(b.contains("addebito idempotente"), "recall: manca la decisione billing: {b}");
        assert!(!b.contains("non deve floodare"), "FLOOD: il tag strutturale `src` non deve comparire: {b}");
        assert!(!b.contains("sessione validata"), "leak: la decisione auth non c'entra col billing: {b}");

        // Goal sul modulo auth: simmetrico.
        let auth = guardian.get_context_pack("auth login", true).await.unwrap();
        let a = auth.decisions.join(" | ");
        assert!(a.contains("sessione validata"), "recall: manca la decisione auth: {a}");
        assert!(!a.contains("non deve floodare"), "FLOOD: `src` non deve comparire neanche qui: {a}");
        assert!(!a.contains("addebito idempotente"), "leak: la decisione billing non c'entra con l'auth: {a}");
    }

    #[tokio::test]
    async fn learn_anchors_the_promotion_to_its_birth_commit() {
        use codeos_memory::InMemoryDecisionStore;
        use codeos_paleo::{Commit, InMemoryHistory};

        let (storage, api, core) = seeded_two_layer_graph().await;
        let api_file = api[0].location.file_path.clone();
        let core_file = core[0].location.file_path.clone();

        let history = InMemoryHistory::new(vec![
            Commit::with_meta(
                "zzz999",
                200,
                "refactor api handlers",
                [api_file.clone(), core_file.clone()],
            ),
            Commit::with_meta(
                "birth01",
                100,
                "introduce api layer over core",
                [api_file.clone(), core_file.clone()],
            ),
        ]);
        let store = Arc::new(InMemoryDecisionStore::new());
        let guardian =
            Guardian::with_memory(storage, store.clone()).with_commit_history(Arc::new(history));

        assert_eq!(guardian.learn().await.unwrap().len(), 1);

        let all = store.all().await.unwrap();
        assert_eq!(all.len(), 1);
        let promo = &all[0];
        // Il contesto è ancorato al commit di nascita e ne cita l'intento.
        assert!(promo.context.contains("birth01"), "ctx = {}", promo.context);
        assert!(
            promo.context.contains("introduce api layer over core"),
            "ctx = {}",
            promo.context
        );
        assert!(promo.context.contains("Fossile di Decisione"));
        // Il tag rende il fossile filtrabile dal Query.
        assert!(
            promo.tags.iter().any(|t| t == "born-at:birth01"),
            "tags = {:?}",
            promo.tags
        );
        // Il fossile non è solo prosa nel contesto: è evidenza STRUTTURATA che
        // viaggia nella Decision. Gli archi qui sono nudi (nessun qname), quindi
        // l'unica evidenza è il commit di nascita citato per hash.
        assert_eq!(
            promo.evidence,
            vec![codeos_memory::Evidence::Commit("birth01".to_string())],
            "evidenza = {:?}",
            promo.evidence
        );

        // Idempotente: l'arricchimento col fossile non altera la chiave di dedup.
        assert!(guardian.learn().await.unwrap().is_empty());
        assert_eq!(store.all().await.unwrap().len(), 1);
    }

    // ---------------------------------------------------------------------------
    // Spazio negativo del 2° ordine: l'invariante che MANCA dove dovrebbe esserci.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn flags_a_missing_invariant_against_an_established_foundation() {
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let api: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::api::h_{i}::run")))
            .collect();
        let web: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::web::v_{i}::show")))
            .collect();
        let jobs: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::jobs::j_{i}::work")))
            .collect();
        let core: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::core::s_{i}::do_it")))
            .collect();

        // api/web/jobs dipendono da core a senso unico; core → jobs è il ritorno
        // anomalo che lascia scoperto l'invariante atteso (core, jobs).
        let mut relations: Vec<Relation> = Vec::new();
        for i in 0..3 {
            relations.push(relation(RelationKind::Calls, api[i].id, core[i].id));
            relations.push(relation(RelationKind::Calls, web[i].id, core[i].id));
            relations.push(relation(RelationKind::Calls, jobs[i].id, core[i].id));
        }
        relations.push(relation(RelationKind::Calls, core[0].id, jobs[0].id));

        storage
            .apply_delta(GraphDelta {
                added_entities: api
                    .iter()
                    .chain(web.iter())
                    .chain(jobs.iter())
                    .chain(core.iter())
                    .cloned()
                    .collect(),
                added_relations: relations,
                ..Default::default()
            })
            .await
            .unwrap();

        let guardian = Guardian::new(storage);
        let gaps = guardian.missing_invariants().await.unwrap();
        assert_eq!(gaps.len(), 1, "gaps = {gaps:?}");
        assert_eq!(gaps[0].upstream, LayerKey("app::core".to_string()));
        assert_eq!(gaps[0].downstream, LayerKey("app::jobs".to_string()));
        // core è rispettato a senso unico da api e web ⇒ support 2.
        assert_eq!(gaps[0].foundation_support, 2);
    }

    #[tokio::test]
    async fn no_missing_invariant_in_a_clean_two_layer_graph() {
        let (storage, _api, _core) = seeded_two_layer_graph().await;
        let guardian = Guardian::new(storage);
        // Una sola fondazione rispettata da un solo layer: niente convenzione, niente buco.
        assert!(guardian.missing_invariants().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn blast_radius_counts_distinct_dependent_entities_not_edges() {
        // Un solo dipendente, ma collegato al bersaglio via DUE archi (Calls +
        // Imports). Il blast radius onesto è 1 entità impattata, non 2 archi:
        // deduplicare per id di relazione (il vecchio comportamento) dava 2.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let target = entity("app::core::widget::build");
        let dependent = entity("app::api::caller::run");
        // Entità di rumore per diluire la frequenza delle keyword.
        let noise_a = entity("app::core::other_a::xx");
        let noise_b = entity("app::core::other_b::yy");
        storage
            .apply_delta(GraphDelta {
                added_entities: vec![target.clone(), dependent.clone(), noise_a, noise_b],
                added_relations: vec![
                    relation(RelationKind::Calls, dependent.id, target.id),
                    relation(RelationKind::Imports, dependent.id, target.id),
                ],
                ..Default::default()
            })
            .await
            .unwrap();
        let guardian = Guardian::new(storage);

        let res = guardian.guard_before("build").await.unwrap();
        assert_eq!(
            res.blast_radius, 1,
            "una sola entità dipende dal bersaglio, anche se via due archi (blast={})",
            res.blast_radius
        );
    }

    #[tokio::test]
    async fn non_discriminative_keyword_does_not_inflate_blast_radius() {
        // "app" compare in OGNI qualified_name: non localizza nulla e non deve
        // diventare un bersaglio, altrimenti il blast radius esplode all'intero
        // grafo. Una keyword specifica invece continua a funzionare.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let target = entity("app::svc::pay::charge");
        let callers: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::ui::screen_{i}::run")))
            .collect();
        let rels: Vec<Relation> = callers
            .iter()
            .map(|c| relation(RelationKind::Calls, c.id, target.id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: std::iter::once(target.clone())
                    .chain(callers.iter().cloned())
                    .collect(),
                added_relations: rels,
                ..Default::default()
            })
            .await
            .unwrap();
        let guardian = Guardian::new(storage);

        // Keyword specifica: trova i 3 chiamanti distinti.
        let specific = guardian.guard_before("charge").await.unwrap();
        assert_eq!(
            specific.blast_radius, 3,
            "il bersaglio 'charge' è dipeso da 3 entità distinte (blast={})",
            specific.blast_radius
        );

        // Keyword universale: "app" matcha tutto → scartata → nessun bersaglio,
        // niente esplosione globale.
        let vague = guardian.guard_before("app").await.unwrap();
        assert_eq!(
            vague.blast_radius, 0,
            "una keyword che matcha l'intero grafo non deve produrre impatto (blast={})",
            vague.blast_radius
        );

        // …e blast radius 0 senza bersaglio NON deve mai presentarsi come
        // «modifica sicura»: dev'essere un onesto «non lo so». La vecchia frase
        // rassicurante era una falsa sicurezza.
        assert!(
            !vague.safe_path.contains("Nessun modulo a rischio"),
            "il vecchio messaggio rassicurante non deve più comparire: {}",
            vague.safe_path
        );
        assert!(
            vague.safe_path.contains("non lo so"),
            "un goal non localizzato deve ammettere l'incertezza, non rassicurare: {}",
            vague.safe_path
        );
    }

    #[tokio::test]
    async fn why_does_not_invent_a_boundary_that_was_never_born() {
        // Senza storia non c'è alcun fossile: il confine 'alpha|beta' non è mai
        // "nato". Il vecchio codice stampava comunque "è nato nel commit  in data ."
        // con i campi vuoti — un confine inventato. Ora lo ammette.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let guardian = Guardian::new(storage);

        let resp = guardian.why("alpha|beta").await.unwrap();

        assert!(
            resp.born_commit.is_empty(),
            "nessun fossile: born_commit deve restare vuoto, era {:?}",
            resp.born_commit
        );
        assert!(
            !resp.explanation.contains("è nato nel commit"),
            "non si afferma una nascita che non esiste: {}",
            resp.explanation
        );
        assert!(
            resp.explanation.contains("non risulta"),
            "un confine assente va dichiarato assente, non inventato: {}",
            resp.explanation
        );
    }

    #[tokio::test]
    async fn why_with_a_single_name_does_not_flood_unrelated_decisions() {
        use codeos_memory::{Decision, DecisionKind, DecisionStore, InMemoryDecisionStore};

        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let store = Arc::new(InMemoryDecisionStore::new());
        let mk = |title: &str, tags: Vec<String>| Decision {
            id: EntityId::new(),
            kind: DecisionKind::Decision,
            author: "human:Marco".to_string(),
            title: title.to_string(),
            context: String::new(),
            rationale: "isolare il dominio".to_string(),
            related_entity_ids: vec![],
            related_decision_ids: vec![],
            supersedes: vec![],
            deprecates: vec![],
            evidence: vec![],
            tags,
            timestamp: "2024-01-01T00:00:00Z".to_string(),
        };
        // (a) cita UN SOLO estremo del confine ("payments"): NON è una decisione "del
        //     confine payments↔orders". Col contratto largo (OR) inondava `why`.
        store
            .record(&mk(
                "Adottare l'architettura esagonale",
                vec!["payments".into()],
            ))
            .await
            .unwrap();
        // (b) cita ENTRAMBI gli estremi (via tag): È la decisione del confine.
        store
            .record(&mk(
                "payments non deve dipendere da orders a runtime",
                vec!["payments".into(), "orders".into()],
            ))
            .await
            .unwrap();
        let guardian = Guardian::with_memory(storage, store);

        // Un solo nome → downstream vuoto. Col vecchio `contains("")` ogni decisione con
        // un tag qualsiasi finiva fra le "correlate".
        let single = guardian.why("foo").await.unwrap();
        assert!(
            single.markdown_decisions.is_empty(),
            "un solo nome non deve agganciare decisioni estranee: {:?}",
            single.markdown_decisions
        );
        assert!(
            single.explanation.contains("due estremi"),
            "con un solo nome why deve chiedere l'espressione 'a|b': {}",
            single.explanation
        );

        // Contratto CORRETTO (anti-flood): `why "payments|orders"` emerge SOLO la
        // decisione che cita ENTRAMBI gli estremi (b), non quella che ne cita uno (a).
        // La vecchia regola (un tag qualsiasi combacia) inondava col crescere del ledger
        // (es. ogni invariante auto-promosso che toccava un estremo) — verificato sul
        // reale: `why` di un confine mostrava 8 decisioni invece di 1.
        let paired = guardian.why("payments|orders").await.unwrap();
        assert_eq!(
            paired.markdown_decisions.len(),
            1,
            "deve emergere SOLO la decisione che cita entrambi gli estremi: {:?}",
            paired.markdown_decisions
        );
        assert!(
            paired.markdown_decisions[0].contains("payments non deve dipendere da orders"),
            "la decisione del confine (entrambi gli estremi) è quella giusta: {:?}",
            paired.markdown_decisions
        );
    }

    #[tokio::test]
    async fn context_pack_does_not_report_low_risk_when_the_goal_is_unlocalized() {
        // "app" compare ovunque → scartata da select_target_entities → nessun
        // bersaglio. blast_count 0 darebbe rischio "low": una falsa sicurezza
        // servita dritta a un'AI. Deve invece essere "unknown", e il markdown deve dirlo.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let target = entity("app::svc::pay::charge");
        let callers: Vec<Entity> = (0..3)
            .map(|i| entity(&format!("app::ui::screen_{i}::run")))
            .collect();
        let rels: Vec<Relation> = callers
            .iter()
            .map(|c| relation(RelationKind::Calls, c.id, target.id))
            .collect();
        storage
            .apply_delta(GraphDelta {
                added_entities: std::iter::once(target.clone())
                    .chain(callers.iter().cloned())
                    .collect(),
                added_relations: rels,
                ..Default::default()
            })
            .await
            .unwrap();
        let guardian = Guardian::new(storage);

        let vague = guardian.get_context_pack("app", true).await.unwrap();
        assert_eq!(
            vague.estimated_risk, "unknown",
            "un goal non localizzato non è a basso rischio, è non valutabile: {}",
            vague.estimated_risk
        );
        assert!(
            vague.relevant_entities.is_empty(),
            "nessun bersaglio: niente entità rilevanti, erano {:?}",
            vague.relevant_entities
        );
        assert!(
            vague.formatted_markdown.contains("non localizzato"),
            "il markdown deve avvertire che il goal non è localizzato: {}",
            vague.formatted_markdown
        );
        assert!(
            !vague.formatted_markdown.contains("rischio:** LOW"),
            "il rischio non deve apparire come LOW per un goal non localizzato: {}",
            vague.formatted_markdown
        );

        // Anti-regressione: un goal localizzato torna a un rischio concreto, non «unknown».
        let precise = guardian.get_context_pack("charge", true).await.unwrap();
        assert_ne!(
            precise.estimated_risk, "unknown",
            "un goal localizzato deve avere un rischio concreto, non «unknown»"
        );
        assert!(
            !precise.relevant_entities.is_empty(),
            "un goal localizzato deve avere entità rilevanti"
        );
    }

    #[tokio::test]
    async fn simulate_does_not_emit_a_plan_for_an_unknown_source() {
        // La sorgente non esiste nel grafo: la vecchia simulate emetteva comunque
        // rischi e un piano in 4 passi con dipendenze vuote — letto come «spostamento
        // facile». Ora dichiara la sorgente sconosciuta e NON propone alcun piano.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        storage
            .apply_delta(GraphDelta {
                added_entities: vec![entity("app::svc::pay::charge")],
                ..Default::default()
            })
            .await
            .unwrap();
        let guardian = Guardian::new(storage);

        let sim = guardian
            .simulate("move ghostmodule to app::svc::billing")
            .await
            .unwrap();

        assert!(
            sim.recommendation_plan.is_empty(),
            "nessun piano per una sorgente che non esiste: {:?}",
            sim.recommendation_plan
        );
        assert!(
            sim.dependencies_to_rewrite.is_empty(),
            "nessuna dipendenza da riscrivere per una sorgente assente: {:?}",
            sim.dependencies_to_rewrite
        );
        assert!(
            sim.risks
                .iter()
                .any(|r| r.contains("Nessuna entità corrisponde")),
            "deve dichiarare la sorgente sconosciuta, non fingere sicurezza: {:?}",
            sim.risks
        );
    }

    #[tokio::test]
    async fn simulate_treats_a_self_move_as_a_noop() {
        // Spostare un modulo su sé stesso non è un refactor: è un no-op. La vecchia
        // simulate produceva "Il confine di 'pay' verrà fuso con 'pay'" e un piano.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let guardian = Guardian::new(storage);

        let sim = guardian.simulate("move pay to pay").await.unwrap();

        assert!(
            sim.changed_boundaries.is_empty(),
            "un no-op non fonde alcun confine: {:?}",
            sim.changed_boundaries
        );
        assert!(
            sim.recommendation_plan.is_empty(),
            "un no-op non richiede un piano: {:?}",
            sim.recommendation_plan
        );
        assert!(
            sim.risks
                .iter()
                .any(|r| r.contains("non c'è nulla da spostare")),
            "deve riconoscere il no-op: {:?}",
            sim.risks
        );
    }

    #[tokio::test]
    async fn simulate_still_plans_a_real_move() {
        // Anti-regressione: sorgente reale e destinazione diversa → la simulate
        // legittima continua a produrre dipendenze da riscrivere e un piano.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let target_ent = entity("app::svc::pay::charge");
        let caller = entity("app::ui::screen::run");
        let rel = relation(RelationKind::Calls, caller.id, target_ent.id);
        storage
            .apply_delta(GraphDelta {
                added_entities: vec![target_ent.clone(), caller.clone()],
                added_relations: vec![rel],
                ..Default::default()
            })
            .await
            .unwrap();
        let guardian = Guardian::new(storage);

        let sim = guardian
            .simulate("move charge to app::svc::billing")
            .await
            .unwrap();

        assert!(
            !sim.recommendation_plan.is_empty(),
            "uno spostamento reale deve avere un piano"
        );
        assert!(
            !sim.dependencies_to_rewrite.is_empty(),
            "uno spostamento reale con un chiamante deve elencare dipendenze: {:?}",
            sim.dependencies_to_rewrite
        );
    }

    #[test]
    fn default_base_ref_detects_master_only_repos() {
        // L'unico vero bug della campagna dei 50 progetti: `mri` senza --base
        // assumeva `main` e falliva exit-128 sui repo storici con `master`
        // (anyhow). Il rilevamento deve trovare `master` quando `main` non c'è —
        // e None su una dir non-git (il chiamante fallisce onestamente).
        let tmp = std::env::temp_dir().join(format!("codeos-mri-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&tmp)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
        };
        git(&["init", "-q", "--initial-branch=master"]);
        std::fs::write(tmp.join("a.txt"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "primo"]);

        assert_eq!(
            default_base_ref(&tmp).as_deref(),
            Some("master"),
            "su un repo solo-master il default rilevato è master"
        );

        let non_git = tmp.join("sotto-non-git");
        std::fs::create_dir_all(&non_git).unwrap();
        // (una sottodir di un repo git è ancora nel repo: usa una dir davvero fuori)
        let outside = std::env::temp_dir().join(format!("codeos-nongit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        assert_eq!(
            default_base_ref(&outside),
            None,
            "fuori da un repo git non c'è un default da rilevare"
        );

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn pr_mri_refuses_a_clean_bill_of_health_when_the_diff_fails() {
        // Ref inesistente → 'git diff' fallisce. Il vecchio codice ingoiava l'errore
        // e restituiva 0 violazioni e rischio "low": un referto pulito su un'analisi
        // mai avvenuta. In ogni ambiente (ref errato, git assente, dir non-git) il
        // diff non si ottiene, quindi mri deve fallire onestamente, non rassicurare.
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let guardian = Guardian::new(storage);

        let result = guardian
            .pr_mri("codeos_ref_inesistente_per_test_xyz", "HEAD")
            .await;

        assert!(
            result.is_err(),
            "un diff non calcolabile deve produrre un errore, non un falso 'low risk'"
        );
    }
}
