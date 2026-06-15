//! Parser Ruby basato su Tree-sitter.
//!
//! La v1 estrae il sottoinsieme che alimenta il grafo architetturale: il file
//! come modulo, `class` (con la superclasse come arco `Extends`), `module`
//! (namespace + mixin), i metodi di istanza e di classe (`def self.foo`), le
//! chiamate, e i `require`/`require_relative` come `Imports`.
//!
//! SCOPE ONESTO della v1 (dichiarato, non nascosto — come per gli altri):
//! - la METAPROGRAMMAZIONE non è espansa: `define_method`, `attr_accessor`,
//!   `method_missing`, `class_eval` generano metodi a runtime che tree-sitter
//!   non vede — blind spot gemello delle macro C++ e dei derive Rust;
//! - `include`/`extend` (mixin) NON sono archi in v1: la composizione di moduli
//!   è diversa dall'ereditarietà, rimandata a una slice esplicita;
//! - le chiamate tengono il nome semplice del metodo (il ricevitore lo risolve
//!   il GraphResolver, anti-FP).
//!
//! Come gli altri parser NON tocca il grafo (invariante 1.4): produce solo
//! `ParsedFileResult` grezzo.

use std::collections::HashMap;
use std::path::Path;

use codeos_types::{
    EntityKind, ParseError, ParsedEntity, ParsedFileResult, ParsedRelation, RelationKind,
    SourceLocation,
};
use tree_sitter::{Node, Parser};

use crate::traits::LanguageParser;

pub struct RubyParser;

impl RubyParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RubyParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for RubyParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        file_extension.eq_ignore_ascii_case("rb")
    }

    fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();
        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&tree_sitter_ruby::language()) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica Ruby non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero Ruby".to_string(),
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
            metadata: ruby_metadata(),
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

/// Contesto lessicale: il `local_id` del genitore e se è un TIPO (class/module),
/// così un `def` annidato è un Method, non una Function libera del file.
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
                message: format!("sintassi Ruby non valida ({}): '{snippet}'", node.kind()),
                location: Some(self.loc(node)),
            });
        }

        match node.kind() {
            "class" => self.handle_class(node, scope),
            "module" => self.handle_module(node, scope),
            "method" => self.handle_method(node, scope, false),
            "singleton_method" => self.handle_method(node, scope, true),
            "call" | "command" => {
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

    fn handle_class(&mut self, node: Node, scope: &Scope) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind: EntityKind::Class,
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: ruby_metadata(),
        });
        // `class Foo < Bar` → arco Extends verso il nome della superclasse.
        if let Some(superclass) = node.child_by_field_name("superclass") {
            // Il nodo `superclass` è `< Bar`: prendi l'ultimo identificatore.
            let target = superclass
                .named_child(0)
                .map(|n| self.text(n))
                .unwrap_or_else(|| {
                    self.text(superclass)
                        .trim_start_matches('<')
                        .trim()
                        .to_string()
                });
            if !target.is_empty() {
                self.relations.push(ParsedRelation {
                    kind: RelationKind::Extends,
                    source_local_id: id.clone(),
                    target_qualified_name: last_segment(&target),
                    location: self.loc(superclass),
                });
            }
        }
        let child = Scope {
            local_id: id,
            in_type: true,
        };
        self.walk_children(node, &child);
    }

    fn handle_module(&mut self, node: Node, scope: &Scope) {
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
            metadata: ruby_kind_metadata("module"),
        });
        let child = Scope {
            local_id: id,
            in_type: true,
        };
        self.walk_children(node, &child);
    }

    /// `def foo` (istanza) e `def self.foo` (singleton/classe). Entrambi sono
    /// Method se dentro una class/module; una `def` al top-level del file è una
    /// Function (in Ruby diventa un metodo privato di Object, ma per il grafo è
    /// una funzione libera del modulo-file).
    fn handle_method(&mut self, node: Node, scope: &Scope, singleton: bool) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            self.walk_children(node, scope);
            return;
        };
        let kind = if scope.in_type {
            EntityKind::Method
        } else {
            EntityKind::Function
        };
        let mut metadata = ruby_metadata();
        if singleton {
            metadata.insert("ruby_kind".to_string(), "singleton_method".to_string());
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
        let child = Scope {
            local_id: id,
            in_type: false,
        };
        self.walk_children(node, &child);
    }

    fn handle_call(&mut self, node: Node, scope: &Scope) {
        let Some(method) = node.child_by_field_name("method") else {
            return;
        };
        let name = self.text(method);
        if name.is_empty() {
            return;
        }
        // `require "x"` / `require_relative "y"` → Imports (Ruby non ha un
        // statement import: il require è l'unità di dipendenza). L'argomento è
        // una stringa; ne prendiamo il contenuto.
        if name == "require" || name == "require_relative" {
            if let Some(path) = self.first_string_argument(node) {
                if !path.is_empty() {
                    self.relations.push(ParsedRelation {
                        kind: RelationKind::Imports,
                        source_local_id: scope.local_id.clone(),
                        target_qualified_name: path,
                        location: self.loc(node),
                    });
                    return;
                }
            }
        }
        self.relations.push(ParsedRelation {
            kind: RelationKind::Calls,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: name,
            location: self.loc(node),
        });
    }

    /// Il contenuto della prima stringa fra gli argomenti di una call (per
    /// `require`): scende in `arguments` → `string` → `string_content`.
    fn first_string_argument(&self, node: Node) -> Option<String> {
        let args = node.child_by_field_name("arguments")?;
        let mut cursor = args.walk();
        for arg in args.named_children(&mut cursor) {
            if arg.kind() == "string" {
                // string → string_content (o stringa vuota se solo delimitatori).
                let mut c2 = arg.walk();
                for part in arg.named_children(&mut c2) {
                    if part.kind() == "string_content" {
                        return Some(self.text(part));
                    }
                }
                return Some(String::new());
            }
        }
        None
    }
}

