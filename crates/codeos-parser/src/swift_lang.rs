//! Parser Swift basato su Tree-sitter.
//!
//! La v1 estrae il sottoinsieme che alimenta il grafo architetturale: il file
//! come modulo, `class`/`struct`/`enum` come tipi, `protocol` come Interface,
//! `extension` come metodi aggiunti al tipo esteso (collegati ad esso, non a una
//! nuova entità), funzioni e metodi (`func`, `init`), l'ereditarietà/conformità
//! come `Extends`, gli `import` e le chiamate.
//!
//! Nella grammatica tree-sitter-swift `class`, `struct`, `enum` ed `extension`
//! sono TUTTI `class_declaration`: si distinguono dalla keyword iniziale (un
//! token figlio) e — per l'enum — dal tipo di body (`enum_class_body`).
//!
//! SCOPE ONESTO della v1 (dichiarato): niente generics/where resolution né
//! property wrapper o macro (`@attached`) — i nomi sono quelli scritti, la
//! risoluzione del ricevitore è del GraphResolver (anti-FP); le `computed
//! property` non sono entità in v1.
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

pub struct SwiftParser;

impl SwiftParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SwiftParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LanguageParser for SwiftParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        file_extension.eq_ignore_ascii_case("swift")
    }

    async fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();
        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&tree_sitter_swift::language()) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica Swift non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero Swift".to_string(),
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
            metadata: swift_metadata(),
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
    /// nome del tipo → local_id, per agganciare un'`extension` (o un metodo) al
    /// tipo esistente invece di creare un duplicato.
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
                message: format!("sintassi Swift non valida ({}): '{snippet}'", node.kind()),
                location: Some(self.loc(node)),
            });
        }

        match node.kind() {
            "class_declaration" => self.handle_type_decl(node, scope),
            "protocol_declaration" => self.handle_protocol(node, scope),
            "function_declaration" | "protocol_function_declaration" => {
                self.handle_function(node, scope)
            }
            "init_declaration" => self.handle_init(node, scope),
            "import_declaration" => self.handle_import(node, scope),
            "call_expression" => {
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

    /// `class` / `struct` / `enum` / `extension`: tutti `class_declaration`, si
    /// distinguono dalla keyword iniziale. L'extension NON crea una nuova entità:
    /// aggancia i suoi membri al tipo esteso (come un metodo fuori-linea).
    fn handle_type_decl(&mut self, node: Node, scope: &Scope) {
        let keyword = self.decl_keyword(node);
        let Some(name) = self.type_decl_name(node) else {
            self.walk_children(node, scope);
            return;
        };

        if keyword.as_deref() == Some("extension") {
            let id = self.ensure_type_entity(&name, node, scope, EntityKind::Class, false);
            let child = Scope {
                local_id: id,
                in_type: true,
            };
            self.walk_children(node, &child);
            return;
        }

        let (kind, is_enum) = match keyword.as_deref() {
            Some("struct") => (EntityKind::Struct, false),
            Some("enum") => (EntityKind::Struct, true),
            _ => (EntityKind::Class, false),
        };
        let id = self.ensure_type_entity(&name, node, scope, kind, true);
        if is_enum {
            if let Some(e) = self.entities.iter_mut().find(|e| e.local_id == id) {
                e.metadata
                    .insert("swift_kind".to_string(), "enum".to_string());
            }
        }
        // Ereditarietà / conformità a protocollo → Extends verso ogni tipo base.
        self.emit_inheritance(node, &id);
        let child = Scope {
            local_id: id,
            in_type: true,
        };
        self.walk_children(node, &child);
    }

    fn handle_protocol(&mut self, node: Node, scope: &Scope) {
        let Some(name) = self.type_decl_name(node) else {
            self.walk_children(node, scope);
            return;
        };
        let id = self.ensure_type_entity(&name, node, scope, EntityKind::Interface, true);
        self.emit_inheritance(node, &id);
        let child = Scope {
            local_id: id,
            in_type: true,
        };
        self.walk_children(node, &child);
    }

    fn handle_function(&mut self, node: Node, scope: &Scope) {
        // Il PRIMO field `name` è il nome della funzione (un `function_declaration`
        // ha anche un `name`=tipo di ritorno: prendiamo solo il primo).
        let Some(name_node) = node.child_by_field_name("name") else {
            self.walk_children(node, scope);
            return;
        };
        let name = self.simple_name(name_node);
        if name.is_empty() {
            self.walk_children(node, scope);
            return;
        }
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
            metadata: swift_metadata(),
        });
        let child = Scope {
            local_id: id,
            in_type: false,
        };
        self.walk_children(node, &child);
    }

    fn handle_init(&mut self, node: Node, scope: &Scope) {
        // `init` è il costruttore: un Method di nome "init" quando è in un tipo.
        let parent = scope.local_id.clone();
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind: EntityKind::Method,
            name: "init".to_string(),
            parent_local_id: Some(parent),
            location: self.loc(node),
            metadata: swift_metadata(),
        });
        let child = Scope {
            local_id: id,
            in_type: false,
        };
        self.walk_children(node, &child);
    }

    fn handle_import(&mut self, node: Node, scope: &Scope) {
        // `import Foundation` → l'identificatore del modulo importato.
        let module = node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "identifier")
            .map(|c| self.simple_name(c))
            .unwrap_or_default();
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
        let Some(callee) = node.named_child(0) else {
            return;
        };
        let target = match callee.kind() {
            "simple_identifier" => self.text(callee),
            // `a.speak()` → navigation_expression, il nome è il suffix finale.
            "navigation_expression" => callee
                .child_by_field_name("suffix")
                .and_then(|s| s.child_by_field_name("suffix"))
                .map(|n| self.simple_name(n))
                .unwrap_or_default(),
            _ => self.simple_name(callee),
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

    /// La keyword iniziale di un `class_declaration` (token anonimo: il suo kind
    /// è il testo stesso). `None` se non riconosciuta.
    fn decl_keyword(&self, node: Node) -> Option<String> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class" | "struct" | "enum" | "extension" | "actor" => {
                    return Some(child.kind().to_string())
                }
                _ => {}
            }
        }
        None
    }

    /// Il nome di un tipo dal field `name` (può essere `type_identifier` o
    /// `user_type` per le extension): ne prende il `type_identifier` interno o il
    /// testo, e l'ultimo segmento per i nomi qualificati.
    fn type_decl_name(&self, node: Node) -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        let raw = self.simple_name(name_node);
        if raw.is_empty() {
            None
        } else {
            Some(last_segment(&raw))
        }
    }

    /// Scende `user_type`/`pattern`/`type_identifier`/`simple_identifier` fino al
    /// testo del nome semplice.
    fn simple_name(&self, node: Node) -> String {
        match node.kind() {
            "simple_identifier" | "type_identifier" | "identifier" => self.text(node),
            "user_type" | "pattern" => node
                .named_child(0)
                .map(|n| self.simple_name(n))
                .unwrap_or_else(|| self.text(node)),
            _ => self.text(node),
        }
    }

    /// Conformità/ereditarietà: per ogni `inheritance_specifier` un arco Extends
    /// dal tipo verso il nome base (Swift non distingue a livello sintattico la
    /// superclasse dai protocolli — il GraphResolver/una v2 può raffinare).
    fn emit_inheritance(&mut self, node: Node, type_id: &str) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "inheritance_specifier" {
                let target = child
                    .child_by_field_name("inherits_from")
                    .map(|n| self.simple_name(n))
                    .unwrap_or_default();
                if !target.is_empty() {
                    self.relations.push(ParsedRelation {
                        kind: RelationKind::Extends,
                        source_local_id: type_id.to_string(),
                        target_qualified_name: last_segment(&target),
                        location: self.loc(child),
                    });
                }
            }
        }
    }

    fn ensure_type_entity(
        &mut self,
        name: &str,
        node: Node,
        scope: &Scope,
        kind: EntityKind,
        declared: bool,
    ) -> String {
        let swift_kind = if declared {
            "type_decl"
        } else {
            "extension_target"
        };
        if let Some(existing) = self.type_entities.get(name) {
            let existing = existing.clone();
            if declared {
                let loc = self.loc(node);
                if let Some(e) = self.entities.iter_mut().find(|e| e.local_id == existing) {
                    if e.metadata.get("swift_kind").map(String::as_str) != Some("type_decl") {
                        e.kind = kind;
                        e.location = loc;
                        e.metadata
                            .insert("swift_kind".to_string(), "type_decl".to_string());
                    }
                }
            }
            return existing;
        }
        let id = self.fresh_id();
        self.type_entities.insert(name.to_string(), id.clone());
        let mut metadata = swift_metadata();
        metadata.insert("swift_kind".to_string(), swift_kind.to_string());
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind,
            name: name.to_string(),
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata,
        });
        id
    }
}

