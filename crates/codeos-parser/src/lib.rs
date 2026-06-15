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

/// Margine di stack (byte) sotto il quale `stacker` alloca un nuovo segmento nello
/// heap PRIMA di scendere ancora nel walk ricorsivo dell'AST. Un albero
/// profondamente annidato (fixture patologiche, file giganti generati) farebbe
/// altrimenti traboccare lo stack e ABBATTERE l'intero server (`Abort trap: 6`).
pub(crate) const STACK_RED_ZONE: usize = 128 * 1024;
/// Dimensione (byte) di ogni nuovo segmento di stack allocato da `stacker` quando si
/// scende sotto [`STACK_RED_ZONE`]. Approccio di rust-analyzer: walk ricorsivo
/// pulito, ma senza crash a qualsiasi profondità.
pub(crate) const STACK_GROW_BY: usize = 2 * 1024 * 1024;

mod actor;
mod cpp;
mod csharp_lang;
mod go;
mod java_lang;
mod python;
mod ruby_lang;
mod rust_lang;
mod swift_lang;
mod traits;
mod typescript;
mod workspace;

pub use actor::ParserActor;
pub use go::GoParser;
pub use java_lang::JavaParser;
pub use python::PythonParser;
pub use rust_lang::RustParser;
pub use traits::LanguageParser;
pub use typescript::TypeScriptParser;

/// `true` se il target di una call è un *path pulito*: identificatori separati da
/// `.` o `::`, senza sintassi d'espressione (parentesi, argomenti, stringhe, `?`,
/// closure…).
///
/// Tree-sitter espone come campo `function` l'intera testa della catena: per
/// `a.b().c()` il callee esterno è `a.b().c`, che ingloba la sub-call `a.b()`.
/// Registrarlo creerebbe un arco *bugiardo* verso un simbolo inesistente. Le
/// sub-call significative (`a.b`) vengono comunque catturate dalla ricorsione di
/// `walk_children`, quindi qui scartiamo soltanto il guscio non risolvibile.
///
/// Condiviso da tutti i parser Tree-sitter: la guardia anti-arco-bugiardo
/// dev'essere identica per ogni linguaggio.
pub(crate) fn is_clean_call_path(target: &str) -> bool {
    if target.is_empty() {
        return false;
    }
    let chars_ok = target
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == ':');
    chars_ok && !target.starts_with('.') && !target.ends_with('.') && !target.ends_with(':')
}

#[cfg(test)]
mod tests {
    use super::is_clean_call_path;

    #[test]
    fn is_clean_call_path_rejects_expressions() {
        assert!(is_clean_call_path("foo"));
        assert!(is_clean_call_path("a.b.c"));
        assert!(is_clean_call_path("Foo::bar"));
        assert!(is_clean_call_path("self.storage.query_relations"));
        assert!(!is_clean_call_path("SqliteStorage::in_memory().unwrap"));
        assert!(!is_clean_call_path("items.iter().map"));
        assert!(!is_clean_call_path("raw.split('"));
        assert!(!is_clean_call_path(""));
        assert!(!is_clean_call_path("foo."));
    }
}
