//! I **Fossili di Decisione**: l'istante di *cristallizzazione* di un confine.
//!
//! # L'idea (il secondo asse: l'intento)
//!
//! Il Campo di Astensione misura *quanto* un invariante è battle-tested (la sua
//! confidenza nel tempo). Resta una domanda più antica: **quando** e **perché**
//! quel confine è nato? L'intento di una scelta architetturale non è scritto da
//! nessuna parte — ma è *fossilizzato* nella storia.
//!
//! > La nascita di un confine tra due layer è il **commit più vecchio** che li ha
//! > toccati entrambi: il primo istante in cui sono entrati in contatto. Quel
//! > commit conserva due cose che nessun grafo statico ha: il **diff strutturale**
//! > che ha cristallizzato la relazione (quali file dei due layer furono toccati
//! > insieme) e l'**intento dichiarato** — il messaggio di commit, le parole di
//! > chi ha disegnato il confine.
//!
//! Recuperare questo fossile trasforma un invariante *dedotto* dallo spazio
//! negativo in uno *ancorato* a un intento umano reale, datato e citabile. È il
//! "perché" originale, estratto dal negativo della storia anziché inventato.

use std::collections::{HashMap, HashSet};

use crate::abstention::co_touches;
use crate::history::Commit;

/// Il fossile di un confine architetturale: l'istante di cristallizzazione della
/// relazione tra due layer, con il diff strutturale e l'intento preservati.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionFossil {
    /// Il layer di base (da cui l'altro dipende).
    pub upstream: String,
    /// Il layer dipendente.
    pub downstream: String,
    /// Hash del commit di **nascita** (il più vecchio che ha co-toccato i due layer).
    pub born_at: String,
    /// Istante della nascita (secondi Unix). 0 se la sorgente non lo fornisce.
    pub born_at_unix: i64,
    /// Il messaggio di quel commit: l'**intento** dichiarato dall'autore.
    pub intent: String,
    /// I file dei due layer toccati alla nascita = il **diff strutturale**
    /// cristallizzato. Ordinato e deduplicato.
    pub born_structure: Vec<String>,
}

/// Scava il fossile della coppia di layer `(upstream, downstream)`: il commit più
/// **vecchio** che ha toccato un file di entrambi. `None` se i due layer non si
/// sono mai co-toccati (nessuna nascita osservabile nella storia fornita).
///
/// La nascita è scelta per **timestamp minimo**, quindi è robusta rispetto
/// all'ordine di `commits`. A parità di timestamp si preferisce l'elemento più in
/// fondo: `git log` emette dal più recente al più vecchio, perciò l'indice
/// maggiore è il più antico. La funzione è pura e simmetrica nella coppia.
pub fn excavate(
    upstream: &str,
    downstream: &str,
    file_layers: &HashMap<String, HashSet<String>>,
    commits: &[Commit],
) -> Option<DecisionFossil> {
    let mut birth: Option<&Commit> = None;
    for commit in commits {
        if !co_touches(commit, upstream, downstream, file_layers) {
            continue;
        }
        birth = Some(match birth {
            None => commit,
            // `<=` ⇒ a parità di timestamp tieni l'ultimo visto (più vecchio in
            // ordine newest-first); altrimenti vince il timestamp minore.
            Some(prev) if commit.timestamp <= prev.timestamp => commit,
            Some(prev) => prev,
        });
    }
    let birth = birth?;

    let mut born_structure: Vec<String> = birth
        .changed_files
        .iter()
        .filter(|file| {
            file_layers
                .get(file.as_str())
                .is_some_and(|layers| layers.contains(upstream) || layers.contains(downstream))
        })
        .cloned()
        .collect();
    born_structure.sort();
    born_structure.dedup();

    Some(DecisionFossil {
        upstream: upstream.to_string(),
        downstream: downstream.to_string(),
        born_at: birth.hash.clone(),
        born_at_unix: birth.timestamp,
        intent: birth.subject.clone(),
        born_structure,
    })
}

