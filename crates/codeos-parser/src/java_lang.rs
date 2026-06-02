//! Parser Java basato su Tree-sitter.
//!
//! La v1 estrae il sottoinsieme architetturale: la classe pubblica come unità
//! (più il modulo dal nome file), classi/interfacce/enum/record, metodi e
//! costruttori, `import`, gerarchie (`extends`/`implements`), chiamate e `new`.
//!
//! Come gli altri parser, NON tocca il grafo (invariante 1.4): produce solo
//! `ParsedFileResult` grezzo, il name resolution è del `GraphResolver`.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use codeos_types::{
    EntityKind, ParseError, ParsedEntity, ParsedFileResult, ParsedRelation, RelationKind,
    SourceLocation,
};
use tree_sitter::{Node, Parser};

use crate::traits::LanguageParser;

pub struct JavaParser;

impl JavaParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JavaParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LanguageParser for JavaParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        file_extension.eq_ignore_ascii_case("java")
    }

    async fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();
        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&tree_sitter_java::language()) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica Java non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero Java".to_string(),
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
            metadata: java_metadata(),
        });

        let scope = Scope {
            local_id: module_id,
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
}

struct FileWalk<'src> {
    source: &'src [u8],
    file_path: String,
    next_id: usize,
    entities: Vec<ParsedEntity>,
    relations: Vec<ParsedRelation>,
    errors: Vec<ParseError>,
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
                message: format!("sintassi Java non valida ({}): '{snippet}'", node.kind()),
                location: Some(self.loc(node)),
            });
        }

        match node.kind() {
            "class_declaration" | "enum_declaration" | "record_declaration" => {
                self.handle_type(node, scope, EntityKind::Class)
            }
            "interface_declaration" | "annotation_type_declaration" => {
                self.handle_type(node, scope, EntityKind::Interface)
            }
            "method_declaration" | "constructor_declaration" => self.handle_method(node, scope),
            "import_declaration" => {
                self.handle_import(node, scope);
                self.walk_children(node, scope);
            }
            "method_invocation" => {
                self.handle_call(node, scope);
                self.walk_children(node, scope);
            }
            "object_creation_expression" => {
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

    fn handle_type(&mut self, node: Node, scope: &Scope, kind: EntityKind) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.push_entity(node, kind, name, scope);
        self.push_heritage(node, &id);
        let child_scope = Scope { local_id: id };
        self.walk_children(node, &child_scope);
    }

    fn handle_method(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.push_entity(node, EntityKind::Method, name, scope);
        let child_scope = Scope { local_id: id };
        self.walk_children(node, &child_scope);
    }

    fn push_entity(&mut self, node: Node, kind: EntityKind, name: String, scope: &Scope) -> String {
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind,
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: java_metadata(),
        });
        id
    }

    fn handle_import(&mut self, node: Node, scope: &Scope) {
        let target = clean_java_import(&self.text(node));
        if target.is_empty() {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Imports,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: target,
            location: self.loc(node),
        });
    }

    fn handle_call(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            return;
        };
        // `obj.metodo()` → `obj.metodo` se `obj` è un path semplice; le catene
        // complesse (`a.b().c()`) ripiegano sul solo nome del metodo.
        let target = match self.field_text(node, "object") {
            Some(obj) if is_simple_receiver(&obj) => format!("{}.{name}", obj.trim()),
            _ => name,
        };
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
        let Some(raw) = self.field_text(node, "type") else {
            return;
        };
        let target = bare_type_name(&raw);
        if target.is_empty() {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Creates,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: target,
            location: self.loc(node),
        });
    }

    fn push_heritage(&mut self, node: Node, source_local_id: &str) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let kind = match child.kind() {
                // `class X extends Y` / `interface X extends Y`.
                "superclass" | "extends_interfaces" => RelationKind::Extends,
                // `class X implements I`.
                "super_interfaces" => RelationKind::Implements,
                _ => continue,
            };
            for target in type_names_in(child, self.source) {
                self.relations.push(ParsedRelation {
                    kind,
                    source_local_id: source_local_id.to_string(),
                    target_qualified_name: target,
                    location: self.loc(child),
                });
            }
        }
    }
}

fn java_metadata() -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert("language".to_string(), "java".to_string());
    out
}

