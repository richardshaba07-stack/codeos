//! Benchmark del percorso caldo del Parser: `parse_file` per ogni linguaggio.
//!
//! Misura il costo di trasformare un sorgente rappresentativo in
//! `ParsedFileResult` (tree-sitter + il nostro walk). È il primo stadio della
//! pipeline di indicizzazione, quello CPU-bound; isolarlo qui ci dà un numero
//! di riferimento per inseguire le regressioni linguaggio per linguaggio.
//!
//! `parse_file` è `async` solo per firma (il lavoro è sincrono): un runtime
//! tokio minimo con `block_on` ha overhead trascurabile rispetto al parsing.
//!
//! Esegui con: `cargo bench -p codeos-parser`.

use std::path::Path;

use codeos_parser::{
    GoParser, JavaParser, LanguageParser, PythonParser, RustParser, TypeScriptParser,
};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;

const PYTHON_SRC: &str = r#"
import os
from typing import List

class UserService:
    def __init__(self, repo):
        self.repo = repo

    def create_user(self, name: str) -> None:
        user = self.repo.insert(name)
        self.notify(user)

    def notify(self, user):
        os.write(user)

def main():
    service = UserService(None)
    service.create_user("alice")
"#;

const RUST_SRC: &str = r#"
use std::collections::HashMap;

pub struct Cache {
    entries: HashMap<String, String>,
}

impl Cache {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.entries.get(key)
    }

    pub fn put(&mut self, key: String, value: String) {
        self.entries.insert(key, value);
    }
}

pub fn build() -> Cache {
    let mut cache = Cache::new();
    cache.put("a".to_string(), "b".to_string());
    cache
}
"#;

const TYPESCRIPT_SRC: &str = r#"
import { Logger } from "./logger";

export interface Repository {
    find(id: string): User | null;
}

export class User {
    constructor(public name: string) {}
}

export class UserService {
    constructor(private repo: Repository, private logger: Logger) {}

    create(name: string): User {
        const user = new User(name);
        this.logger.info(name);
        return user;
    }
}
"#;

const GO_SRC: &str = r#"
package store

import "fmt"

type Server struct {
    addr string
}

func (s *Server) Start() error {
    boot()
    fmt.Println(s.addr)
    return nil
}

func boot() {
    cfg := newConfig()
    _ = cfg
}

func newConfig() *Server {
    return &Server{addr: "127.0.0.1"}
}
"#;

const JAVA_SRC: &str = r#"
package com.example.store;

import java.util.List;

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

/// Esegue una singola `parse_file` e consuma il risultato con `black_box` perché
/// l'ottimizzatore non possa eliminare il lavoro.
fn parse_once(rt: &Runtime, parser: &dyn LanguageParser, path: &str, src: &str) {
    let result = rt.block_on(parser.parse_file(Path::new(path), src));
    black_box(result);
}

fn bench_parsing(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime tokio per il benchmark");
    let mut group = c.benchmark_group("parse_file");

    let python = PythonParser::new();
    group.bench_function("python", |b| {
        b.iter(|| parse_once(&rt, &python, "bench/sample.py", PYTHON_SRC))
    });

    let rust = RustParser::new();
    group.bench_function("rust", |b| {
        b.iter(|| parse_once(&rt, &rust, "bench/sample.rs", RUST_SRC))
    });

    let typescript = TypeScriptParser::new();
    group.bench_function("typescript", |b| {
        b.iter(|| parse_once(&rt, &typescript, "bench/sample.ts", TYPESCRIPT_SRC))
    });

    let go = GoParser::new();
    group.bench_function("go", |b| {
        b.iter(|| parse_once(&rt, &go, "bench/sample.go", GO_SRC))
    });

    let java = JavaParser::new();
    group.bench_function("java", |b| {
        b.iter(|| parse_once(&rt, &java, "bench/sample.java", JAVA_SRC))
    });

    group.finish();
}

criterion_group!(benches, bench_parsing);
criterion_main!(benches);
