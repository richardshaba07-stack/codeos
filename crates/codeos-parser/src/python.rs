//! `PythonParser`: visita l'AST di Tree-sitter e produce dati grezzi.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use codeos_types::{
    EntityKind, ParseError, ParsedEntity, ParsedFileResult, ParsedRelation, RelationKind,
    SourceLocation,
};
use tree_sitter::{Node, Parser};

use crate::traits::LanguageParser;

/// Parser per il linguaggio Python, basato su Tree-sitter.
pub struct PythonParser;

impl PythonParser {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PythonParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LanguageParser for PythonParser {
    fn can_parse(&self, file_extension: &str) -> bool {
        file_extension.eq_ignore_ascii_case("py")
    }

    async fn parse_file(&self, file_path: &Path, source_code: &str) -> ParsedFileResult {
        let path_str = file_path.to_string_lossy().to_string();

        let mut parser = Parser::new();
        if let Err(err) = parser.set_language(&tree_sitter_python::language()) {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: format!("grammatica Python non caricata: {err}"),
                    location: None,
                }],
                ..Default::default()
            };
        }

        let Some(tree) = parser.parse(source_code, None) else {
            return ParsedFileResult {
                file_path: path_str.clone(),
                errors: vec![ParseError {
                    message: "Tree-sitter non ha prodotto un albero".to_string(),
                    location: None,
                }],
                ..Default::default()
            };
        };

        let mut walk = FileWalk::new(source_code.as_bytes(), path_str.clone());
        let root = tree.root_node();

        // DECISION: per ogni file creiamo un'entità `Module` radice. È la sorgente
        // delle relazioni `Imports` e delle `Calls` a livello di modulo, ed è
        // l'ancora del name resolution (Blocco 3). Il briefing (sez. 6.2) non la
        // elenca, ma senza di essa gli import non avrebbero un `source_local_id`.
        let module_name = file_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "module".to_string());
        let module_id = walk.fresh_id();
        let module_loc = walk.loc(root);
        walk.entities.push(ParsedEntity {
            local_id: module_id.clone(),
            kind: EntityKind::Module,
            name: module_name,
            parent_local_id: None,
            location: module_loc,
            metadata: python_metadata(),
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

/// L'entità che racchiude il punto del codice in cui ci troviamo durante la
/// visita. Serve a collegare ogni entità/relazione al suo genitore e a
/// distinguere `Method` (dentro una classe) da `Function`.
#[derive(Clone)]
struct Scope {
    local_id: String,
    kind: EntityKind,
}

/// Stato mutabile della visita di un singolo file.
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

    /// Un `local_id` univoco all'interno del file (sez. 6.2). La visita è
    /// monothread, quindi un semplice contatore basta.
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

    /// Posizione 1-based per le righe, 0-based per le colonne (convenzione VS Code).
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
            let location = self.loc(node);
            self.errors.push(ParseError {
                message: format!("sintassi non valida ({}): '{snippet}'", node.kind()),
                location: Some(location),
            });
            // Non ci fermiamo: continuiamo a visitare i figli recuperabili.
        }

        match node.kind() {
            "class_definition" => self.handle_class(node, scope),
            "function_definition" => self.handle_function(node, scope),
            "import_statement" => self.handle_import(node, scope),
            "import_from_statement" => self.handle_import_from(node, scope),
            "call" => {
                self.handle_call(node, scope);
                // Le `call` possono annidarsi negli argomenti, es. f(g(x)).
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
            // Raccogliamo i figli per rilasciare il prestito del cursore prima di
            // richiamare `walk` (che vuole `&mut self`). `Node` è `Copy`.
            let children: Vec<Node> = node.children(&mut cursor).collect();
            for child in children {
                self.walk(child, scope);
            }
        })
    }

    fn handle_class(&mut self, node: Node, scope: &Scope) {
        let name = self.field_text(node, "name").unwrap_or_default();
        let id = self.fresh_id();
        let location = self.loc(node);
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind: EntityKind::Class,
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location,
            metadata: python_metadata(),
        });
        let child_scope = Scope {
            local_id: id,
            kind: EntityKind::Class,
        };
        self.walk_children(node, &child_scope);
    }

    fn handle_function(&mut self, node: Node, scope: &Scope) {
        let name = self.field_text(node, "name").unwrap_or_default();
        // DECISION: `Method` se l'entità che racchiude è una `Class`, altrimenti
        // `Function`. Raffina la regola del briefing ("parent.is_some() => Method"):
        // con l'entità `Module` radice OGNI funzione ha un parent, quindi quel
        // criterio renderebbe metodo anche una funzione top-level.
        let kind = if scope.kind == EntityKind::Class {
            EntityKind::Method
        } else {
            EntityKind::Function
        };
        let id = self.fresh_id();
        let location = self.loc(node);
        self.entities.push(ParsedEntity {
            local_id: id.clone(),
            kind,
            name,
            parent_local_id: Some(scope.local_id.clone()),
            location,
            metadata: python_metadata(),
        });
        let child_scope = Scope { local_id: id, kind };
        self.walk_children(node, &child_scope);
    }

    /// `import os`, `import a.b.c`, `import x as y`.
    fn handle_import(&mut self, node: Node, scope: &Scope) {
        let mut cursor = node.walk();
        let names: Vec<Node> = node.children_by_field_name("name", &mut cursor).collect();
        for name_node in names {
            let target = match name_node.kind() {
                "aliased_import" => self.field_text(name_node, "name").unwrap_or_default(),
                _ => self.text(name_node),
            };
            if !target.is_empty() {
                self.push_import(&scope.local_id, target, name_node);
            }
        }
    }

    /// `from a.b import c, d as e`, `from . import f`, `from m import *`.
    fn handle_import_from(&mut self, node: Node, scope: &Scope) {
        let module = self.field_text(node, "module_name").unwrap_or_default();
        let mut cursor = node.walk();
        let names: Vec<Node> = node.children_by_field_name("name", &mut cursor).collect();

        if names.is_empty() {
            // `from m import *`: registriamo la dipendenza dal modulo.
            self.push_import(&scope.local_id, module, node);
            return;
        }

        for name_node in names {
            let imported = match name_node.kind() {
                "aliased_import" => self.field_text(name_node, "name").unwrap_or_default(),
                _ => self.text(name_node),
            };
            if imported.is_empty() {
                continue;
            }
            let target = if module.is_empty() {
                imported
            } else {
                format!("{module}.{imported}")
            };
            self.push_import(&scope.local_id, target, name_node);
        }
    }

    fn push_import(&mut self, source_local_id: &str, target_qualified_name: String, node: Node) {
        let location = self.loc(node);
        self.relations.push(ParsedRelation {
            kind: RelationKind::Imports,
            source_local_id: source_local_id.to_string(),
            target_qualified_name,
            location,
        });
    }

    /// Da un nodo `call` estrae il testo del figlio `function` come target
    /// (sez. 6.2). Es. `createUser`, `self.bar`, `repo.save`.
    fn handle_call(&mut self, node: Node, scope: &Scope) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let target = self.text(func);
        if target.is_empty() {
            return;
        }
        let location = self.loc(node);
        self.relations.push(ParsedRelation {
            kind: RelationKind::Calls,
            source_local_id: scope.local_id.clone(),
            target_qualified_name: target,
            location,
        });
    }
}

