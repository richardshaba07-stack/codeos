//! Regole di layering **dichiarate a mano** dall'umano in `.codeos/config.yaml`
//! (item 15 della roadmap), e la loro convivenza con quelle *scoperte* (item 16).
//!
//! Lo spazio negativo del grafo deduce gli invarianti che *nessuno ha scritto*;
//! qui facciamo il contrario: diamo all'umano il potere di **dettare** un confine
//! che il grafo non ha (ancora) evidenza per provare — un divieto vale per decreto,
//! con confidenza 1.0, anche su un layer giovane senza supporto strutturale.
//!
//! Schema (lo stesso documentato in `config/example.codeos.yaml`):
//!
//! ```yaml
//! architecture:
//!   rules:
//!     - name: "UI can't access Data"
//!       type: layer_dependency
//!       from: ["ui"]
//!       to: ["data"]
//! ```
//!
//! Semantica: `from` **non deve dipendere da** `to`. Nel modello del Guardian una
//! dipendenza lecita va `downstream → upstream`, quindi un divieto `from → to`
//! mappa su `upstream = from`, `downstream = to`: un arco `from → to` inverte la
//! freccia ed è una violazione.
//!
//! Niente nuove dipendenze (`serde_yaml` & co.): il parser è **scritto in casa**,
//! coerente con la filosofia del progetto. Riconosce liste inline (`["a", "b"]`),
//! scalari singoli e liste a blocchi (`- a` su righe successive).

use std::path::Path;

use codeos_types::bus::RuleOrigin;
use codeos_types::EntityId;

use crate::invariant::{LayerKey, LayeringRule};

/// Una regola di dipendenza vietata, così come dichiarata nella config: `from` non
/// deve dipendere da `to`. Stringhe grezze (i `LayerKey` come compaiono nel grafo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredRule {
    /// Il layer che NON deve dipendere (diventerà `upstream`).
    pub from: String,
    /// Il layer da cui è vietato dipendere (diventerà `downstream`).
    pub to: String,
}

/// Carica le regole dichiarate da `<repo>/.codeos/config.yaml`. File assente o
/// illeggibile ⇒ nessuna regola (degrada con grazia: la config è opzionale).
pub fn load_declared_rules(config_path: &Path) -> Vec<LayeringRule> {
    match std::fs::read_to_string(config_path) {
        Ok(text) => declared_layering_rules(&text),
        Err(_) => Vec::new(),
    }
}

/// Legge un eventuale `layer_depth: N` dalla config: la profondità con cui i
/// `qualified_name` vengono raggruppati in layer. Serve quando il default non si
/// adatta all'annidamento del progetto — es. un layout `src/pkg/` collassa l'intero
/// package in un solo layer (`src::pkg`), rendendo invisibili i confini fra moduli.
/// `None` se assente/non valido ⇒ si usa il default. Parser minimale e indipendente
/// dalle regole (cerca la chiave ovunque, indentata o no), coerente con lo stile.
pub fn declared_layer_depth(yaml: &str) -> Option<usize> {
    yaml.lines().find_map(|line| {
        line.trim()
            .strip_prefix("layer_depth:")
            .and_then(|rest| rest.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1)
    })
}

/// Carica `layer_depth` da `<repo>/.codeos/config.yaml`. `None` se assente/illeggibile.
pub fn load_layer_depth(config_path: &Path) -> Option<usize> {
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| declared_layer_depth(&t))
}

/// Converte il testo YAML della config nelle [`LayeringRule`] dichiarate, pronte da
/// fondere con quelle scoperte. Confidenza 1.0 (decreto umano), origine `Declared`.
pub fn declared_layering_rules(yaml: &str) -> Vec<LayeringRule> {
    parse_layer_dependency_rules(yaml)
        .into_iter()
        .filter(|r| !r.from.is_empty() && !r.to.is_empty() && r.from != r.to)
        .map(|r| LayeringRule {
            id: EntityId::new(),
            upstream: LayerKey(r.from),
            downstream: LayerKey(r.to),
            support: 0,
            confidence: 1.0,
            origin: RuleOrigin::Declared,
        })
        .collect()
}

/// Quale dei due campi-lista (`from`/`to`) sta raccogliendo gli scalari di una
/// lista a blocchi.
#[derive(Clone, Copy)]
enum PendingList {
    From,
    To,
}

