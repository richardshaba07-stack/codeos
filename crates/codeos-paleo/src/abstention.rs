//! Il **Campo di Astensione**: la matematica.
//!
//! Due funzioni pure e un piccolo tipo dato. Tutto l'I/O (leggere git) vive in
//! [`crate::history`]; qui si conta soltanto.

use std::collections::{HashMap, HashSet};

use crate::history::Commit;

/// Punteggio z per un intervallo di confidenza al 95% (lower bound di Wilson).
pub const Z_95: f64 = 1.959_963_984_540_054;

/// La statistica di astensione di un invariante, misurata sul *negativo del tempo*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Abstention {
    /// Quanti commit hanno toccato **entrambi** i layer: ogni occasione è una
    /// possibilità concreta di cablare l'arco proibito. È la dimensione del campione.
    pub occasions: u32,
    /// In quante di quelle occasioni la freccia è stata **invertita** (violazioni
    /// note). Per una regola appena scoperta è 0 — la direzione proibita ha zero
    /// archi nel grafo, ed è proprio questo a farla emergere.
    pub violations: u32,
}

impl Abstention {
    pub fn new(occasions: u32, violations: u32) -> Self {
        Self {
            occasions,
            violations,
        }
    }

    /// Le astensioni: occasioni in cui il team **non** ha invertito la freccia.
    /// Saturante (le violazioni non possono mai superare le occasioni: ogni
    /// violazione È un'occasione).
    pub fn abstentions(&self) -> u32 {
        self.occasions.saturating_sub(self.violations)
    }

    /// **Lower bound di Wilson** del tasso di astensione `abstentions/occasions`.
    ///
    /// Perché il lower bound e non la media `p̂`? Perché vogliamo una confidenza
    /// *prudente* che premi l'esposizione: 30 astensioni su 30 occasioni valgono
    /// più di 2 su 2, anche se la media è 1.0 in entrambi i casi. L'intervallo di
    /// Wilson si restringe verso 1.0 al crescere del campione — esattamente la
    /// proprietà che ci serve. Con `occasions == 0` non c'è evidenza temporale:
    /// ritorna 0.0.
    pub fn wilson_lower_bound(&self, z: f64) -> f64 {
        let n = self.occasions as f64;
        if n == 0.0 {
            return 0.0;
        }
        let p = self.abstentions() as f64 / n;
        let z2 = z * z;
        let denom = 1.0 + z2 / n;
        let center = (p + z2 / (2.0 * n)) / denom;
        let margin = (z / denom) * ((p * (1.0 - p) / n) + (z2 / (4.0 * n * n))).sqrt();
        (center - margin).clamp(0.0, 1.0)
    }
}

/// Conta le **occasioni**: i commit che hanno toccato almeno un file del layer
/// `layer_a` **e** almeno un file del layer `layer_b`.
///
/// `file_layers` mappa ogni path al/ai layer che vi sono definiti (un file può
/// ospitare entità di più layer, raro ma gestito). La funzione è pura e simmetrica
/// in `layer_a`/`layer_b`.
pub fn occasions(
    layer_a: &str,
    layer_b: &str,
    file_layers: &HashMap<String, HashSet<String>>,
    commits: &[Commit],
) -> u32 {
    commits
        .iter()
        .filter(|commit| co_touches(commit, layer_a, layer_b, file_layers))
        .count() as u32
}

/// Il **profilo temporale** delle occasioni di un invariante: non solo QUANTE (il
/// conteggio di [`occasions`]) ma QUANDO — la prima e l'ultima volta che i due layer
/// sono stati co-toccati. È la dimensione temporale del rischio (Guardian 2.0): una
/// confidenza alta ma esercitata l'ultima volta molto tempo fa è "battle-tested ma
/// forse stantia", diversa da una esercitata di recente.
///
/// NON sostituisce il Campo di Astensione (trap #2): lo **qualifica** col tempo,
/// lasciando intatto il lower bound di Wilson. L'`occasions` qui dentro è identico a
/// quello di [`occasions`]; ciò che aggiunge è `first_ts`/`last_ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccasionWindow {
    /// Quante occasioni (= [`occasions`]): la dimensione del campione.
    pub occasions: u32,
    /// Timestamp Unix dell'occasione più VECCHIA (la nascita del confine).
    pub first_ts: i64,
    /// Timestamp Unix dell'occasione più RECENTE (l'ultima volta esercitato).
    pub last_ts: i64,
}