/// L'ultimo segmento di un nome qualificato `A::B::C` → `C`.
fn last_segment(qualified: &str) -> String {
    qualified
        .rsplit("::")
        .next()
        .unwrap_or(qualified)
        .trim()
        .to_string()
}

fn ruby_metadata() -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert("language".to_string(), "ruby".to_string());
    out
}

fn ruby_kind_metadata(ruby_kind: &str) -> HashMap<String, String> {
    let mut out = ruby_metadata();
    out.insert("ruby_kind".to_string(), ruby_kind.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(src: &str) -> ParsedFileResult {
        RubyParser::new().parse_file(Path::new("app/models/user.rb"), src)
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
    async fn extracts_module_class_methods_extends_and_calls() {
        let src = r#"
require "json"
require_relative "base"

module Store
  class Cache < Base
    def get(key)
      lookup(key)
    end

    def self.create
      new
    end
  end

  def helper
    42
  end
end
"#;
        let r = parse(src).await;
        assert_eq!(find(&r, "Store").kind, EntityKind::Module);
        assert_eq!(find(&r, "Cache").kind, EntityKind::Class);
        assert_eq!(find(&r, "get").kind, EntityKind::Method);
        // `def self.create` è un metodo (singleton).
        let create = find(&r, "create");
        assert_eq!(create.kind, EntityKind::Method);
        assert_eq!(
            create.metadata.get("ruby_kind").map(String::as_str),
            Some("singleton_method")
        );

        // `class Cache < Base` → Extends verso Base.
        assert!(
            r.relations.iter().any(|rel| rel.kind == RelationKind::Extends
                && rel.target_qualified_name == "Base"),
            "manca Extends Cache→Base"
        );

        // require / require_relative → Imports.
        let imports: Vec<&str> = r
            .relations
            .iter()
            .filter(|rel| rel.kind == RelationKind::Imports)
            .map(|rel| rel.target_qualified_name.as_str())
            .collect();
        assert!(imports.contains(&"json"), "imports: {imports:?}");
        assert!(imports.contains(&"base"), "imports: {imports:?}");

        // `lookup(key)` dentro get → Calls verso lookup.
        assert!(
            r.relations.iter().any(|rel| rel.kind == RelationKind::Calls
                && rel.target_qualified_name == "lookup"),
            "manca la chiamata a lookup"
        );
    }

    #[tokio::test]
    async fn top_level_def_is_a_function_not_a_method() {
        let src = "def main\n  puts 'hi'\nend\n";
        let r = parse(src).await;
        assert_eq!(find(&r, "main").kind, EntityKind::Function);
    }

    #[tokio::test]
    async fn require_is_not_counted_as_a_call() {
        let src = "require \"set\"\n";
        let r = parse(src).await;
        assert!(
            !r.relations.iter().any(
                |rel| rel.kind == RelationKind::Calls && rel.target_qualified_name == "require"
            ),
            "require non deve essere una Calls"
        );
        assert!(r
            .relations
            .iter()
            .any(|rel| rel.kind == RelationKind::Imports && rel.target_qualified_name == "set"));
    }

    #[tokio::test]
    async fn syntax_error_does_not_panic() {
        let r = parse("class def def end end (((").await;
        // Non deve panicare; gli errori sono dichiarati o l'albero è parziale.
        let _ = r.entities.len();
    }
}
