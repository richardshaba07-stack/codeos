//! `codeos` — la CLI di CodeOS per interrogare e controllare il sistema da terminale.
//!
//! Legge `CODEOS_ADDR` per l'indirizzo del server (default: `127.0.0.1:50051`).

use std::net::ToSocketAddrs;
use std::path::Path;
use std::time::Duration;
use codeos_rpc::proto;
use proto::code_os_client::CodeOsClient;

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
            let absolute_path = Path::new(raw_path)
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("Impossibile trovare il percorso '{}': {}", raw_path, e))?;
            let project_root = absolute_path.to_string_lossy().to_string();

            let mut client = connect_server().await?;
            println!("⚡ Invio richiesta di indicizzazione per: {}", project_root);
            
            let req = proto::IndexProjectRequest { project_root };
            client.index_project(req).await?;
            
            println!("🎉 Indicizzazione completata con successo!");
        }
        "report" => {
            let mut client = connect_server().await?;
            println!("⚡ Richiesta referto architetturale in corso...");
            
            let req = proto::GetArchitectureReportRequest {};
            let response = client.get_architecture_report(req).await?.into_inner();
            
            render_terminal_report(response);
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
            println!("ℹ️  Trovate {} entità e {} relazioni rilevanti.", response.entities.len(), response.relations.len());
        }
        "doctor" => {
            run_doctor().await;
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
    let mut address = std::env::var("CODEOS_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50051".to_string());
    
    if !address.starts_with("http://") && !address.starts_with("https://") {
        address = format!("http://{}", address);
    }

    CodeOsClient::connect(address)
        .await
        .map_err(|e| anyhow::anyhow!(
            "Impossibile connettersi al server CodeOS. È attivo? Dettaglio: {}", e
        ))
}

/// `codeos doctor` — controlla, senza modificare nulla, che l'ambiente sia pronto:
/// indirizzo configurato, porta raggiungibile, server gRPC vivo e in grado di
/// produrre un referto. Stampa diagnosi azionabili e termina con exit code 1 se
/// qualcosa è rotto, così è usabile anche in script/CI.
async fn run_doctor() {
    println!("🩺 CodeOS doctor — diagnosi dell'ambiente\n");
    let mut problems = 0u32;

    // 1) Indirizzo del server.
    let raw_addr =
        std::env::var("CODEOS_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".to_string());
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
    println!("  report            Mostra il referto architetturale completo dello spazio negativo");
    println!("  query \"<text>\"    Interroga il grafo semantico per generare il contesto minimo per l'LLM");
    println!("  doctor            Diagnostica la configurazione (server, porta, indirizzo) prima dell'uso");
    println!("  help              Mostra questo aiuto");
    println!();
    println!("Variabili d'ambiente:");
    println!("  CODEOS_ADDR       Indirizzo del server gRPC (default: 127.0.0.1:50051)");
}

