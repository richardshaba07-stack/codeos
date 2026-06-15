//! Parser C / C++ basato su Tree-sitter (grammatica `tree-sitter-cpp`, che è
//! un superset di C: lo stesso parser copre `.c` e `.cpp`).
//!
//! La v1 estrae il sottoinsieme che alimenta il grafo architetturale: il file
//! come modulo, i `namespace` come moduli annidati, `class`/`struct`/`union` e
//! `enum` come tipi, le funzioni libere e i metodi (collegati al loro tipo,
//! anche per le definizioni fuori-linea `Foo::bar`), gli `#include` e le
//! chiamate.
//!
//! SCOPE ONESTO della v1 (dichiarato, non nascosto — come per Go/TS):
//! - niente espansione delle MACRO (`#define`): tree-sitter le vede come testo,
//!   un corpo macro-generato non è indicizzato (blind spot gemello dei derive
//!   Rust e delle macro `forward_to_deserialize_any!`);
//! - niente OVERLOAD resolution né template instantiation: i nomi sono i nomi
//!   semplici come scritti (la risoluzione è del GraphResolver, anti-FP);
//! - l'ereditarietà (`class B : public A`) NON è ancora un arco `Extends`:
//!   rimandata a una slice esplicita (capability-first), v1 estrae solo le
//!   entità e le chiamate.
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

pub struct CppParser;

impl CppParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CppParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageParser for CppParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        // C e C++: header e source di tutte le convenzioni comuni.
        matches!(
            file_extension.to_ascii_lowercase().as_str(),
            "c" | "h" | "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hxx" | "h++" | "hh" | "inl" | "ipp"
        )
    }

    fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();
        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&tree_sitter_cpp::language()) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica C++ non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero C++".to_string(),
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
            metadata: cpp_metadata(),
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

