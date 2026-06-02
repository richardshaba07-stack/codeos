//! `codeos` — la CLI di CodeOS per interrogare e controllare il sistema da terminale.
//!
//! Legge `CODEOS_ADDR` per l'indirizzo del server (default: `127.0.0.1:50051`).

use codeos_rpc::proto;
use proto::code_os_client::CodeOsClient;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    let command = args[1].as_str();
    match command {
        "index" => {
            if args.len() < 3 {
                eprintln!("Errore: manca il percorso del progetto da indicizzare.");
                eprintln!("Uso: codeos index <path>");
                std::process::exit(1);
            }
            let raw_path = &args[2];
            let absolute_path = Path::new(raw_path).canonicalize().map_err(|e| {
                anyhow::anyhow!("Impossibile trovare il percorso '{}': {}", raw_path, e)
            })?;
            let project_root = absolute_path.to_string_lossy().to_string();

            let mut client = connect_server().await?;
            println!("⚡ Invio richiesta di indicizzazione per: {}", project_root);

            let req = proto::IndexProjectRequest { project_root };
            client.index_project(req).await?;

            println!("🎉 Indicizzazione completata con successo!");
        }
        "report" => {
            let opts = parse_report_options(&args[2..]);
            let mut client = connect_server().await?;
            // In modalità JSON l'output deve essere puro (parsabile da CI/AI): niente
            // chiacchiere su stdout.
            if !opts.json {
                println!("⚡ Richiesta referto architetturale in corso...");
            }

            let req = proto::GetArchitectureReportRequest {};
            let response = client.get_architecture_report(req).await?.into_inner();

            if opts.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report_to_json(&response))?
                );
            } else {
                render_terminal_report(response, &opts);
            }
        }
        "query" => {
            if args.len() < 3 {
                eprintln!("Errore: inserire la domanda/query in linguaggio naturale.");
                eprintln!("Uso: codeos query \"<domanda>\"");
                std::process::exit(1);
            }
            let query_text = &args[2];

            let mut client = connect_server().await?;
            println!("⚡ Interrogazione del grafo semantico...");

            let req = proto::QueryGraphRequest {
                natural_language: query_text.clone(),
            };
            let response = client.query_graph(req).await?.into_inner();

            println!("\n=== CONTESTO GENERATO PER LLM ===\n");
            println!("{}", response.formatted_context);
            println!("==================================\n");
            println!(
                "ℹ️  Trovate {} entità e {} relazioni rilevanti.",
                response.entities.len(),
                response.relations.len()
            );
        }
        "doctor" => {
            run_doctor().await;
        }
        "guard" => {
            let mut client = connect_server().await?;
            if args.len() < 3 {
                eprintln!("Errore: usa `--before \"goal\"` o `--after`.");
                std::process::exit(1);
            }
            if args[2] == "--before" {
                if args.len() < 4 {
                    eprintln!("Errore: specifica il goal dopo `--before`.");
                    std::process::exit(1);
                }
                let goal = &args[3];
                let req = proto::GuardBeforeRequest { goal: goal.clone() };
                let res = client.guard_before(req).await?.into_inner();
                println!("🛡️  FIREWALL ARCHITETTURALE - GUARD BEFORE");
                println!("------------------------------------------");
                println!("🎯 Goal: \"{}\"", goal);
                println!(
                    "📊 Raggio d'impatto (Blast Radius): {} entità",
                    res.blast_radius
                );
                println!("🛣️  Percorso sicuro consigliato: {}", res.safe_path);
                println!("\n📂 File target a rischio:");
                for f in &res.target_files {
                    println!("  • {}", f);
                }
                println!("\n🧱 Confini da preservare:");
                for b in &res.boundaries {
                    println!("  • {}", b);
                }
            } else if args[2] == "--after" {
                let req = proto::GuardAfterRequest {};
                let res = client.guard_after(req).await?.into_inner();
                println!("🛡️  FIREWALL ARCHITETTURALE - GUARD AFTER");
                println!("-----------------------------------------");
                if res.violations.is_empty() {
                    println!("✅ Nessuna violazione architetturale rilevata! Ottimo lavoro.");
                } else {
                    println!(
                        "🔴 Rilevate {} violazioni architetturali!",
                        res.violations.len()
                    );
                    for vio in &res.violations {
                        let loc_str = if let Some(loc) = &vio.location {
                            format!("{}:{}", loc.file_path, loc.start_line)
                        } else {
                            "posizione sconosciuta".to_string()
                        };
                        println!("  • [{}] {}", loc_str, vio.message);
                    }
                    println!("\n🔧 Correzioni proposte:");
                    for fix in &res.proposed_fixes {
                        println!("  • {}", fix);
                    }
                }
            } else {
                eprintln!(
                    "Errore: flag '{}' sconosciuto. Usa `--before` o `--after`.",
                    args[2]
                );
                std::process::exit(1);
            }
        }
        "context" => {
            if args.len() < 3 {
                eprintln!("Errore: specifica il goal.");
                eprintln!("Uso: codeos context \"goal\" [--for ai]");
                std::process::exit(1);
            }
            let goal = &args[2];
            let for_ai = args
                .iter()
                .any(|arg| arg == "--for" || arg == "ai" || arg == "--for=ai");
            let mut client = connect_server().await?;
            let req = proto::GetContextPackRequest {
                goal: goal.clone(),
                for_ai,
            };
            let res = client.get_context_pack(req).await?.into_inner();
            println!("{}", res.formatted_markdown);
        }
        "mri" => {
            let mut base = "main".to_string();
            let mut head = "HEAD".to_string();
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--base" && i + 1 < args.len() {
                    base = args[i + 1].clone();
                    i += 2;
                } else if args[i] == "--head" && i + 1 < args.len() {
                    head = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            let mut client = connect_server().await?;
            let req = proto::PrMriRequest { base, head };
            let res = client.pr_mri(req).await?.into_inner();
            println!("🩺 PR ARCHITECTURE MRI REPORT");
            println!("-----------------------------");
            println!("📋 Sintesi: {}", res.summary);
            println!("📈 Variazione Blast Radius: {}", res.blast_radius_change);
            println!("⚠️  Livello di rischio: {}", res.risk_score.to_uppercase());
            println!("\n🔍 Nuove dipendenze introdotte:");
            for dep in &res.new_dependencies {
                println!("  • {}", dep);
            }
            println!("\n🔴 Confini violati:");
            for vio in &res.violated_boundaries {
                println!("  • {}", vio);
            }
            println!("\n🔥 Hotspot storici toccati:");
            for hot in &res.historical_hotspots {
                println!("  • {}", hot);
            }
            println!("\n🔌 Nuove dipendenze esterne:");
            for ext in &res.new_external_dependencies {
                println!("  • {}", ext);
            }
            println!("\n🧪 Test influenzati:");
            for t in &res.impacted_tests {
                println!("  • {}", t);
            }
        }
        "why" => {
            if args.len() < 3 {
                eprintln!("Errore: specifica l'espressione (es. 'modulo_a|modulo_b').");
                std::process::exit(1);
            }
            let expr = &args[2];
            let mut client = connect_server().await?;
            let req = proto::WhyRequest { expr: expr.clone() };
            let res = client.why(req).await?.into_inner();
            println!("🕰️  TIME MACHINE ARCHITETTURALE - WHY");
            println!("------------------------------------");
            if res.history_insufficient {
                println!("⚠️  BADGE WARNING: La storia Git del repository è insufficiente per tracciare i confini in modo affidabile!");
            }
            println!("💬 Spiegazione: {}", res.explanation);
            println!("📅 Data di nascita: {}", res.born_date);
            println!("🔑 Hash commit nascita: {}", res.born_commit);
            println!("✍️  Intento originario: \"{}\"", res.intent);
            println!("\n📂 File co-modificati alla nascita:");
            for f in &res.co_changed_files {
                println!("  • {}", f);
            }
            println!("\n📜 Decisioni correlate:");
            for dec in &res.markdown_decisions {
                println!("{}", dec);
            }
        }
        "simulate" => {
            if args.len() < 3 {
                eprintln!("Errore: specifica l'espressione di refactoring (es. 'move A to B').");
                std::process::exit(1);
            }
            let expr = &args[2];
            let mut client = connect_server().await?;
            let req = proto::SimulateRequest { expr: expr.clone() };
            let res = client.simulate(req).await?.into_inner();
            println!("🧪 WHAT-IF REFACTOR SIMULATOR");
            println!("-----------------------------");
            println!("🔄 Dipendenze da riscrivere/aggiornare:");
            for dep in &res.dependencies_to_rewrite {
                println!("  • {}", dep);
            }
            println!("\n🧱 Confini che cambieranno:");
            for boundary in &res.changed_boundaries {
                println!("  • {}", boundary);
            }
            println!("\n⚠️  Rischi identificati:");
            for risk in &res.risks {
                println!("  • {}", risk);
            }
            println!("\n🧪 Test raccomandati:");
            for t in &res.suggested_tests {
                println!("  • {}", t);
            }
            println!("\n📋 Piano d'azione consigliato:");
            for step in &res.recommendation_plan {
                println!("  {}", step);
            }
        }
        "help" | "--help" | "-h" => {
            print_usage();
        }
        other => {
            eprintln!("Errore: comando '{}' sconosciuto.", other);
            print_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn connect_server() -> anyhow::Result<CodeOsClient<tonic::transport::Channel>> {
    let mut address =
        std::env::var("CODEOS_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".to_string());

    if !address.starts_with("http://") && !address.starts_with("https://") {
        address = format!("http://{}", address);
    }

    CodeOsClient::connect(address).await.map_err(|e| {
        anyhow::anyhow!(
            "Impossibile connettersi al server CodeOS. È attivo? Dettaglio: {}",
            e
        )
    })
}

/// `codeos doctor` — controlla, senza modificare nulla, che l'ambiente sia pronto:
/// indirizzo configurato, porta raggiungibile, server gRPC vivo e in grado di
/// produrre un referto. Stampa diagnosi azionabili e termina con exit code 1 se
/// qualcosa è rotto, così è usabile anche in script/CI.
async fn run_doctor() {
    println!("🩺 CodeOS doctor — diagnosi dell'ambiente\n");
    let mut problems = 0u32;

    // 1) Indirizzo del server.
    let raw_addr = std::env::var("CODEOS_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".to_string());
    let source = if std::env::var("CODEOS_ADDR").is_ok() {
        "CODEOS_ADDR"
    } else {
        "default"
    };
    println!("  [✓] Indirizzo server: {raw_addr} ({source})");

    // 2) Risoluzione + raggiungibilità TCP della porta.
    let host_port = raw_addr
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string();
    let reachable = match host_port.to_socket_addrs() {
        Ok(mut addrs) => addrs.next().map(|sock| {
            std::net::TcpStream::connect_timeout(&sock, Duration::from_millis(800)).is_ok()
        }),
        Err(_) => None,
    };
    match reachable {
        Some(true) => println!("  [✓] Porta raggiungibile: connessione TCP a {host_port} ok"),
        Some(false) => {
            problems += 1;
            println!("  [✗] Porta NON raggiungibile: nessuno in ascolto su {host_port}");
            println!("      → Avvia il server: CODEOS_REPO=<tuo-progetto> ./codeos-server");
        }
        None => {
            problems += 1;
            println!("  [✗] Indirizzo non valido: impossibile risolvere '{host_port}'");
            println!("      → Controlla CODEOS_ADDR (formato host:porta, es. 127.0.0.1:50051)");
        }
    }

    // 3) Liveness gRPC reale: il server risponde a una RPC vera?
    if reachable == Some(true) {
        match connect_server().await {
            Ok(mut client) => {
                let req = proto::GetArchitectureReportRequest {};
                match client.get_architecture_report(req).await {
                    Ok(resp) => {
                        let r = resp.into_inner();
                        println!("  [✓] Server gRPC vivo: referto ottenuto");
                        println!(
                            "      → {} invarianti, {} fossili, {} lacune nel grafo corrente",
                            r.invariants.len(),
                            r.fossils.len(),
                            r.gaps.len()
                        );
                        let calibrated = r.invariants.iter().any(|i| i.calibrated);
                        if r.invariants.is_empty() {
                            println!(
                                "  [!] Grafo vuoto: esegui `codeos index <path>` per popolarlo"
                            );
                        } else if !calibrated {
                            println!(
                                "  [!] Confidenza non calibrata: avvia il server con CODEOS_REPO\n      puntato al repo git per attivare Campo di Astensione + Fossili"
                            );
                        }
                    }
                    Err(status) => {
                        problems += 1;
                        println!("  [✗] Il server risponde ma la RPC fallisce: {status}");
                    }
                }
            }
            Err(e) => {
                problems += 1;
                println!("  [✗] Handshake gRPC fallito: {e}");
            }
        }
    } else {
        println!("  [-] Liveness gRPC saltata (porta non raggiungibile)");
    }

    println!();
    if problems == 0 {
        println!("✅ Tutto a posto: CodeOS è pronto all'uso.");
    } else {
        println!("⚠️  {problems} problema/i rilevato/i. Risolvi i punti [✗] qui sopra.");
        std::process::exit(1);
    }
}

fn print_usage() {
    println!("🏛️  CodeOS — Architectural Intelligence Layer CLI");
    println!();
    println!("Uso:");
    println!("  codeos <comando> [argomenti]");
    println!();
    println!("Comandi:");
    println!("  index <path>      Indicizza il progetto all'interno del percorso fornito");
    println!("  report [opzioni]  Mostra il referto architetturale dello spazio negativo (compatto di default)");
    println!("  query \"<text>\"    Interroga il grafo semantico per generare il contesto minimo per l'LLM");
    println!("  doctor            Diagnostica la configurazione (server, porta, indirizzo) prima dell'uso");
    println!("  help              Mostra questo aiuto");
    println!();
    println!("Opzioni di `report`:");
    println!("  --verbose, -v     Mostra tutto: anche gli invarianti a bassa confidenza e i fossili per esteso");
    println!("  --only high-risk  Mostra solo gli esiti ad alto rischio");
    println!(
        "  --json            Stampa il referto come JSON (per CI e agent AI), senza decorazioni"
    );
    println!();
    println!("Variabili d'ambiente:");
    println!("  CODEOS_ADDR       Indirizzo del server gRPC (default: 127.0.0.1:50051)");
}

/// Opzioni del comando `report`, derivate dai flag CLI.
struct ReportOptions {
    /// Mostra tutto: anche gli invarianti a bassa confidenza (info) e i fossili per
    /// esteso. Senza, il referto è **compatto** (solo warning/high_risk, liste cap).
    verbose: bool,
    /// Stampa il referto come JSON puro invece del rendering da terminale.
    json: bool,
    /// Mostra solo gli esiti ad alto rischio.
    only_high_risk: bool,
}

/// Quanti invarianti/lacune mostrare al massimo nel referto **compatto**: oltre
/// questa soglia compare un riepilogo «… e altri N». In `--verbose` nessun cap.
const COMPACT_INVARIANTS: usize = 6;
const COMPACT_GAPS: usize = 4;

/// Interpreta i flag che seguono `report`. Tollerante: un flag sconosciuto è
/// ignorato (forward-compat), così aggiungere opzioni non rompe gli script.
fn parse_report_options(args: &[String]) -> ReportOptions {
    let mut opts = ReportOptions {
        verbose: false,
        json: false,
        only_high_risk: false,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--verbose" | "-v" => opts.verbose = true,
            "--json" => opts.json = true,
            // Accetta sia "--only high-risk" (due token) sia "--only=high-risk".
            "--only" => {
                if let Some(val) = args.get(i + 1) {
                    opts.only_high_risk |= is_high_risk_token(val);
                    i += 1;
                }
            }
            _ if arg.starts_with("--only=") => {
                opts.only_high_risk |= is_high_risk_token(&arg["--only=".len()..]);
            }
            _ => {}
        }
        i += 1;
    }
    opts
}