impl OccasionWindow {
    /// Da quanti secondi l'invariante non è esercitato, rispetto a `now` (di solito
    /// l'istante del commit più recente del repo). Saturante a 0: un'occasione "nel
    /// futuro" rispetto a `now` non produce staleness negativa. Più alto ⇒ più
    /// stantio: l'esposizione c'è stata, ma non di recente.
    pub fn staleness_secs(&self, now: i64) -> i64 {
        (now - self.last_ts).max(0)
    }
}

/// Come [`occasions`], ma restituisce il **profilo temporale** delle occasioni:
/// conteggio + timestamp della prima e dell'ultima. `None` se nessuna occasione —
/// nessuna evidenza temporale, niente da datare e niente inventato (stessa onestà di
/// [`occasions`] che ritorna 0, e dello 0.0 di [`Abstention::wilson_lower_bound`]
/// senza esposizione).
pub fn occasion_window(
    layer_a: &str,
    layer_b: &str,
    file_layers: &HashMap<String, HashSet<String>>,
    commits: &[Commit],
) -> Option<OccasionWindow> {
    let mut occasions = 0u32;
    let mut first_ts = i64::MAX;
    let mut last_ts = i64::MIN;
    for commit in commits {
        if co_touches(commit, layer_a, layer_b, file_layers) {
            occasions += 1;
            first_ts = first_ts.min(commit.timestamp);
            last_ts = last_ts.max(commit.timestamp);
        }
    }
    if occasions == 0 {
        return None;
    }
    Some(OccasionWindow {
        occasions,
        first_ts,
        last_ts,
    })
}

/// Un'occasione della STORIA di un confine: un commit che ha co-toccato entrambi i
/// layer, con l'intento dichiarato dall'autore (il soggetto, verbatim). È l'unità del
/// Crono-Semantic Mining: niente è inventato — hash, istante e messaggio sono citazioni.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryOccasion {
    /// Hash del commit (citabile/verificabile con `git show`).
    pub hash: String,
    /// Istante del commit (secondi Unix).
    pub timestamp: i64,
    /// Il soggetto del messaggio: l'intento dichiarato, MAI riscritto.
    pub subject: String,
}

/// La **storia del confine**: i commit che hanno co-toccato `layer_a` e `layer_b`
/// (le *occasioni* del Campo di Astensione), dal più recente, al massimo `max`.
///
/// È il cuore del Crono-Semantic Mining applicato a `why`: la nascita (il fossile)
/// dice quando il confine è APPARSO — spesso un commit iniziale poco informativo —
/// mentre la storia dice come è stato ESERCITATO nel tempo, con l'intento dichiarato
/// di ogni commit. Trap #1 rispettata: il "razionale" è il messaggio dell'autore,
/// verbatim col suo hash — mai una spiegazione sintetizzata. Vuoto = nessuna
/// occasione nota, mai inventato (la stessa onestà di [`occasions`] = 0).
pub fn boundary_story(
    layer_a: &str,
    layer_b: &str,
    file_layers: &HashMap<String, HashSet<String>>,
    commits: &[Commit],
    max: usize,
) -> Vec<BoundaryOccasion> {
    let mut story: Vec<BoundaryOccasion> = commits
        .iter()
        .filter(|c| co_touches(c, layer_a, layer_b, file_layers))
        .map(|c| BoundaryOccasion {
            hash: c.hash.clone(),
            timestamp: c.timestamp,
            subject: c.subject.clone(),
        })
        .collect();
    // Dal più recente; a parità di istante, per hash (deterministico).
    story.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| a.hash.cmp(&b.hash))
    });
    story.truncate(max);
    story
}