/// Contesto lessicale del walk: il `local_id` del genitore (modulo, namespace o
/// tipo) e se quel genitore è un TIPO (così una `function_definition` annidata
/// è un Method, non una Function libera).
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
    /// nome del tipo → local_id, per collegare un metodo definito FUORI dalla
    /// classe (`void Foo::bar() {}`) alla sua classe anche quando la definizione
    /// del metodo precede, o sta in un file diverso da, la `class Foo`.
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
        // Gli errori di sintassi si registrano ma NON fermano il walk: un file
        // con una macro non espansa o un costrutto raro produce meno entità,
        // mai un panic (i corpi macro restano blind spot dichiarato).
        if node.is_error() || node.is_missing() {
            let snippet: String = self
                .text(node)
                .replace('\n', "\\n")
                .chars()
                .take(80)
                .collect();
            self.errors.push(ParseError {
                message: format!("sintassi C++ non valida ({}): '{snippet}'", node.kind()),
                location: Some(self.loc(node)),
            });
        }

        match node.kind() {
            "namespace_definition" => self.handle_namespace(node, scope),
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                self.handle_type(node, scope)
            }
            "enum_specifier" => self.handle_enum(node, scope),
            "function_definition" => self.handle_function(node, scope),
            "field_declaration" => self.handle_field_declaration(node, scope),
            "preproc_include" => self.handle_include(node, scope),
            "call_expression" => {
                self.handle_call(node, scope);
                self.walk_children(node, scope);
            }
            _ => self.walk_children(node, scope),
        }
    }

    fn walk_children(&mut self, node: Node, scope: &Scope) {
        // Cresci lo stack nell'heap se sta per finire: niente stack overflow sul
        // walk ricorsivo di un AST C++ profondo (template/macro annidate).
        stacker::maybe_grow(crate::STACK_RED_ZONE, crate::STACK_GROW_BY, || {
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            for child in children {
                self.walk(child, scope);
            }
        })
    }

    fn handle_namespace(&mut self, node: Node, scope: &Scope) {
        // `namespace { }` anonimo non ha nome: scende senza creare un'entità.
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
            metadata: cpp_kind_metadata("namespace"),
        });
        let child = Scope {
            local_id: id,
            in_type: false,
        };
        self.walk_children(node, &child);
    }

    fn handle_type(&mut self, node: Node, scope: &Scope) {
        // `struct Foo;` (forward declaration, senza body) non è una definizione:
        // niente entità, eviterebbe un duplicato vuoto della vera definizione.
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            self.walk_children(node, scope);
            return;
        };
        if node.child_by_field_name("body").is_none() {
            return;
        }
        let kind = match node.kind() {
            "class_specifier" => EntityKind::Class,
            _ => EntityKind::Struct, // struct e union
        };
        let id = self.ensure_type_entity(&name, node, scope, kind, true);
        let child = Scope {
            local_id: id,
            in_type: true,
        };
        self.walk_children(node, &child);
    }

    fn handle_enum(&mut self, node: Node, scope: &Scope) {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            self.walk_children(node, scope);
            return;
        };
        // Niente EntityKind::Enum nel modello: un enum è un tipo-valore nominato,
        // mappato a Struct con un marcatore onesto nei metadata.
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind: EntityKind::Struct,
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location: self.loc(node),
            metadata: cpp_kind_metadata("enum"),
        });
        // Gli enumeratori non sono entità di prima classe in v1.
        let _ = id;
    }

    fn handle_function(&mut self, node: Node, scope: &Scope) {
        let Some(decl) = node.child_by_field_name("declarator") else {
            self.walk_children(node, scope);
            return;
        };
        let Some(name_info) = self.declarator_name(decl) else {
            self.walk_children(node, scope);
            return;
        };
        self.emit_callable(node, scope, name_info);
    }

    /// Una `field_declaration` dentro una classe può essere la DICHIARAZIONE di
    /// un metodo (`void foo();` senza corpo): se contiene un `function_declarator`
    /// la registriamo come Method, così l'interfaccia della classe è nel grafo
    /// anche quando l'implementazione è in un `.cpp` separato.
    fn handle_field_declaration(&mut self, node: Node, scope: &Scope) {
        if !scope.in_type {
            self.walk_children(node, scope);
            return;
        }
        let Some(decl) = node.child_by_field_name("declarator") else {
            self.walk_children(node, scope);
            return;
        };
        if !contains_function_declarator(decl) {
            self.walk_children(node, scope);
            return;
        }
        if let Some(name_info) = self.declarator_name(decl) {
            self.emit_callable(node, scope, name_info);
        } else {
            self.walk_children(node, scope);
        }
    }

    /// Emette una Function o un Method dal nome estratto da un declarator, poi
    /// scende nel corpo per raccogliere le chiamate sotto il nuovo scope.
    fn emit_callable(&mut self, node: Node, scope: &Scope, name_info: DeclName) {
        let DeclName { name, qualifier } = name_info;
        // Tre casi per decidere Function vs Method e il genitore:
        // (1) nome qualificato `Foo::bar` → Method di Foo (definizione fuori-linea);
        // (2) dentro un tipo (in_type) → Method della classe corrente;
        // (3) altrimenti → Function libera.
        let (kind, parent_id, receiver) = if let Some(type_name) = qualifier {
            let pid = self.ensure_type_entity(&type_name, node, scope, EntityKind::Class, false);
            (EntityKind::Method, pid, Some(type_name))
        } else if scope.in_type {
            (EntityKind::Method, scope.local_id.clone(), None)
        } else {
            (EntityKind::Function, scope.local_id.clone(), None)
        };

        let mut metadata = cpp_metadata();
        if let Some(r) = &receiver {
            metadata.insert("receiver".to_string(), r.clone());
        }
        let id = self.fresh_id();
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind,
            name,
            parent_local_id: Some(parent_id),
            location: self.loc(node),
            metadata,
        });
        let child = Scope {
            local_id: id,
            in_type: false,
        };
        self.walk_children(node, &child);
    }

    /// Crea (o ritrova) l'entità del tipo `name`. `declared=true` quando viene
    /// dalla DEFINIZIONE reale (`class/struct Foo { … }`), `false` quando è
    /// sintetizzata dal qualificatore di un metodo fuori-linea (`Foo::bar`).
    /// Stessa logica di promozione del parser Go (`ensure_type_entity`): la
    /// definizione reale vince sul placeholder, una sola identità per tipo/file.
    fn ensure_type_entity(
        &mut self,
        name: &str,
        node: Node,
        scope: &Scope,
        kind: EntityKind,
        declared: bool,
    ) -> String {
        let cpp_kind = if declared {
            "type_decl"
        } else {
            "receiver_target"
        };
        if let Some(existing) = self.type_entities.get(name) {
            let existing = existing.clone();
            if declared {
                let loc = self.loc(node);
                if let Some(e) = self.entities.iter_mut().find(|e| e.local_id == existing) {
                    if e.metadata.get("cpp_kind").map(String::as_str) != Some("type_decl") {
                        e.kind = kind;
                        e.location = loc;
                        e.metadata
                            .insert("cpp_kind".to_string(), "type_decl".to_string());
                    }
                }
            }
            return existing;
        }
        let id = self.fresh_id();
        self.type_entities.insert(name.to_string(), id.clone());
        let mut metadata = cpp_metadata();
        metadata.insert("cpp_kind".to_string(), cpp_kind.to_string());
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

    fn handle_include(&mut self, node: Node, scope: &Scope) {
        let Some(path_node) = node.child_by_field_name("path") else {
            return;
        };
        // `"foo.h"` (string_literal) o `<vector>` (system_lib_string): in entrambi
        // i casi togliamo i delimitatori e teniamo il path come scritto.
        let raw = self.text(path_node);
        let path = raw
            .trim()
            .trim_matches('"')
            .trim_start_matches('<')
            .trim_end_matches('>')
            .to_string();
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
        // Il bersaglio è l'ULTIMO segmento di ciò che viene invocato:
        //  `foo()`            → foo
        //  `obj.method()`     → method  (field_expression, field = method)
        //  `ptr->method()`    → method
        //  `ns::free()`       → free    (qualified_identifier, ultimo segmento)
        // Tenere il solo nome semplice è la stessa scelta degli altri parser:
        // la risoluzione del ricevitore è del GraphResolver (anti-FP).
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

    /// Estrae il nome semplice invocato da una `call_expression`.
    fn call_target_name(&self, func: Node) -> String {
        match func.kind() {
            "identifier" | "field_identifier" => self.text(func),
            "field_expression" => func
                .child_by_field_name("field")
                .map(|n| self.text(n))
                .unwrap_or_default(),
            "qualified_identifier" => func
                .child_by_field_name("name")
                .map(|n| self.call_target_name(n))
                .unwrap_or_else(|| last_segment(&self.text(func))),
            "template_function" => func
                .child_by_field_name("name")
                .map(|n| self.call_target_name(n))
                .unwrap_or_default(),
            "parenthesized_expression" => func
                .named_child(0)
                .map(|n| self.call_target_name(n))
                .unwrap_or_default(),
            _ => last_segment(&self.text(func)),
        }
    }

    /// Scende attraverso i declarator annidati (`pointer`, `reference`,
    /// `parenthesized`, `function`) fino al nome dichiarato, restituendo anche
    /// l'eventuale QUALIFICATORE (`Foo` in `Foo::bar`) per i metodi fuori-linea.
    fn declarator_name(&self, node: Node) -> Option<DeclName> {
        match node.kind() {
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator"
            | "init_declarator" => {
                let inner = node.child_by_field_name("declarator")?;
                self.declarator_name(inner)
            }
            "identifier" | "field_identifier" | "destructor_name" | "operator_name" => {
                Some(DeclName {
                    name: self.text(node),
                    qualifier: None,
                })
            }
            "qualified_identifier" => {
                // `Foo::bar` → name=bar, qualifier=Foo; `A::B::bar` → qualifier=B.
                let scope_node = node.child_by_field_name("scope");
                let name_node = node.child_by_field_name("name")?;
                let inner = self.declarator_name(name_node)?;
                let qualifier = scope_node
                    .map(|n| last_segment(&self.text(n)))
                    .filter(|s| !s.is_empty())
                    .or(inner.qualifier);
                Some(DeclName {
                    name: inner.name,
                    qualifier,
                })
            }
            "template_function" => {
                let name_node = node.child_by_field_name("name")?;
                self.declarator_name(name_node)
            }
            _ => None,
        }
    }
}

/// Nome di una dichiarazione + l'eventuale tipo qualificante (`Foo::bar`).
struct DeclName {
    name: String,
    qualifier: Option<String>,
}

/// `true` se il sotto-albero del declarator contiene un `function_declarator`
/// (distingue `void foo();` da un campo dato `int x;` in una classe).
fn contains_function_declarator(node: Node) -> bool {
    if node.kind() == "function_declarator" {
        return true;
    }
    node.child_by_field_name("declarator")
        .is_some_and(contains_function_declarator)
}

/// L'ultimo segmento di un nome qualificato `a::b::c` → `c`.
fn last_segment(qualified: &str) -> String {
    qualified
        .rsplit("::")
        .next()
        .unwrap_or(qualified)
        .trim()
        .to_string()
}

fn cpp_metadata() -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert("language".to_string(), "cpp".to_string());
    out
}

