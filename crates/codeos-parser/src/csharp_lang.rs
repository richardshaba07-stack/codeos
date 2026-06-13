//! Parser C# basato su Tree-sitter.
//!
//! La v1 estrae il sottoinsieme che alimenta il grafo architetturale: il file
//! come modulo, i `namespace` come moduli annidati, `class`/`struct`/`enum` come
//! tipi, `interface` come Interface, metodi e costruttori, le basi (`class A : B`)
//! come `Extends`, gli `using` come `Imports` e le chiamate.
//!
//! SCOPE ONESTO della v1 (dichiarato): niente risoluzione di overload/generics
//! né distinzione fra classe base e interfacce nella `base_list` (sintatticamente
//! indistinguibili — entrambe `Extends`, il GraphResolver/una v2 può raffinare);
//! le proprietà (`get`/`set`) non sono entità in v1; i nomi delle chiamate sono
//! quelli scritti (la risoluzione del ricevitore è del GraphResolver, anti-FP).
//!
//! Come gli altri parser NON tocca il grafo (invariante 1.4).

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use codeos_types::{
    EntityKind, ParseError, ParsedEntity, ParsedFileResult, ParsedRelation, RelationKind,
    SourceLocation,
};
use tree_sitter::{Node, Parser};

use crate::traits::LanguageParser;

pub struct CSharpParser;