fn python_metadata() -> HashMap<String, String> {
    let mut out = HashMap::new();
    out.insert("language".to_string(), "python".to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"import os
from a.b import c

class UserService(Base):
    def create_user(self, name):
        repo.save(name)

def top_level():
    UserService()
"#;

    async fn parse(src: &str) -> ParsedFileResult {
        PythonParser::new()
            .parse_file(Path::new("pkg/test.py"), src)
            .await
    }

    fn find<'a>(result: &'a ParsedFileResult, name: &str) -> &'a ParsedEntity {
        result
            .entities
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("entità '{name}' assente"))
    }

    #[tokio::test]
    async fn extracts_module_class_method_function_with_correct_kinds() {
        let result = parse(SRC).await;

        let module = find(&result, "test");
        assert_eq!(module.kind, EntityKind::Module);
        assert!(module.parent_local_id.is_none());

        let service = find(&result, "UserService");
        assert_eq!(service.kind, EntityKind::Class);
        assert_eq!(
            service.parent_local_id.as_deref(),
            Some(module.local_id.as_str())
        );

        let method = find(&result, "create_user");
        assert_eq!(method.kind, EntityKind::Method);
        assert_eq!(
            method.parent_local_id.as_deref(),
            Some(service.local_id.as_str())
        );

        let function = find(&result, "top_level");
        assert_eq!(function.kind, EntityKind::Function);
        assert_eq!(
            function.parent_local_id.as_deref(),
            Some(module.local_id.as_str())
        );
    }

    #[tokio::test]
    async fn extracts_imports_as_dotted_targets() {
        let result = parse(SRC).await;
        let imports: Vec<&str> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Imports)
            .map(|r| r.target_qualified_name.as_str())
            .collect();
        assert!(imports.contains(&"os"), "imports = {imports:?}");
        assert!(imports.contains(&"a.b.c"), "imports = {imports:?}");
    }

    #[tokio::test]
    async fn extracts_calls_attributed_to_their_enclosing_scope() {
        let result = parse(SRC).await;
        let calls: Vec<&ParsedRelation> = result
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .collect();
        let targets: Vec<&str> = calls
            .iter()
            .map(|c| c.target_qualified_name.as_str())
            .collect();
        assert!(targets.contains(&"repo.save"), "calls = {targets:?}");
        assert!(targets.contains(&"UserService"), "calls = {targets:?}");

        // `repo.save` è dentro `create_user`: la sorgente dev'essere quel metodo.
        let method = find(&result, "create_user");
        let save_call = calls
            .iter()
            .find(|c| c.target_qualified_name == "repo.save")
            .expect("call repo.save assente");
        assert_eq!(save_call.source_local_id, method.local_id);
    }

    #[tokio::test]
    async fn syntactically_broken_file_does_not_panic() {
        let result = parse("def broken(:\n    x =\n").await;
        // Niente panic. Almeno l'entità Module è presente; gli errori sono dati.
        assert!(!result.entities.is_empty());
        assert!(
            !result.errors.is_empty(),
            "ci aspettavamo errori di sintassi"
        );
    }

    #[tokio::test]
    async fn deeply_nested_input_does_not_overflow_the_stack() {
        // Regressione del crash GRAVE (Abort trap: 6) visto sul collaudo Microsoft:
        // un'espressione profondamente annidata fa traboccare lo stack del walk
        // ricorsivo. Senza `stacker` questo test ABORTIREBBE il processo (lo stack del
        // worker tokio è ~2 MB); con `stacker` lo stack cresce nell'heap e completa.
        // Il solo RITORNO senza crash è la prova: l'annidamento è di sole parentesi,
        // non asseriamo entità, solo la sopravvivenza del processo.
        let depth = 40_000;
        let src = format!("x = {}1{}\n", "(".repeat(depth), ")".repeat(depth));
        let result = parse(&src).await;
        assert!(
            !result.entities.is_empty(),
            "deve restare almeno il Module dopo un parse profondo, senza crash"
        );
    }
}
