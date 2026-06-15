//! `codeos-server` — l'eseguibile che avvia CodeOS dietro la facciata gRPC.
//!
//! Configurazione via variabili d'ambiente (tutte opzionali):
//! - `CODEOS_ADDR`      — indirizzo di ascolto (default `127.0.0.1:50051`).
//! - `CODEOS_DB`        — path del file SQLite del grafo (default: in memoria).
//! - `CODEOS_DECISIONS` — directory della memoria storica Markdown (default: effimera).
//! - `CODEOS_REPO`      — root del repository git da cui il Guardian legge la storia
//!   (abilita Campo di Astensione + Fossili di Decisione nel referto; default: nessuna).
//! - `RUST_LOG`         — filtro dei log (es. `info`, `codeos_rpc=debug`). Se NON
//!   impostata, il default mostra `info` per CodeOS e silenzia il rumore di rete
//!   (tonic/h2/hyper a `warn`), così il server è osservabile out-of-the-box.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use codeos_memory::{DecisionStore, InMemoryDecisionStore};
use codeos_storage::{GraphStorage, SqliteStorage};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // RUST_LOG ha la precedenza; senza, un default sensato: `info` per CodeOS,
    // `warn` per le librerie di rete chiacchierone. `from_default_env` da solo
    // degraderebbe a "solo ERROR" ⇒ log vuoto e server muto (gap osservato).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("info,h2=warn,hyper=warn,tonic=warn,tower=warn")
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let addr: SocketAddr = std::env::var("CODEOS_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50051".to_string())
        .parse()?;

    // Storage del grafo: su file se `CODEOS_DB` è impostata, altrimenti in memoria.
    let storage: Arc<dyn GraphStorage> = match std::env::var("CODEOS_DB") {
        Ok(path) => {
            tracing::info!(path, "storage del grafo: SQLite su file");
            Arc::new(SqliteStorage::open(path)?)
        }
        Err(_) => {
            tracing::info!("storage del grafo: SQLite in memoria (effimero)");
            Arc::new(SqliteStorage::in_memory()?)
        }
    };

    // Memoria storica (il LEDGER DI INTENTO — lo strato non-derivabile del moat).
    // Precedenza: CODEOS_DECISIONS esplicito → poi, se c'è un repo, il default
    // PERSISTENTE `<repo>/.codeos/decisions` (versionabile col codice) → infine
    // effimero. Il default persistente è ciò che rende `why` NON inerte
    // out-of-the-box: l'intento registrato sopravvive ai riavvii e vive accanto al
    // sorgente, invece di evaporare in memoria (era questo il motivo per cui il
    // ledger restava vuoto nei collaudi).
    let decisions: Arc<dyn DecisionStore> = match std::env::var("CODEOS_DECISIONS") {
        Ok(dir) => {
            tracing::info!(
                dir,
                "memoria storica: Markdown ispezionabile (CODEOS_DECISIONS)"
            );
            Arc::new(codeos_memory::MarkdownDecisionStore::new(dir).await?)
        }
        Err(_) => match std::env::var("CODEOS_REPO") {
            Ok(root) => {
                let dir = PathBuf::from(&root).join(".codeos").join("decisions");
                tracing::info!(
                    dir = %dir.display(),
                    "memoria storica: ledger persistente in <repo>/.codeos/decisions (default)"
                );
                Arc::new(codeos_memory::MarkdownDecisionStore::new(dir).await?)
            }
            Err(_) => {
                tracing::info!("memoria storica: effimera (né CODEOS_DECISIONS né CODEOS_REPO)");
                Arc::new(InMemoryDecisionStore::new())
            }
        },
    };

    // Storia git per il Guardian: se `CODEOS_REPO` punta a un repo, abilita la
    // confidenza calibrata dal Campo di Astensione e i Fossili di Decisione nel referto.
    let repo_root: Option<PathBuf> = match std::env::var("CODEOS_REPO") {
        Ok(root) => {
            tracing::info!(
                root,
                "storia git agganciata: Campo di Astensione + Fossili attivi"
            );
            Some(PathBuf::from(root))
        }
        Err(_) => {
            tracing::info!("nessuna storia git: referto architetturale solo strutturale");
            None
        }
    };

    let dispatcher =
        codeos_core::spawn_with_storage_decisions_and_repo(storage, decisions, repo_root);

    tracing::info!(%addr, "CodeOS gRPC in ascolto");
    codeos_rpc::serve(dispatcher, addr).await?;
    Ok(())
}
