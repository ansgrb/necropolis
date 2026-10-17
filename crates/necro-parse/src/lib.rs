//! Manifest detection and Tree-sitter AST symbol/import extraction (blueprint §8 Phase 1).
//!
//! Supports Rust, Python, Go, TypeScript/JavaScript, Kotlin, Swift, and Dart via native
//! Tree-sitter AST traversal, plus manifest stack detection (`Cargo.toml`, `pubspec.yaml`,
//! `package.json`, `go.mod`, `pyproject.toml`, `Package.swift`, `build.gradle(.kts)`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedSymbol {
    pub name: String,
    /// `class | fn | method | const | type | field | enum | interface`
    pub kind: String,
    pub start_line: i32,
    pub end_line: i32,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseOutput {
    pub lang: Option<String>,
    pub role: Option<String>,
    pub loc: i32,
    pub symbols: Vec<ExtractedSymbol>,
    pub imports: Vec<String>,
}

pub fn detect_language(rel_path: &str) -> Option<&'static str> {
    let ext = Path::new(rel_path).extension()?.to_str()?;
    match ext {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "go" => Some("go"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "kt" | "kts" => Some("kotlin"),
        "swift" => Some("swift"),
        "dart" => Some("dart"),
        "sql" => Some("sql"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "json" => Some("json"),
        "md" => Some("markdown"),
        "sh" => Some("shell"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("cpp"),
        _ => None,
    }
}

/// Infers a short one-line role for a file based on path and symbols.
pub fn infer_file_role(rel_path: &str, symbols: &[ExtractedSymbol]) -> String {
    let lower = rel_path.to_lowercase();
    if lower == "readme.md" || lower.ends_with("/readme.md") {
        return "project documentation".into();
    }
    for (name, role) in [
        ("cargo.toml", "Rust crate manifest"),
        ("pubspec.yaml", "Flutter/Dart package manifest"),
        ("package.json", "Node/JS package manifest"),
        ("go.mod", "Go module manifest"),
        ("pyproject.toml", "Python project manifest"),
        ("package.swift", "Swift package manifest"),
        ("build.gradle", "Gradle build script"),
        ("build.gradle.kts", "Gradle Kotlin build script"),
        ("docker-compose.yml", "Docker Compose services"),
        ("dockerfile", "container build definition"),
        ("makefile", "build automation targets"),
    ] {
        if lower == name || lower.ends_with(&format!("/{name}")) {
            return role.into();
        }
    }
    if lower.ends_with("main.rs") || lower.ends_with("main.go") || lower.ends_with("main.dart") || lower.ends_with("main.py") || lower.ends_with("main.swift") {
        return "application entry point".into();
    }
    if lower.ends_with("lib.rs") || lower.ends_with("__init__.py") || lower.ends_with("index.ts") || lower.ends_with("index.js") {
        return "library root / module exports".into();
    }
    if lower.contains("test") || lower.ends_with("_test.go") || lower.ends_with("_test.dart") {
        return "test suite".into();
    }
    if !symbols.is_empty() {
        let preview: Vec<&str> = symbols.iter().take(3).map(|s| s.name.as_str()).collect();
        return format!("defines {}", preview.join(", "));
    }
    "source file".into()
}

fn ts_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "kotlin" => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
        "swift" => Some(tree_sitter_swift::LANGUAGE.into()),
        _ => None,
    }
}

/// Parses `source` (which MUST already be redacted by `necro-redact`) and extracts symbols + imports.
pub fn parse_source(rel_path: &str, redacted_source: &str) -> ParseOutput {
    let loc = redacted_source.lines().count() as i32;
    let lang = detect_language(rel_path).map(|s| s.to_string());
    let mut symbols = Vec::new();
    let mut imports = BTreeSet::new();

    if let Some(ref l) = lang {
        if let Some(ts_lang) = ts_language(l) {
            let mut parser = Parser::new();
            if parser.set_language(&ts_lang).is_ok() {
                if let Some(tree) = parser.parse(redacted_source, None) {
                    walk_ast(tree.root_node(), redacted_source.as_bytes(), l, false, &mut symbols, &mut imports);
                }
            }
        } else if l == "dart" {
            parse_dart_fallback(redacted_source, &mut symbols, &mut imports);
        }
    }

    let role = Some(infer_file_role(rel_path, &symbols));
    ParseOutput { lang, role, loc, symbols, imports: imports.into_iter().collect() }
}

