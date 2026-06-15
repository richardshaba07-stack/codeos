//! `QueryActor`: l'attore che risponde ai comandi `QueryGraph`.
//!
//! Riceve i comandi su un `mpsc` (instradati dal Dispatcher), delega al
//! [`QueryEngine`] e deposita la risposta sul `reply_to`. Comunica solo via
//! canali e via trait `GraphStorage` (invarianti 1.3 e 1.4).

use std::sync::Arc;

use codeos_memory::DecisionStore;
use codeos_storage::GraphStorage;
use codeos_types::bus::Command;
use tokio::sync::mpsc;

use crate::engine::QueryEngine;

/// L'attore del Query Engine.
pub struct QueryActor {
    // `Arc` perché ogni handler gira in un task ISOLATO (`tokio::spawn`): se un bug
    // panica (es. lo slicing su confine multibyte che la Fase 0 ha scovato in
    // `compress_context`), il panic resta confinato nel figlio e l'attore SOPRAVVIVE.
    engine: Arc<QueryEngine>,
}

impl QueryActor {
    pub fn new(storage: Arc<dyn GraphStorage>) -> Self {
        Self {
            engine: Arc::new(QueryEngine::new(storage)),
        }
    }

    /// Variante che aggancia il Memory Engine: le risposte includeranno il
    /// *perché* (le decisioni relative alle entità selezionate).
    pub fn with_decisions(
        storage: Arc<dyn GraphStorage>,
        decisions: Arc<dyn DecisionStore>,
    ) -> Self {
        Self {
            engine: Arc::new(QueryEngine::with_decisions(storage, decisions)),
        }
    }

    /// Esegue un handler in un task ISOLATO. Se l'handler va in PANIC (un bug di
    /// slicing su testo multibyte, un `unwrap` imprevisto…), il panic resta confinato
    /// nel task figlio: l'attore riceve un `JoinError`, restituisce UN errore per QUELLA
    /// richiesta e RESTA VIVO per tutte le successive. È il backstop contro la modalità
    /// di fallimento peggiore — un singolo panic che chiude il canale e rende il query
    /// actor «non raggiungibile» a catena (misurata dalla Fase 0: una query oltre-budget
    /// con un carattere multibyte abbatteva il server per tutte le query seguenti).
    /// **Non sostituisce** la correzione dei bug (quella resta la prima difesa): è
    /// difesa in profondità, perché un server che mente per omissione a ogni richiesta
    /// è peggio di uno che dichiara onestamente un singolo errore.
    async fn isolate<T>(
        handler: &'static str,
        fut: impl std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    ) -> anyhow::Result<T>
    where
        T: Send + 'static,
    {
        match tokio::spawn(fut).await {
            Ok(result) => result,
            Err(join_err) => {
                tracing::error!(
                    handler,
                    error = %join_err,
                    "QueryActor: handler in PANIC, isolato — l'attore resta operativo"
                );
                Err(anyhow::anyhow!(
                    "errore interno nell'handler «{handler}»: la richiesta è fallita, ma il \
                     server resta operativo per le successive"
                ))
            }
        }
    }

    /// Consuma i comandi finché il canale resta aperto.
    pub async fn run(self, mut commands: mpsc::Receiver<Command>) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::QueryGraph { query, reply_to } => {
                    let engine = Arc::clone(&self.engine);
                    let result =
                        Self::isolate("query", async move { engine.query(&query).await }).await;
                    if let Err(err) = &result {
                        tracing::warn!(error = %err, "QueryActor: query fallita");
                    }
                    let _ = reply_to.send(result).await;
                }
                Command::CallPath { from, to, reply_to } => {
                    let engine = Arc::clone(&self.engine);
                    let result = Self::isolate("call_path", async move {
                        engine.call_path_by_name(&from, &to).await
                    })
                    .await;
                    if let Err(err) = &result {
                        tracing::warn!(error = %err, "QueryActor: call_path fallita");
                    }
                    let _ = reply_to.send(result).await;
                }
                Command::Impact { name, reply_to } => {
                    let engine = Arc::clone(&self.engine);
                    let result =
                        Self::isolate("impact", async move { engine.impact_by_name(&name).await })
                            .await;
                    if let Err(err) = &result {
                        tracing::warn!(error = %err, "QueryActor: impact fallita");
                    }
                    let _ = reply_to.send(result).await;
                }
                Command::ImpactTransitive { name, reply_to } => {
                    let engine = Arc::clone(&self.engine);
                    let result = Self::isolate("impact_transitive", async move {
                        engine.impact_transitive_by_name(&name).await
                    })
                    .await;
                    if let Err(err) = &result {
                        tracing::warn!(error = %err, "QueryActor: impact_transitive fallita");
                    }
                    let _ = reply_to.send(result).await;
                }
                other => {
                    tracing::warn!(?other, "query actor: comando non di sua competenza");
                }
            }
        }
        tracing::debug!("query actor: canale comandi chiuso, esco");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn isolate_returns_value_on_success() {
        let ok = QueryActor::isolate("test", async { Ok(7u32) }).await;
        assert_eq!(ok.unwrap(), 7);
    }

    #[tokio::test]
    async fn isolate_converts_panic_into_error_and_survives() {
        // Un handler che PANICA non deve propagare il panic (che ucciderebbe l'attore):
        // dev'essere convertito in un Err onesto. È la garanzia che un singolo bug non
        // diventi un'interruzione dell'intero servizio di query.
        let panicked = QueryActor::isolate::<u32>("test", async {
            panic!("boom simulato");
            #[allow(unreachable_code)]
            Ok(0)
        })
        .await;
        assert!(
            panicked.is_err(),
            "il panic dev'essere isolato e reso Err, non propagato"
        );
    }
}
