//! Parser Go basato su Tree-sitter.
//!
//! La v1 estrae il sottoinsieme che alimenta meglio il grafo architetturale:
//! package come modulo, tipi (`struct`/`interface`), funzioni e metodi
//! (collegati al loro tipo ricevente), `import` e chiamate.
//!
//! Come per gli altri parser, NON tocca il grafo (invariante 1.4): produce solo
//! `ParsedFileResult` grezzo. Il name resolution è del `GraphResolver`.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use codeos_types::{
    EntityKind, ParseError, ParsedEntity, ParsedFileResult, ParsedRelation, RelationKind,
    SourceLocation,
};
use tree_sitter::{Node, Parser};

use crate::traits::LanguageParser;

pub struct GoParser;

impl GoParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GoParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LanguageParser for GoParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        file_extension.eq_ignore_ascii_case("go")
    }

    async fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();
        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&tree_sitter_go::language()) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica Go non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero Go".to_string(),
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
            metadata: go_metadata(),
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

/// Contesto lessicale corrente del walk. In Go basta il `local_id` del genitore
/// (modulo, tipo o funzione che racchiude): a differenza di Rust non serve
/// distinguere il `kind` del genitore, perché ogni handler conosce già il
/// proprio (un `func` è sempre Function, un `method_declaration` sempre Method).
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
    /// nome del tipo → local_id, per collegare i metodi al loro ricevente anche
    /// quando il `func (r Foo) ...` precede la `type Foo struct {...}`.
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
                message: format!("sintassi Go non valida ({}): '{snippet}'", node.kind()),
                location: Some(self.loc(node)),
            });
        }

        match node.kind() {
            "type_spec" => self.handle_type_spec(node, scope),
            "function_declaration" => self.handle_function(node, scope),
            "method_declaration" => self.handle_method(node, scope),
            "import_spec" => self.handle_import_spec(node, scope),
            "call_expression" => {
                self.handle_call(node, scope);
                self.walk_children(node, scope);
            }
            "composite_literal" => {
                self.handle_composite(node, scope);
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

    fn handle_type_spec(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let kind = match node.child_by_field_name("type").map(|t| t.kind()) {
            Some("interface_type") => EntityKind::Interface,
            _ => EntityKind::Struct,
        };
        let id = self.ensure_type_entity(&name, node, scope, kind);
        let child_scope = Scope { local_id: id };
        self.walk_children(node, &child_scope);
    }

    fn handle_function(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind: EntityKind::Function,
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: go_metadata(),
        });
        let child_scope = Scope { local_id: id };
        self.walk_children(node, &child_scope);
    }

    fn handle_method(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.field_text(node, "name") else {
            self.walk_children(node, scope);
            return;
        };
        // Il ricevente `(r Foo)` / `(r *Foo)` determina il tipo di appartenenza:
        // il metodo è figlio del tipo, non del modulo.
        let receiver = self.receiver_type_name(node);
        let (parent_id, mut metadata) = match &receiver {
            Some(type_name) => (
                self.ensure_type_entity(type_name, node, scope, EntityKind::Struct),
                go_metadata(),
            ),
            None => (scope.local_id.clone(), go_metadata()),
        };
        if let Some(type_name) = &receiver {
            metadata.insert("receiver".to_string(), type_name.clone());
        }
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind: EntityKind::Method,
            name,
            parent_local_id: Some(parent_id),
            location: self.loc(node),
            metadata,
        });
        let child_scope = Scope { local_id: id };
        self.walk_children(node, &child_scope);
    }

    /// Cerca il primo `type_identifier` dentro il ricevente, gestendo sia
    /// `(r Foo)` sia `(r *Foo)` sia i ricevente generici `(r Foo[T])`.
    fn receiver_type_name(&self, node: Node) -> Option<String> {
        let receiver = node.child_by_field_name("receiver")?;
        let mut stack = vec![receiver];
        while let Some(current) = stack.pop() {
            if current.kind() == "type_identifier" {
                let value = self.text(current);
                if !value.is_empty() {
                    return Some(value);
                }
            }
            let mut cursor = current.walk();
            for child in current.children(&mut cursor) {
                stack.push(child);
            }
        }
        None
    }

    fn ensure_type_entity(
        &mut self,
        name: &str,
        node: Node,
        scope: &Scope,
        kind: EntityKind,
    ) -> String {
        if let Some(existing) = self.type_entities.get(name) {
            return existing.clone();
        }
        let id = self.fresh_id();
        self.type_entities.insert(name.to_string(), id.clone());
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind,
            name: name.to_string(),
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: go_metadata(),
        });
        id
    }

    fn handle_import_spec(&mut self, node: Node, scope: &Scope) {
        let Some(path_node) = node.child_by_field_name("path") else {
            return;
        };
        let path = unquote(&self.text(path_node));
        if path.is_empty() {
            return;
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Imports,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: path,
            location: self.loc(node),
        });
    }

    fn handle_call(&mut self, node: Node, scope: &Scope) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let target = clean_go_target(&self.text(func));
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

    fn handle_composite(&mut self, node: Node, scope: &Scope) {
        // Solo i letterali di tipo nominato (`Foo{...}`) sono una "creazione" di
        // un tipo: gli slice/map/array (`[]int{}`, `map[string]int{}`) no.
        let Some(type_node) = node.child_by_field_name("type") else {
            return;
        };
        if type_node.kind() != "type_identifier" {
            return;
        }
        let target = self.text(type_node);
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
}