fn walk_ast(
    node: Node<'_>,
    src: &[u8],
    lang: &str,
    inside_type: bool,
    symbols: &mut Vec<ExtractedSymbol>,
    imports: &mut BTreeSet<String>,
) {
    let kind = node.kind();

    // Import extraction across supported languages
    if matches!(
        kind,
        "use_declaration"
            | "import_statement"
            | "import_from_statement"
            | "import_declaration"
            | "import_header"
    ) {
        if let Ok(text) = node.utf8_text(src) {
            let clean = text.lines().next().unwrap_or("").trim().trim_end_matches(';').to_string();
            if !clean.is_empty() && clean.len() <= 200 {
                imports.insert(clean);
            }
        }
    }

    let mapped_kind: Option<&'static str> = match kind {
        "struct_item" | "class_definition" | "class_declaration" => Some("class"),
        "enum_item" | "enum_declaration" | "enum_class_body" => Some("enum"),
        "trait_item" | "interface_declaration" | "protocol_declaration" => Some("interface"),
        "type_item" | "type_alias_declaration" | "typealias_declaration" => Some("type"),
        "const_item" | "static_item" => Some("const"),
        "method_declaration" | "method_definition" => Some("method"),
        "function_item" | "function_definition" | "function_declaration" => {
            Some(if inside_type { "method" } else { "fn" })
        }
        "type_spec" if lang == "go" => Some("type"),
        _ => None,
    };

    let now_inside_type = inside_type
        || matches!(
            kind,
            "impl_item" | "trait_item" | "class_definition" | "class_declaration" | "interface_declaration"
        );

    if let Some(sym_kind) = mapped_kind {
        if let Some(name) = extract_node_name(node, src) {
            let start_line = node.start_position().row as i32 + 1;
            let end_line = (node.end_position().row as i32 + 1).max(start_line);
            let signature = node
                .utf8_text(src)
                .ok()
                .and_then(|t| t.lines().next())
                .map(|l| l.trim().trim_end_matches('{').trim().chars().take(180).collect::<String>())
                .filter(|s| !s.is_empty());
            symbols.push(ExtractedSymbol {
                name,
                kind: sym_kind.into(),
                start_line,
                end_line,
                signature,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_ast(child, src, lang, now_inside_type, symbols, imports);
    }
}

fn extract_node_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        if let Ok(t) = n.utf8_text(src) {
            let s = t.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "identifier" | "type_identifier" | "simple_identifier" | "field_identifier") {
            if let Ok(t) = child.utf8_text(src) {
                let s = t.trim();
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// Fast line-oriented fallback for Dart so Flutter repos still yield clean classes, functions, and imports.
fn parse_dart_fallback(src: &str, symbols: &mut Vec<ExtractedSymbol>, imports: &mut BTreeSet<String>) {
    for (idx, raw) in src.lines().enumerate() {
        let line_no = idx as i32 + 1;
        let line = raw.trim();
        if line.starts_with("import ") || line.starts_with("export ") {
            imports.insert(line.trim_end_matches(';').to_string());
            continue;
        }
        for (prefix, kind) in [("class ", "class"), ("abstract class ", "class"), ("mixin ", "interface"), ("enum ", "enum")] {
            if let Some(rest) = line.strip_prefix(prefix) {
                if let Some(name) = rest.split(|c: char| !c.is_alphanumeric() && c != '_').find(|s| !s.is_empty()) {
                    symbols.push(ExtractedSymbol {
                        name: name.to_string(),
                        kind: kind.into(),
                        start_line: line_no,
                        end_line: line_no,
                        signature: Some(line.trim_end_matches('{').trim().chars().take(180).collect()),
                    });
                }
            }
        }
    }
}

/// Extracts stack tags and dependencies from known manifest files.
pub fn parse_manifest_stack(rel_path: &str, content: &str) -> Vec<String> {
    let fname = Path::new(rel_path).file_name().and_then(|s| s.to_str()).unwrap_or("");
    let mut stack = BTreeSet::new();
    match fname {
        "Cargo.toml" => {
            stack.insert("rust".into());
            if let Ok(v) = toml::from_str::<toml::Table>(content) {
                for section in ["dependencies", "dev-dependencies", "workspace.dependencies"] {
                    let tbl = if section == "workspace.dependencies" {
                        v.get("workspace").and_then(|w| w.get("dependencies")).and_then(|d| d.as_table())
                    } else {
                        v.get(section).and_then(|d| d.as_table())
                    };
                    if let Some(t) = tbl {
                        for k in t.keys().take(25) {
                            stack.insert(k.to_lowercase());
                        }
                    }
                }
            }
        }
        "pubspec.yaml" => {
            stack.insert("dart".into());
            if content.contains("flutter:") {
                stack.insert("flutter".into());
            }
            for known in ["riverpod", "flutter_riverpod", "bloc", "flutter_bloc", "dio", "drift", "firebase_core", "go_router", "freezed", "hive", "isar", "provider"] {
                if content.contains(known) {
                    stack.insert(known.into());
                }
            }
        }
        "package.json" => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
                for sec in ["dependencies", "devDependencies"] {
                    if let Some(obj) = v.get(sec).and_then(|d| d.as_object()) {
                        for k in obj.keys().take(25) {
                            stack.insert(k.to_lowercase());
                        }
                    }
                }
            }
        }
        "go.mod" => {
            stack.insert("go".into());
            for line in content.lines() {
                let t = line.trim();
                if let Some(pkg) = t.split_whitespace().next() {
                    if pkg.contains('/') && !pkg.starts_with("//") && pkg != "module" {
                        if let Some(short) = pkg.rsplit('/').next() {
                            stack.insert(short.to_lowercase());
                        }
                    }
                }
            }
        }
        "pyproject.toml" => {
            stack.insert("python".into());
            for known in ["fastapi", "django", "flask", "sqlalchemy", "pydantic", "pytest", "torch", "pandas", "numpy", "scikit-learn"] {
                if content.to_lowercase().contains(known) {
                    stack.insert(known.into());
                }
            }
        }
        "Package.swift" => {
            stack.insert("swift".into());
        }
        "build.gradle" | "build.gradle.kts" => {
            stack.insert("gradle".into());
            if content.contains("kotlin") {
                stack.insert("kotlin".into());
            }
            if content.contains("com.android") {
                stack.insert("android".into());
            }
        }
        _ => {}
    }
    stack.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols_and_imports() {
        let code = r#"
use std::collections::HashMap;

pub struct Scanner {
    pub count: usize,
}

impl Scanner {
    pub fn run(&self) -> bool { true }
}

pub fn top_level() {}
"#;
        let out = parse_source("src/lib.rs", code);
        assert_eq!(out.lang.as_deref(), Some("rust"));
        assert!(out.imports.iter().any(|i| i.contains("std::collections::HashMap")));
        assert!(out.symbols.iter().any(|s| s.name == "Scanner" && s.kind == "class"));
        assert!(out.symbols.iter().any(|s| s.name == "run" && s.kind == "method"));
        assert!(out.symbols.iter().any(|s| s.name == "top_level" && s.kind == "fn"));
    }

    #[test]
    fn extracts_python_and_go() {
        let py = "import os\nclass Worker:\n    def step(self):\n        pass\n";
        let out = parse_source("worker.py", py);
        assert!(out.symbols.iter().any(|s| s.name == "Worker" && s.kind == "class"));
        assert!(out.symbols.iter().any(|s| s.name == "step" && s.kind == "method"));

        let go = "package main\nimport \"fmt\"\ntype Server struct{}\nfunc Run() {}\n";
        let out_go = parse_source("main.go", go);
        assert!(out_go.symbols.iter().any(|s| s.name == "Server" && s.kind == "type"));
        assert!(out_go.symbols.iter().any(|s| s.name == "Run" && s.kind == "fn"));
    }

    #[test]
    fn parses_cargo_and_pubspec_manifests() {
        let cargo = "[package]\nname = \"demo\"\n[dependencies]\ntokio = \"1\"\nserde = \"1\"\n";
        let s = parse_manifest_stack("Cargo.toml", cargo);
        assert!(s.contains(&"rust".to_string()) && s.contains(&"tokio".to_string()));

        let pubspec = "name: app\ndependencies:\n  flutter:\n    sdk: flutter\n  riverpod: ^2.0.0\n";
        let s2 = parse_manifest_stack("pubspec.yaml", pubspec);
        assert!(s2.contains(&"flutter".to_string()) && s2.contains(&"riverpod".to_string()));
    }
}