/// `import static foo.Bar.baz;` / `import a.b.*;` → `foo.Bar.baz` / `a.b`.
fn clean_java_import(raw: &str) -> String {
    let body = raw
        .trim()
        .trim_start_matches("import")
        .trim()
        .trim_start_matches("static")
        .trim()
        .trim_end_matches(';')
        .trim();
    body.trim_end_matches('*')
        .trim_end_matches('.')
        .trim()
        .to_string()
}

/// `java.util.ArrayList<String>` → `java.util.ArrayList`; rimuove generici e
/// argomenti, tiene il path qualificato (il resolver match-a sull'ultimo segmento).
fn bare_type_name(raw: &str) -> String {
    raw.split('<').next().unwrap_or(raw).trim().to_string()
}

/// `true` se il ricevente di una chiamata è un path semplice (`a`, `a.b.c`),
/// non un'espressione complessa (con chiamate, spazi o a-capo).
fn is_simple_receiver(obj: &str) -> bool {
    let obj = obj.trim();
    !obj.is_empty() && !obj.contains(['(', ')', ' ', '\n', '\t', '[', ']', '{', '}'])
}

/// Raccoglie i nomi dei tipi (`type_identifier`/`scoped_type_identifier`) dentro
/// una clausola di gerarchia.
fn type_names_in(node: Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(current.kind(), "type_identifier" | "scoped_type_identifier") {
            let value = current.utf8_text(source).unwrap_or("").trim();
            if !value.is_empty() && !out.iter().any(|existing| existing == value) {
                out.push(value.to_string());
            }
            // Non scendere oltre: evita di raccogliere gli argomenti generici.
            continue;
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

    const SRC: &str = r#"package com.example.store;

import java.util.List;
import com.example.db.Database;

interface Repository {
    String get(String key);
}

public class Cache extends BaseCache implements Repository {
    public String get(String key) {
        return lookup(key);
    }

    private String lookup(String key) {
        Database db = new Database();
        return key;
    }
}
"#;

    async fn parse(src: &str) -> ParsedFileResult {
        // Nome file diverso dai tipi, per non confondere il modulo (dallo stem).
        JavaParser::new()
            .parse_file(Path::new("com/example/store/app.java"), src)
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
    async fn extracts_java_types_methods_imports_heritage_and_calls() {
        let result = parse(SRC).await;
        assert!(
            result.errors.is_empty(),
            "Java valido non deve produrre errori: {:?}",
            result.errors
        );

        assert_eq!(find(&result, "app").kind, EntityKind::Module);
        let cache = find(&result, "Cache");
        assert_eq!(cache.kind, EntityKind::Class);
        assert_eq!(find(&result, "Repository").kind, EntityKind::Interface);

        // `lookup` è un metodo della classe Cache (nome univoco nel file).
        let lookup = find(&result, "lookup");
        assert_eq!(lookup.kind, EntityKind::Method);
        assert_eq!(
            lookup.parent_local_id.as_deref(),
            Some(cache.local_id.as_str())
        );
        assert!(result
            .entities
            .iter()
            .all(|e| e.metadata.get("language").map(String::as_str) == Some("java")));

        let imports: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Imports)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(imports.contains(&"java.util.List"), "imports = {imports:?}");
        assert!(
            imports.contains(&"com.example.db.Database"),
            "imports = {imports:?}"
        );

        // extends BaseCache, implements Repository.
        assert!(
            result
                .relations
                .iter()
                .any(|r| r.kind == RelationKind::Extends && r.target_qualified_name == "BaseCache"),
            "manca extends BaseCache"
        );
        assert!(
            result
                .relations
                .iter()
                .any(|r| r.kind == RelationKind::Implements
                    && r.target_qualified_name == "Repository"),
            "manca implements Repository"
        );

        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(calls.contains(&"lookup"), "calls = {calls:?}");

        let creates: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Creates)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(creates.contains(&"Database"), "creates = {creates:?}");
    }

    #[tokio::test]
    async fn qualified_method_call_keeps_receiver() {
        let src = r#"package p;

public class Service {
    void run(Database db) {
        db.connect();
    }
}
"#;
        let result = JavaParser::new()
            .parse_file(Path::new("p/svc.java"), src)
            .await;
        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(calls.contains(&"db.connect"), "calls = {calls:?}");
    }
}
