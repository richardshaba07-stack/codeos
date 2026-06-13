//! Il trait universale che ogni parser di linguaggio deve implementare.

use codeos_types::ParsedFileResult;
use std::path::Path;

/// Un parser specifico per un linguaggio.
///
/// Produce dati **grezzi** ([`ParsedFileResult`]): `local_id` validi solo nel
/// file e `target_qualified_name` come scritti nel sorgente. Non assegna
/// `EntityId` e non tocca il grafo (invariante 1.4) — quello è compito del
/// `GraphResolver` (Blocco 3).
///
/// `parse_file` è **sincrono**: il parsing tree-sitter è CPU puro (nessun I/O,
/// nessun await), così l'indicizzazione può parallelizzarlo con `rayon` su
/// tutti i core. La lettura del file dal disco è responsabilità del chiamante
/// (`ParserActor`), separata dal parsing.
pub trait LanguageParser: Send + Sync {
    /// `true` se questo parser gestisce l'estensione data (senza punto, es. `"py"`).
    fn can_parse(&self, file_extension: &str) -> bool;

    /// Analizza il sorgente già letto dal disco. Non deve mai fare panic: gli
    /// errori di sintassi vanno riportati in [`ParsedFileResult::errors`], non
    /// propagati.
    fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult;
}
