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
}