fn last_segment(qualified: &str) -> String {
    qualified
        .rsplit('.')
        .next()
        .unwrap_or(qualified)
        .trim()
        .to_string()
}

fn swift_metadata() -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert("language".to_string(), "swift".to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(src: &str) -> ParsedFileResult {
        // Nome file NEUTRO (non un nome di tipo) così il Module-file sintetico non
        // collide con una classe omonima — in Swift Animal.swift+class Animal è
        // comune; il grafo tiene entrambe le entità, ma il test cerca per nome.
        SwiftParser::new()
            .parse_file(Path::new("Sources/App/Zoo.swift"), src)
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
    async fn extracts_types_protocol_methods_inheritance_and_calls() {
        let src = r#"
import Foundation

protocol Drawable {
    func draw()
}

class Animal: Drawable {
    var name: String
    init(n: String) { self.name = n }
    func speak() -> String { return "..." }
    func draw() { speak() }
}

struct Point {
    var x: Int
}

enum Color {
    case red, green
}

func freeFunc(a: Animal) {
    a.speak()
    freeHelper()
}
"#;
        let r = parse(src).await;
        assert_eq!(find(&r, "Drawable").kind, EntityKind::Interface);
        assert_eq!(find(&r, "Animal").kind, EntityKind::Class);
        assert_eq!(find(&r, "Point").kind, EntityKind::Struct);
        let color = find(&r, "Color");
        assert_eq!(color.kind, EntityKind::Struct);
        assert_eq!(
            color.metadata.get("swift_kind").map(String::as_str),
            Some("enum")
        );
        // metodo dentro la classe.
        assert_eq!(find(&r, "speak").kind, EntityKind::Method);
        // funzione libera.
        assert_eq!(find(&r, "freeFunc").kind, EntityKind::Function);
        // init come Method.
        assert_eq!(find(&r, "init").kind, EntityKind::Method);

        // `class Animal: Drawable` → Extends/conformità verso Drawable.
        assert!(
            r.relations
                .iter()
                .any(|rel| rel.kind == RelationKind::Extends
                    && rel.target_qualified_name == "Drawable"),
            "manca Extends Animal→Drawable"
        );
        // import → Imports.
        assert!(r
            .relations
            .iter()
            .any(|rel| rel.kind == RelationKind::Imports
                && rel.target_qualified_name == "Foundation"));
        // chiamate: speak() (in draw) e freeHelper() (in freeFunc).
        let calls: Vec<&str> = r
            .relations
            .iter()
            .filter(|rel| rel.kind == RelationKind::Calls)
            .map(|rel| rel.target_qualified_name.as_str())
            .collect();
        assert!(calls.contains(&"speak"), "calls: {calls:?}");
        assert!(calls.contains(&"freeHelper"), "calls: {calls:?}");
    }

    #[tokio::test]
    async fn extension_attaches_to_existing_type_no_duplicate() {
        let src = r#"
class Widget {
    func base() {}
}
extension Widget {
    func extra() { base() }
}
"#;
        let r = parse(src).await;
        // Una sola entità Widget (l'extension non la duplica).
        let widgets: Vec<&ParsedEntity> =
            r.entities.iter().filter(|e| e.name == "Widget").collect();
        assert_eq!(
            widgets.len(),
            1,
            "Widget non deve essere duplicato dall'extension"
        );
        // Il metodo dell'extension appartiene a Widget.
        let widget_id = &widgets[0].local_id;
        let extra = find(&r, "extra");
        assert_eq!(extra.kind, EntityKind::Method);
        assert_eq!(extra.parent_local_id.as_ref(), Some(widget_id));
    }

    #[tokio::test]
    async fn syntax_error_does_not_panic() {
        let r = parse("class { func ( enum >>>").await;
        let _ = r.entities.len();
    }
}