impl CSharpParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CSharpParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LanguageParser for CSharpParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        file_extension.eq_ignore_ascii_case("cs")
    }

    async fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();
        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&tree_sitter_c_sharp::language()) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica C# non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero C#".to_string(),
                    location: None,
                }],
                ..Default::default()
            };
        };

        let root = tree.root_node();
        let mut walk = FileWalk::new(source_code.as_bytes(), path_str.clone());
        let module_id = walk.fresh_id();
        let module_name = file_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "module".to_string());
        walk.entities.push(ParsedEntity {
            local_id: module_id.clone(),
            kind: EntityKind::Module,
            name: module_name,
            parent_local_id: None,
            location: walk.loc(root),
            metadata: csharp_metadata(),
        });

        let scope = Scope {
            local_id: module_id,
            in_type: false,
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
    in_type: bool,
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
                message: format!("sintassi C# non valida ({}): '{snippet}'", node.kind()),
                location: Some(self.loc(node)),
            });
        }

        match node.kind() {
            // I namespace (anche file-scoped `namespace X;`) sono moduli annidati.
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                self.handle_namespace(node, scope)
            }
            "interface_declaration" => self.handle_type(node, scope, EntityKind::Interface, None),
            "class_declaration" => self.handle_type(node, scope, EntityKind::Class, None),
            "record_declaration" => {
                self.handle_type(node, scope, EntityKind::Class, Some("record"))
            }
            "struct_declaration" => self.handle_type(node, scope, EntityKind::Struct, None),
            "enum_declaration" => self.handle_type(node, scope, EntityKind::Struct, Some("enum")),
            "method_declaration" => self.handle_method(node, scope),
            "constructor_declaration" => self.handle_method(node, scope),
            "using_directive" => self.handle_using(node, scope),
            "invocation_expression" => {
                self.handle_call(node, scope);
                self.walk_children(node, scope);
            }
            _ => self.walk_children(node, scope),
        }
    }

    fn walk_children(&mut self, node: Node, scope: &Scope) {
        stacker::maybe_grow(crate::STACK_RED_ZONE, crate::STACK_GROW_BY, || {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            for child in children {
                self.walk(child, scope);
            }
        })
    }

    fn handle_namespace(&mut self, node: Node, scope: &Scope) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
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
            metadata: csharp_kind_metadata("namespace"),
        });
        let child = Scope {
            local_id: id,
            in_type: false,
        };
        self.walk_children(node, &child);
    }

    fn handle_type(&mut self, node: Node, scope: &Scope, kind: EntityKind, cs_kind: Option<&str>) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            self.walk_children(node, scope);
            return;
        };
        let mut metadata = csharp_metadata();
        if let Some(k) = cs_kind {
            metadata.insert("cs_kind".to_string(), k.to_string());
        }
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind,
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata,
        });
        // `class A : B, IC` → la base_list: ogni base è un Extends (la distinzione
        // classe/interfaccia non è sintattica in C#).
        self.emit_base_list(node, &id);
        let child = Scope {
            local_id: id,
            in_type: true,
        };
        self.walk_children(node, &child);
    }

    fn handle_method(&mut self, node: Node, scope: &Scope) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            self.walk_children(node, scope);
            return;
        };
        // Un metodo/costruttore è sempre dentro un tipo in C#; al di fuori (raro,
        // top-level statements) resta una Function.
        let kind = if scope.in_type {
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
            metadata: csharp_metadata(),
        });
        let child = Scope {
            local_id: id,
            in_type: false,
        };
        self.walk_children(node, &child);
    }

    fn handle_using(&mut self, node: Node, scope: &Scope) {
        // `using System;` / `using System.Collections.Generic;` → Imports.
        // Saltiamo gli using alias (`using X = Y;`) e gli static in v1: la forma
        // semplice è la dipendenza di namespace, l'unità che conta per il grafo.
        let name_node = node.child_by_field_name("name").or_else(|| {
            // Alcune versioni espongono il nome come figlio non-field: prendi il
            // primo identifier/qualified_name.
            first_named_child_of_kind(node, &["identifier", "qualified_name"])
        });
        let Some(name_node) = name_node else {
            return;
        };
        let module = self.text(name_node);
        if module.is_empty() {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Imports,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: module,
            location: self.loc(node),
        });
    }

    fn handle_call(&mut self, node: Node, scope: &Scope) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let target = self.call_target_name(func);
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

    /// Il nome semplice invocato: `Foo()` → Foo; `obj.Method()` /
    /// `Namespace.Type.Method()` → Method (il `name` del member access).
    fn call_target_name(&self, func: Node) -> String {
        match func.kind() {
            "identifier" => self.text(func),
            "member_access_expression" => func
                .child_by_field_name("name")
                .map(|n| self.text(n))
                .unwrap_or_default(),
            "generic_name" => func
                .child_by_field_name("name")
                .map(|n| self.text(n))
                .unwrap_or_else(|| last_dot_segment(&self.text(func))),
            _ => last_dot_segment(&self.text(func)),
        }
    }

    /// Ogni tipo nella `base_list` di una dichiarazione → un arco Extends.
    fn emit_base_list(&mut self, node: Node, type_id: &str) {
        let bases = node
            .child_by_field_name("bases")
            .or_else(|| first_named_child_of_kind(node, &["base_list"]));
        let Some(bases) = bases else {
            return;
        };
        let mut cursor = bases.walk();
        for base in bases.named_children(&mut cursor) {
            let name = last_dot_segment(&self.text(base));
            if !name.is_empty() {
                self.relations.push(ParsedRelation {
                    kind: RelationKind::Extends,
                    source_local_id: type_id.to_string(),
                    target_qualified_name: name,
                    location: self.loc(base),
                });
            }
        }
    }
}

/// Il primo figlio nominato il cui kind è in `kinds` (helper che evita un
/// cursor a vita corta nel mezzo di un'espressione `or_else`).
fn first_named_child_of_kind<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node
        .named_children(&mut cursor)
        .find(|c| kinds.contains(&c.kind()));
    found
}

/// L'ultimo segmento di un nome puntato `A.B.C` → `C`.
fn last_dot_segment(qualified: &str) -> String {
    qualified
        .rsplit('.')
        .next()
        .unwrap_or(qualified)
        .trim()
        .trim_end_matches(|c: char| c == '<' || c == '(')
        .to_string()
}

fn csharp_metadata() -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert("language".to_string(), "csharp".to_string());
    out
}

