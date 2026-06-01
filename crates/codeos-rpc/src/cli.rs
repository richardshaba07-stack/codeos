//! `codeos` — la CLI di CodeOS per interrogare e controllare il sistema da terminale.
//!
//! Legge `CODEOS_ADDR` per l'indirizzo del server (default: `127.0.0.1:50051`).

use std::path::Path;
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
    upstreams.sort_by(|a, b| b.1.cmp(&a.1));

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
    downstreams.sort_by(|a, b| b.1.cmp(&a.1));

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
        for inv in &report.invariants {
            let conf_pct = (inv.confidence * 100.0).round();
            let source = if inv.calibrated { "tempo / git log" } else { "strutturale / statico" };
            println!(
                "  • '{}' NON deve dipendere da '{}'\n    [Supporto: {} archi | Confidenza: {}% | Calibrato: {}]",
                inv.upstream, inv.downstream, inv.support, conf_pct, source
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
        for gap in &report.gaps {
            println!(
                "  • La fondazione '{}' è violata dall'eccezione '{}'\n    (Attenzione: {} altri layer rispettano questa fondazione)",
                gap.upstream, gap.downstream, gap.foundation_support
            );
        }
    }
    println!("\n=======================================================\n");
}
