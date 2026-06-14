//! Server **MCP** (Model Context Protocol) su stdio: `codeos mcp`.
//!
//! È il canale di DISTRIBUZIONE del moat: un agente (Claude Code, Cursor, …)
//! aggiunge CodeOS come MCP server e chiama `query`/`why`/`context`/`decide`
//! come tool nativi — senza shell-out, senza parsing di output umano.
//!
//! Scelta di forma: un ADATTATORE SOTTILE. Niente logica nuova qui dentro —
//! ogni tool inoltra all'RPC gRPC esistente (lo stesso che serve CLI e IDE),
//! così c'è UNA sola semantica e l'MCP non può divergere dal resto.
//!
//! Protocollo, onestamente minimale: JSON-RPC 2.0, un messaggio per riga
//! (transport stdio di MCP), metodi `initialize`, `tools/list`, `tools/call`,
//! `ping`; capability dichiarata: SOLO `tools` (niente resources/prompts/
//! sampling finché non servono — meglio un server piccolo e corretto che una
//! superficie ampia e mal implementata).

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::connect_server;
use codeos_rpc::proto;

/// Avvia il server MCP su stdin/stdout. Ritorna quando stdin si chiude
/// (l'host MCP ha terminato la sessione).
pub async fn serve() -> anyhow::Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                // Parse error: id ignoto per definizione → id null (JSON-RPC 2.0).
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                });
                write_line(&mut stdout, &resp).await?;
                continue;
            }
        };
        if let Some(response) = handle(request).await {
            write_line(&mut stdout, &response).await?;
        }
    }
    Ok(())
}

async fn write_line(stdout: &mut tokio::io::Stdout, v: &Value) -> anyhow::Result<()> {
    let mut buf = serde_json::to_vec(v)?;
    buf.push(b'\n');
    stdout.write_all(&buf).await?;
    stdout.flush().await?;
    Ok(())
}

/// Gestisce UN messaggio JSON-RPC. `None` = nessuna risposta dovuta (notifiche).
/// Pura rispetto allo stdio: testabile senza processo.
pub async fn handle(request: Value) -> Option<Value> {
    let method = request.get("method")?.as_str()?.to_string();
    let id = request.get("id").cloned();

    // Le notifiche (senza id) non ricevono MAI risposta, qualunque sia il metodo.
    if id.is_none() || id == Some(Value::Null) {
        return None;
    }
    let id = id.unwrap();

    let result: Result<Value, (i64, String)> = match method.as_str() {
        "initialize" => {
            // Echo della versione richiesta dal client (siamo un server minimale e
            // version-tollerante); fallback alla baseline stabile del protocollo.
            let requested = request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            Ok(json!({
                "protocolVersion": requested,
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "codeos",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // Gli errori di ESECUZIONE del tool sono un risultato con isError
            // (l'agente li vede e può reagire), NON un errore JSON-RPC: quello è
            // riservato ai problemi di protocollo (metodo/forma sconosciuti).
            Ok(call_tool(&name, &args).await)
        }
        _ => Err((-32601, format!("metodo sconosciuto: {method}"))),
    };

    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        }),
    })
}