fn go_metadata() -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert("language".to_string(), "go".to_string());
    out
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').trim_matches('`').to_string()
}

/// Pulisce il bersaglio di una `call_expression`: rimuove i generici e gli spazi.
/// `pkg.Func` resta `pkg.Func`; `Foo[int]` diventa `Foo`.
fn clean_go_target(raw: &str) -> String {
    raw.split('[').next().unwrap_or(raw).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"package store

import (
	"fmt"
	"github.com/example/db"
)

type Cache struct {
	size int
}

type Store interface {
	Get(key string) string
}

func (c *Cache) Get(key string) string {
	return lookup(key)
}

func lookup(key string) string {
	fmt.Println(key)
	return key
}

func New() *Cache {
	return &Cache{size: 10}
}
"#;

    async fn parse(src: &str) -> ParsedFileResult {
        GoParser::new()
            .parse_file(Path::new("internal/store/cache.go"), src)
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
    async fn extracts_go_types_functions_methods_imports_and_calls() {
        let result = parse(SRC).await;
        assert!(
            result.errors.is_empty(),
            "Go valido non deve produrre errori: {:?}",
            result.errors
        );

        // Modulo dal nome del file, tipi struct/interface, funzioni e metodo.
        assert_eq!(find(&result, "cache").kind, EntityKind::Module);
        let cache = find(&result, "Cache");
        assert_eq!(cache.kind, EntityKind::Struct);
        assert_eq!(find(&result, "Store").kind, EntityKind::Interface);
        assert_eq!(find(&result, "lookup").kind, EntityKind::Function);
        assert_eq!(find(&result, "New").kind, EntityKind::Function);

        // Il metodo `Get` è figlio del tipo ricevente `Cache`, non del modulo.
        let get = find(&result, "Get");
        assert_eq!(get.kind, EntityKind::Method);
        assert_eq!(
            get.parent_local_id.as_deref(),
            Some(cache.local_id.as_str())
        );
        assert_eq!(
            get.metadata.get("receiver").map(String::as_str),
            Some("Cache")
        );

        // Tutte le entità sono marcate come Go (per il resolver per-linguaggio).
        assert!(result
            .entities
            .iter()
            .all(|e| e.metadata.get("language").map(String::as_str) == Some("go")));

        let imports: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Imports)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(imports.contains(&"fmt"), "imports = {imports:?}");
        assert!(
            imports.contains(&"github.com/example/db"),
            "imports = {imports:?}"
        );

        let calls: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(calls.contains(&"lookup"), "calls = {calls:?}");
        assert!(calls.contains(&"fmt.Println"), "calls = {calls:?}");

        // `&Cache{...}` è una creazione del tipo nominato.
        let creates: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Creates)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(creates.contains(&"Cache"), "creates = {creates:?}");
    }

    #[tokio::test]
    async fn method_links_to_type_even_when_declared_before_it() {
        // Il metodo precede la dichiarazione del tipo: `ensure_type_entity` deve
        // creare il segnaposto e `type` riusarlo (stesso local_id).
        let src = r#"package p

func (s *Server) Start() {}

type Server struct{}
"#;
        let result = GoParser::new()
            .parse_file(Path::new("p/server.go"), src)
            .await;
        let server = find(&result, "Server");
        assert_eq!(server.kind, EntityKind::Struct);
        let start = find(&result, "Start");
        assert_eq!(
            start.parent_local_id.as_deref(),
            Some(server.local_id.as_str()),
            "il metodo deve agganciarsi al tipo anche se dichiarato prima"
        );
        // Un solo entità `Server` (nessun duplicato segnaposto + reale).
        assert_eq!(
            result
                .entities
                .iter()
                .filter(|e| e.name == "Server")
                .count(),
            1
        );
    }
}