fn cpp_kind_metadata(cpp_kind: &str) -> HashMap<String, String> {
    let mut out = cpp_metadata();
    out.insert("cpp_kind".to_string(), cpp_kind.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(src: &str, file: &str) -> ParsedFileResult {
        CppParser::new().parse_file(Path::new(file), src)
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
    async fn extracts_classes_methods_functions_and_calls() {
        let src = r#"
#include <vector>
#include "cache.h"

namespace store {

class Cache {
public:
    int get(int key);
    void clear();
};

int Cache::get(int key) {
    return lookup(key);
}

void Cache::clear() {
}

int lookup(int key) {
    return key;
}

}  // namespace store
"#;
        let r = parse(src, "store/cache.cpp").await;
        // namespace come Module annidato.
        assert_eq!(find(&r, "store").kind, EntityKind::Module);
        // class come Class.
        assert_eq!(find(&r, "Cache").kind, EntityKind::Class);
        // metodo dichiarato nel corpo della classe.
        assert_eq!(find(&r, "get").kind, EntityKind::Method);
        // funzione libera.
        assert_eq!(find(&r, "lookup").kind, EntityKind::Function);

        // Il metodo fuori-linea `Cache::get` è attribuito alla classe Cache, non
        // duplicato come funzione libera (una sola identità Cache).
        let cache_id = &find(&r, "Cache").local_id;
        let get_methods: Vec<&ParsedEntity> =
            r.entities.iter().filter(|e| e.name == "get").collect();
        assert!(
            get_methods
                .iter()
                .any(|m| m.parent_local_id.as_ref() == Some(cache_id)),
            "Cache::get deve appartenere a Cache"
        );

        // #include → Imports (sia <vector> sia \"cache.h\").
        let imports: Vec<&str> = r
            .relations
            .iter()
            .filter(|rel| rel.kind == RelationKind::Imports)
            .map(|rel| rel.target_qualified_name.as_str())
            .collect();
        assert!(imports.contains(&"vector"), "import: {imports:?}");
        assert!(imports.contains(&"cache.h"), "import: {imports:?}");

        // La chiamata `lookup(key)` dentro Cache::get → Calls verso "lookup".
        assert!(
            r.relations.iter().any(|rel| rel.kind == RelationKind::Calls
                && rel.target_qualified_name == "lookup"),
            "manca la chiamata a lookup"
        );
    }

    #[tokio::test]
    async fn plain_c_struct_and_function() {
        // La stessa grammatica copre il C puro (.c): struct + funzione + chiamata.
        let src = r#"
#include "list.h"

struct Node {
    int value;
};

int sum(struct Node* n) {
    return total(n);
}
"#;
        let r = parse(src, "list.c").await;
        assert_eq!(find(&r, "Node").kind, EntityKind::Struct);
        assert_eq!(find(&r, "sum").kind, EntityKind::Function);
        assert!(r
            .relations
            .iter()
            .any(|rel| rel.kind == RelationKind::Calls && rel.target_qualified_name == "total"));
    }

    #[tokio::test]
    async fn member_call_keeps_simple_name() {
        // `obj.method()` e `ptr->method()` → Calls verso il nome semplice
        // (il ricevitore lo risolve il GraphResolver, anti-FP).
        let src = r#"
void run(Widget* w, Widget& r) {
    w->update();
    r.refresh();
}
"#;
        let r = parse(src, "run.cpp").await;
        let calls: Vec<&str> = r
            .relations
            .iter()
            .filter(|rel| rel.kind == RelationKind::Calls)
            .map(|rel| rel.target_qualified_name.as_str())
            .collect();
        assert!(calls.contains(&"update"), "calls: {calls:?}");
        assert!(calls.contains(&"refresh"), "calls: {calls:?}");
    }

    #[tokio::test]
    async fn enum_is_a_named_type_not_a_crash() {
        let src = "enum Color { Red, Green, Blue };\n";
        let r = parse(src, "color.h").await;
        let color = find(&r, "Color");
        assert_eq!(color.kind, EntityKind::Struct);
        assert_eq!(
            color.metadata.get("cpp_kind").map(String::as_str),
            Some("enum")
        );
    }

    #[tokio::test]
    async fn syntax_error_does_not_panic() {
        // Un frammento rotto produce errori dichiarati, mai un panic.
        let r = parse("class { int ;;; void (", "broken.cpp").await;
        assert!(!r.errors.is_empty());
    }
}
