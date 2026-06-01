//! `codeos-parser` — analisi del sorgente in dati grezzi (Blocco 2).
//!
//! Espone:
//! - [`LanguageParser`]: il trait universale che ogni parser di linguaggio
//!   implementa.
//! - [`PythonParser`]: l'implementazione per Python basata su Tree-sitter.
//! - [`RustParser`] e [`TypeScriptParser`]: parser Tree-sitter per indicizzare
//!   anche CodeOS stesso.
//! - [`ParserActor`]: l'attore che legge i file, li analizza e pubblica
//!   `FilesIndexed` sul bus.
//!
//! Il parser produce `codeos_types::ParsedFileResult` (dati grezzi con
//! `local_id` e `target_qualified_name`) e **non tocca mai il grafo**
//! (invariante 1.4): il name resolution e gli `EntityId` sono compito del
//! `GraphResolver` (Blocco 3).

mod actor;
mod python;
mod rust_lang;
mod traits;
mod typescript;

pub use actor::ParserActor;
pub use python::PythonParser;
pub use rust_lang::RustParser;
pub use traits::LanguageParser;
pub use typescript::TypeScriptParser;
