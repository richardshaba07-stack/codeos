//! `codeos-graph` — il cuore: costruzione del grafo.
//!
//! Espone:
//! - [`GraphResolver`]: trasforma i `ParsedFileResult` grezzi in un `GraphDelta`
//!   con `EntityId` globali, eseguendo il name resolution (Blocco 3, sez. 7.2).
//! - [`GraphActor`]: l'attore che consuma `CodeOsEvent::FilesIndexed`, invoca il
//!   resolver, persiste il delta via `GraphStorage` e pubblica `GraphUpdated`.
//!
//! È il consumatore di `FilesIndexed` e il produttore di `GraphUpdated`.

mod actor;
mod resolver;

pub use actor::GraphActor;
pub use resolver::GraphResolver;