impl DecisionFossil {
    /// Rende relativi i path di `born_structure` rimuovendo il prefisso specificato.
    pub fn make_paths_relative(&mut self, prefix: &str) {
        for file in &mut self.born_structure {
            if let Some(stripped) = file.strip_prefix(prefix) {
                *file = stripped.to_string();
            }
        }
    }
}

/// Rileva se la storia del repository è insufficiente per tracciare i confini in modo affidabile.
///
/// La classificazione risponde a tre euristiche della roadmap P2-8:
/// 1. La storia ha pochissimi commit utili (meno di 3).
/// 2. Il commit di nascita tocca troppi file (es. initial commit massivo, con più di 30 file modificati).
/// 3. Quasi tutti i confini (>= 80%, se ci sono almeno 2 confini) nascono nello stesso commit.
pub fn is_history_insufficient(
    commits: &[Commit],
    fossils: &[DecisionFossil],
) -> bool {
    let useful_commits = commits.iter().filter(|c| !c.changed_files.is_empty()).count();
    if useful_commits < 3 {
        return true;
    }

    for fossil in fossils {
        if let Some(commit) = commits.iter().find(|c| c.hash == fossil.born_at) {
            if commit.changed_files.len() > 30 {
                return true;
            }
        }
    }

    if fossils.len() >= 2 {
        let mut birth_counts = std::collections::HashMap::new();
        for fossil in fossils {
            *birth_counts.entry(&fossil.born_at).or_insert(0) += 1;
        }
        for &count in birth_counts.values() {
            if count as f64 / fossils.len() as f64 >= 0.8 {
                return true;
            }
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
    fn excavate_finds_the_oldest_co_touch_as_birth() {
        let fl = file_layers();
        // Newest-first, come `git log`: il primo è recente, l'ultimo è la nascita.
        let commits = vec![
            Commit::with_meta("c3", 300, "tweak api", ["app/api/h.py"]),
            Commit::with_meta(
                "c2",
                200,
                "co-touch later",
                ["app/api/h.py", "app/core/s.py"],
            ),
            Commit::with_meta(
                "c1",
                100,
                "draw the boundary",
                ["app/api/h.py", "app/core/s.py", "README.md"],
            ),
        ];
        let f = excavate("app::api", "app::core", &fl, &commits).expect("fossile atteso");
        assert_eq!(f.born_at, "c1");
        assert_eq!(f.born_at_unix, 100);
        assert_eq!(f.intent, "draw the boundary");
        // Il diff di cristallizzazione esclude i file estranei ai due layer (README).
        assert_eq!(
            f.born_structure,
            vec!["app/api/h.py".to_string(), "app/core/s.py".to_string()]
        );
    }

    #[test]
    fn excavate_is_none_without_a_co_change() {
        let fl = file_layers();
        let commits = vec![
            Commit::with_meta("a", 1, "api only", ["app/api/h.py"]),
            Commit::with_meta("b", 2, "core only", ["app/core/s.py"]),
        ];
        assert!(excavate("app::api", "app::core", &fl, &commits).is_none());
    }

    #[test]
    fn excavate_uses_timestamp_not_slice_order() {
        let fl = file_layers();
        // Ordine "sporco": il più vecchio (ts=10) sta in mezzo, non in fondo.
        let commits = vec![
            Commit::with_meta("mid", 50, "later", ["app/api/h.py", "app/core/s.py"]),
            Commit::with_meta("oldest", 10, "birth", ["app/api/h.py", "app/core/s.py"]),
            Commit::with_meta("newest", 90, "latest", ["app/api/h.py", "app/core/s.py"]),
        ];
        let f = excavate("app::api", "app::core", &fl, &commits).unwrap();
        assert_eq!(f.born_at, "oldest");
        assert_eq!(f.intent, "birth");
    }

    #[test]
    fn excavate_is_symmetric_in_the_layer_pair() {
        let fl = file_layers();
        let commits = vec![Commit::with_meta(
            "x",
            5,
            "birth",
            ["app/api/h.py", "app/core/s.py"],
        )];
        let a = excavate("app::api", "app::core", &fl, &commits).unwrap();
        let b = excavate("app::core", "app::api", &fl, &commits).unwrap();
        assert_eq!(a.born_at, b.born_at);
        assert_eq!(a.born_structure, b.born_structure);
    }

    #[test]
    fn test_make_paths_relative() {
        let mut fossil = DecisionFossil {
            upstream: "up".to_string(),
            downstream: "down".to_string(),
            born_at: "hash".to_string(),
            born_at_unix: 1234,
            intent: "intent".to_string(),
            born_structure: vec![
                "/root/src/main.rs".to_string(),
                "/root/src/lib.rs".to_string(),
            ],
        };
        fossil.make_paths_relative("/root/");
        assert_eq!(fossil.born_structure, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn test_is_history_insufficient() {
        // 1. Pochi commit utili (meno di 3)
        let commits = vec![
            Commit::with_meta("c1", 100, "init", ["file1.rs"]),
            Commit::with_meta("c2", 200, "work", ["file2.rs"]),
        ];
        let fossils = vec![
            DecisionFossil {
                upstream: "up".to_string(),
                downstream: "down".to_string(),
                born_at: "c1".to_string(),
                born_at_unix: 100,
                intent: "init".to_string(),
                born_structure: vec!["file1.rs".to_string()],
            }
        ];
        assert!(is_history_insufficient(&commits, &fossils));

        // 2. Commit con troppi file (initial commit massivo > 30 file)
        let mut mass_files = Vec::new();
        for i in 0..35 {
            mass_files.push(format!("file_{i}.rs"));
        }
        let commits_mass = vec![
            Commit::with_meta("c1", 100, "mass init", mass_files),
            Commit::with_meta("c2", 200, "work1", ["file_a.rs"]),
            Commit::with_meta("c3", 300, "work2", ["file_b.rs"]),
        ];
        let fossils_mass = vec![
            DecisionFossil {
                upstream: "up".to_string(),
                downstream: "down".to_string(),
                born_at: "c1".to_string(),
                born_at_unix: 100,
                intent: "mass init".to_string(),
                born_structure: vec!["file_1.rs".to_string()],
            }
        ];
        assert!(is_history_insufficient(&commits_mass, &fossils_mass));

        // 3. Quasi tutti i confini nascono nello stesso commit (>= 80%)
        let commits_same = vec![
            Commit::with_meta("c1", 100, "init", ["file1.rs", "file2.rs"]),
            Commit::with_meta("c2", 200, "work", ["file3.rs"]),
            Commit::with_meta("c3", 300, "work2", ["file4.rs"]),
        ];
        let fossils_same = vec![
            DecisionFossil {
                upstream: "a".to_string(),
                downstream: "b".to_string(),
                born_at: "c1".to_string(),
                born_at_unix: 100,
                intent: "init".to_string(),
                born_structure: vec!["file1.rs".to_string()],
            },
            DecisionFossil {
                upstream: "c".to_string(),
                downstream: "d".to_string(),
                born_at: "c1".to_string(),
                born_at_unix: 100,
                intent: "init".to_string(),
                born_structure: vec!["file2.rs".to_string()],
            },
        ];
        assert!(is_history_insufficient(&commits_same, &fossils_same));

        // Caso sufficiente
        let commits_ok = vec![
            Commit::with_meta("c1", 100, "init", ["file1.rs"]),
            Commit::with_meta("c2", 200, "work1", ["file2.rs"]),
            Commit::with_meta("c3", 300, "work2", ["file3.rs"]),
            Commit::with_meta("c4", 400, "work3", ["file4.rs"]),
        ];
        let fossils_ok = vec![
            DecisionFossil {
                upstream: "a".to_string(),
                downstream: "b".to_string(),
                born_at: "c2".to_string(),
                born_at_unix: 200,
                intent: "work1".to_string(),
                born_structure: vec!["file2.rs".to_string()],
            },
            DecisionFossil {
                upstream: "c".to_string(),
                downstream: "d".to_string(),
                born_at: "c3".to_string(),
                born_at_unix: 300,
                intent: "work2".to_string(),
                born_structure: vec!["file3.rs".to_string()],
            },
        ];
        assert!(!is_history_insufficient(&commits_ok, &fossils_ok));
    }
}