fn csharp_kind_metadata(cs_kind: &str) -> HashMap<String, String> {
    let mut out = csharp_metadata();
    out.insert("cs_kind".to_string(), cs_kind.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(src: &str) -> ParsedFileResult {
        CSharpParser::new()
            .parse_file(Path::new("App/Models/Domain.cs"), src)
            .await
    }

    fn find<'a>(result: &'a ParsedFileResult, name: &str) -> &'a ParsedEntity {
        result
            .entities
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("entità '{name}' non trovata in {:?}", names(result)))
    }

    fn names(result: &ParsedFileResult) -> Vec<&str> {
        result.entities.iter().map(|e| e.name.as_str()).collect()
    }

    #[tokio::test]
    async fn extracts_namespace_types_methods_bases_usings_and_calls() {
        let src = r#"
using System;
using System.Collections.Generic;

namespace App.Models
{
    public interface IGreeter
    {
        string Greet();
    }

    public class Animal : Base, IGreeter
    {
        private string name;
        public Animal(string n) { name = n; }
        public string Greet() { return Speak(); }
        public string Speak() { return "..."; }
    }

    public struct Point { public int X; }

    public enum Color { Red, Green }
}
"#;
        let r = parse(src).await;
        assert_eq!(find(&r, "App.Models").kind, EntityKind::Module);
        assert_eq!(find(&r, "IGreeter").kind, EntityKind::Interface);
        assert_eq!(find(&r, "Animal").kind, EntityKind::Class);
        assert_eq!(find(&r, "Point").kind, EntityKind::Struct);
        let color = find(&r, "Color");
        assert_eq!(color.kind, EntityKind::Struct);
        assert_eq!(
            color.metadata.get("cs_kind").map(String::as_str),
            Some("enum")
        );
        assert_eq!(find(&r, "Greet").kind, EntityKind::Method);
        // Il costruttore `Animal(...)` è un Method di nome Animal.
        let ctors: Vec<&ParsedEntity> = r
            .entities
            .iter()
            .filter(|e| e.name == "Animal" && e.kind == EntityKind::Method)
            .collect();
        assert_eq!(
            ctors.len(),
            1,
            "il costruttore Animal deve essere un Method"
        );

        // `class Animal : Base, IGreeter` → Extends verso Base e IGreeter.
        let extends: Vec<&str> = r
            .relations
            .iter()
            .filter(|rel| rel.kind == RelationKind::Extends)
            .map(|rel| rel.target_qualified_name.as_str())
            .collect();
        assert!(extends.contains(&"Base"), "extends: {extends:?}");
        assert!(extends.contains(&"IGreeter"), "extends: {extends:?}");

        // using → Imports.
        let imports: Vec<&str> = r
            .relations
            .iter()
            .filter(|rel| rel.kind == RelationKind::Imports)
            .map(|rel| rel.target_qualified_name.as_str())
            .collect();
        assert!(imports.contains(&"System"), "imports: {imports:?}");
        assert!(
            imports.contains(&"System.Collections.Generic"),
            "imports: {imports:?}"
        );

        // `Speak()` chiamata dentro Greet → Calls verso Speak.
        assert!(
            r.relations
                .iter()
                .any(|rel| rel.kind == RelationKind::Calls && rel.target_qualified_name == "Speak"),
            "manca la chiamata a Speak"
        );
    }

    #[tokio::test]
    async fn member_call_keeps_simple_name() {
        let src = r#"
class Runner {
    void Go(Widget w) {
        w.Update();
        Console.WriteLine("x");
    }
}
"#;
        let r = parse(src).await;
        let calls: Vec<&str> = r
            .relations
            .iter()
            .filter(|rel| rel.kind == RelationKind::Calls)
            .map(|rel| rel.target_qualified_name.as_str())
            .collect();
        assert!(calls.contains(&"Update"), "calls: {calls:?}");
        assert!(calls.contains(&"WriteLine"), "calls: {calls:?}");
    }

    #[tokio::test]
    async fn syntax_error_does_not_panic() {
        let r = parse("namespace { class public public {{{ void (").await;
        let _ = r.entities.len();
    }
}