/// Il catalogo dei tool: gli stessi assi della CLI (struttura, tempo, intento),
/// con schemi d'input minimi. Le descrizioni dicono COSA torna e quando il tool
/// si astiene — un agente deve sapere che «vuoto» è una risposta onesta.
fn tool_definitions() -> Value {
    json!([
        {
            "name": "codeos_query",
            "description": "Contesto architetturale per un task in linguaggio naturale: il PERCHÉ dal ledger di intento (in testa), i file/entità rilevanti, dipendenze, impatto, percorsi di chiamata. Le sezioni assenti = il grafo non sa, mai inventato.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string", "description": "Il task o la domanda (es. 'sistemare il login OAuth')."}},
                "required": ["text"]
            }
        },
        {
            "name": "codeos_why",
            "description": "Perché esiste il confine tra due moduli: nascita (commit+intento), STORIA del confine (i commit che l'hanno esercitato, verbatim), decisioni del ledger. Si astiene se non c'è prova («non lo invento»).",
            "inputSchema": {
                "type": "object",
                "properties": {"boundary": {"type": "string", "description": "I due estremi: 'moduloA|moduloB'."}},
                "required": ["boundary"]
            }
        },
        {
            "name": "codeos_impact",
            "description": "Chi chiama un'entità: chiamanti CONFERMATI (archi risolti) separati dai POSSIBILI (match di nome non risolti, da verificare). Con transitive=true: chi la raggiunge a qualunque distanza (solo confermati, con gli hop).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Nome dell'entità (es. 'save_checkpoint')."},
                    "transitive": {"type": "boolean", "description": "Raggio transitivo invece dei soli diretti."}
                },
                "required": ["name"]
            }
        },
        {
            "name": "codeos_context_pack",
            "description": "Context pack compatto per un goal: WHY dal ledger (in testa), FILES, ENTITIES, BOUNDARIES da preservare, PATTERNS, TESTS, RISK. Formato chiave:valore pensato per agenti.",
            "inputSchema": {
                "type": "object",
                "properties": {"goal": {"type": "string", "description": "Il goal della modifica."}},
                "required": ["goal"]
            }
        },
        {
            "name": "codeos_decide",
            "description": "Registra una decisione architetturale nel ledger di intento (il PERCHÉ che git non dice). Persiste in <repo>/.codeos/decisions e riemerge in codeos_why, codeos_query e codeos_context_pack.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "La decisione, in una riga."},
                    "why": {"type": "string", "description": "Il razionale: perché è così."},
                    "boundary": {"type": "string", "description": "Opzionale: 'moduloA|moduloB' — aggancia la decisione al confine."},
                    "tags": {"type": "string", "description": "Opzionale: tag separati da virgola (nomi di moduli/entità)."},
                    "author": {"type": "string", "description": "Opzionale: autore (default 'agent:mcp')."}
                },
                "required": ["title", "why"]
            }
        },
        {
            "name": "codeos_licenses",
            "description": "Licenze delle dipendenze (dai metadati locali; vuota = sconosciuta, mai indovinata) + avvisi nei SORGENTI (tag SPDX, intestazioni di copyright, file LICENSE vendored) + violazioni della policy del ledger (decisioni con tag license-deny:<ID>). Da chiamare PRIMA di aggiungere una dipendenza o incollare codice di terzi.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "codeos_report",
            "description": "Referto architetturale in JSON: invarianti di layering (confidenza Wilson calibrata su git), candidati in formazione, fossili di decisione, lacune, qualità del grafo. Per orientarsi prima di toccare l'architettura.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "codeos_audit",
            "description": "Verifica l'integrità del ledger di intento: segnala le decisioni la cui PROVENIENZA è sparita (commit riscritto/squashato o file ADR cancellato). Anti-FP: solo fatti verificati via git/filesystem, mai un sospetto. Sola lettura, nessun server. Utile prima di fidarsi del ledger o come gate in CI.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Radice del repository (default: CODEOS_REPO o la cartella corrente)."}
                }
            }
        }
    ])
}

/// Esegue un tool inoltrando al gRPC. Ogni esito — anche il fallimento — è un
/// RISULTATO testuale per l'agente (`isError` true sui fallimenti).
async fn call_tool(name: &str, args: &Value) -> Value {
    match dispatch_tool(name, args).await {
        Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
        Err(e) => json!({
            "content": [{"type": "text", "text": format!("errore: {e}")}],
            "isError": true
        }),
    }
}

fn arg_str(args: &Value, key: &str) -> anyhow::Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("argomento obbligatorio mancante: '{key}'"))
}