/// Riconosce l'unico filtro di severità supportato oggi (`high-risk`), tollerando
/// le grafie con trattino/underscore.
fn is_high_risk_token(s: &str) -> bool {
    matches!(s, "high-risk" | "high_risk" | "highrisk")
}

/// Decide se una severità ("info"/"warning"/"high_risk") va mostrata, date le opzioni.
/// Compatto di default nasconde gli `info` (probabile rumore); `--verbose` mostra
/// tutto; `--only high-risk` tiene solo l'alto rischio.
fn severity_passes(severity: &str, opts: &ReportOptions) -> bool {
    if opts.only_high_risk {
        return severity == "high_risk";
    }
    if opts.verbose {
        return true;
    }
    severity == "high_risk" || severity == "warning"
}

/// Serializza il referto in un `serde_json::Value` per il flag `--json`: una forma
/// stabile e piatta, pensata per CI e agent AI (nessuna decorazione, solo dati).
fn report_to_json(report: &proto::GetArchitectureReportResponse) -> serde_json::Value {
    use serde_json::json;
    json!({
        "invariants": report.invariants.iter().map(|i| json!({
            "upstream": i.upstream,
            "downstream": i.downstream,
            "support": i.support,
            "confidence": i.confidence,
            "calibrated": i.calibrated,
            "severity": i.severity,
            "origin": i.origin,
        })).collect::<Vec<_>>(),
        "fossils": report.fossils.iter().map(|f| json!({
            "upstream": f.upstream,
            "downstream": f.downstream,
            "born_at": f.born_at,
            "born_at_unix": f.born_at_unix,
            "intent": f.intent,
            "born_structure": f.born_structure,
        })).collect::<Vec<_>>(),
        "gaps": report.gaps.iter().map(|g| json!({
            "upstream": g.upstream,
            "downstream": g.downstream,
            "foundation_support": g.foundation_support,
            "severity": g.severity,
        })).collect::<Vec<_>>(),
        "quality": report.quality.as_ref().map(|q| json!({
            "total_entities": q.total_entities,
            "external_entities": q.external_entities,
            "total_relations": q.total_relations,
            "resolved_relations": q.resolved_relations,
            "unresolved_relations": q.unresolved_relations,
            "low_confidence_relations": q.low_confidence_relations,
        })),
    })
}

