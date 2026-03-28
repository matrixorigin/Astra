//! Code intelligence via Tree-sitter — symbol extraction and outlines.
//!
//! Provides:
//! - Function/method/class extraction
//! - Module outlines (function signatures without bodies)
//! - Symbol search by name
//!
//! Supports: Rust, Python, TypeScript/JavaScript, Go

#![allow(dead_code)] // Symbol/find_symbols are exported for future tools

use std::path::Path;

/// Extracted symbol from source code.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// Symbol name (e.g., "parse_config", "UserService")
    pub name: String,
    /// Symbol kind (function, method, class, struct, interface, etc.)
    pub kind: SymbolKind,
    /// Start line (1-indexed)
    pub start_line: usize,
    /// End line (1-indexed)
    pub end_line: usize,
    /// Signature (e.g., "fn parse_config(path: &str) -> Result<Config>")
    pub signature: String,
    /// Parent symbol name (for methods, nested items)
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Interface,
    Trait,
    Enum,
    Constant,
    Variable,
    Module,
    Import,
    Type,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "method",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Enum => "enum",
            Self::Constant => "const",
            Self::Variable => "var",
            Self::Module => "mod",
            Self::Import => "import",
            Self::Type => "type",
        }
    }
}

/// Detect language from file extension.
pub fn detect_language(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "rs" => Some(Language::Rust),
        "py" => Some(Language::Python),
        "ts" | "tsx" => Some(Language::TypeScript),
        "js" | "jsx" => Some(Language::JavaScript),
        "go" => Some(Language::Go),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
}

/// Extract symbols from source code.
pub fn extract_symbols(source: &str, lang: Language) -> Vec<Symbol> {
    let mut parser = tree_sitter::Parser::new();

    // Set language grammar
    let language = match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::TypeScript | Language::JavaScript => {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    };

    if parser.set_language(&language).is_err() {
        return Vec::new();
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let root = tree.root_node();
    let mut symbols = Vec::new();

    match lang {
        Language::Rust => extract_rust_symbols(root, source, &mut symbols, None),
        Language::Python => extract_python_symbols(root, source, &mut symbols, None),
        Language::TypeScript | Language::JavaScript => {
            extract_ts_symbols(root, source, &mut symbols, None)
        }
        Language::Go => extract_go_symbols(root, source, &mut symbols, None),
    }

    symbols
}

/// Generate a module outline (signatures only, no bodies).
pub fn generate_outline(source: &str, lang: Language) -> String {
    let symbols = extract_symbols(source, lang);
    let mut lines = Vec::new();

    for sym in symbols {
        let indent = if sym.parent.is_some() { "  " } else { "" };
        lines.push(format!(
            "{}L{}: {} {}",
            indent,
            sym.start_line,
            sym.kind.as_str(),
            sym.signature
        ));
    }

    lines.join("\n")
}

/// Find symbols matching a name pattern.
pub fn find_symbols(source: &str, lang: Language, pattern: &str) -> Vec<Symbol> {
    let lower_pattern = pattern.to_lowercase();
    extract_symbols(source, lang)
        .into_iter()
        .filter(|s| s.name.to_lowercase().contains(&lower_pattern))
        .collect()
}

// ─── Rust Symbol Extraction ──────────────────────────────────────────────────

fn extract_rust_symbols(
    node: tree_sitter::Node,
    source: &str,
    symbols: &mut Vec<Symbol>,
    parent: Option<&str>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(sym) = parse_rust_function(child, source, parent) {
                    symbols.push(sym);
                }
            }
            "struct_item" => {
                if let Some(name) = get_child_text(child, "type_identifier", source) {
                    let sig = get_signature_line(child, source);
                    symbols.push(Symbol {
                        name: name.clone(),
                        kind: SymbolKind::Struct,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: sig,
                        parent: parent.map(String::from),
                    });
                    // Extract methods inside impl blocks referencing this struct
                }
            }
            "enum_item" => {
                if let Some(name) = get_child_text(child, "type_identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Enum,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "trait_item" => {
                if let Some(name) = get_child_text(child, "type_identifier", source) {
                    symbols.push(Symbol {
                        name: name.clone(),
                        kind: SymbolKind::Trait,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                    // Recurse into trait body for method signatures
                    extract_rust_symbols(child, source, symbols, Some(&name));
                }
            }
            "impl_item" => {
                // Get the type being implemented
                let impl_type = get_impl_type(child, source);
                extract_rust_symbols(child, source, symbols, impl_type.as_deref());
            }
            "mod_item" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name: name.clone(),
                        kind: SymbolKind::Module,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: format!("mod {}", name),
                        parent: parent.map(String::from),
                    });
                    extract_rust_symbols(child, source, symbols, Some(&name));
                }
            }
            "const_item" | "static_item" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Constant,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "type_item" => {
                if let Some(name) = get_child_text(child, "type_identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Type,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            _ => {
                // Recurse into other nodes (e.g., blocks)
                extract_rust_symbols(child, source, symbols, parent);
            }
        }
    }
}

