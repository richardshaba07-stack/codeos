//! Parser Rust basato su Tree-sitter.
//!
//! La v1 estrae il sottoinsieme che alimenta meglio il grafo architetturale:
//! moduli, tipi, funzioni/metodi, `use` e chiamate.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use codeos_types::{
    EntityKind, ParseError, ParsedEntity, ParsedFileResult, ParsedRelation, RelationKind,
    SourceLocation,
};
use tree_sitter::{Node, Parser};

use crate::is_clean_call_path;
use crate::traits::LanguageParser;

pub struct RustParser;

impl RustParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RustParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LanguageParser for RustParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        file_extension.eq_ignore_ascii_case("rs")
    }

    async fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();
        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&tree_sitter_rust::language()) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica Rust non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero Rust".to_string(),
                    location: None,
                }],
                ..Default::default()
            };
        };

        let root = tree.root_node();
        let mut walk = FileWalk::new(source_code.as_bytes(), path_str.clone());
        let module_id = walk.fresh_id();
        let module_name = file_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "module".to_string());
        walk.entities.push(ParsedEntity {
            local_id: module_id.clone(),
            kind: EntityKind::Module,
            name: module_name,
            parent_local_id: None,
            location: walk.loc(root),
            metadata: rust_metadata(None, None),
        });

        let scope = Scope {
            local_id: module_id,
            kind: EntityKind::Module,
            impl_trait: None,
        };
        walk.walk_children(root, &scope);

        ParsedFileResult {
            file_path: path_str,
            entities: walk.entities,
            relations: walk.relations,
            errors: walk.errors,
        }
    }
}

#[derive(Clone)]
struct Scope {
    local_id: String,
    kind: EntityKind,
    /// Se lo scope è il corpo di un `impl Trait for Tipo`, il testo del trait
    /// (con i generici, es. `From<u64>`); altrimenti `None`. Serve a disambiguare
    /// i nomi degli item — vedi `scoped_name`.
    impl_trait: Option<String>,
}

struct FileWalk<'src> {
    source: &'src [u8],
    file_path: String,
    next_id: usize,
    entities: Vec<ParsedEntity>,
    relations: Vec<ParsedRelation>,
    errors: Vec<ParseError>,
    type_entities: HashMap<String, String>,
}

impl<'src> FileWalk<'src> {
    fn new(source: &'src [u8], file_path: String) -> Self {
        Self {
            source,
            file_path,
            next_id: 0,
            entities: Vec::new(),
            relations: Vec::new(),
            errors: Vec::new(),
            type_entities: HashMap::new(),
        }
    }

    fn fresh_id(&mut self) -> String {
        let id = format!("node_{}", self.next_id);
        self.next_id += 1;
        id
    }

    fn text(&self, node: Node) -> String {
        node.utf8_text(self.source).unwrap_or("").to_string()
    }