/// La sezione **Qualità del grafo** (roadmap P2-7): dichiara esplicitamente quanto
/// fidarsi del referto. `info_invariants` è il numero di invarianti a confidenza
/// bassa (severità "info"), che insieme agli archi a bassa confidenza formano la
/// stima dei "possibili falsi positivi".
fn render_graph_quality(quality: &Option<proto::GraphQuality>, info_invariants: u64) {
    println!("\n🔬 QUALITÀ DEL GRAFO (quanto fidarsi di questo referto)");
    println!("------------------------------------------------------");
    let Some(q) = quality else {
        println!("  (Il server non ha riportato metriche di qualità.)");
        return;
    };
    let pct = if q.total_relations > 0 {
        (q.resolved_relations as f64 / q.total_relations as f64 * 100.0).round() as u64
    } else {
        100
    };
    println!(
        "  • Entità totali:           {} (di cui {} esterne tracciate)",
        q.total_entities, q.external_entities
    );
    println!(
        "  • Relazioni risolte:       {} / {} ({}%)",
        q.resolved_relations, q.total_relations, pct
    );
    println!(
        "  • Relazioni non risolte:   {} (riferimenti non agganciati: un arco mancante è meglio di uno che mente)",
        q.unresolved_relations
    );
    if q.low_confidence_relations > 0 {
        println!(
            "  • Archi a bassa confidenza: {} (esclusi dal mining)",
            q.low_confidence_relations
        );
    }
    let false_positives = q.low_confidence_relations + info_invariants;
    if false_positives > 0 {
        println!(
            "  • Possibili falsi positivi: {} (archi a bassa confidenza + invarianti < 50%)",
            false_positives
        );
    } else {
        println!("  • Possibili falsi positivi: ✅ nessuno evidente");
    }
}

