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
        "path" => {
            if args.len() < 4 {
                eprintln!("Errore: servono due nomi: l'entità di partenza e quella di arrivo.");
                eprintln!("Uso: codeos path <da> <a>");
                std::process::exit(1);
            }
            let from = &args[2];
            let to = &args[3];

            let mut client = connect_server().await?;
            println!("⚡ Cerco il cammino di chiamata da \"{from}\" a \"{to}\"...");

            let req = proto::CallPathRequest {
                from: from.clone(),
                to: to.clone(),
            };
            let response = client.call_path(req).await?.into_inner();

            println!("\n{}", response.formatted);

            // Esito binario: esci con codice ≠0 quando NON c'è un cammino trovato
            // (no_path/unknown/ambiguous), così script e CI lo colgono senza dover
            // analizzare il testo. Lo stato resta esplicito e onesto.
            if response.status != "found" {
                std::process::exit(1);
            }
        }
        "impact" => {
            // Flag opzionale --transitive (-t): il raggio TRANSITIVO (chi raggiunge
            // l'entità a QUALUNQUE distanza) invece dei soli chiamanti diretti. Il
            // nome è il primo argomento che non è un flag.
            let transitive = args.iter().any(|a| a == "--transitive" || a == "-t");
            let Some(name) = args.iter().skip(2).find(|a| !a.starts_with('-')).cloned() else {
                eprintln!("Errore: serve il nome dell'entità di cui misurare l'impatto.");
                eprintln!("Uso: codeos impact <nome> [--transitive]");
                std::process::exit(1);
            };

            let mut client = connect_server().await?;
            let status = if transitive {
                println!(
                    "⚡ Misuro l'impatto TRANSITIVO di \"{name}\" (chi la raggiunge, a qualunque distanza)..."
                );
                let req = proto::ImpactTransitiveRequest { name: name.clone() };
                let response = client.impact_transitive(req).await?.into_inner();
                println!("\n{}", response.formatted);
                response.status
            } else {
                println!("⚡ Misuro l'impatto di \"{name}\" (chi la chiama)...");
                let req = proto::ImpactRequest { name: name.clone() };
                let response = client.impact(req).await?.into_inner();
                println!("\n{}", response.formatted);
                response.status
            };

            // Esito esplicito: esci con codice ≠0 quando il nome NON si è risolto a
            // un'entità (unknown/ambiguous), così script e CI lo colgono senza
            // analizzare il testo. Attenzione: "found" con liste vuote è comunque
            // successo — l'entità esiste, semplicemente nessuno la chiama (per
            // quanto noto): non lo confondiamo con «nome inesistente».
            if status != "found" {
                std::process::exit(1);
            }
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
                            "      → {} invarianti, {} in formazione, {} fossili, {} lacune nel grafo corrente",
                            r.invariants.len(),
                            r.candidates.len(),
                            r.fossils.len(),
                            r.gaps.len()
                        );
                        let calibrated = r.invariants.iter().any(|i| i.calibrated);
                        // "Grafo vuoto" SOLO se non ci sono entità: 0 invarianti su un
                        // grafo popolato NON è un grafo vuoto, è semplicemente assenza
                        // di confini di layering chiari (collaudo: 714 entità segnalate
                        // come "vuoto" — falso allarme corretto).
                        let entity_count =
                            r.quality.as_ref().map(|q| q.total_entities).unwrap_or(0);
                        if entity_count == 0 {
                            println!(
                                "  [!] Grafo vuoto: esegui `codeos index <path>` per popolarlo"
                            );
                        } else if r.invariants.is_empty() {
                            println!(
                                "  [i] Grafo popolato ({entity_count} entità) ma nessun invariante di layering estratto (nessun confine asimmetrico chiaro, o manca la storia git)"
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

    // 4) Configurazione del server via variabili d'ambiente. Filosofia
    // anti-falso-positivo: marco come [✗] solo il *set-but-broken* (variabile
    // impostata che punta a un percorso inesistente) — l'unico caso inequivocabile,
    // che farebbe fallire l'avvio o degradare il referto in silenzio. Variabile non
    // impostata = scelta legittima (i default sono documentati), quindi solo nota [-].
    let repo = std::env::var("CODEOS_REPO").ok();
    let repo_diag = match repo.as_deref() {
        Some(p) => {
            let path = Path::new(p);
            let exists = path.exists();
            let is_git = exists && path.join(".git").exists();
            diagnose_repo(Some(p), exists, is_git)
        }
        None => diagnose_repo(None, false, false),
    };
    report_env_diag(&repo_diag, &mut problems);

    let db = std::env::var("CODEOS_DB").ok();
    let db_diag = match db.as_deref() {
        Some(v) => {
            let looks_like_uri = v.starts_with("file:") || v.contains("://") || v.contains('?');
            let path = Path::new(v);
            let parent_exists = match path.parent() {
                // Path senza componente di cartella (es. "graph.db") ⇒ cwd, che esiste.
                Some(dir) => dir.as_os_str().is_empty() || dir.exists(),
                None => true,
            };
            let file_exists = path.is_file();
            diagnose_db(Some(v), looks_like_uri, parent_exists, file_exists)
        }
        None => diagnose_db(None, false, false, false),
    };
    report_env_diag(&db_diag, &mut problems);

    println!();
    if problems == 0 {
        println!("✅ Tutto a posto: CodeOS è pronto all'uso.");
    } else {
        println!("⚠️  {problems} problema/i rilevato/i. Risolvi i punti [✗] qui sopra.");
        std::process::exit(1);
    }
}

/// Severità di una riga di diagnosi. Solo `Problem` conta come problema (incrementa
/// il contatore e fa uscire `doctor` con codice 1); `Ok` e `Note` sono informative.
#[derive(Debug, PartialEq, Eq)]
enum DiagKind {
    Ok,
    Note,
    Problem,
}

/// Una riga di diagnosi: severità, messaggio e un suggerimento azionabile opzionale.
#[derive(Debug)]
struct EnvDiag {
    kind: DiagKind,
    message: String,
    hint: Option<String>,
}

impl EnvDiag {
    fn ok(message: impl Into<String>) -> Self {
        Self {
            kind: DiagKind::Ok,
            message: message.into(),
            hint: None,
        }
    }
    fn note(message: impl Into<String>) -> Self {
        Self {
            kind: DiagKind::Note,
            message: message.into(),
            hint: None,
        }
    }
    fn problem(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            kind: DiagKind::Problem,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

/// Decisione **pura** per `CODEOS_REPO`, dati i fatti già rilevati dal filesystem
/// (separata dall'I/O così è testabile senza server né disco). Anti-falso-positivo:
/// solo *impostata-ma-inesistente* è un problema; non impostata o «esiste ma non è
/// git» sono note (il server degrada con grazia, non si rompe).
fn diagnose_repo(value: Option<&str>, exists: bool, is_git: bool) -> EnvDiag {
    match value {
        None => EnvDiag::note(
            "CODEOS_REPO non impostata: referto solo strutturale (Campo di Astensione e Fossili git disattivati)",
        ),
        Some(path) if !exists => EnvDiag::problem(
            format!("CODEOS_REPO impostata ma il percorso non esiste: '{path}'"),
            "Correggi CODEOS_REPO: deve puntare alla root del repository git",
        ),
        Some(path) if !is_git => EnvDiag::note(format!(
            "CODEOS_REPO punta a '{path}': esiste ma non sembra un repo git (manca .git), la storia non verrà letta"
        )),
        Some(path) => EnvDiag::ok(format!("CODEOS_REPO: '{path}' (storia git agganciabile)")),
    }
}

/// Decisione **pura** per `CODEOS_DB`, dati i fatti già rilevati dal filesystem.
/// Anti-falso-positivo: è un problema solo se la cartella che conterrebbe il DB non
/// esiste (l'apertura fallirebbe). Il file mancante NON è un errore: SQLite lo crea
/// al primo avvio. Le forme URI (`file:…`) sfuggono al controllo path e restano note.
fn diagnose_db(
    value: Option<&str>,
    looks_like_uri: bool,
    parent_exists: bool,
    file_exists: bool,
) -> EnvDiag {
    match value {
        None => EnvDiag::note(
            "CODEOS_DB non impostata: grafo SQLite in memoria (effimero), nessuna persistenza tra i riavvii",
        ),
        Some(_) if looks_like_uri => {
            EnvDiag::note("CODEOS_DB in forma URI (file:…): salto il controllo del filesystem")
        }
        Some(path) if !parent_exists => EnvDiag::problem(
            format!("CODEOS_DB impostata ('{path}') ma la cartella che la conterrebbe non esiste: l'apertura del DB fallirebbe"),
            "Crea la cartella padre, oppure correggi CODEOS_DB",
        ),
        Some(path) if file_exists => {
            EnvDiag::ok(format!("CODEOS_DB: userà il file esistente '{path}'"))
        }
        Some(path) => {
            EnvDiag::ok(format!("CODEOS_DB: il file '{path}' verrà creato al primo avvio"))
        }
    }
}

/// Stampa una [`EnvDiag`] col glifo della sua severità e l'eventuale hint, e
/// incrementa il contatore dei problemi solo se è un `Problem`.
fn report_env_diag(diag: &EnvDiag, problems: &mut u32) {
    match diag.kind {
        DiagKind::Ok => println!("  [✓] {}", diag.message),
        DiagKind::Note => println!("  [-] {}", diag.message),
        DiagKind::Problem => {
            *problems += 1;
            println!("  [✗] {}", diag.message);
        }
    }
    if let Some(hint) = &diag.hint {
        println!("      → {hint}");
    }
}

/// Catalogo dei comandi della CLI: `(sintassi, descrizione)`. **Unica fonte di
/// verità** per l'help. Ogni `arm` del dispatcher in `main` deve avere qui la sua
/// voce: un comando che funziona ma non è documentato è invisibile all'utente — il
/// tool mente per omissione su ciò che sa fare. Il test
/// `usage_documents_every_implemented_command` blinda questo invariante.
const COMMANDS: &[(&str, &str)] = &[
    (
        "index <path>",
        "Indicizza il progetto nel percorso indicato (popola il grafo semantico).",
    ),
    (
        "report [opzioni]",
        "Mostra il referto architetturale dello spazio negativo (compatto di default).",
    ),
    (
        "query \"<text>\"",
        "Interroga il grafo in linguaggio naturale e genera il contesto minimo per un LLM.",
    ),
    (
        "path <da> <a>",
        "Mostra il cammino di chiamata onesto tra due entità (segue solo archi Calls risolti; mai inventato).",
    ),
    (
        "impact <nome> [--transitive]",
        "Mostra chi chiama un'entità, separando i chiamanti confermati (archi Calls risolti) dai possibili (riferimenti non risolti che combaciano). Con --transitive: chi la raggiunge a QUALUNQUE distanza (solo confermati), con la distanza in hop.",
    ),
    (
        "doctor",
        "Diagnostica l'ambiente (indirizzo, porta, server gRPC) prima dell'uso.",
    ),
    (
        "guard --before \"<goal>\" | --after",
        "Firewall architetturale: stima l'impatto di una modifica (--before) o rileva le violazioni introdotte (--after).",
    ),
    (
        "context \"<goal>\" [--for ai]",
        "Genera un \"context pack\" in Markdown per un obiettivo (--for ai per il formato pensato per agent AI).",
    ),
    (
        "mri [--base <ref>] [--head <ref>]",
        "\"MRI\" architetturale di un PR: confronta due ref git (default: main..HEAD) e misura il rischio.",
    ),
    (
        "why \"<a>|<b>\"",
        "Time machine: perché esiste il confine tra due elementi (nascita, intento, decisioni correlate).",
    ),
    (
        "simulate \"move <src> to <dst>\"",
        "What-if di refactoring: cosa cambierebbe spostando un elemento da <src> a <dst>.",
    ),
    ("help", "Mostra questo aiuto."),
];

/// Costruisce il testo completo dell'help. Separato da [`print_usage`] così è
/// verificabile da unit test senza dover catturare lo stdout.
fn usage_text() -> String {
    let mut out = String::new();
    out.push_str("🏛️  CodeOS — Architectural Intelligence Layer CLI\n\n");
    out.push_str("Uso:\n  codeos <comando> [argomenti]\n\n");
    out.push_str("Comandi:\n");
    for (syntax, desc) in COMMANDS {
        out.push_str(&format!("  {syntax}\n      {desc}\n"));
    }
    out.push_str("\nOpzioni di `report`:\n");
    out.push_str(
        "  --verbose, -v     Mostra tutto: anche gli invarianti a bassa confidenza e i fossili per esteso\n",
    );
    out.push_str("  --only high-risk  Mostra solo gli esiti ad alto rischio\n");
    out.push_str(
        "  --json            Stampa il referto come JSON (per CI e agent AI), senza decorazioni\n",
    );
    out.push_str("\nVariabili d'ambiente:\n");
    out.push_str("  CODEOS_ADDR       Indirizzo del server gRPC (default: 127.0.0.1:50051)\n");
    out
}

fn print_usage() {
    print!("{}", usage_text());
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
            "staleness_secs": i.staleness_secs,
            "temporal_risk": temporal_risk(i.confidence, &i.severity, i.staleness_secs),
        })).collect::<Vec<_>>(),
        // Gli invarianti in formazione (stadio 1): niente confidence/severity — un
        // confine non ancora formato non si stima (trap #3); `needed` dice quanto manca.
        "candidates": report.candidates.iter().map(|c| json!({
            "upstream": c.upstream,
            "downstream": c.downstream,
            "support": c.support,
            "needed": c.needed,
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
            let risk_pct =
                (temporal_risk(inv.confidence, &inv.severity, inv.staleness_secs) * 100.0).round();
            if opts.verbose {
                let source = if inv.calibrated {
                    "tempo / git log"
                } else {
                    "strutturale / statico"
                };
                println!(
                    "  • {} '{}' NON deve dipendere da '{}'\n    [Origine: {} | Supporto: {} archi | Confidenza: {}% | Rischio temporale: {}% | Calibrato: {}]{}",
                    severity_badge(&inv.severity), inv.upstream, inv.downstream, origin_label(&inv.origin), inv.support, conf_pct, risk_pct, source, staleness_note(inv.staleness_secs)
                );
            } else {
                println!(
                    "  {} '{}' NON deve dipendere da '{}'  [sup {} · conf {}% · 🎯 risk {}% · {}]{}",
                    severity_badge(&inv.severity),
                    inv.upstream,
                    inv.downstream,
                    inv.support,
                    conf_pct,
                    risk_pct,
                    origin_label(&inv.origin),
                    staleness_note(inv.staleness_secs)
                );
            }
        }
        let hidden = invariants.len() - cap;
        if hidden > 0 {
            println!("  … e altri {hidden} (usa --verbose per l'elenco completo)");
        }
    }

    // --- INVARIANTI IN FORMAZIONE (stadio 1: candidati sotto soglia) ---
    // Lo stesso spazio negativo puro degli invarianti, ma non ancora a soglia piena.
    // Derivati e mai persistiti: un segnale, non una verità. Niente severità (un
    // confine non formato non si stima): in compatto un esempio + conteggio (come i
    // fossili), in verbose l'elenco completo.
    println!("\n🌱 INVARIANTI IN FORMAZIONE (Stadio 1 — Candidati)");
    println!("--------------------------------------------------");
    if report.candidates.is_empty() {
        println!("  (Nessun confine in formazione: nessuna asimmetria pura sotto soglia)");
    } else if opts.verbose {
        for c in &report.candidates {
            println!(
                "  • '{}' sta emergendo come dipendente a senso unico da '{}'\n    [Supporto: {} archi · {} alla promozione a invariante]",
                c.downstream,
                c.upstream,
                c.support,
                needed_phrase(c.needed)
            );
        }
    } else {
        // Il candidato in testa è il più vicino alla soglia (ordine: supporto desc).
        let c = &report.candidates[0];
        println!(
            "  • {} confini in formazione; es. '{}' → '{}' ({} alla promozione)",
            report.candidates.len(),
            c.downstream,
            c.upstream,
            needed_phrase(c.needed)
        );
        println!("    (derivati e mai persistiti; usa --verbose per l'elenco completo)");
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

/// Quanti archi mancano a un candidato per diventare invariante, in italiano con
/// l'accordo singolare/plurale ("gli manca 1 arco" / "gli mancano N archi").
/// `needed` è sempre ≥ 1 (un candidato è sotto soglia per costruzione).
fn needed_phrase(needed: u32) -> String {
    if needed == 1 {
        "gli manca 1 arco".to_string()
    } else {
        format!("gli mancano {needed} archi")
    }
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

/// Nota di rischio TEMPORALE per un invariante: vuota se manca il dato (niente storia
/// git, o confine mai co-toccato) o se è stato esercitato di recente; altrimenti
/// segnala da quanti giorni il confine non è esercitato. È il "confidente ma stantio"
/// reso visibile — la dimensione temporale del rischio (Guardian 2.0).
fn staleness_note(staleness_secs: Option<i64>) -> String {
    const STALE_THRESHOLD_SECS: i64 = 180 * 24 * 60 * 60; // ~6 mesi
    match staleness_secs {
        Some(s) if s > STALE_THRESHOLD_SECS => {
            format!(" · ⏳ stantio: ultimo esercizio {}g fa", s / (24 * 60 * 60))
        }
        _ => String::new(),
    }
}

/// **Rischio TEMPORALE** di un invariante (Guardian 2.0): un singolo numero in `[0,1]`
/// che combina i fattori GIÀ calibrati — quanto è REALE il confine (confidenza
/// Wilson), quanto è IMPORTANTE (severità) e quanto è ATTIVO (freschezza). NON
/// sostituisce nulla (trap #2): è una DERIVATA mostrata accanto a confidenza/severità,
/// non al posto loro. La freschezza è `1.0` se il confine è esercitato di recente O se
/// manca il dato temporale (l'assenza di evidenza NON penalizza); decade in modo
/// esponenziale (mezza-vita ~1 anno) quando è stantio.
///
/// Interpretazione onesta v1: alto ⇒ confine reale, importante e ATTIVO ⇒ massima
/// attenzione toccandolo ORA. Un confine stantio pesa meno perché meno conteso di
/// recente — NON perché sia sicuro violarlo: la confidenza resta alta e visibile a
/// parte. È una scelta di modello v1, dichiarata, facile da rivedere.
fn temporal_risk(confidence: f64, severity: &str, staleness_secs: Option<i64>) -> f64 {
    let sev = match severity {
        "high_risk" => 1.0,
        "warning" => 0.6,
        _ => 0.3, // info
    };
    let freshness = match staleness_secs {
        Some(s) if s > 0 => {
            const HALF_LIFE_SECS: f64 = 365.0 * 24.0 * 60.0 * 60.0;
            0.5_f64.powf(s as f64 / HALF_LIFE_SECS)
        }
        _ => 1.0, // niente dato temporale o esercitato adesso ⇒ freschezza piena
    };
    (confidence * sev * freshness).clamp(0.0, 1.0)
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

#[cfg(test)]
mod tests {
    use super::{
        diagnose_db, diagnose_repo, needed_phrase, proto, report_to_json, usage_text, DiagKind,
    };

    /// Ogni comando gestito dal dispatcher in `main` deve comparire nell'help.
    /// È il guard-rail contro la regressione «comando aggiunto al `match`, scordato
    /// nell'usage»: un comando funzionante ma non documentato è invisibile, cioè il
    /// tool mente per omissione su ciò che sa fare. La lista attesa è volutamente
    /// hard-coded (non derivata da `COMMANDS`) così rimuovere una voce dal catalogo
    /// fa fallire il test invece di passare in silenzio.
    #[test]
    fn usage_documents_every_implemented_command() {
        let usage = usage_text();
        for cmd in [
            "index", "report", "query", "path", "impact", "doctor", "guard", "context", "mri",
            "why", "simulate", "help",
        ] {
            assert!(
                usage.contains(cmd),
                "comando '{cmd}' gestito da main ma assente dall'help (usage_text)"
            );
        }
    }

    #[test]
    fn temporal_risk_combines_confidence_severity_and_freshness() {
        use super::temporal_risk;
        let yr = 365 * 24 * 60 * 60;
        // Confine reale (conf 1.0), importante (high_risk) e FRESCO (nessuna staleness)
        // ⇒ rischio massimo.
        assert!((temporal_risk(1.0, "high_risk", None) - 1.0).abs() < 1e-9);
        // La staleness fa decadere (mezza-vita ~1 anno): a 1 anno ≈ metà.
        let fresh = temporal_risk(1.0, "high_risk", Some(0));
        let one_year = temporal_risk(1.0, "high_risk", Some(yr));
        assert!(
            one_year < fresh,
            "stantio ⇒ rischio più basso: {one_year} < {fresh}"
        );
        assert!((one_year - 0.5).abs() < 0.02, "a 1 anno ≈ 0.5: {one_year}");
        // La severità pesa: a parità di tutto, 'info' < 'high_risk'.
        assert!(temporal_risk(1.0, "info", None) < temporal_risk(1.0, "high_risk", None));
        // Assenza di dato temporale NON penalizza (freschezza piena, trap #2).
        assert!((temporal_risk(0.8, "warning", None) - 0.8 * 0.6).abs() < 1e-9);
    }

    #[test]
    fn staleness_note_appears_only_when_meaningfully_stale() {
        use super::staleness_note;
        // Nessun dato (niente storia git / mai co-toccato) ⇒ nota vuota.
        assert_eq!(staleness_note(None), "");
        // Esercitato di recente (sotto ~6 mesi) ⇒ vuota: niente rumore.
        assert_eq!(staleness_note(Some(30 * 24 * 60 * 60)), "");
        // Stantio (oltre ~6 mesi) ⇒ nota con l'età in giorni.
        let note = staleness_note(Some(400 * 24 * 60 * 60));
        assert!(
            note.contains("stantio") && note.contains("400g"),
            "atteso «stantio … 400g»: {note}"
        );
    }

    /// `doctor` deve segnalare `CODEOS_REPO` solo quando è *impostata ma rotta*.
    /// Non impostata o «esiste ma non è git» non sono problemi: il referto degrada
    /// con grazia. È la regola anti-falso-positivo applicata alla diagnosi.
    #[test]
    fn diagnose_repo_flags_only_set_but_missing() {
        // Non impostata ⇒ nota, non problema (default legittimo).
        assert_eq!(diagnose_repo(None, false, false).kind, DiagKind::Note);
        // Impostata ma il path non esiste ⇒ problema inequivocabile.
        assert_eq!(
            diagnose_repo(Some("/non/esiste"), false, false).kind,
            DiagKind::Problem
        );
        // Esiste ma non è un repo git ⇒ nota (il server degrada con grazia).
        assert_eq!(
            diagnose_repo(Some("/tmp"), true, false).kind,
            DiagKind::Note
        );
        // Esiste ed è git ⇒ ok.
        assert_eq!(diagnose_repo(Some("/repo"), true, true).kind, DiagKind::Ok);
    }

    /// `doctor` deve segnalare `CODEOS_DB` solo quando la cartella che conterrebbe
    /// il file non esiste (l'apertura fallirebbe). Il file ancora assente NON è un
    /// errore: SQLite lo crea al primo avvio — segnalarlo sarebbe un falso positivo.
    #[test]
    fn diagnose_db_flags_only_missing_parent_dir() {
        // Non impostata ⇒ nota (grafo in memoria).
        assert_eq!(diagnose_db(None, false, false, false).kind, DiagKind::Note);
        // Forma URI ⇒ nota (controllo filesystem saltato, niente falsi positivi).
        assert_eq!(
            diagnose_db(Some("file:/x?mode=ro"), true, false, false).kind,
            DiagKind::Note
        );
        // Cartella padre mancante ⇒ problema (l'apertura fallirebbe).
        assert_eq!(
            diagnose_db(Some("/non/esiste/g.db"), false, false, false).kind,
            DiagKind::Problem
        );
        // Padre esiste, file presente ⇒ ok.
        assert_eq!(
            diagnose_db(Some("/tmp/g.db"), false, true, true).kind,
            DiagKind::Ok
        );
        // Padre esiste, file ancora assente ⇒ ok (SQLite lo crea), NON problema.
        assert_eq!(
            diagnose_db(Some("/tmp/g.db"), false, true, false).kind,
            DiagKind::Ok
        );
    }

    /// `report_to_json` deve esporre gli invarianti in formazione sotto `candidates`,
    /// coi quattro campi pubblici (upstream/downstream/support/needed) e SENZA
    /// confidence/severity: un confine non ancora formato non si stima (trap #3).
    /// È il guard-rail che il candidato messo sul filo resti visibile nel JSON.
    #[test]
    fn report_to_json_surfaces_candidates() {
        let report = proto::GetArchitectureReportResponse {
            candidates: vec![proto::LayeringCandidate {
                upstream: "core".to_string(),
                downstream: "ui".to_string(),
                support: 2,
                needed: 1,
            }],
            ..Default::default()
        };
        let json = report_to_json(&report);
        let candidates = json["candidates"]
            .as_array()
            .expect("`candidates` deve essere un array");
        assert_eq!(candidates.len(), 1, "il candidato deve comparire nel JSON");
        let c = &candidates[0];
        assert_eq!(c["upstream"], "core");
        assert_eq!(c["downstream"], "ui");
        assert_eq!(c["support"], 2);
        assert_eq!(c["needed"], 1);
        // Nessuna stima su un confine non formato: confidence/severity assenti (trap #3).
        assert!(
            c.get("confidence").is_none(),
            "un candidato non porta confidence (trap #3)"
        );
        assert!(
            c.get("severity").is_none(),
            "un candidato non porta severity (trap #3)"
        );
    }

    /// `needed_phrase` deve concordare in numero: «gli manca 1 arco» al singolare,
    /// «gli mancano N archi» al plurale. È microcopy, ma un tool che scrive «1 archi»
    /// suona rotto — e la fiducia nel referto si gioca anche su questi dettagli.
    #[test]
    fn needed_phrase_agrees_in_number() {
        assert_eq!(needed_phrase(1), "gli manca 1 arco");
        assert_eq!(needed_phrase(2), "gli mancano 2 archi");
        assert_eq!(needed_phrase(3), "gli mancano 3 archi");
    }
}