fn parse_rust_function(
    node: tree_sitter::Node,
    source: &str,
    parent: Option<&str>,
) -> Option<Symbol> {
    let name = get_child_text(node, "identifier", source)?;
    let sig = get_signature_line(node, source);

    Some(Symbol {
        name,
        kind: if parent.is_some() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        },
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        signature: sig,
        parent: parent.map(String::from),
    })
}

fn get_impl_type(node: tree_sitter::Node, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_identifier" || child.kind() == "generic_type" {
            return Some(child.utf8_text(source.as_bytes()).ok()?.to_string());
        }
    }
    None
}

// ─── Python Symbol Extraction ────────────────────────────────────────────────

fn extract_python_symbols(
    node: tree_sitter::Node,
    source: &str,
    symbols: &mut Vec<Symbol>,
    parent: Option<&str>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: if parent.is_some() {
                            SymbolKind::Method
                        } else {
                            SymbolKind::Function
                        },
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "class_definition" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name: name.clone(),
                        kind: SymbolKind::Class,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                    // Recurse into class body
                    extract_python_symbols(child, source, symbols, Some(&name));
                }
            }
            _ => {
                extract_python_symbols(child, source, symbols, parent);
            }
        }
    }
}

// ─── TypeScript/JavaScript Symbol Extraction ─────────────────────────────────