    fn field_text(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field).map(|n| self.text(n))
    }

    fn loc(&self, node: Node) -> SourceLocation {
        let start = node.start_position();
        let end = node.end_position();
        SourceLocation {
            file_path: self.file_path.clone(),
            start_line: start.row as u32 + 1,
            start_column: start.column as u32,
            end_line: end.row as u32 + 1,
            end_column: end.column as u32,
        }
    }

    fn walk(&mut self, node: Node, scope: &Scope) {
        if node.is_error() || node.is_missing() {
            let snippet: String = self
                .text(node)
                .replace('\n', "\\n")
                .chars()
                .take(80)
                .collect();
            self.errors.push(ParseError {
                message: format!("sintassi Rust non valida ({}): '{snippet}'", node.kind()),
                location: Some(self.loc(node)),
            });
        }

        match node.kind() {
            "mod_item" => self.handle_mod(node, scope),
            "struct_item" => self.handle_type(node, scope, EntityKind::Struct, "struct"),
            "enum_item" => self.handle_type(node, scope, EntityKind::Struct, "enum"),
            "trait_item" => self.handle_type(node, scope, EntityKind::Interface, "trait"),
            "type_item" => self.handle_type(node, scope, EntityKind::Interface, "type"),
            "impl_item" => self.handle_impl(node, scope),
            "function_item" | "function_signature_item" => self.handle_function(node, scope),
            "use_declaration" => {
                self.handle_use(node, scope);
                self.walk_children(node, scope);
            }
            "call_expression" => {
                self.handle_call(node, scope);
                self.walk_children(node, scope);
            }
            "macro_invocation" => {
                self.handle_macro_call(node, scope);
                self.walk_children(node, scope);
            }
            _ => self.walk_children(node, scope),
        }
    }

    fn walk_children(&mut self, node: Node, scope: &Scope) {
        // Cresci lo stack nell'heap se sta per finire: evita lo stack overflow sul
        // walk ricorsivo di un AST profondamente annidato (vedi STACK_RED_ZONE).
        stacker::maybe_grow(crate::STACK_RED_ZONE, crate::STACK_GROW_BY, || {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            for child in children {
                self.walk(child, scope);
            }
        })
    }

    fn handle_mod(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind: EntityKind::Module,
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: rust_metadata(Some("rust_kind"), Some("mod")),
        });
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Module,
            impl_trait: None,
        };
        self.walk_children(node, &child_scope);
    }

    fn handle_type(&mut self, node: Node, scope: &Scope, kind: EntityKind, rust_kind: &str) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = if let Some(existing) = self.type_entities.get(&name) {
            existing.clone()
        } else {
            let id = self.fresh_id();
            self.type_entities.insert(name.clone(), id.clone());
            self.entities.push(ParsedEntity {
                local_id: id.clone(),
                kind,
                name,
                parent_local_id: Some(scope.local_id.clone()),
                location: self.loc(node),
                metadata: rust_metadata(Some("rust_kind"), Some(rust_kind)),
            });
            id
        };
        let child_scope = Scope {
            local_id: id,
            kind,
            impl_trait: None,
        };
        self.walk_children(node, &child_scope);
    }

    fn handle_impl(&mut self, node: Node, scope: &Scope) {
        let Some(type_name) = self.impl_target_name(node) else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.ensure_type_entity(&type_name, node, scope);
        // In un `impl Trait for Tipo`, trait diversi — o lo stesso trait con
        // generici diversi — producono item omonimi: `Display::fmt` e `Debug::fmt`,
        // oppure `From<u64>::from` e `From<&str>::from`. Senza il trait nel
        // `qualified_name` questi collidono sul vincolo UNIQUE e l'INSERT fa
        // abortire l'intero indice. Usiamo il testo completo del trait (con i
        // generici) come disambiguatore: l'equivalente di `<Tipo as Trait>::item`.
        let impl_trait = self
            .field_text(node, "trait")
            .map(|t| t.split_whitespace().collect::<String>())
            .filter(|t| !t.is_empty());
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Struct,
            impl_trait,
        };
        self.walk_children(node, &child_scope);
    }

    fn impl_target_name(&self, node: Node) -> Option<String> {
        if let Some(raw) = self.field_text(node, "type") {
            return bare_type_name(&raw);
        }
        let text = self.text(node);
        let head = text.split('{').next().unwrap_or("").trim();
        let candidate = head
            .split(" for ")
            .last()
            .unwrap_or(head)
            .split_whitespace()
            .last()
            .unwrap_or("");
        bare_type_name(candidate)
    }

    fn ensure_type_entity(&mut self, name: &str, node: Node, scope: &Scope) -> String {
        if let Some(existing) = self.type_entities.get(name) {
            return existing.clone();
        }
        let id = self.fresh_id();
        self.type_entities.insert(name.to_string(), id.clone());
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind: EntityKind::Struct,
            name: name.to_string(),
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: rust_metadata(Some("rust_kind"), Some("impl_target")),
        });
        id
    }

    fn handle_function(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let kind = if matches!(scope.kind, EntityKind::Struct | EntityKind::Interface) {
            EntityKind::Method
        } else {
            EntityKind::Function
        };
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind,
            name: scoped_name(scope, &name),
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: rust_metadata(None, None),
        });
        let child_scope = Scope {
            local_id: id,
            kind,
            impl_trait: None,
        };
        self.walk_children(node, &child_scope);
    }

    fn handle_use(&mut self, node: Node, scope: &Scope) {
        for target in expand_use_targets(&self.text(node)) {
            self.relations.push(ParsedRelation {
                kind: RelationKind::Imports,
                source_local_id: scope.local_id.clone(),
                target_qualified_name: target,
                location: self.loc(node),
            });
        }
    }

    fn handle_call(&mut self, node: Node, scope: &Scope) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let target = clean_call_target(&self.text(func));
        if is_noise_call(&target) || !is_clean_call_path(&target) || is_noise_method(&target) {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Calls,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: target,
            location: self.loc(node),
        });
    }

    fn handle_macro_call(&mut self, node: Node, scope: &Scope) {
        let target = self
            .field_text(node, "macro")
            .or_else(|| {
                let text = self.text(node);
                text.split('!').next().map(str::to_string)
            })
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if target.is_empty() {
            return;
        }
        // Le macro "di rumore" (assert!, vec!, println!, format!, …) sono onnipresenti
        // ma non esprimono una dipendenza architetturale: si risolverebbero a nulla
        // (`Unresolved`) e gonfierebbero il grafo — specie nei moduli di test. Le
        // scartiamo, così il grafo resta una mappa di relazioni *significative*.
        if is_noise_macro(&target) {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Calls,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: target,
            location: self.loc(node),
        });
    }
}