async fn dispatch_tool(name: &str, args: &Value) -> anyhow::Result<String> {
    match name {
        "codeos_query" => {
            let text = arg_str(args, "text")?;
            let mut client = connect_server().await?;
            let res = client
                .query_graph(proto::QueryGraphRequest {
                    natural_language: text,
                })
                .await?
                .into_inner();
            Ok(res.formatted_context)
        }
        "codeos_why" => {
            let boundary = arg_str(args, "boundary")?;
            let mut client = connect_server().await?;
            let res = client
                .why(proto::WhyRequest { expr: boundary })
                .await?
                .into_inner();
            let mut out = res.explanation;
            if !res.intent.is_empty() {
                out.push_str(&format!("\nIntento alla nascita: «{}»", res.intent));
            }
            if !res.boundary_story.is_empty() {
                out.push_str("\nStoria del confine (commit che l'hanno esercitato):");
                for line in &res.boundary_story {
                    out.push_str(&format!("\n- {line}"));
                }
            }
            if !res.markdown_decisions.is_empty() {
                out.push_str("\nDecisioni dal ledger:");
                for dec in &res.markdown_decisions {
                    out.push_str(&format!("\n{dec}"));
                }
            }
            Ok(out)
        }
        "codeos_impact" => {
            let entity = arg_str(args, "name")?;
            let transitive = args
                .get("transitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut client = connect_server().await?;
            if transitive {
                let res = client
                    .impact_transitive(proto::ImpactTransitiveRequest { name: entity })
                    .await?
                    .into_inner();
                Ok(res.formatted)
            } else {
                let res = client
                    .impact(proto::ImpactRequest { name: entity })
                    .await?
                    .into_inner();
                Ok(res.formatted)
            }
        }
        "codeos_context_pack" => {
            let goal = arg_str(args, "goal")?;
            let mut client = connect_server().await?;
            let res = client
                .get_context_pack(proto::GetContextPackRequest { goal, for_ai: true })
                .await?
                .into_inner();
            Ok(res.formatted_markdown)
        }
        "codeos_decide" => {
            let title = arg_str(args, "title")?;
            let rationale = arg_str(args, "why")?;
            let author = arg_str(args, "author").unwrap_or_else(|_| "agent:mcp".to_string());
            let mut tags: Vec<String> = args
                .get("tags")
                .and_then(Value::as_str)
                .map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if let Some(boundary) = args.get("boundary").and_then(Value::as_str) {
                let parts: Vec<&str> = if boundary.contains("->") {
                    boundary.split("->").collect()
                } else {
                    boundary.split('|').collect()
                };
                for part in parts {
                    let p = part.trim();
                    if !p.is_empty() && !tags.iter().any(|t| t == p) {
                        tags.push(p.to_string());
                    }
                }
            }
            let mut client = connect_server().await?;
            let res = client
                .record_decision(proto::RecordDecisionRequest {
                    author,
                    title: title.clone(),
                    context: String::new(),
                    rationale,
                    related_entity_ids: Vec::new(),
                    related_decision_ids: Vec::new(),
                    tags,
                    supersedes: Vec::new(),
                    deprecates: Vec::new(),
                })
                .await?
                .into_inner();
            Ok(format!(
                "Decisione registrata nel ledger di intento (id {}): «{title}». \
                 Riemerge in codeos_why, codeos_query e codeos_context_pack.",
                res.decision_id
            ))
        }
        "codeos_licenses" => {
            let mut client = connect_server().await?;
            let res = client
                .licenses(proto::LicensesRequest {})
                .await?
                .into_inner();
            let mut out = String::new();
            for d in &res.dependencies {
                let lic = if d.license.is_empty() {
                    "SCONOSCIUTA"
                } else {
                    d.license.as_str()
                };
                out.push_str(&format!("{} [{}]: {}\n", d.name, d.ecosystem, lic));
            }
            if res.source_notices.is_empty() {
                out.push_str("\nSORGENTI: nessun avviso (SPDX/copyright/LICENSE) trovato.\n");
            } else {
                out.push_str(&format!(
                    "\nSORGENTI: {} avvisi:\n",
                    res.source_notices.len()
                ));
                const MAX_SHOWN: usize = 40;
                for n in res.source_notices.iter().take(MAX_SHOWN) {
                    let place = if n.line > 0 {
                        format!("{}:{}", n.path, n.line)
                    } else {
                        n.path.clone()
                    };
                    let text = if n.text.is_empty() {
                        "NON CLASSIFICATA (astensione)"
                    } else {
                        n.text.as_str()
                    };
                    out.push_str(&format!("- {place} [{}]: {text}\n", n.kind));
                }
                let hidden = res.source_notices.len().saturating_sub(MAX_SHOWN);
                if hidden > 0 {
                    out.push_str(&format!("… e altri {hidden} avvisi non mostrati.\n"));
                }
                if res.notices_truncated > 0 {
                    out.push_str(&format!(
                        "ATTENZIONE: {} avvisi tagliati dal tetto server-side (lista parziale).\n",
                        res.notices_truncated
                    ));
                }
            }
            if res.denied_count == 0 {
                out.push_str("\nPOLICY: nessun divieto nel ledger (registrane con codeos_decide, tag license-deny:<ID>).");
            } else if res.violations.is_empty() {
                out.push_str(&format!(
                    "\nPOLICY: nessuna violazione ({} divieti attivi).",
                    res.denied_count
                ));
            } else {
                out.push_str(&format!("\nPOLICY: {} VIOLAZIONI:\n", res.violations.len()));
                for v in &res.violations {
                    out.push_str(&format!(
                        "- {} — «{}» contiene «{}» — vietato da: «{}»\n",
                        v.dependency, v.license, v.denied, v.decision_title
                    ));
                }
            }
            Ok(out)
        }
        "codeos_report" => {
            let mut client = connect_server().await?;
            let res = client
                .get_architecture_report(proto::GetArchitectureReportRequest {})
                .await?
                .into_inner();
            Ok(serde_json::to_string_pretty(&crate::report_to_json(&res))?)
        }
        "codeos_audit" => {
            // Sola lettura del ledger + git/filesystem: NESSUNA connessione al server.
            // Riusa la stessa logica della CLI `audit` (crate::audit_report).
            let repo = arg_str(args, "path")
                .ok()
                .or_else(|| std::env::var("CODEOS_REPO").ok())
                .unwrap_or_else(|| ".".to_string());
            let (text, _broken) = crate::audit_report(&repo).await?;
            Ok(text)
        }
        other => anyhow::bail!(
            "tool sconosciuto: '{other}' (disponibili: codeos_query, codeos_why, \
             codeos_impact, codeos_context_pack, codeos_decide, codeos_report, \
             codeos_licenses, codeos_audit)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialize_declares_tools_capability_and_echoes_version() {
        let resp = handle(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26"}
        }))
        .await
        .expect("initialize ha sempre risposta");
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert!(resp["result"]["capabilities"].get("tools").is_some());
        assert_eq!(resp["result"]["serverInfo"]["name"], "codeos");
    }

    #[tokio::test]
    async fn tools_list_exposes_the_six_axes() {
        let resp = handle(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "codeos_query",
            "codeos_why",
            "codeos_impact",
            "codeos_context_pack",
            "codeos_decide",
            "codeos_report",
            "codeos_audit",
        ] {
            assert!(names.contains(&expected), "manca {expected}: {names:?}");
        }
        // Ogni tool dichiara uno schema d'input (un agente non deve indovinare).
        for t in tools {
            assert!(t.get("inputSchema").is_some(), "schema mancante: {t}");
        }
    }

    #[tokio::test]
    async fn notifications_get_no_reply_and_unknown_methods_get_a_protocol_error() {
        // Una notifica (senza id) non riceve MAI risposta.
        assert!(handle(json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .await
        .is_none());

        // Metodo ignoto CON id: errore di protocollo -32601 (non silenzio).
        let resp = handle(json!({"jsonrpc": "2.0", "id": 3, "method": "boh/inesistente"}))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn unknown_tool_is_a_tool_error_not_a_protocol_error() {
        // Errore di ESECUZIONE (tool sconosciuto): risultato con isError, così
        // l'agente lo legge e corregge — non un errore JSON-RPC che ucciderebbe
        // il giro. Non serve un server gRPC: il dispatch fallisce prima.
        let resp = handle(json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "codeos_inesistente", "arguments": {}}
        }))
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("tool sconosciuto"), "{text}");
    }

    #[tokio::test]
    async fn missing_required_argument_is_reported_honestly() {
        let resp = handle(json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "codeos_query", "arguments": {}}
        }))
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("text"),
            "deve nominare l'argomento mancante: {text}"
        );
    }
}
