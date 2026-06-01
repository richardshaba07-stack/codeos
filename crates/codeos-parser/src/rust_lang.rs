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
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children {
            self.walk(child, scope);
        }
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
        let child_scope = Scope { local_id: id, kind };
        self.walk_children(node, &child_scope);
    }

    fn handle_impl(&mut self, node: Node, scope: &Scope) {
        let Some(type_name) = self.impl_target_name(node) else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.ensure_type_entity(&type_name, node, scope);
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Struct,
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
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: rust_metadata(None, None),
        });
        let child_scope = Scope { local_id: id, kind };
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
        if target.is_empty() || is_noise_call(&target) {
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

fn bare_type_name(raw: &str) -> Option<String> {
    let before_generics = raw.split('<').next().unwrap_or(raw);
    let cleaned = before_generics
        .trim()
        .trim_start_matches('&')
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
            !calls.iter().any(|c| matches!(*c, "vec" | "assert_eq" | "println")),
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
}
