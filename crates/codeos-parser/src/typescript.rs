//! Parser TypeScript/TSX basato su Tree-sitter.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use codeos_types::{
    EntityKind, ParseError, ParsedEntity, ParsedFileResult, ParsedRelation, RelationKind,
    SourceLocation,
};
use tree_sitter::{Node, Parser};

use crate::traits::LanguageParser;

pub struct TypeScriptParser;

impl TypeScriptParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TypeScriptParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LanguageParser for TypeScriptParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        matches!(
            file_extension.to_ascii_lowercase().as_str(),
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs"
        )
    }

    async fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();
        let extension = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let is_js = matches!(extension.to_ascii_lowercase().as_str(), "js" | "jsx" | "mjs" | "cjs");
        let language_str = if is_js { "javascript".to_string() } else { "typescript".to_string() };
        let language = if extension.eq_ignore_ascii_case("tsx") || extension.eq_ignore_ascii_case("jsx") {
            tree_sitter_typescript::language_tsx()
        } else {
            tree_sitter_typescript::language_typescript()
        };

        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&language) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica TypeScript non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero TypeScript".to_string(),
                    location: None,
                }],
                ..Default::default()
            };
        };

        let root = tree.root_node();
        let mut walk = FileWalk::new(source_code.as_bytes(), path_str.clone(), language_str.clone());
        let module_id = walk.fresh_id();
        let module_name = file_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "module".to_string());
        let mut metadata = HashMap::new();
        metadata.insert("language".to_string(), language_str);
        walk.entities.push(ParsedEntity {
            local_id: module_id.clone(),
            kind: EntityKind::Module,
            name: module_name,
            parent_local_id: None,
            location: walk.loc(root),
            metadata,
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
    language: String,
    next_id: usize,
    entities: Vec<ParsedEntity>,
    relations: Vec<ParsedRelation>,
    errors: Vec<ParseError>,
}

impl<'src> FileWalk<'src> {
    fn new(source: &'src [u8], file_path: String, language: String) -> Self {
        Self {
            source,
            file_path,
            language,
            next_id: 0,
            entities: Vec::new(),
            relations: Vec::new(),
            errors: Vec::new(),
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
                message: format!(
                    "sintassi TypeScript non valida ({}): '{snippet}'",
                    node.kind()
                ),
                location: Some(self.loc(node)),
            });
        }

        match node.kind() {
            "class_declaration" | "abstract_class_declaration" => self.handle_class(node, scope),
            "interface_declaration" => self.handle_interface(node, scope),
            "type_alias_declaration" => self.handle_type_alias(node, scope),
            "function_declaration" => self.handle_function(node, scope),
            "method_definition" | "abstract_method_signature" | "method_signature" => {
                self.handle_method(node, scope)
            }
            "variable_declarator" => self.handle_variable_function(node, scope),
            "import_statement" => {
                self.handle_import(node, scope);
                self.walk_children(node, scope);
            }
            "call_expression" => {
                self.handle_call(node, scope);
                self.walk_children(node, scope);
            }
            "new_expression" => {
                self.handle_new(node, scope);
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

    fn handle_class(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.push_entity(node, EntityKind::Class, name, scope);
        self.push_heritage_relations(node, &id);
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Class,
        };
        self.walk_children(node, &child_scope);
    }

    fn handle_interface(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.push_entity(node, EntityKind::Interface, name, scope);
        self.push_heritage_relations(node, &id);
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Interface,
        };
        self.walk_children(node, &child_scope);
    }

    fn handle_type_alias(&mut self, node: Node, scope: &Scope) {
        if let Some(name) = self.field_text(node, "name") {
            self.push_entity(node, EntityKind::Interface, name, scope);
        }
        self.walk_children(node, scope);
    }

    fn handle_function(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.push_entity(node, EntityKind::Function, name, scope);
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Function,
        };
        self.walk_children(node, &child_scope);
    }

    fn handle_method(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.push_entity(node, EntityKind::Method, name, scope);
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Method,
        };
        self.walk_children(node, &child_scope);
    }

    fn handle_variable_function(&mut self, node: Node, scope: &Scope) {
        let Some(value) = node.child_by_field_name("value") else {
            self.walk_children(node, scope);
            return;
        };
        if !matches!(value.kind(), "arrow_function" | "function") {
            self.walk_children(node, scope);
            return;
        }
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.push_entity(node, EntityKind::Function, name, scope);
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Function,
        };
        self.walk_children(node, &child_scope);
    }

    fn push_entity(&mut self, node: Node, kind: EntityKind, name: String, scope: &Scope) -> String {
        let id = self.fresh_id();
        let parent_local_id = if scope.kind == EntityKind::Module
            || matches!(kind, EntityKind::Method | EntityKind::Function)
        {
            Some(scope.local_id.clone())
        } else {
            Some(scope.local_id.clone())
        };
        let mut metadata = HashMap::new();
        metadata.insert("language".to_string(), self.language.clone());
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind,
            name,
            parent_local_id,
            location: self.loc(node),
            metadata,
        });
        id
    }

    fn handle_import(&mut self, node: Node, scope: &Scope) {
        let Some(source) = node
            .child_by_field_name("source")
            .map(|n| unquote(&self.text(n)))
            .or_else(|| self.last_string_child(node))
        else {
            return;
        };
        if source.is_empty() {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Imports,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: source,
            location: self.loc(node),
        });
    }

    fn last_string_child(&self, node: Node) -> Option<String> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .filter(|child| child.kind() == "string")
            .last()
            .map(|child| unquote(&self.text(child)))
    }

    fn handle_call(&mut self, node: Node, scope: &Scope) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let target = clean_ts_target(&self.text(func));
        if target.is_empty() {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Calls,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: target,
            location: self.loc(node),
        });
    }

    fn handle_new(&mut self, node: Node, scope: &Scope) {
        let target = node
            .child_by_field_name("constructor")
            .or_else(|| first_named_child(node))
            .map(|n| clean_ts_target(&self.text(n)))
            .unwrap_or_default();
        if target.is_empty() || target == "new" {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Creates,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: target,
            location: self.loc(node),
        });
    }

    fn push_heritage_relations(&mut self, node: Node, source_local_id: &str) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_heritage" | "extends_clause" => {
                    for target in type_names_in(child, self.source) {
                        self.relations.push(ParsedRelation {
                            kind: RelationKind::Extends,
                            source_local_id: source_local_id.to_string(),
                            target_qualified_name: target,
                            location: self.loc(child),
                        });
                    }
                }
                "implements_clause" => {
                    for target in type_names_in(child, self.source) {
                        self.relations.push(ParsedRelation {
                            kind: RelationKind::Implements,
                            source_local_id: source_local_id.to_string(),
                            target_qualified_name: target,
                            location: self.loc(child),
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

fn first_named_child(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    let first = node.named_children(&mut cursor).next();
    first
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

fn clean_ts_target(raw: &str) -> String {
    raw.split('<').next().unwrap_or(raw).trim().to_string()
}

fn type_names_in(node: Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(
            current.kind(),
            "type_identifier" | "identifier" | "nested_type_identifier"
        ) {
            let value = current.utf8_text(source).unwrap_or("").trim();
            if !value.is_empty()
                && !matches!(value, "extends" | "implements")
                && !out.iter().any(|existing| existing == value)
            {
                out.push(value.to_string());
            }
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"import * as vscode from 'vscode';
import { CodeOsClient } from './client';

interface WatchHandlers {
  onEnd(): void;
}

class ExtensionHost extends BaseHost implements WatchHandlers {
  start() {
    connectClient();
    new CodeOsClient('127.0.0.1:50051', './proto');
  }
}

export function connectClient() {
  vscode.window.showInformationMessage('ok');
}
"#;

    async fn parse(src: &str) -> ParsedFileResult {
        TypeScriptParser::new()
            .parse_file(Path::new("vscode-extension/src/extension.ts"), src)
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
    async fn extracts_typescript_symbols_imports_and_calls() {
        let result = parse(SRC).await;

        assert_eq!(find(&result, "extension").kind, EntityKind::Module);
        assert_eq!(find(&result, "WatchHandlers").kind, EntityKind::Interface);
        let class = find(&result, "ExtensionHost");
        assert_eq!(class.kind, EntityKind::Class);
        let method = find(&result, "start");
        assert_eq!(method.kind, EntityKind::Method);
        assert_eq!(
            method.parent_local_id.as_deref(),
            Some(class.local_id.as_str())
        );
        assert_eq!(find(&result, "connectClient").kind, EntityKind::Function);

        let imports: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Imports)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(imports.contains(&"vscode"), "imports = {imports:?}");
        assert!(imports.contains(&"./client"), "imports = {imports:?}");

        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| matches!(r.kind, RelationKind::Calls | RelationKind::Creates))
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(calls.contains(&"connectClient"), "calls = {calls:?}");
        assert!(calls.contains(&"CodeOsClient"), "calls = {calls:?}");
    }
}