/// `true` se il commit ha toccato almeno un file di `layer_a` **e** almeno uno di
/// `layer_b`: la definizione di *occasione*. Condivisa col modulo
/// [`crate::fossil`], che la usa per individuare la *nascita* di un confine.
pub(crate) fn co_touches(
    commit: &Commit,
    layer_a: &str,
    layer_b: &str,
    file_layers: &HashMap<String, HashSet<String>>,
) -> bool {
    let mut touched_a = false;
    let mut touched_b = false;
    for file in &commit.changed_files {
        if let Some(layers) = file_layers.get(file) {
            touched_a |= layers.contains(layer_a);
            touched_b |= layers.contains(layer_b);
        }
        if touched_a && touched_b {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Commit;

    fn file_layers() -> HashMap<String, HashSet<String>> {
        let mut m: HashMap<String, HashSet<String>> = HashMap::new();
        m.entry("app/api/h.py".into())
            .or_default()
            .insert("app::api".into());
        m.entry("app/core/s.py".into())
            .or_default()
            .insert("app::core".into());
        m.entry("README.md".into()).or_default(); // file senza layer
        m
    }

    #[test]
    fn counts_only_commits_that_touch_both_layers() {
        let fl = file_layers();
        let commits = vec![
            Commit::new(["app/api/h.py", "app/core/s.py"]), // occasione
            Commit::new(["app/api/h.py"]),                  // solo api
            Commit::new(["app/core/s.py", "README.md"]),    // solo core
            Commit::new(["README.md"]),                     // niente
            Commit::new(["app/core/s.py", "app/api/h.py"]), // occasione (ordine inverso)
        ];
        assert_eq!(occasions("app::api", "app::core", &fl, &commits), 2);
        // Simmetrica.
        assert_eq!(occasions("app::core", "app::api", &fl, &commits), 2);
    }

    #[test]
    fn no_occasions_without_history() {
        let fl = file_layers();
        assert_eq!(occasions("app::api", "app::core", &fl, &[]), 0);
    }

    #[test]
    fn boundary_story_returns_co_touching_commits_most_recent_first_and_capped() {
        let fl = file_layers();
        let commits = vec![
            Commit::with_meta(
                "aaa",
                100,
                "nasce il confine",
                ["app/api/h.py", "app/core/s.py"],
            ),
            Commit::with_meta("bbb", 300, "solo api", ["app/api/h.py"]), // NON occasione
            Commit::with_meta(
                "ccc",
                200,
                "rinforza il contratto api-core",
                ["app/core/s.py", "app/api/h.py"],
            ),
            Commit::with_meta(
                "ddd",
                400,
                "ultima revisione del confine",
                ["app/api/h.py", "app/core/s.py", "README.md"],
            ),
        ];

        let story = boundary_story("app::api", "app::core", &fl, &commits, 10);
        // Solo le occasioni vere (3 su 4), dal più recente, con l'intento VERBATIM.
        let got: Vec<(&str, &str)> = story
            .iter()
            .map(|o| (o.hash.as_str(), o.subject.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("ddd", "ultima revisione del confine"),
                ("ccc", "rinforza il contratto api-core"),
                ("aaa", "nasce il confine"),
            ]
        );

        // Il cap TIENE le più recenti (le più informative per "perché è così ORA").
        let capped = boundary_story("app::api", "app::core", &fl, &commits, 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].hash, "ddd");
        assert_eq!(capped[1].hash, "ccc");

        // Niente storia ⇒ vuoto, mai inventato.
        assert!(boundary_story("app::api", "app::core", &fl, &[], 10).is_empty());
    }

    #[test]
    fn wilson_lower_bound_grows_with_exposure() {
        // Stessa media (astensione perfetta, p̂ = 1.0) ma esposizione diversa:
        // più occasioni ⇒ confidenza più alta. È la proprietà chiave dell'idea.
        let few = Abstention::new(2, 0).wilson_lower_bound(Z_95);
        let many = Abstention::new(30, 0).wilson_lower_bound(Z_95);
        let lots = Abstention::new(300, 0).wilson_lower_bound(Z_95);

        assert!(few < many, "few={few} many={many}");
        assert!(many < lots, "many={many} lots={lots}");
        // Valori attesi (calcolati a mano dalla formula di Wilson).
        assert!((0.30..0.40).contains(&few), "few={few}");
        assert!((0.85..0.92).contains(&many), "many={many}");
        assert!(lots > 0.98, "lots={lots}");
    }

    #[test]
    fn violations_lower_the_confidence() {
        // A parità di occasioni, una violazione abbassa il tasso di astensione.
        let clean = Abstention::new(20, 0).wilson_lower_bound(Z_95);
        let dirty = Abstention::new(20, 5).wilson_lower_bound(Z_95);
        assert!(dirty < clean, "dirty={dirty} clean={clean}");
    }

    #[test]
    fn no_evidence_is_zero_confidence() {
        assert_eq!(Abstention::new(0, 0).wilson_lower_bound(Z_95), 0.0);
    }

    #[test]
    fn abstentions_never_underflow() {
        // Difesa: violazioni > occasioni non deve andare in underflow.
        assert_eq!(Abstention::new(3, 10).abstentions(), 0);
    }

    #[test]
    fn occasion_window_reports_first_and_last_timestamps() {
        // Il profilo temporale: conta le occasioni come `occasions`, ma riporta anche
        // QUANDO. Tre occasioni a ts 100/300/200; un commit a ts 999 tocca SOLO api
        // (non è un'occasione) e NON deve spostare l'ultimo timestamp a 999.
        let fl = file_layers();
        let commits = vec![
            Commit::with_meta("a", 100, "", ["app/api/h.py", "app/core/s.py"]),
            Commit::with_meta("b", 999, "", ["app/api/h.py"]), // solo api: non occasione
            Commit::with_meta("c", 300, "", ["app/core/s.py", "app/api/h.py"]),
            Commit::with_meta("d", 200, "", ["app/api/h.py", "app/core/s.py"]),
        ];
        let w =
            occasion_window("app::api", "app::core", &fl, &commits).expect("ci sono tre occasioni");
        assert_eq!(w.occasions, 3, "tre commit co-toccano i due layer");
        assert_eq!(w.first_ts, 100, "la prima occasione è a 100");
        assert_eq!(
            w.last_ts, 300,
            "l'ultima OCCASIONE è a 300 — il commit a 999 tocca solo api, non conta"
        );
    }

    #[test]
    fn occasion_window_is_none_without_occasions() {
        // Nessun commit co-tocca i due layer ⇒ nessuna evidenza temporale ⇒ None
        // onesto (niente timestamp inventato), coerente con `occasions` che dà 0.
        let fl = file_layers();
        let commits = vec![
            Commit::with_meta("a", 100, "", ["app/api/h.py"]), // solo api
            Commit::with_meta("b", 200, "", ["README.md"]),    // niente layer
        ];
        assert!(
            occasion_window("app::api", "app::core", &fl, &commits).is_none(),
            "nessuna co-occorrenza: None"
        );
        assert!(
            occasion_window("app::api", "app::core", &fl, &[]).is_none(),
            "nessuna storia: None"
        );
    }

    #[test]
    fn staleness_secs_measures_time_since_last_exercise() {
        let w = OccasionWindow {
            occasions: 5,
            first_ts: 100,
            last_ts: 1000,
        };
        assert_eq!(
            w.staleness_secs(1500),
            500,
            "1500 - 1000 = 500 s di stantio"
        );
        assert_eq!(w.staleness_secs(1000), 0, "esercitato proprio a `now`: 0");
        assert_eq!(
            w.staleness_secs(800),
            0,
            "`now` prima dell'ultima occasione: saturato a 0, mai negativo"
        );
    }
}
