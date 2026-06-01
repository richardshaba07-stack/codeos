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
}