#[derive(Default)]
struct RuleBuilder {
    rule_type: Option<String>,
    from: Vec<String>,
    to: Vec<String>,
}

impl RuleBuilder {
    /// Emette tutte le coppie `(from_i, to_j)` se la regola è una dipendenza di
    /// layer ben formata. `type` assente è tollerato (lenienza); `type` presente ma
    /// diverso da `layer_dependency` viene ignorato.
    fn emit(self, out: &mut Vec<DeclaredRule>) {
        let is_layer_dep = self
            .rule_type
            .as_deref()
            .map(|t| t == "layer_dependency")
            .unwrap_or(true);
        if !is_layer_dep || self.from.is_empty() || self.to.is_empty() {
            return;
        }
        for from in &self.from {
            for to in &self.to {
                out.push(DeclaredRule {
                    from: from.clone(),
                    to: to.clone(),
                });
            }
        }
    }
}

/// Estrae le regole `type: layer_dependency` dal testo YAML, senza dipendenze
/// esterne. Robusto a indentazione e commenti; non valida l'intero documento.
fn parse_layer_dependency_rules(yaml: &str) -> Vec<DeclaredRule> {
    let mut out = Vec::new();
    let mut current: Option<RuleBuilder> = None;
    let mut pending: Option<PendingList> = None;

    for raw in yaml.lines() {
        let line = strip_comment(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Una riga `- <scalare>` che NON apre un campo-regola noto è un elemento di
        // una lista a blocchi, appartenente all'ultimo `from:`/`to:` in sospeso.
        // NB: non basta cercare `:` — i `LayerKey` contengono `::` (es. `app::api`).
        if let Some(rest) = trimmed.strip_prefix("- ") {
            if !starts_rule_field(rest) {
                if let (Some(builder), Some(slot)) = (current.as_mut(), pending) {
                    let value = unquote(rest);
                    if !value.is_empty() {
                        match slot {
                            PendingList::From => builder.from.push(value),
                            PendingList::To => builder.to.push(value),
                        }
                    }
                }
                continue;
            }
            // `- key: value`: inizia un nuovo elemento-regola. Chiudi il precedente.
            if let Some(builder) = current.take() {
                builder.emit(&mut out);
            }
            current = Some(RuleBuilder::default());
            pending = None;
            apply_field(current.as_mut().unwrap(), rest, &mut pending);
            continue;
        }

        // Riga `key: value` di proprietà dell'elemento-regola corrente.
        if let Some(builder) = current.as_mut() {
            if trimmed.contains(':') {
                apply_field(builder, trimmed, &mut pending);
            }
        }
    }

    if let Some(builder) = current.take() {
        builder.emit(&mut out);
    }
    out
}

/// `true` se la riga (già privata del `- `) apre un campo-regola noto, e quindi
/// segna l'inizio di un nuovo elemento-regola. Distingue `- name:`/`- type:` dagli
/// scalari di una lista a blocchi (es. `- app::api`, che NON è un campo).
fn starts_rule_field(rest: &str) -> bool {
    let key = rest.split_once(':').map(|(k, _)| k.trim()).unwrap_or("");
    matches!(key, "name" | "type" | "from" | "to")
}

/// Applica una riga `key: value` al builder corrente, aggiornando l'eventuale lista
/// a blocchi in sospeso.
fn apply_field(builder: &mut RuleBuilder, text: &str, pending: &mut Option<PendingList>) {
    let Some((key, value)) = text.split_once(':') else {
        return;
    };
    let key = key.trim();
    let value = value.trim();
    match key {
        "type" => {
            builder.rule_type = Some(unquote(value));
            *pending = None;
        }
        "from" => {
            if value.is_empty() {
                *pending = Some(PendingList::From);
            } else {
                builder.from = parse_list_value(value);
                *pending = None;
            }
        }
        "to" => {
            if value.is_empty() {
                *pending = Some(PendingList::To);
            } else {
                builder.to = parse_list_value(value);
                *pending = None;
            }
        }
        // Altri campi (`name`, ...) chiudono qualsiasi lista a blocchi aperta.
        _ => {
            *pending = None;
        }
    }
}

/// Interpreta il valore di un campo lista: inline (`["a", "b"]` o `[a]`) oppure uno
/// scalare singolo (`a`). Le liste a blocchi sono gestite altrove (righe `- a`).
fn parse_list_value(raw: &str) -> Vec<String> {
    let s = raw.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let inner = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(s);
    inner
        .split(',')
        .map(|p| unquote(p.trim()))
        .filter(|p| !p.is_empty())
        .collect()
}

/// Rimuove eventuali apici (singoli o doppi) attorno a un valore scalare.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Taglia un commento `#` di fine riga, rispettando i `#` dentro virgolette.
fn strip_comment(line: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return line[..i].to_string(),
            _ => {}
        }
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inline_lists_into_declared_rules() {
        let yaml = r#"
architecture:
  rules:
    - name: "UI can't access Data"
      type: layer_dependency
      from: ["ui"]
      to: ["data"]
    - name: "Business logic can't access UI"
      type: layer_dependency
      from: ["business_logic"]
      to: ["ui"]
"#;
        let rules = declared_layering_rules(yaml);
        assert_eq!(rules.len(), 2, "regole = {rules:?}");
        // from non deve dipendere da to ⇒ upstream = from, downstream = to.
        assert_eq!(rules[0].upstream, LayerKey("ui".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("data".to_string()));
        assert_eq!(rules[0].origin, RuleOrigin::Declared);
        assert!((rules[0].confidence - 1.0).abs() < 1e-6);
        assert_eq!(rules[1].upstream, LayerKey("business_logic".to_string()));
        assert_eq!(rules[1].downstream, LayerKey("ui".to_string()));
    }

    #[test]
    fn expands_the_cartesian_product_of_from_and_to() {
        let yaml = r#"
architecture:
  rules:
    - type: layer_dependency
      from: ["a", "b"]
      to: ["x", "y"]
"#;
        let rules = declared_layering_rules(yaml);
        // 2 from × 2 to = 4 divieti.
        assert_eq!(rules.len(), 4);
        let pairs: Vec<(String, String)> = rules
            .iter()
            .map(|r| (r.upstream.0.clone(), r.downstream.0.clone()))
            .collect();
        assert!(pairs.contains(&("a".to_string(), "x".to_string())));
        assert!(pairs.contains(&("b".to_string(), "y".to_string())));
    }

    #[test]
    fn parses_block_style_lists() {
        let yaml = r#"
architecture:
  rules:
    - type: layer_dependency
      from:
        - app::api
      to:
        - app::db
"#;
        let rules = declared_layering_rules(yaml);
        assert_eq!(rules.len(), 1, "regole = {rules:?}");
        assert_eq!(rules[0].upstream, LayerKey("app::api".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("app::db".to_string()));
    }

    #[test]
    fn tolerates_single_scalar_values() {
        let yaml = r#"
architecture:
  rules:
    - type: layer_dependency
      from: app::web
      to: app::core
"#;
        let rules = declared_layering_rules(yaml);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].upstream, LayerKey("app::web".to_string()));
        assert_eq!(rules[0].downstream, LayerKey("app::core".to_string()));
    }

    #[test]
    fn ignores_self_dependencies_and_non_layer_rules() {
        let yaml = r#"
architecture:
  rules:
    - type: layer_dependency
      from: ["same"]
      to: ["same"]
    - type: naming_convention
      from: ["a"]
      to: ["b"]
"#;
        // La regola riflessiva è scartata; quella non-layer è ignorata.
        assert!(declared_layering_rules(yaml).is_empty());
    }

    #[test]
    fn empty_or_garbage_yields_no_rules() {
        assert!(declared_layering_rules("").is_empty());
        assert!(declared_layering_rules("just some text\nno rules here").is_empty());
    }

    #[test]
    fn strips_trailing_comments() {
        let yaml = r#"
architecture:
  rules:
    - type: layer_dependency   # questa è una regola
      from: ["ui"]             # il layer di presentazione
      to: ["data"]
"#;
        let rules = declared_layering_rules(yaml);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].upstream, LayerKey("ui".to_string()));
    }

    #[test]
    fn missing_config_file_is_silently_empty() {
        let rules = load_declared_rules(Path::new("/nonexistent/.codeos/config.yaml"));
        assert!(rules.is_empty());
    }
}