fn extract_ts_symbols(
    node: tree_sitter::Node,
    source: &str,
    symbols: &mut Vec<Symbol>,
    parent: Option<&str>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Function,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "method_definition" => {
                if let Some(name) = get_child_text(child, "property_identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Method,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "class_declaration" => {
                if let Some(name) = get_child_text(child, "type_identifier", source)
                    .or_else(|| get_child_text(child, "identifier", source))
                {
                    symbols.push(Symbol {
                        name: name.clone(),
                        kind: SymbolKind::Class,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                    extract_ts_symbols(child, source, symbols, Some(&name));
                }
            }
            "interface_declaration" => {
                if let Some(name) = get_child_text(child, "type_identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Interface,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = get_child_text(child, "type_identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Type,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                // const/let/var declarations
                if let Some(decl) = child.child_by_field_name("declarator") {
                    if let Some(name) = get_child_text(decl, "identifier", source) {
                        symbols.push(Symbol {
                            name,
                            kind: SymbolKind::Variable,
                            start_line: child.start_position().row + 1,
                            end_line: child.end_position().row + 1,
                            signature: get_signature_line(child, source),
                            parent: parent.map(String::from),
                        });
                    }
                }
            }
            _ => {
                extract_ts_symbols(child, source, symbols, parent);
            }
        }
    }
}

// ─── Go Symbol Extraction ────────────────────────────────────────────────────

fn extract_go_symbols(
    node: tree_sitter::Node,
    source: &str,
    symbols: &mut Vec<Symbol>,
    parent: Option<&str>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Function,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "method_declaration" => {
                if let Some(name) = get_child_text(child, "field_identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Method,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "type_declaration" => {
                // Could be struct, interface, or type alias
                if let Some(spec) = child.child(1) {
                    if let Some(name) = get_child_text(spec, "type_identifier", source) {
                        let kind = if spec.kind() == "struct_type" {
                            SymbolKind::Struct
                        } else if spec.kind() == "interface_type" {
                            SymbolKind::Interface
                        } else {
                            SymbolKind::Type
                        };
                        symbols.push(Symbol {
                            name,
                            kind,
                            start_line: child.start_position().row + 1,
                            end_line: child.end_position().row + 1,
                            signature: get_signature_line(child, source),
                            parent: parent.map(String::from),
                        });
                    }
                }
            }
            "const_declaration" | "var_declaration" => {
                // Extract const/var names
                extract_go_symbols(child, source, symbols, parent);
            }
            "const_spec" | "var_spec" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: SymbolKind::Constant,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            _ => {
                extract_go_symbols(child, source, symbols, parent);
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn get_child_text(node: tree_sitter::Node, kind: &str, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return Some(child.utf8_text(source.as_bytes()).ok()?.to_string());
        }
    }
    None
}

fn get_signature_line(node: tree_sitter::Node, source: &str) -> String {
    let start = node.start_position();
    let end = node.end_position();

    // Get the first line of the node
    let lines: Vec<&str> = source.lines().collect();
    if start.row < lines.len() {
        let first_line = lines[start.row].trim();
        // Truncate at opening brace for multi-line signatures
        if let Some(brace_pos) = first_line.find('{') {
            return first_line[..brace_pos].trim().to_string();
        }
        // If no brace on first line, might be a multi-line sig — take first line
        if first_line.len() > 100 {
            return format!("{}...", &first_line[..100]);
        }
        return first_line.to_string();
    }

    format!("(lines {}-{})", start.row + 1, end.row + 1)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_rust_language() {
        assert_eq!(
            detect_language(Path::new("src/main.rs")),
            Some(Language::Rust)
        );
    }

    #[test]
    fn detect_python_language() {
        assert_eq!(
            detect_language(Path::new("app.py")),
            Some(Language::Python)
        );
    }

    #[test]
    fn detect_typescript_language() {
        assert_eq!(
            detect_language(Path::new("index.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            detect_language(Path::new("App.tsx")),
            Some(Language::TypeScript)
        );
    }

    #[test]
    fn extract_rust_functions() {
        let source = r#"
pub fn main() {
    println!("hello");
}

fn helper(x: i32) -> i32 {
    x + 1
}
"#;
        let symbols = extract_symbols(source, Language::Rust);
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "main");
        assert_eq!(symbols[0].kind, SymbolKind::Function);
        assert_eq!(symbols[1].name, "helper");
    }

    #[test]
    fn extract_rust_struct_and_impl() {
        let source = r#"
pub struct Config {
    path: String,
}

impl Config {
    pub fn new(path: &str) -> Self {
        Self { path: path.to_string() }
    }

    fn validate(&self) -> bool {
        true
    }
}
"#;
        let symbols = extract_symbols(source, Language::Rust);
        // Should find: struct Config, method new, method validate
        let struct_sym = symbols.iter().find(|s| s.name == "Config");
        assert!(struct_sym.is_some());
        assert_eq!(struct_sym.unwrap().kind, SymbolKind::Struct);

        let new_sym = symbols.iter().find(|s| s.name == "new");
        assert!(new_sym.is_some());
        assert_eq!(new_sym.unwrap().kind, SymbolKind::Method);
        assert_eq!(new_sym.unwrap().parent, Some("Config".to_string()));
    }

    #[test]
    fn extract_python_class() {
        let source = r#"
class UserService:
    def __init__(self, db):
        self.db = db

    def get_user(self, user_id: int) -> dict:
        return self.db.find(user_id)

def helper():
    pass
"#;
        let symbols = extract_symbols(source, Language::Python);

        let class_sym = symbols.iter().find(|s| s.name == "UserService");
        assert!(class_sym.is_some());
        assert_eq!(class_sym.unwrap().kind, SymbolKind::Class);

        let method_sym = symbols.iter().find(|s| s.name == "get_user");
        assert!(method_sym.is_some());
        assert_eq!(method_sym.unwrap().kind, SymbolKind::Method);
        assert_eq!(method_sym.unwrap().parent, Some("UserService".to_string()));

        let func_sym = symbols.iter().find(|s| s.name == "helper");
        assert!(func_sym.is_some());
        assert_eq!(func_sym.unwrap().kind, SymbolKind::Function);
    }

    #[test]
    fn extract_go_functions() {
        let source = r#"
package main

func main() {
    fmt.Println("hello")
}

func (s *Server) Start() error {
    return nil
}
"#;
        let symbols = extract_symbols(source, Language::Go);

        let main_sym = symbols.iter().find(|s| s.name == "main");
        assert!(main_sym.is_some());
        assert_eq!(main_sym.unwrap().kind, SymbolKind::Function);

        let method_sym = symbols.iter().find(|s| s.name == "Start");
        assert!(method_sym.is_some());
        assert_eq!(method_sym.unwrap().kind, SymbolKind::Method);
    }

    #[test]
    fn generate_rust_outline() {
        let source = r#"
pub fn parse_config(path: &str) -> Result<Config, Error> {
    // ...
}

pub struct Config {
    name: String,
}
"#;
        let outline = generate_outline(source, Language::Rust);
        assert!(outline.contains("fn parse_config"));
        assert!(outline.contains("struct Config"));
    }

    #[test]
    fn find_symbols_by_name() {
        let source = r#"
fn get_user() {}
fn get_config() {}
fn set_user() {}
"#;
        let matches = find_symbols(source, Language::Rust, "user");
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().any(|s| s.name == "get_user"));
        assert!(matches.iter().any(|s| s.name == "set_user"));
    }
}