fn render_terminal_report(report: proto::GetArchitectureReportResponse, opts: &ReportOptions) {
    println!("\n=======================================================");
    println!("       🏛️  REFERTO ARCHITETTURALE DI CODEOS  ");
    println!("=======================================================");
    if !opts.verbose && !opts.only_high_risk {
        println!("(compatto — `--verbose` per tutto, `--json` per CI/AI, `--only high-risk` per i soli rischi)");
    }

    // --- SINTESI AD ALTO LIVELLO ---
    println!("\n📋 SINTESI DIREZIONALE");
    println!("----------------------");

    // Calcolo Fondazioni Principali
    let mut upstream_counts = std::collections::HashMap::new();
    for inv in &report.invariants {
        *upstream_counts.entry(&inv.upstream).or_insert(0) += 1;
    }
    let mut upstreams: Vec<(&String, i32)> = upstream_counts.into_iter().collect();
    upstreams.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    if upstreams.is_empty() {
        println!("  • Fondazioni: Nessun modulo identificato come fondazione solida.");
    } else {
        let top_upstreams: Vec<String> = upstreams
            .iter()
            .take(3)
            .map(|(name, count)| format!("{} (supportato da {} layer)", name, count))
            .collect();
        println!("  • Fondazioni principali: {}", top_upstreams.join(", "));
    }

    // Calcolo Layer più dipendenti
    let mut downstream_counts = std::collections::HashMap::new();
    for inv in &report.invariants {
        *downstream_counts.entry(&inv.downstream).or_insert(0) += 1;
    }
    let mut downstreams: Vec<(&String, i32)> = downstream_counts.into_iter().collect();
    downstreams.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    if downstreams.is_empty() {
        println!("  • Dipendenze: Nessun modulo identificato ad alta dipendenza.");
    } else {
        let top_downstreams: Vec<String> = downstreams
            .iter()
            .take(3)
            .map(|(name, count)| format!("{} (dipende da {} layer)", name, count))
            .collect();
        println!("  • Layer più dipendenti:  {}", top_downstreams.join(", "));
    }

    // Calcolo Rischi
    if report.gaps.is_empty() {
        println!("  • Rischi rilevati:       ✅ Nessun accoppiamento bidirezionale anomalo.");
    } else {
        println!("  • Rischi rilevati:       ⚠️  Rilevate {} lacune architetturali (accoppiamenti bidirezionali).", report.gaps.len());
    }

    // Azioni consigliate
    println!("\n🎯 AZIONI CONSIGLIATE");
    println!("---------------------");
    let mut actions = Vec::new();
    if !report.gaps.is_empty() {
        actions.push(format!(
            "Risolvi l'accoppiamento ciclico anomalo tra i layer '{}' e '{}'",
            report.gaps[0].upstream, report.gaps[0].downstream
        ));
    }
    let uncalibrated = report.invariants.iter().any(|inv| !inv.calibrated);
    if uncalibrated {
        actions.push("Avvia il server impostando CODEOS_REPO puntando alla cartella git per calibrare la confidenza sul tempo reale".to_string());
    }
    if report.invariants.is_empty() {
        actions.push("Indicizza altri file o moduli del progetto per far emergere gli invarianti strutturali latenti".to_string());
    } else {
        actions.push(format!(
            "Consolida il confine architetturale a difesa di '{}'",
            report.invariants[0].upstream
        ));
    }

    for (idx, action) in actions.iter().enumerate() {
        println!("  {}. {}", idx + 1, action);
    }

    // --- QUALITÀ DEL GRAFO (P2-7): quanto fidarsi del referto qui sopra ---
    let info_invariants = report
        .invariants
        .iter()
        .filter(|inv| inv.severity == "info")
        .count() as u64;
    render_graph_quality(&report.quality, info_invariants);

    // --- INVARIANTI DI LAYERING ---
    println!("\n🧱 INVARIANTI DI LAYERING (Asse Struttura & Tempo)");
    println!("--------------------------------------------------");
    // Filtra per severità (compatto = niente "info") e ordina i più gravi in cima.
    let mut invariants: Vec<&proto::LayeringInvariant> = report
        .invariants
        .iter()
        .filter(|inv| severity_passes(&inv.severity, opts))
        .collect();
    invariants.sort_by_key(|inv| std::cmp::Reverse(severity_rank(&inv.severity)));
    if invariants.is_empty() {
        if report.invariants.is_empty() {
            println!("  (Nessun invariante di layering scoperto)");
        } else {
            println!("  (Nessuno oltre la soglia di rilevanza; usa --verbose per vederli tutti)");
        }
    } else {
        let cap = if opts.verbose {
            invariants.len()
        } else {
            COMPACT_INVARIANTS.min(invariants.len())
        };
        for inv in &invariants[..cap] {
            let conf_pct = (inv.confidence * 100.0).round();
            if opts.verbose {
                let source = if inv.calibrated {
                    "tempo / git log"
                } else {
                    "strutturale / statico"
                };
                println!(
                    "  • {} '{}' NON deve dipendere da '{}'\n    [Origine: {} | Supporto: {} archi | Confidenza: {}% | Calibrato: {}]",
                    severity_badge(&inv.severity), inv.upstream, inv.downstream, origin_label(&inv.origin), inv.support, conf_pct, source
                );
            } else {
                println!(
                    "  {} '{}' NON deve dipendere da '{}'  [sup {} · conf {}% · {}]",
                    severity_badge(&inv.severity),
                    inv.upstream,
                    inv.downstream,
                    inv.support,
                    conf_pct,
                    origin_label(&inv.origin)
                );
            }
        }
        let hidden = invariants.len() - cap;
        if hidden > 0 {
            println!("  … e altri {hidden} (usa --verbose per l'elenco completo)");
        }
    }

    // --- FOSSILI DI DECISIONE ---
    println!("\n🦴 FOSSILI DI DECISIONE (Asse Intento)");
    println!("--------------------------------------");
    if report.fossils.is_empty() {
        println!("  (Nessun fossile estratto dalla storia git)");
    } else if opts.verbose {
        for f in &report.fossils {
            let hash = if f.born_at.len() >= 12 {
                &f.born_at[..12]
            } else {
                &f.born_at
            };
            println!("  • Confine '{}' → '{}'", f.downstream, f.upstream);
            println!("    Nato nel commit: [{}] «{}»", hash, f.intent);
            if !f.born_structure.is_empty() {
                println!("    File co-modificati: {}", f.born_structure.join(", "));
            }
        }
    } else {
        // Compatto: il dettaglio è verboso (2-3 righe a fossile). Qui un solo
        // esempio + il conteggio; la storia completa è dietro --verbose.
        let f = &report.fossils[0];
        let hash = if f.born_at.len() >= 12 {
            &f.born_at[..12]
        } else {
            &f.born_at
        };
        println!(
            "  • {} confini datati; es. '{}' → '{}' nato in [{}] «{}»",
            report.fossils.len(),
            f.downstream,
            f.upstream,
            hash,
            f.intent
        );
        println!("    (usa --verbose per la storia completa di ogni confine)");
    }

    // --- LACUNE DEL SECONDO ORDINE ---
    println!("\n🕳️  LACUNE DEL SECONDO ORDINE (Asse Meta)");
    println!("------------------------------------------");
    let mut gaps: Vec<&proto::ArchitecturalGap> = report
        .gaps
        .iter()
        .filter(|g| severity_passes(&g.severity, opts))
        .collect();
    gaps.sort_by_key(|g| std::cmp::Reverse(severity_rank(&g.severity)));
    if gaps.is_empty() {
        if report.gaps.is_empty() {
            println!("  ✅ Ogni fondazione è pienamente rispettata senza eccezioni.");
        } else {
            println!("  (Nessuna oltre la soglia di rilevanza; usa --verbose per vederle tutte)");
        }
    } else {
        let cap = if opts.verbose {
            gaps.len()
        } else {
            COMPACT_GAPS.min(gaps.len())
        };
        for gap in &gaps[..cap] {
            println!(
                "  • {} La fondazione '{}' è violata dall'eccezione '{}'",
                severity_badge(&gap.severity),
                gap.upstream,
                gap.downstream
            );
            // Il "perché" della lacuna (roadmap 13): non è un buco arbitrario, è
            // un'eccezione a una convenzione che il resto del codice rispetta.
            if opts.verbose {
                println!(
                    "    Perché: '{up}' è una fondazione rispettata a senso unico da {n} altri layer, \
                     ma '{down}' dipende da '{up}' E '{up}' dipende da '{down}' (accoppiamento bidirezionale).",
                    up = gap.upstream, down = gap.downstream, n = gap.foundation_support
                );
            }
        }
        let hidden = gaps.len() - cap;
        if hidden > 0 {
            println!("  … e altre {hidden} (usa --verbose per l'elenco completo)");
        }
    }
    println!("\n=======================================================\n");
}

/// Badge leggibile per una severità trasportata come stringa ("info"/"warning"/
/// "high_risk"). Sconosciuto → neutro.
fn severity_badge(severity: &str) -> &'static str {
    match severity {
        "high_risk" => "🔴",
        "warning" => "🟡",
        "info" => "⚪️",
        _ => "•",
    }
}

/// Etichetta leggibile per la provenienza di una regola ("discovered"/"declared").
/// Una regola dichiarata è una volontà esplicita dell'umano; una scoperta è dedotta
/// dallo spazio negativo del grafo.
fn origin_label(origin: &str) -> &'static str {
    match origin {
        "declared" => "📜 dichiarato",
        "discovered" => "🔍 scoperto",
        _ => "🔍 scoperto",
    }
}

/// Ordine di priorità per l'ordinamento (più alto = mostrato prima).
fn severity_rank(severity: &str) -> u8 {
    match severity {
        "high_risk" => 3,
        "warning" => 2,
        "info" => 1,
        _ => 0,
    }
}
