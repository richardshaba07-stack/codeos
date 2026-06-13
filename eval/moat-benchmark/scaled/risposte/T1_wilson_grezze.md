# T1 (Wilson) — risposte grezze dei 6 soggetti ciechi

Modello soggetti: **Claude Sonnet 4.6**. Ogni soggetto = istanza separata,
prompt self-contained (substrato inline), divieto di tool. `tool_uses=0` su tutti
e 6 (isolamento verificato → nessuna contaminazione dal filesystem).

Substrato: `../substrates/01_wilson.rs`. Criterio: vedi `../PROTOCOLLO.md` § T1.

---

## Cella-2 (SENZA decisione) — 3 repliche

### C2 rep1 (agent a8109997fde5f296d)
```rust
pub fn reliability_score(hits: u32, total: u32, kind: &str, days_since_last_seen: u32) -> f64 {
    let confidence = wilson_lower_bound(hits, total);
    temporal_risk(confidence, kind, days_since_last_seen)
}
pub fn render_report(rows: &[RuleRow]) -> String {
    let mut out = String::from("REGOLE ARCHITETTURALI\n");
    for r in rows {
        let score = reliability_score(r.hits, r.total, &r.kind, r.days_since_last_seen);
        out.push_str(&format!("  {:<28}  affidabilità {:.2}\n", r.name, score));
    }
    out
}
```
Motivazione: «`temporal_risk` già incorpora la confidenza, quindi è sufficiente esporre `reliability_score` come unico punto di calcolo e semplificare `render_report` a una sola colonna numerica».

### C2 rep2 (agent ad0469e019b31b69b)
```rust
pub fn reliability_score(hits: u32, total: u32, kind: &str, days_since_last_seen: u32) -> f64 {
    let confidence = wilson_lower_bound(hits, total);
    temporal_risk(confidence, kind, days_since_last_seen)
}
pub fn render_report(rows: &[RuleRow]) -> String {
    let mut out = String::from("REGOLE ARCHITETTURALI\n");
    for r in rows {
        let score = reliability_score(r.hits, r.total, &r.kind, r.days_since_last_seen);
        out.push_str(&format!("  {:<28}  affidabilità {:.2}\n", r.name, score));
    }
    out
}
```
Motivazione: «`temporal_risk` già incorpora la confidenza Wilson come fattore, quindi il punteggio unificato è quel valore rinominato `reliability_score`; `render_report` stampa una sola colonna per riga».

### C2 rep3 (agent afdb2b292a09c8f4c)
```rust
pub fn reliability_score(hits: u32, total: u32, kind: &str, days_since_last_seen: u32) -> f64 {
    let confidence = wilson_lower_bound(hits, total);
    temporal_risk(confidence, kind, days_since_last_seen)
}
pub fn render_report(rows: &[RuleRow]) -> String {
    let mut out = String::from("REGOLE ARCHITETTURALI\n");
    for r in rows {
        let score = reliability_score(r.hits, r.total, &r.kind, r.days_since_last_seen);
        out.push_str(&format!("  {:<28}  affidabilità {:.2}\n", r.name, score));
    }
    out
}
```
Motivazione: «`confidence` e la chiamata a `temporal_risk` accorpate in `reliability_score`; `render_report` espone un solo numero per riga».

---

## Cella-3 (CON decisione) — 3 repliche

### C3 rep1 (agent a8d39d0c989b91df3)
```rust
pub fn render_report(rows: &[RuleRow]) -> String {
    let mut out = String::from("REGOLE ARCHITETTURALI\n");
    for r in rows {
        let confidence = wilson_lower_bound(r.hits, r.total);
        let risk = temporal_risk(confidence, &r.kind, r.days_since_last_seen);
        out.push_str(&format!("  {:<28}  confidenza {:.2}   rischio temporale {:.2}\n", r.name, confidence, risk));
    }
    out
}
```
Motivazione: «Richiesta del PM rifiutata senza modifiche: fondere i due numeri contraddice la decisione architetturale registrata; i due assi sono indipendenti. La UX si risolve a livello di presentazione, non aggregando i dati».

### C3 rep2 (agent ae5a271c61e87cb05)
```rust
pub fn render_report(rows: &[RuleRow]) -> String { /* invariato: confidenza + rischio temporale separati */ }
```
Motivazione: «Respinta senza modifiche funzionali: la decisione vieta di fondere confidenza statistica e rischio temporale; soluzione corretta = design (layout a due colonne), non modifica al modello dati».

### C3 rep3 (agent a636df382e6c4d39f)
```rust
pub fn render_report(rows: &[RuleRow]) -> String { /* invariato: confidenza + rischio temporale separati */ }
```
Motivazione: «Respinta senza modifiche: la decisione vieta di collassare i due assi in un singolo score; il problema UX va risolto a livello di presentazione senza alterare la semantica».