fn rust_metadata(extra_key: Option<&str>, extra_val: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert("language".to_string(), "rust".to_string());
    if let (Some(k), Some(v)) = (extra_key, extra_val) {
        out.insert(k.to_string(), v.to_string());
    }
    out
}

/// Disambigua il nome di un item dichiarato nel corpo di un `impl Trait for Tipo`:
/// antepone il trait (`fmt` → `Display::fmt`), così item omonimi provenienti da
/// impl di trait diversi non collidono nel `qualified_name`. Negli altri scope
/// (modulo, `impl` inerente) restituisce il nome invariato.
fn scoped_name(scope: &Scope, raw: &str) -> String {
    match &scope.impl_trait {
        Some(trait_name) => format!("{trait_name}::{raw}"),
        None => raw.to_string(),
    }
}

fn bare_type_name(raw: &str) -> Option<String> {
    let before_generics = raw.split('<').next().unwrap_or(raw);
    let after_ref = before_generics.trim().trim_start_matches('&').trim_start();
    // Un riferimento può portare una lifetime esplicita: `&'de RawValue`,
    // `&'a mut Value`. Dopo aver tolto `&` resta `'de RawValue`; la lifetime
    // inizia con `'` e DEVE essere scartata, altrimenti la `take_while` qui
    // sotto si ferma subito sull'apostrofo, il nome-tipo esce vuoto e l'impl
    // perde il suo Self type → `handle_impl` ripiega su `walk_children` e i
    // metodi finiscono mal-classificati come FUNZIONI libere del modulo
    // (es. serde_json `impl Deserializer<'de> for &'de RawValue`: 31 metodi
    // `deserialize_*` diventavano `raw::deserialize_*`, kind sbagliato e
    // qualname senza tipo/trait). Disciplina anti-FP: lo strip è puramente
    // sintattico (riferimento + lifetime + `mut`/`dyn`), non inventa nulla.
    let no_lifetime = if let Some(rest) = after_ref.strip_prefix('\'') {
        rest.split_once(char::is_whitespace)
            .map(|(_, tail)| tail.trim_start())
            .unwrap_or("")
    } else {
        after_ref
    };
    let cleaned = no_lifetime
        .trim_start_matches("mut ")
        .trim_start_matches("dyn ")
        .trim();
    let last = cleaned.rsplit("::").next().unwrap_or(cleaned);
    let name: String = last
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// `true` se la macro è una delle "macro di rumore" della libreria standard (o di
/// test): onnipresenti, non risolvibili a un'entità e prive di valore architetturale.
/// Il match è sull'ultimo segmento del path (`std::println` → `println`).
fn is_noise_macro(target: &str) -> bool {
    let name = target.rsplit("::").next().unwrap_or(target).trim();
    matches!(
        name,
        "assert"
            | "assert_eq"
            | "assert_ne"
            | "debug_assert"
            | "debug_assert_eq"
            | "debug_assert_ne"
            | "panic"
            | "unreachable"
            | "unimplemented"
            | "todo"
            | "dbg"
            | "print"
            | "println"
            | "eprint"
            | "eprintln"
            | "write"
            | "writeln"
            | "format"
            | "format_args"
            | "vec"
            | "matches"
            | "concat"
            | "stringify"
            | "include"
            | "include_str"
            | "include_bytes"
            | "env"
            | "option_env"
            | "line"
            | "file"
            | "column"
    )
}

/// `true` se il bersaglio di una `call_expression` è un costruttore-wrapper della
/// libreria standard (`Ok`/`Err`/`Some`) o un costrutto onnipresente senza valore
/// architetturale: sono chiamate che non rappresentano una dipendenza tra moduli.
fn is_noise_call(target: &str) -> bool {
    matches!(target, "Ok" | "Err" | "Some")
}

/// `true` se il bersaglio è una *method-call* (`receiver.metodo`) il cui metodo è
/// una conversione / unwrap della libreria standard: `.clone()`, `.to_string()`,
/// `.into()`, `.unwrap()`… Onnipresenti, non risolvibili senza inferenza di tipo
/// (resterebbero comunque `Unresolved`) e privi di valore architetturale.
///
/// Filtro VOLUTAMENTE stretto e **solo Rust**:
/// - agisce solo su path con `.` (method-call con receiver), mai su `Tipo::metodo`
///   con `::`: quello resta compito della risoluzione — gli interni si collegano
///   (P0-2), gli esterni restano onestamente `Unresolved`;
/// - l'elenco contiene SOLO nomi che in questo workspace **non** esistono come
///   metodo interno. È una scelta misurata, non a intuito: `as_str`, `is_empty`,
///   `len`, `push`, `send`… sono esclusi perché sono metodi *reali* di CodeOS (es.
///   `ResolutionStrategy::as_str`, `GraphDelta::is_empty`) e scartarli cancellerebbe
///   un arco vero invece di solo rumore.
///
/// Scartare una call non può MAI creare un arco bugiardo, solo ometterne uno: per
/// questo il drop a livello di parser è sicuro ("un arco mancante è meglio di uno
/// che mente").
fn is_noise_method(target: &str) -> bool {
    let Some((_, method)) = target.rsplit_once('.') else {
        return false;
    };
    matches!(
        method,
        "clone"
            | "to_string"
            | "to_string_lossy"
            | "to_owned"
            | "to_vec"
            | "into"
            | "as_ref"
            | "as_mut"
            | "as_slice"
            | "as_bytes"
            | "unwrap"
            | "unwrap_or"
            | "unwrap_or_else"
            | "unwrap_or_default"
            | "expect"
    )
}

fn clean_call_target(raw: &str) -> String {
    raw.replace("::<", "<")
        .split('<')
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_end_matches('!')
        .to_string()
}

fn expand_use_targets(raw: &str) -> Vec<String> {
    let body = raw
        .trim()
        .trim_start_matches("pub ")
        .trim_start_matches("use ")
        .trim_end_matches(';')
        .trim();
    if body.is_empty() {
        return Vec::new();
    }
    expand_braced_use(body)
        .into_iter()
        .map(|s| s.trim().trim_start_matches("self::").to_string())
        .filter(|s| !s.is_empty() && s != "self")
        .collect()
}

fn expand_braced_use(value: &str) -> Vec<String> {
    let Some(open) = value.find('{') else {
        return vec![value.to_string()];
    };
    let Some(close) = value.rfind('}') else {
        return vec![value.to_string()];
    };
    let prefix = value[..open].trim_end_matches("::");
    let suffix = value[close + 1..].trim_start_matches("::");
    let inner = &value[open + 1..close];
    split_top_level_commas(inner)
        .into_iter()
        .flat_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return Vec::new();
            }
            let combined = match (prefix.is_empty(), suffix.is_empty()) {
                (true, true) => part.to_string(),
                (true, false) => format!("{part}::{suffix}"),
                (false, true) => format!("{prefix}::{part}"),
                (false, false) => format!("{prefix}::{part}::{suffix}"),
            };
            expand_braced_use(&combined)
        })
        .collect()
}

fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (idx, ch) in value.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&value[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"use crate::traits::LanguageParser;
use codeos_types::{EntityKind, ParsedRelation};

pub struct ParserActor;

impl ParserActor {
    pub fn new() -> Self {
        helper();
        Self
    }
}

fn helper() {}
"#;

    async fn parse(src: &str) -> ParsedFileResult {
        RustParser::new()
            .parse_file(Path::new("crates/codeos-parser/src/actor.rs"), src)
            .await
    }

    fn find<'a>(result: &'a ParsedFileResult, name: &str) -> &'a ParsedEntity {
        result
            .entities
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("entita '{name}' assente"))
    }

    #[tokio::test]
    async fn extracts_rust_types_functions_methods_imports_and_calls() {
        let result = parse(SRC).await;

        assert_eq!(find(&result, "actor").kind, EntityKind::Module);
        let actor = find(&result, "ParserActor");
        assert_eq!(actor.kind, EntityKind::Struct);
        let method = find(&result, "new");
        assert_eq!(method.kind, EntityKind::Method);
        assert_eq!(
            method.parent_local_id.as_deref(),
            Some(actor.local_id.as_str())
        );
        assert_eq!(find(&result, "helper").kind, EntityKind::Function);

        let imports: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Imports)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(
            imports.contains(&"crate::traits::LanguageParser"),
            "imports = {imports:?}"
        );
        assert!(
            imports.contains(&"codeos_types::EntityKind"),
            "imports = {imports:?}"
        );
        assert!(
            imports.contains(&"codeos_types::ParsedRelation"),
            "imports = {imports:?}"
        );

        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(calls.contains(&"helper"), "calls = {calls:?}");
    }

    /// Regressione: in semver `display.rs` lo stesso tipo implementa sia `Display`
    /// sia `Debug`, e `impls.rs` ha più `impl From<…>` — tutti con metodi omonimi
    /// (`fmt`, `from`). Senza il trait nel nome questi item collidono sul vincolo
    /// `UNIQUE(qualified_name)` e l'INSERT fa abortire l'intero indice (misurato
    /// sul crate reale: 8 file letti, 0 entità salvate). Il trait nel nome
    /// — l'equivalente di `<Tipo as Trait>::item` — li tiene distinti.
    #[tokio::test]
    async fn trait_impl_methods_are_disambiguated_by_trait() {
        const SRC: &str = r#"
pub struct Version;

impl Version {
    pub fn parse() {}
}

impl Display for Version {
    fn fmt(&self) {}
}

impl Debug for Version {
    fn fmt(&self) {}
}

impl From<u64> for Version {
    fn from(_v: u64) {}
}

impl From<&str> for Version {
    fn from(_v: &str) {}
}
"#;
        let result = parse(SRC).await;

        // Il metodo inerente resta col nome nudo (caso comune: nessuna regressione).
        assert_eq!(find(&result, "parse").kind, EntityKind::Method);

        // I quattro item da impl di trait hanno nomi distinti ⇒ niente collisione.
        for name in [
            "Display::fmt",
            "Debug::fmt",
            "From<u64>::from",
            "From<&str>::from",
        ] {
            assert_eq!(
                find(&result, name).kind,
                EntityKind::Method,
                "atteso un metodo disambiguato '{name}'"
            );
        }

        // Nessun item conserva il nome nudo che collideva.
        let bare: Vec<&str> = result
            .entities
            .iter()
            .map(|e| e.name.as_str())
            .filter(|n| *n == "fmt" || *n == "from")
            .collect();
        assert!(
            bare.is_empty(),
            "nomi nudi collidenti ancora presenti: {bare:?}"
        );

        // Tutti restano sotto l'unico tipo `Version`: il tipo non è frammentato,
        // a disambiguare è solo il nome del metodo.
        let version = find(&result, "Version");
        for name in ["parse", "Display::fmt", "Debug::fmt"] {
            assert_eq!(
                find(&result, name).parent_local_id.as_deref(),
                Some(version.local_id.as_str()),
                "'{name}' dovrebbe stare sotto Version"
            );
        }
    }

    /// Regressione (misurata su serde_json, ~10× semver): un `impl Trait for
    /// &'a Tipo` — riferimento con lifetime ESPLICITA — faceva tornare `None` a
    /// `bare_type_name` (dopo lo strip di `&` resta `'de RawValue` e la
    /// `take_while` si ferma sull'apostrofo → nome vuoto). `handle_impl`
    /// ripiegava su `walk_children` e i metodi finivano mal-classificati come
    /// FUNZIONI libere del modulo: in serde_json i 31 `deserialize_*` di
    /// `impl Deserializer<'de> for &'de RawValue` diventavano `raw::deserialize_*`
    /// (kind sbagliato, qualname senza tipo né trait). Ora il Self type viene
    /// riconosciuto e i metodi stanno sotto il tipo, disambiguati dal trait.
    #[tokio::test]
    async fn methods_of_lifetime_reference_impls_are_attributed_to_the_type() {
        const SRC: &str = r#"
pub struct RawValue;

impl<'de> Deserializer<'de> for &'de RawValue {
    fn deserialize_any<V>(self, _v: V) {}
    fn deserialize_bool<V>(self, _v: V) {}
}
"#;
        let result = parse(SRC).await;
        let raw = find(&result, "RawValue");

        // I due metodi dell'impl su riferimento+lifetime sono Method sotto RawValue,
        // e portano il trait come disambiguatore (non il nome nudo).
        for leaf in ["deserialize_any", "deserialize_bool"] {
            let methods: Vec<&ParsedEntity> = result
                .entities
                .iter()
                .filter(|e| e.kind == EntityKind::Method && e.name.ends_with(leaf))
                .collect();
            assert_eq!(
                methods.len(),
                1,
                "atteso 1 Method '{leaf}', trovati: {:?}",
                methods.iter().map(|e| &e.name).collect::<Vec<_>>()
            );
            assert_eq!(
                methods[0].parent_local_id.as_deref(),
                Some(raw.local_id.as_str()),
                "'{leaf}' deve stare sotto RawValue"
            );
            assert!(
                methods[0].name.contains("Deserializer"),
                "'{leaf}' deve portare il trait disambiguante: {}",
                methods[0].name
            );
        }

        // Non deve restare ALCUNA funzione libera mal-classificata (la vecchia bug).
        let stray: Vec<&str> = result
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function && e.name.starts_with("deserialize_"))
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            stray.is_empty(),
            "metodi di un impl su riferimento trapelati come funzioni libere: {stray:?}"
        );
    }

    #[test]
    fn bare_type_name_strips_reference_and_lifetime() {
        // Casi che già funzionavano: nessuna regressione.
        assert_eq!(bare_type_name("Number").as_deref(), Some("Number"));
        assert_eq!(bare_type_name("&Number").as_deref(), Some("Number"));
        assert_eq!(
            bare_type_name("&mut Deserializer<R>").as_deref(),
            Some("Deserializer")
        );
        assert_eq!(bare_type_name("dyn Trait").as_deref(), Some("Trait"));
        // Casi che PRIMA del fix tornavano None: riferimento con lifetime esplicita.
        assert_eq!(bare_type_name("&'de RawValue").as_deref(), Some("RawValue"));
        assert_eq!(bare_type_name("&'a mut Value").as_deref(), Some("Value"));
        assert_eq!(
            bare_type_name("&'de Map<String, Value>").as_deref(),
            Some("Map")
        );
        assert_eq!(bare_type_name("&'static str").as_deref(), Some("str"));
    }

    #[tokio::test]
    async fn noise_macros_are_not_recorded_as_calls() {
        // `assert_eq!`/`vec!`/`println!` sono rumore; `my_router!` è una macro
        // applicativa e deve restare una dipendenza (Calls) significativa.
        let src = r#"
fn run() {
    let v = vec![1, 2, 3];
    assert_eq!(v.len(), 3);
    println!("done");
    my_router!(home);
}
"#;
        let result = parse(src).await;
        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(*c, "vec" | "assert_eq" | "println")),
            "le macro di rumore non devono comparire fra le call: {calls:?}"
        );
        assert!(
            calls.contains(&"my_router"),
            "la macro applicativa deve restare una call: {calls:?}"
        );
    }

    #[test]
    fn is_noise_macro_matches_std_macros_and_paths() {
        assert!(is_noise_macro("assert_eq"));
        assert!(is_noise_macro("std::println"));
        assert!(is_noise_macro("vec"));
        assert!(!is_noise_macro("my_router"));
        assert!(!is_noise_macro("tracing::info"));
    }

    #[test]
    fn is_noise_method_matches_only_std_conversions_with_receiver() {
        // Conversioni / unwrap std su un receiver: rumore, da scartare.
        assert!(is_noise_method("x.clone"));
        assert!(is_noise_method("self.label.to_string"));
        assert!(is_noise_method("p.to_string_lossy"));
        assert!(is_noise_method("opt.unwrap"));
        assert!(is_noise_method("res.expect"));
        assert!(is_noise_method("v.into"));
        // Esclusi perché sono metodi INTERNI reali di CodeOS: un arco vero, non
        // rumore (verificato sul grafo: as_str/is_empty/len/push esistono).
        assert!(!is_noise_method("strategy.as_str"));
        assert!(!is_noise_method("delta.is_empty"));
        assert!(!is_noise_method("items.len"));
        assert!(!is_noise_method("buf.push"));
        assert!(!is_noise_method("self.storage.query_relations"));
        // Senza receiver via `.` non è una method-call: non lo tocchiamo.
        assert!(!is_noise_method("clone"));
        assert!(!is_noise_method("String::from"));
    }

    #[tokio::test]
    async fn enum_constructor_wrappers_are_not_recorded_as_calls() {
        let src = r#"
fn run() -> Result<i32, String> {
    let x = Some(1);
    do_work();
    Ok(42)
}
"#;
        let result = parse(src).await;
        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(
            !calls.iter().any(|c| matches!(*c, "Ok" | "Some" | "Err")),
            "i wrapper Ok/Err/Some non devono comparire fra le call: {calls:?}"
        );
        assert!(calls.contains(&"do_work"), "calls = {calls:?}");
    }

    #[tokio::test]
    async fn std_conversion_methods_are_not_recorded_as_calls() {
        // `.clone()` / `.to_string()` sono rumore std (non risolvibili, senza valore
        // architetturale): vanno scartati. Invece `.as_str()` resta — in CodeOS è un
        // metodo interno reale (enum converter), un arco potenzialmente vero.
        let src = r#"
fn run(&self) {
    let a = self.name.clone();
    let b = self.label.to_string();
    let c = self.kind.as_str();
    self.storage.query_relations();
    do_work();
}
"#;
        let result = parse(src).await;
        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(
            !calls.iter().any(|c| c.ends_with(".clone")),
            "le clone() std non devono comparire fra le call: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.ends_with(".to_string")),
            "le to_string() std non devono comparire fra le call: {calls:?}"
        );
        assert!(
            calls.contains(&"self.kind.as_str"),
            "as_str (metodo interno) NON è rumore e deve restare: {calls:?}"
        );
        assert!(
            calls.contains(&"self.storage.query_relations"),
            "il method-call applicativo deve restare: {calls:?}"
        );
        assert!(calls.contains(&"do_work"), "calls = {calls:?}");
    }

    #[test]
    fn expands_basic_braced_use_items() {
        let targets = expand_use_targets("use std::{path::Path, sync::{Arc, Mutex}};");
        assert!(
            targets.contains(&"std::path::Path".to_string()),
            "targets = {targets:?}"
        );
        assert!(
            targets.contains(&"std::sync::Arc".to_string()),
            "targets = {targets:?}"
        );
        assert!(
            targets.contains(&"std::sync::Mutex".to_string()),
            "targets = {targets:?}"
        );
    }

    #[tokio::test]
    async fn chained_calls_do_not_record_garbage_targets() {
        // Tree-sitter dà come callee esterno l'intera catena (`build().unwrap`):
        // senza ripulitura finirebbe nel grafo come arco bugiardo. Vogliamo che
        // resti solo la sub-call pulita, catturata dalla ricorsione.
        let src = r#"
fn run() {
    let s = build().unwrap();
    items.iter().map(|x| x.id).collect();
    self.storage.query_relations();
}
"#;
        let result = parse(src).await;
        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(
            !calls
                .iter()
                .any(|c| c.contains('(') || c.contains(')') || c.contains('\'')),
            "nessun target deve contenere sintassi d'espressione: {calls:?}"
        );
        assert!(
            calls.contains(&"build"),
            "la sub-call pulita deve restare: {calls:?}"
        );
        assert!(
            calls.contains(&"self.storage.query_relations"),
            "il method-call con path pulito deve restare: {calls:?}"
        );
    }
}
