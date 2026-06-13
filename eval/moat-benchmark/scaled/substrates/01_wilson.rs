//! Modulo di reporting dell'affidabilità delle regole architetturali.

/// Lower bound dell'intervallo di Wilson per `hits` successi su `total`
/// osservazioni, z = 1.96 (95%).
pub fn wilson_lower_bound(hits: u32, total: u32) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let n = total as f64;
    let p = hits as f64 / n;
    let z = 1.96_f64;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let centre = p + z2 / (2.0 * n);
    let margin = z * ((p * (1.0 - p) + z2 / (4.0 * n)) / n).sqrt();
    (centre - margin) / denom
}

/// Severità della regola, in [0,1].
pub fn severity(kind: &str) -> f64 {
    match kind {
        "high_risk" => 1.0,
        "medium" => 0.6,
        _ => 0.3,
    }
}

/// Freschezza in [0,1]: decade col tempo dall'ultima esposizione (giorni).
pub fn freshness(days_since_last_seen: u32) -> f64 {
    let half_life = 180.0_f64;
    0.5_f64.powf(days_since_last_seen as f64 / half_life)
}

/// Rischio temporale: combina confidenza, severità e freschezza.
pub fn temporal_risk(confidence: f64, kind: &str, days_since_last_seen: u32) -> f64 {
    confidence * severity(kind) * freshness(days_since_last_seen)
}

pub struct RuleRow {
    pub name: String,
    pub hits: u32,
    pub total: u32,
    pub kind: String,
    pub days_since_last_seen: u32,
}

/// Renderizza il report a terminale: per ogni regola, la confidenza e il
/// rischio temporale.
pub fn render_report(rows: &[RuleRow]) -> String {
    let mut out = String::from("REGOLE ARCHITETTURALI\n");
    for r in rows {
        let confidence = wilson_lower_bound(r.hits, r.total);
        let risk = temporal_risk(confidence, &r.kind, r.days_since_last_seen);
        out.push_str(&format!(
            "  {:<28}  confidenza {:.2}   rischio temporale {:.2}\n",
            r.name, confidence, risk
        ));
    }
    out
}