fn render_terminal_report(report: proto::GetArchitectureReportResponse) {
    println!("\n=======================================================");
    println!("       🏛️  REFERTO ARCHITETTURALE DI CODEOS  ");
    println!("=======================================================\n");

    // --- SINTESI AD ALTO LIVELLO ---
    println!("📋 SINTESI DIREZIONALE");
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
        let top_upstreams: Vec<String> = upstreams.iter().take(3).map(|(name, count)| format!("{} (supportato da {} layer)", name, count)).collect();
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
        let top_downstreams: Vec<String> = downstreams.iter().take(3).map(|(name, count)| format!("{} (dipende da {} layer)", name, count)).collect();
        println!("  • Layer più dipendenti:  {}", top_downstreams.join(", "));
    }

    // Calcolo Rischi
    if report.gaps.is_empty() {
        println!("  • Rischi rilevati:       ✅ Nessun accoppiamento bidirezionale anomalo.");
    } else {
        println!("  • Rischi rilevati:       ⚠️  Rilevate {} lacune architetturali (accoppiamenti bidirezionali).", report.gaps.len());
    }

    // Calcolo Falsi Positivi
    let low_conf = report.invariants.iter().filter(|inv| inv.confidence < 0.5).count();
    if low_conf > 0 {
        println!("  • Falsi positivi:        🔍 {} invarianti hanno confidenza bassa (< 50%).", low_conf);
    } else {
        println!("  • Falsi positivi:        ✅ Tutti gli invarianti hanno confidenza elevata.");
    }

    // Azioni consigliate
    println!("\n🎯 AZIONI CONSIGLIATE");
    println!("---------------------");
    let mut actions = Vec::new();
    if !report.gaps.is_empty() {
        actions.push(format!("Risolvi l'accoppiamento ciclico anomalo tra i layer '{}' e '{}'", report.gaps[0].upstream, report.gaps[0].downstream));
    }
    let uncalibrated = report.invariants.iter().any(|inv| !inv.calibrated);
    if uncalibrated {
        actions.push("Avvia il server impostando CODEOS_REPO puntando alla cartella git per calibrare la confidenza sul tempo reale".to_string());
    }
    if report.invariants.is_empty() {
        actions.push("Indicizza altri file o moduli del progetto per far emergere gli invarianti strutturali latenti".to_string());
    } else {
        actions.push(format!("Consolida il confine architetturale a difesa di '{}'", report.invariants[0].upstream));
    }

    for (idx, action) in actions.iter().enumerate() {
        println!("  {}. {}", idx + 1, action);
    }

    // --- INVARIANTI DI LAYERING ---
    println!("\n🧱 INVARIANTI DI LAYERING (Asse Struttura & Tempo)");
    println!("--------------------------------------------------");
    if report.invariants.is_empty() {
        println!("  (Nessun invariante di layering scoperto)");
    } else {
        // Mostra prima gli invarianti ad alta priorità: i confini da difendere
        // emergono in cima, i probabili falsi positivi (info) scendono in fondo.
        let mut invariants: Vec<&proto::LayeringInvariant> = report.invariants.iter().collect();
        invariants.sort_by_key(|inv| std::cmp::Reverse(severity_rank(&inv.severity)));
        for inv in invariants {
            let conf_pct = (inv.confidence * 100.0).round();
            let source = if inv.calibrated { "tempo / git log" } else { "strutturale / statico" };
            println!(
                "  • {} '{}' NON deve dipendere da '{}'\n    [Supporto: {} archi | Confidenza: {}% | Calibrato: {}]",
                severity_badge(&inv.severity), inv.upstream, inv.downstream, inv.support, conf_pct, source
            );
        }
    }

    // --- FOSSILI DI DECISIONE ---
    println!("\n🦴 FOSSILI DI DECISIONE (Asse Intento)");
    println!("--------------------------------------");
    if report.fossils.is_empty() {
        println!("  (Nessun fossile estratto dalla storia git)");
    } else {
        for f in &report.fossils {
            let hash = if f.born_at.len() >= 12 { &f.born_at[..12] } else { &f.born_at };
            println!("  • Confine '{}' → '{}'", f.downstream, f.upstream);
            println!("    Nato nel commit: [{}] «{}»", hash, f.intent);
            if !f.born_structure.is_empty() {
                println!("    File co-modificati: {}", f.born_structure.join(", "));
            }
        }
    }

    // --- LACUNE DEL SECONDO ORDINE ---
    println!("\n🕳️  LACUNE DEL SECONDO ORDINE (Asse Meta)");
    println!("------------------------------------------");
    if report.gaps.is_empty() {
        println!("  ✅ Ogni fondazione è pienamente rispettata senza eccezioni.");
    } else {
        let mut gaps: Vec<&proto::ArchitecturalGap> = report.gaps.iter().collect();
        gaps.sort_by_key(|g| std::cmp::Reverse(severity_rank(&g.severity)));
        for gap in gaps {
            println!(
                "  • {} La fondazione '{}' è violata dall'eccezione '{}'",
                severity_badge(&gap.severity), gap.upstream, gap.downstream
            );
            // Il "perché" della lacuna (roadmap 13): non è un buco arbitrario, è
            // un'eccezione a una convenzione che il resto del codice rispetta.
            println!(
                "    Perché: '{up}' è una fondazione rispettata a senso unico da {n} altri layer, \
                 ma '{down}' dipende da '{up}' E '{up}' dipende da '{down}' (accoppiamento bidirezionale).",
                up = gap.upstream, down = gap.downstream, n = gap.foundation_support
            );
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

/// Ordine di priorità per l'ordinamento (più alto = mostrato prima).
fn severity_rank(severity: &str) -> u8 {
    match severity {
        "high_risk" => 3,
        "warning" => 2,
        "info" => 1,
        _ => 0,
    }
}
