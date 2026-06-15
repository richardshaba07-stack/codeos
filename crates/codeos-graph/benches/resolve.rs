//! Benchmark del percorso caldo del GraphResolver: `resolve` di un batch
//! multi-file in un `GraphDelta`.
//!
//! È il secondo stadio dell'indicizzazione e l'algoritmo più interessante del
//! sistema: costruisce i `qualified_name`, indicizza il batch e risolve ogni
//! relazione con la cascata a più stadi del Passo 3. Misurarlo isolato ci dà il
//! riferimento per il name resolution, separato dal costo del parsing (i file
//! sono pre-parsati UNA volta, fuori dal loop cronometrato).
//!
//! `resolve` legge lo storage ma NON lo scrive (restituisce il delta senza
//! applicarlo): su un DB in memoria vuoto ogni iterazione parte dallo stesso
//! stato, quindi la misura è deterministica e ripetibile.
//!
//! Esegui con: `cargo bench -p codeos-graph`.

use std::path::Path;

use codeos_graph::GraphResolver;
use codeos_parser::{LanguageParser, PythonParser};
use codeos_storage::SqliteStorage;
use codeos_types::ParsedFileResult;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;

/// Numero di file sintetici nel batch: abbastanza da far emergere il costo
/// dell'indicizzazione e delle ricerche, restando rapido da eseguire.
const FILE_COUNT: usize = 50;

/// Un modulo Python autocontenuto con relazioni `BelongsTo` e `Calls` da
/// risolvere: `handle → process` (metodo dello stesso scope) e
/// `process → helper{i}` (funzione di modulo).
fn module_source(i: usize) -> String {
    format!(
        "class Service{i}:\n\
        \x20   def handle(self, req):\n\
        \x20       return self.process(req)\n\
        \n\
        \x20   def process(self, req):\n\
        \x20       return helper{i}(req)\n\
        \n\
        def helper{i}(req):\n\
        \x20   return req\n"
    )
}

fn bench_resolve(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime tokio per il benchmark");
    let parser = PythonParser::new();

    // Pre-parsa il batch una sola volta: cronometriamo SOLO la risoluzione.
    let parsed: Vec<ParsedFileResult> = (0..FILE_COUNT)
        .map(|i| {
            let src = module_source(i);
            let path = format!("pkg/mod{i}.py");
            parser.parse_file(Path::new(&path), &src)
        })
        .collect();

    // Storage vuoto e resolver condivisi: `resolve` non persiste, quindi lo
    // stato resta costante fra le iterazioni.
    let storage = SqliteStorage::in_memory().expect("storage in memoria");
    let resolver = GraphResolver::new(None);

    c.bench_function("resolve_50_python_modules", |b| {
        b.iter(|| {
            let delta = rt
                .block_on(resolver.resolve(&parsed, &storage))
                .expect("resolve non deve fallire");
            black_box(delta);
        })
    });
}

criterion_group!(benches, bench_resolve);
criterion_main!(benches);
