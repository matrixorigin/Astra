//! Code intelligence via Tree-sitter — symbol extraction and outlines.
//!
//! Provides:
//! - Function/method/class extraction
//! - Module outlines (function signatures without bodies)
//! - Symbol search by name
//!
//! Supports: Rust, Python, TypeScript/JavaScript, Go

#![allow(dead_code)] // Symbol/find_symbols are exported for future tools

use std::cell::RefCell;
use std::collections::HashMap;
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
    Constructor,
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
            Self::Constructor => "ctor",
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
        "java" => Some(Language::Java),
        "c" | "h" => Some(Language::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Some(Language::Cpp),
        "rb" => Some(Language::Ruby),
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
    Java,
    C,
    Cpp,
    Ruby,
}

thread_local! {
    static PARSER_CACHE: RefCell<HashMap<u8, tree_sitter::Parser>> = RefCell::new(HashMap::new());
}

/// Get a tree-sitter Language grammar for our Language enum.
fn ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::TypeScript | Language::JavaScript => {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::C | Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
    }
}

/// Parse source code using a cached thread-local parser.
/// Returns None if parsing fails.
fn cached_parse(source: &str, lang: Language) -> Option<tree_sitter::Tree> {
    let key = lang as u8;
    PARSER_CACHE.with(|cache| {
        let mut map = cache.borrow_mut();
        let parser = map.entry(key).or_insert_with(|| {
            let mut p = tree_sitter::Parser::new();
            let _ = p.set_language(&ts_language(lang));
            p
        });
        // Ensure the right language is set (in case of C/Cpp sharing)
        let _ = parser.set_language(&ts_language(lang));
        parser.parse(source, None)
    })
}

/// Extract symbols from source code.
pub fn extract_symbols(source: &str, lang: Language) -> Vec<Symbol> {
    let tree = match cached_parse(source, lang) {
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
        Language::Java => extract_java_symbols(root, source, &mut symbols, None),
        Language::C | Language::Cpp => extract_cpp_symbols(root, source, &mut symbols, None),
        Language::Ruby => extract_ruby_symbols(root, source, &mut symbols, None),
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

// ─── Java extraction ─────────────────────────────────────────────────────────

fn extract_java_symbols(
    node: tree_sitter::Node,
    source: &str,
    symbols: &mut Vec<Symbol>,
    parent: Option<&str>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_declaration" => {
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
                    if let Some(body) = child.child_by_field_name("body") {
                        extract_java_symbols(body, source, symbols, Some(&name));
                    }
                }
            }
            "interface_declaration" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name: name.clone(),
                        kind: SymbolKind::Interface,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                    if let Some(body) = child.child_by_field_name("body") {
                        extract_java_symbols(body, source, symbols, Some(&name));
                    }
                }
            }
            "enum_declaration" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
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
            "method_declaration" | "constructor_declaration" => {
                if let Some(name) = get_child_text(child, "identifier", source) {
                    symbols.push(Symbol {
                        name,
                        kind: if child.kind() == "constructor_declaration" {
                            SymbolKind::Constructor
                        } else {
                            SymbolKind::Method
                        },
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "field_declaration" => {
                // Extract field names (may have multiple declarators)
                let mut decl_cursor = child.walk();
                for decl_child in child.children(&mut decl_cursor) {
                    if decl_child.kind() == "variable_declarator" {
                        if let Some(name) = get_child_text(decl_child, "identifier", source) {
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
            }
            _ => {
                extract_java_symbols(child, source, symbols, parent);
            }
        }
    }
}

// ─── C/C++ extraction ────────────────────────────────────────────────────────

fn extract_cpp_symbols(
    node: tree_sitter::Node,
    source: &str,
    symbols: &mut Vec<Symbol>,
    parent: Option<&str>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                // Try to get function name from declarator
                if let Some(decl) = child.child_by_field_name("declarator") {
                    if let Some(name) = extract_cpp_declarator_name(decl, source) {
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
            }
            "declaration" => {
                // Could be function declaration, variable, etc.
                if let Some(decl) = child.child_by_field_name("declarator") {
                    // Check if it's a function declaration (has parameter list)
                    let is_func = decl.kind() == "function_declarator"
                        || has_child_kind(decl, "parameter_list");
                    if let Some(name) = extract_cpp_declarator_name(decl, source) {
                        symbols.push(Symbol {
                            name,
                            kind: if is_func {
                                SymbolKind::Function
                            } else {
                                SymbolKind::Variable
                            },
                            start_line: child.start_position().row + 1,
                            end_line: child.end_position().row + 1,
                            signature: get_signature_line(child, source),
                            parent: parent.map(String::from),
                        });
                    }
                }
            }
            "struct_specifier" | "class_specifier" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = name.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                    if !name_str.is_empty() {
                        symbols.push(Symbol {
                            name: name_str.clone(),
                            kind: if child.kind() == "class_specifier" {
                                SymbolKind::Class
                            } else {
                                SymbolKind::Struct
                            },
                            start_line: child.start_position().row + 1,
                            end_line: child.end_position().row + 1,
                            signature: get_signature_line(child, source),
                            parent: parent.map(String::from),
                        });
                        // Recurse into body
                        if let Some(body) = child.child_by_field_name("body") {
                            extract_cpp_symbols(body, source, symbols, Some(&name_str));
                        }
                    }
                }
            }
            "enum_specifier" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = name.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                    if !name_str.is_empty() {
                        symbols.push(Symbol {
                            name: name_str,
                            kind: SymbolKind::Enum,
                            start_line: child.start_position().row + 1,
                            end_line: child.end_position().row + 1,
                            signature: get_signature_line(child, source),
                            parent: parent.map(String::from),
                        });
                    }
                }
            }
            "namespace_definition" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = name.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                    symbols.push(Symbol {
                        name: name_str.clone(),
                        kind: SymbolKind::Module,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                    if let Some(body) = child.child_by_field_name("body") {
                        extract_cpp_symbols(body, source, symbols, Some(&name_str));
                    }
                }
            }
            _ => {
                extract_cpp_symbols(child, source, symbols, parent);
            }
        }
    }
}

fn extract_cpp_declarator_name(node: tree_sitter::Node, source: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" => {
            Some(node.utf8_text(source.as_bytes()).ok()?.to_string())
        }
        "function_declarator" | "pointer_declarator" | "reference_declarator" => {
            // Recurse into nested declarator
            if let Some(inner) = node.child_by_field_name("declarator") {
                extract_cpp_declarator_name(inner, source)
            } else {
                // Try first child as fallback
                node.child(0)
                    .and_then(|c| extract_cpp_declarator_name(c, source))
            }
        }
        "qualified_identifier" => {
            // For things like ClassName::method
            let text = node.utf8_text(source.as_bytes()).ok()?;
            Some(text.to_string())
        }
        _ => {
            // Try to find an identifier child
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" || child.kind() == "field_identifier" {
                    return Some(child.utf8_text(source.as_bytes()).ok()?.to_string());
                }
            }
            None
        }
    }
}

fn has_child_kind(node: tree_sitter::Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| c.kind() == kind)
}

// ─── Ruby extraction ─────────────────────────────────────────────────────────

fn extract_ruby_symbols(
    node: tree_sitter::Node,
    source: &str,
    symbols: &mut Vec<Symbol>,
    parent: Option<&str>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "class" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = name.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                    if !name_str.is_empty() {
                        symbols.push(Symbol {
                            name: name_str.clone(),
                            kind: SymbolKind::Class,
                            start_line: child.start_position().row + 1,
                            end_line: child.end_position().row + 1,
                            signature: get_signature_line(child, source),
                            parent: parent.map(String::from),
                        });
                        // Recurse into class body
                        extract_ruby_symbols(child, source, symbols, Some(&name_str));
                    }
                }
            }
            "module" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = name.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                    if !name_str.is_empty() {
                        symbols.push(Symbol {
                            name: name_str.clone(),
                            kind: SymbolKind::Module,
                            start_line: child.start_position().row + 1,
                            end_line: child.end_position().row + 1,
                            signature: get_signature_line(child, source),
                            parent: parent.map(String::from),
                        });
                        extract_ruby_symbols(child, source, symbols, Some(&name_str));
                    }
                }
            }
            "method" | "singleton_method" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let name_str = name.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                    symbols.push(Symbol {
                        name: name_str,
                        kind: SymbolKind::Method,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: parent.map(String::from),
                    });
                }
            }
            "constant" => {
                // Top-level constants
                let name_str = child.utf8_text(source.as_bytes()).unwrap_or("").to_string();
                if !name_str.is_empty() && parent.is_none() {
                    symbols.push(Symbol {
                        name: name_str,
                        kind: SymbolKind::Constant,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        signature: get_signature_line(child, source),
                        parent: None,
                    });
                }
            }
            _ => {
                extract_ruby_symbols(child, source, symbols, parent);
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
            // Find a valid char boundary at or before byte 100
            let mut end = 100;
            while !first_line.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            return format!("{}...", &first_line[..end]);
        }
        return first_line.to_string();
    }

    format!("(lines {}-{})", start.row + 1, end.row + 1)
}

// ─── Call Graph Extraction ───────────────────────────────────────────────────

/// A function call found within a symbol's body.
#[derive(Debug, Clone)]
pub struct CallSite {
    /// Name of the function/method being called
    pub callee: String,
    /// Line number of the call (1-indexed)
    pub line: usize,
    /// Optional receiver (e.g., "self", "config", "Vec")
    pub receiver: Option<String>,
}

/// Extract function/method calls within the body of a given symbol range.
/// Returns a list of call sites found between `start_line` and `end_line`.
pub fn extract_calls(
    source: &str,
    lang: Language,
    start_line: usize,
    end_line: usize,
) -> Vec<CallSite> {
    let tree = match cached_parse(source, lang) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut calls = Vec::new();
    collect_calls(
        tree.root_node(),
        source,
        start_line,
        end_line,
        lang,
        &mut calls,
    );
    // Deduplicate by (callee, line)
    calls.sort_by(|a, b| a.line.cmp(&b.line).then(a.callee.cmp(&b.callee)));
    calls.dedup_by(|a, b| a.line == b.line && a.callee == b.callee);
    calls
}

fn collect_calls(
    node: tree_sitter::Node,
    source: &str,
    start_line: usize,
    end_line: usize,
    lang: Language,
    calls: &mut Vec<CallSite>,
) {
    let node_start = node.start_position().row + 1;
    let node_end = node.end_position().row + 1;

    // Skip nodes entirely outside our range
    if node_end < start_line || node_start > end_line {
        return;
    }

    let kind = node.kind();

    // Match call expressions based on language
    let is_call = match lang {
        Language::Rust => kind == "call_expression" || kind == "macro_invocation",
        Language::Python => kind == "call",
        Language::TypeScript | Language::JavaScript => {
            kind == "call_expression" || kind == "new_expression"
        }
        Language::Go => kind == "call_expression",
        Language::Java => kind == "method_invocation" || kind == "object_creation_expression",
        Language::C | Language::Cpp => kind == "call_expression",
        Language::Ruby => kind == "call" || kind == "method_call",
    };

    if is_call {
        if let Some(cs) = parse_call_site(node, source, lang) {
            if cs.line >= start_line && cs.line <= end_line {
                calls.push(cs);
            }
        }
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_calls(child, source, start_line, end_line, lang, calls);
    }
}

fn parse_call_site(node: tree_sitter::Node, source: &str, lang: Language) -> Option<CallSite> {
    let line = node.start_position().row + 1;

    match lang {
        Language::Rust => {
            // call_expression: function child is the callee
            if node.kind() == "macro_invocation" {
                // macro!(...) — first child is the macro name
                let name_node = node.child(0)?;
                let name = node_text(name_node, source);
                return Some(CallSite {
                    callee: format!("{name}!"),
                    line,
                    receiver: None,
                });
            }
            let func = node.child(0)?;
            if func.kind() == "field_expression" {
                // receiver.method() form
                let receiver_node = func.child(0)?;
                let method_node = func.child_by_field_name("field")?;
                Some(CallSite {
                    callee: node_text(method_node, source),
                    line,
                    receiver: Some(node_text(receiver_node, source)),
                })
            } else if func.kind() == "scoped_identifier" {
                // Path::method() form
                Some(CallSite {
                    callee: node_text(func, source),
                    line,
                    receiver: None,
                })
            } else {
                Some(CallSite {
                    callee: node_text(func, source),
                    line,
                    receiver: None,
                })
            }
        }
        Language::Python => {
            let func = node.child_by_field_name("function")?;
            if func.kind() == "attribute" {
                let obj = func.child_by_field_name("object")?;
                let attr = func.child_by_field_name("attribute")?;
                Some(CallSite {
                    callee: node_text(attr, source),
                    line,
                    receiver: Some(node_text(obj, source)),
                })
            } else {
                Some(CallSite {
                    callee: node_text(func, source),
                    line,
                    receiver: None,
                })
            }
        }
        Language::TypeScript | Language::JavaScript => {
            if node.kind() == "new_expression" {
                let ctor = node.child(1)?;
                return Some(CallSite {
                    callee: format!("new {}", node_text(ctor, source)),
                    line,
                    receiver: None,
                });
            }
            let func = node.child_by_field_name("function")?;
            if func.kind() == "member_expression" {
                let obj = func.child_by_field_name("object")?;
                let prop = func.child_by_field_name("property")?;
                Some(CallSite {
                    callee: node_text(prop, source),
                    line,
                    receiver: Some(node_text(obj, source)),
                })
            } else {
                Some(CallSite {
                    callee: node_text(func, source),
                    line,
                    receiver: None,
                })
            }
        }
        Language::Go => {
            let func = node.child_by_field_name("function")?;
            if func.kind() == "selector_expression" {
                let obj = func.child_by_field_name("operand")?;
                let sel = func.child_by_field_name("field")?;
                Some(CallSite {
                    callee: node_text(sel, source),
                    line,
                    receiver: Some(node_text(obj, source)),
                })
            } else {
                Some(CallSite {
                    callee: node_text(func, source),
                    line,
                    receiver: None,
                })
            }
        }
        Language::Java => {
            if node.kind() == "method_invocation" {
                let name = node.child_by_field_name("name")?;
                let obj = node.child_by_field_name("object");
                Some(CallSite {
                    callee: node_text(name, source),
                    line,
                    receiver: obj.map(|o| node_text(o, source)),
                })
            } else {
                // object_creation_expression
                let typ = node.child_by_field_name("type")?;
                Some(CallSite {
                    callee: format!("new {}", node_text(typ, source)),
                    line,
                    receiver: None,
                })
            }
        }
        Language::C | Language::Cpp => {
            let func = node.child_by_field_name("function")?;
            if func.kind() == "field_expression" {
                let obj = func.child_by_field_name("argument")?;
                let field = func.child_by_field_name("field")?;
                Some(CallSite {
                    callee: node_text(field, source),
                    line,
                    receiver: Some(node_text(obj, source)),
                })
            } else {
                Some(CallSite {
                    callee: node_text(func, source),
                    line,
                    receiver: None,
                })
            }
        }
        Language::Ruby => {
            let method = node.child_by_field_name("method")?;
            let recv = node.child_by_field_name("receiver");
            Some(CallSite {
                callee: node_text(method, source),
                line,
                receiver: recv.map(|r| node_text(r, source)),
            })
        }
    }
}

// ─── Scope Context ──────────────────────────────────────────────────────────

/// The enclosing scope at a given line.
#[derive(Debug, Clone)]
pub struct ScopeContext {
    /// Breadcrumb path from outermost to innermost scope
    /// e.g., ["impl ToolExecutor", "fn str_replace"]
    pub breadcrumbs: Vec<String>,
    /// The innermost symbol containing the target line
    pub symbol: Option<Symbol>,
}

/// Identify the symbol/identifier at exact (line, column) using tree-sitter AST.
/// Returns (identifier_text, node_kind) or None if not on an identifier.
pub fn identifier_at_position(
    source: &str,
    lang: Language,
    line: usize,
    column: usize,
) -> Option<(String, String)> {
    let tree = cached_parse(source, lang)?;

    let point = tree_sitter::Point {
        row: line.saturating_sub(1),
        column,
    };

    let node = tree.root_node().descendant_for_point_range(point, point)?;

    // Walk up to find an identifier or type_identifier node
    let mut current = node;
    for _ in 0..5 {
        let kind = current.kind();
        if kind == "identifier"
            || kind == "type_identifier"
            || kind == "field_identifier"
            || kind == "property_identifier"
            || kind == "method_name"
            || kind == "name"
        {
            let text = current.utf8_text(source.as_bytes()).ok()?.to_string();
            return Some((text, kind.to_string()));
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    // If we landed on a leaf node with content, use it directly
    let text = node.utf8_text(source.as_bytes()).ok()?.to_string();
    if !text.is_empty() && text.len() < 100 && !text.contains('\n') {
        Some((text, node.kind().to_string()))
    } else {
        None
    }
}

/// Find the enclosing scope at a given line (1-indexed).
pub fn scope_at_line(source: &str, lang: Language, line: usize) -> ScopeContext {
    let symbols = extract_symbols(source, lang);

    // Find all symbols that contain this line, sorted by specificity (smaller range = more specific)
    let mut containing: Vec<&Symbol> = symbols
        .iter()
        .filter(|s| s.start_line <= line && s.end_line >= line)
        .collect();

    // Sort by range size ascending (most specific first)
    containing.sort_by_key(|s| s.end_line - s.start_line);

    let innermost = containing.first().copied().cloned();

    // Build breadcrumb trail from outermost to innermost
    // We reverse to go from outermost to innermost
    containing.reverse();
    let breadcrumbs: Vec<String> = containing
        .iter()
        .map(|s| {
            if s.signature.len() > 80 {
                format!("{} {}...", s.kind.as_str(), s.name)
            } else {
                format!("{} {}", s.kind.as_str(), s.signature)
            }
        })
        .collect();

    ScopeContext {
        breadcrumbs,
        symbol: innermost,
    }
}

fn node_text(node: tree_sitter::Node, source: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte();
    if start <= end && end <= source.len() {
        source[start..end].to_string()
    } else {
        String::new()
    }
}

/// Extract the doc comment block immediately preceding a given line.
///
/// Walks backward from `symbol_line - 1` collecting contiguous comment lines.
/// Handles language-specific doc comment styles:
/// - Rust: `///`, `//!`, `/** ... */`
/// - Python: `"""..."""` docstring (first expression in function body at symbol_line + 1)
/// - TypeScript/JavaScript/Go/Java/C/C++: `/** ... */`, `///`, `//`
/// - Ruby: `#` comment blocks
///
/// Returns the cleaned doc text (comment markers stripped) or empty string if no doc found.
pub fn extract_doc_comment(source: &str, lang: Language, symbol_line: usize) -> String {
    let lines: Vec<&str> = source.lines().collect();
    if symbol_line == 0 || symbol_line > lines.len() {
        return String::new();
    }

    // For Python, check for docstring INSIDE the function body
    if lang == Language::Python {
        if let Some(docstring) = extract_python_docstring(&lines, symbol_line) {
            return docstring;
        }
    }

    // Walk backward collecting comment lines
    let mut doc_lines: Vec<String> = Vec::new();
    let mut i = symbol_line.saturating_sub(1); // 0-indexed line before symbol (symbol_line is 1-indexed)
    if i == 0 && symbol_line > 1 {
        // symbol_line is 1-indexed, so line before it is at index symbol_line - 2
    }
    // Convert to 0-indexed
    let start_idx = symbol_line.checked_sub(2).unwrap_or(0);

    // Check for block comment first (/** ... */)
    if start_idx < lines.len() {
        let trimmed = lines[start_idx].trim();
        if trimmed.ends_with("*/") {
            // Walk back to find the opening `/**` or `/*`
            let mut block_lines: Vec<String> = Vec::new();
            i = start_idx;
            loop {
                let line = lines[i].trim();
                if line.starts_with("/**") || line.starts_with("/*") {
                    // Strip the opening marker
                    let content = line
                        .trim_start_matches("/**")
                        .trim_start_matches("/*")
                        .trim();
                    let content = content.trim_end_matches("*/").trim();
                    if !content.is_empty() {
                        block_lines.push(content.to_string());
                    }
                    block_lines.reverse();
                    return block_lines.join("\n");
                }
                // Strip leading * and trailing */
                let content = line.trim_start_matches('*').trim_end_matches("*/").trim();
                block_lines.push(content.to_string());
                if i == 0 {
                    break;
                }
                i -= 1;
            }
        }
    }

    // Walk backward collecting single-line comments
    i = start_idx;
    loop {
        if i >= lines.len() {
            break;
        }
        let line = lines[i].trim();

        // Check for doc comment patterns by language
        let (is_doc, content) = match lang {
            Language::Rust => {
                if let Some(rest) = line.strip_prefix("///") {
                    (true, rest.trim().to_string())
                } else if let Some(rest) = line.strip_prefix("//!") {
                    (true, rest.trim().to_string())
                } else {
                    (false, String::new())
                }
            }
            Language::Go => {
                if let Some(rest) = line.strip_prefix("//") {
                    (true, rest.trim().to_string())
                } else {
                    (false, String::new())
                }
            }
            Language::Ruby => {
                if let Some(rest) = line.strip_prefix('#') {
                    (true, rest.trim().to_string())
                } else {
                    (false, String::new())
                }
            }
            _ => {
                // JS/TS/Java/C/C++: accept // and ///
                if let Some(rest) = line.strip_prefix("///") {
                    (true, rest.trim().to_string())
                } else if let Some(rest) = line.strip_prefix("//") {
                    (true, rest.trim().to_string())
                } else {
                    (false, String::new())
                }
            }
        };

        if is_doc {
            doc_lines.push(content);
        } else if line.is_empty() && !doc_lines.is_empty() {
            // Allow one blank line in the middle of a doc block
            doc_lines.push(String::new());
        } else {
            break;
        }

        if i == 0 {
            break;
        }
        i -= 1;
    }

    doc_lines.reverse();
    // Trim leading/trailing blank lines
    while doc_lines.first().map_or(false, |l| l.is_empty()) {
        doc_lines.remove(0);
    }
    while doc_lines.last().map_or(false, |l| l.is_empty()) {
        doc_lines.pop();
    }
    doc_lines.join("\n")
}

/// Extract Python docstring: the first string literal in the function body.
fn extract_python_docstring(lines: &[&str], symbol_line: usize) -> Option<String> {
    // Look at lines after the def/class line for a triple-quoted string
    let body_start = symbol_line; // 0-indexed line after the symbol (symbol_line is 1-indexed)
    if body_start >= lines.len() {
        return None;
    }

    let first_body = lines[body_start].trim();
    if first_body.starts_with("\"\"\"") || first_body.starts_with("'''") {
        let quote = if first_body.starts_with("\"\"\"") {
            "\"\"\""
        } else {
            "'''"
        };
        // Single-line docstring
        if first_body.ends_with(quote) && first_body.len() > 6 {
            let content = first_body
                .trim_start_matches(quote)
                .trim_end_matches(quote)
                .trim();
            return Some(content.to_string());
        }
        // Multi-line docstring
        let mut doc_lines = Vec::new();
        let first_content = first_body.trim_start_matches(quote).trim();
        if !first_content.is_empty() {
            doc_lines.push(first_content.to_string());
        }
        for j in (body_start + 1)..lines.len() {
            let line = lines[j].trim();
            if line.contains(quote) {
                let content = line.trim_end_matches(quote).trim();
                if !content.is_empty() {
                    doc_lines.push(content.to_string());
                }
                return Some(doc_lines.join("\n"));
            }
            doc_lines.push(line.to_string());
        }
    }
    None
}

/// Check if a position in source code falls inside a comment or string literal.
///
/// Parses the source with tree-sitter and walks ancestors of the node at
/// the given (line, column) position. Returns true if ANY ancestor is a
/// comment, string, or doc-comment node.
///
/// Used by find_references to filter out false-positive grep matches that
/// appear in comments or string literals.
pub fn is_in_comment_or_string(source: &str, lang: Language, line: usize, column: usize) -> bool {
    let tree = match cached_parse(source, lang) {
        Some(t) => t,
        None => return false,
    };

    // tree-sitter uses 0-indexed rows
    let point = tree_sitter::Point {
        row: line.saturating_sub(1),
        column,
    };

    let Some(node) = tree.root_node().descendant_for_point_range(point, point) else {
        return false;
    };

    // Walk up from the node to check if any ancestor is a comment or string
    let mut current = Some(node);
    while let Some(n) = current {
        let kind = n.kind();
        if is_non_code_node(kind) {
            return true;
        }
        current = n.parent();
    }

    false
}

/// Check if a tree-sitter node kind represents non-code content (comments, strings, etc.)
fn is_non_code_node(kind: &str) -> bool {
    // Comments (across all languages)
    kind == "line_comment"
        || kind == "block_comment"
        || kind == "comment"
        || kind == "doc_comment"
        // Rust-specific
        || kind == "inner_line_doc_comment"
        || kind == "outer_line_doc_comment"
        || kind == "inner_block_doc_comment"
        || kind == "outer_block_doc_comment"
        // String literals
        || kind == "string_literal"
        || kind == "raw_string_literal"
        || kind == "string"
        || kind == "raw_string"
        || kind == "string_content"
        || kind == "interpreted_string_literal"  // Go
        || kind == "raw_string_literal"
        || kind == "template_string"  // JS/TS
        || kind == "string_fragment"
        || kind == "heredoc_body"  // Ruby
        // Python specific
        || kind == "concatenated_string"
}

// ─── Type Member Extraction ─────────────────────────────────────────────────

/// A field, property, or variant belonging to a type definition.
#[derive(Debug, Clone)]
pub struct Member {
    /// Member name (e.g., "name", "age", "Variant")
    pub name: String,
    /// Member kind: "field", "method", "variant", "property"
    pub kind: String,
    /// Type annotation if available (e.g., "String", "usize", "Option<i32>")
    pub type_annotation: String,
    /// Line number (1-indexed)
    pub line: usize,
    /// Visibility/access (e.g., "pub", "pub(crate)", "private", "protected")
    pub visibility: String,
    /// Default value or initializer if present
    pub default_value: String,
}

/// Extract members (fields, methods, variants) from a type definition at a
/// specific line in the source. Finds the enclosing struct/class/enum/interface
/// and returns its members.
pub fn extract_members(source: &str, lang: Language, type_line: usize) -> Vec<Member> {
    let tree = match cached_parse(source, lang) {
        Some(t) => t,
        None => return Vec::new(),
    };

    // Find the type node at the given line
    let target_row = type_line.saturating_sub(1);
    let root = tree.root_node();
    let type_node = find_type_node_at_line(root, target_row, lang);

    match type_node {
        Some(node) => match lang {
            Language::Rust => extract_rust_members(node, source),
            Language::Python => extract_python_members(node, source),
            Language::TypeScript | Language::JavaScript => extract_ts_members(node, source),
            Language::Go => extract_go_members(node, source),
            _ => Vec::new(),
        },
        None => Vec::new(),
    }
}

fn find_type_node_at_line(
    root: tree_sitter::Node,
    target_row: usize,
    lang: Language,
) -> Option<tree_sitter::Node> {
    let type_kinds: &[&str] = match lang {
        Language::Rust => &["struct_item", "enum_item", "trait_item"],
        Language::Python => &["class_definition"],
        Language::TypeScript | Language::JavaScript => &[
            "class_declaration",
            "interface_declaration",
            "type_alias_declaration",
        ],
        Language::Go => &["type_declaration", "type_spec"],
        Language::Java => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        _ => &[],
    };

    find_node_at_line_by_kinds(root, target_row, type_kinds)
}

fn find_node_at_line_by_kinds<'a>(
    node: tree_sitter::Node<'a>,
    target_row: usize,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    if kinds.contains(&node.kind()) && node.start_position().row == target_row {
        return Some(node);
    }
    // Also match if target is within the node's range (user points at any line)
    if kinds.contains(&node.kind())
        && target_row >= node.start_position().row
        && target_row <= node.end_position().row
    {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_node_at_line_by_kinds(child, target_row, kinds) {
            return Some(found);
        }
    }
    None
}

fn extract_rust_members(node: tree_sitter::Node, source: &str) -> Vec<Member> {
    let mut members = Vec::new();
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "field_declaration_list" => {
                let mut fc = child.walk();
                for field in child.children(&mut fc) {
                    if field.kind() == "field_declaration" {
                        let vis = extract_rust_visibility(field, source);
                        let name =
                            get_child_text(field, "field_identifier", source).unwrap_or_default();
                        let type_ann = get_child_text(field, "type_identifier", source)
                            .or_else(|| get_child_by_kind_text(field, source))
                            .unwrap_or_default();
                        members.push(Member {
                            name,
                            kind: "field".to_string(),
                            type_annotation: type_ann,
                            line: field.start_position().row + 1,
                            visibility: vis,
                            default_value: String::new(),
                        });
                    }
                }
            }
            "enum_variant_list" => {
                let mut vc = child.walk();
                for variant in child.children(&mut vc) {
                    if variant.kind() == "enum_variant" {
                        let name =
                            get_child_text(variant, "identifier", source).unwrap_or_default();
                        members.push(Member {
                            name,
                            kind: "variant".to_string(),
                            type_annotation: String::new(),
                            line: variant.start_position().row + 1,
                            visibility: "pub".to_string(),
                            default_value: String::new(),
                        });
                    }
                }
            }
            "declaration_list" => {
                // Trait items (method signatures)
                let mut dc = child.walk();
                for item in child.children(&mut dc) {
                    if item.kind() == "function_item" || item.kind() == "function_signature_item" {
                        let name = get_child_text(item, "identifier", source).unwrap_or_default();
                        let sig = get_signature_line(item, source);
                        members.push(Member {
                            name,
                            kind: "method".to_string(),
                            type_annotation: sig,
                            line: item.start_position().row + 1,
                            visibility: String::new(),
                            default_value: String::new(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    members
}

fn extract_rust_visibility(node: tree_sitter::Node, source: &str) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child
                .utf8_text(source.as_bytes())
                .unwrap_or("pub")
                .to_string();
        }
    }
    String::new()
}

fn get_child_by_kind_text(node: tree_sitter::Node, source: &str) -> Option<String> {
    // For complex types (generic_type, reference_type, etc.), get the full text
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k.ends_with("_type") || k == "generic_type" || k == "scoped_type_identifier" {
            return Some(child.utf8_text(source.as_bytes()).ok()?.to_string());
        }
    }
    None
}

fn extract_python_members(node: tree_sitter::Node, source: &str) -> Vec<Member> {
    let mut members = Vec::new();
    // Look at the class body for assignments and methods
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "block" {
            let mut bc = child.walk();
            for stmt in child.children(&mut bc) {
                match stmt.kind() {
                    "function_definition" => {
                        let name = get_child_text(stmt, "identifier", source).unwrap_or_default();
                        let sig = get_signature_line(stmt, source);
                        members.push(Member {
                            name,
                            kind: "method".to_string(),
                            type_annotation: sig,
                            line: stmt.start_position().row + 1,
                            visibility: String::new(),
                            default_value: String::new(),
                        });
                    }
                    "expression_statement" => {
                        // Class-level assignments like: name: str = "default"
                        let text = stmt
                            .utf8_text(source.as_bytes())
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if let Some(colon_pos) = text.find(':') {
                            let name = text[..colon_pos].trim().to_string();
                            let rest = text[colon_pos + 1..].trim();
                            let (type_ann, default) = if let Some(eq_pos) = rest.find('=') {
                                (
                                    rest[..eq_pos].trim().to_string(),
                                    rest[eq_pos + 1..].trim().to_string(),
                                )
                            } else {
                                (rest.to_string(), String::new())
                            };
                            if !name.is_empty() && !name.contains(' ') && !name.starts_with('#') {
                                members.push(Member {
                                    name,
                                    kind: "property".to_string(),
                                    type_annotation: type_ann,
                                    line: stmt.start_position().row + 1,
                                    visibility: String::new(),
                                    default_value: default,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    members
}

fn extract_ts_members(node: tree_sitter::Node, source: &str) -> Vec<Member> {
    let mut members = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "class_body"
            || child.kind() == "interface_body"
            || child.kind() == "object_type"
        {
            let mut bc = child.walk();
            for item in child.children(&mut bc) {
                match item.kind() {
                    "method_definition" | "method_signature" => {
                        let name =
                            get_child_text(item, "property_identifier", source).unwrap_or_default();
                        let sig = get_signature_line(item, source);
                        members.push(Member {
                            name,
                            kind: "method".to_string(),
                            type_annotation: sig,
                            line: item.start_position().row + 1,
                            visibility: extract_ts_visibility(item, source),
                            default_value: String::new(),
                        });
                    }
                    "public_field_definition" | "property_signature" => {
                        let name =
                            get_child_text(item, "property_identifier", source).unwrap_or_default();
                        let type_ann = get_child_text(item, "type_annotation", source)
                            .map(|t| t.trim_start_matches(':').trim().to_string())
                            .unwrap_or_default();
                        members.push(Member {
                            name,
                            kind: "property".to_string(),
                            type_annotation: type_ann,
                            line: item.start_position().row + 1,
                            visibility: extract_ts_visibility(item, source),
                            default_value: String::new(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    members
}

fn extract_ts_visibility(node: tree_sitter::Node, source: &str) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "accessibility_modifier" {
            return child
                .utf8_text(source.as_bytes())
                .unwrap_or("public")
                .to_string();
        }
    }
    String::new()
}

fn extract_go_members(node: tree_sitter::Node, source: &str) -> Vec<Member> {
    let mut members = Vec::new();
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.kind() == "type_spec" {
            // Recurse into the type_spec to find struct_type or interface_type
            let mut tc = child.walk();
            for type_child in child.children(&mut tc) {
                match type_child.kind() {
                    "struct_type" => {
                        let mut fc = type_child.walk();
                        for field_list in type_child.children(&mut fc) {
                            if field_list.kind() == "field_declaration_list" {
                                let mut flc = field_list.walk();
                                for field in field_list.children(&mut flc) {
                                    if field.kind() == "field_declaration" {
                                        let name =
                                            get_child_text(field, "field_identifier", source)
                                                .unwrap_or_default();
                                        let type_ann = field
                                            .utf8_text(source.as_bytes())
                                            .unwrap_or_default()
                                            .trim()
                                            .to_string();
                                        // Remove the name from front to get type
                                        let type_part = type_ann
                                            .strip_prefix(&name)
                                            .unwrap_or(&type_ann)
                                            .trim()
                                            .to_string();
                                        members.push(Member {
                                            name,
                                            kind: "field".to_string(),
                                            type_annotation: type_part,
                                            line: field.start_position().row + 1,
                                            visibility: String::new(),
                                            default_value: String::new(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                    "interface_type" => {
                        let mut ic = type_child.walk();
                        for method_list in type_child.children(&mut ic) {
                            if method_list.kind() == "method_spec_list"
                                || method_list.kind() == "method_spec"
                            {
                                let text = method_list
                                    .utf8_text(source.as_bytes())
                                    .unwrap_or_default()
                                    .trim()
                                    .to_string();
                                if !text.is_empty()
                                    && !text.starts_with('{')
                                    && !text.starts_with('}')
                                {
                                    members.push(Member {
                                        name: text
                                            .split('(')
                                            .next()
                                            .unwrap_or("")
                                            .trim()
                                            .to_string(),
                                        kind: "method".to_string(),
                                        type_annotation: text,
                                        line: method_list.start_position().row + 1,
                                        visibility: String::new(),
                                        default_value: String::new(),
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    members
}

// ─── Implementation / Type Hierarchy ────────────────────────────────────────

/// An implementation relationship found in source code.
#[derive(Debug, Clone)]
pub struct ImplRelation {
    /// The trait/interface being implemented
    pub trait_name: String,
    /// The type implementing it
    pub type_name: String,
    /// File where the impl was found
    pub file: String,
    /// Line number of the impl block
    pub line: usize,
}

/// Find impl blocks in a Rust source file.
/// Returns all `impl Trait for Type` relationships.
pub fn find_rust_impls(source: &str, file_path: &str) -> Vec<ImplRelation> {
    let tree = match cached_parse(source, Language::Rust) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut impls = Vec::new();
    collect_rust_impls(tree.root_node(), source, file_path, &mut impls);
    impls
}

fn collect_rust_impls(
    node: tree_sitter::Node,
    source: &str,
    file_path: &str,
    impls: &mut Vec<ImplRelation>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "impl_item" {
            // Check if this is `impl Trait for Type`
            let text = child.utf8_text(source.as_bytes()).unwrap_or_default();
            let first_line = text.lines().next().unwrap_or("");

            if let Some(for_pos) = first_line.find(" for ") {
                // impl TRAIT for TYPE
                let before_for = &first_line[..for_pos];
                let after_for = &first_line[for_pos + 5..];

                let trait_name = before_for
                    .strip_prefix("impl ")
                    .unwrap_or(before_for)
                    .trim()
                    .split('<')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();

                let type_name = after_for
                    .split('{')
                    .next()
                    .unwrap_or(after_for)
                    .split('<')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();

                if !trait_name.is_empty() && !type_name.is_empty() {
                    impls.push(ImplRelation {
                        trait_name,
                        type_name,
                        file: file_path.to_string(),
                        line: child.start_position().row + 1,
                    });
                }
            }
        }
        collect_rust_impls(child, source, file_path, impls);
    }
}

// ─── Import / Use Statement Extraction ──────────────────────────────────────

/// An import/use statement extracted from source code.
#[derive(Debug, Clone)]
pub struct ImportStatement {
    /// The full import path (e.g., "std::collections::HashMap", "os.path")
    pub path: String,
    /// The imported name(s) — the leaf items being imported
    pub names: Vec<String>,
    /// Line number (1-indexed)
    pub line: usize,
    /// Whether it's a wildcard import (e.g., `use foo::*`, `from foo import *`)
    pub is_wildcard: bool,
}

/// Extract import/use statements from source code.
pub fn extract_imports(source: &str, lang: Language) -> Vec<ImportStatement> {
    let tree = match cached_parse(source, lang) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let root = tree.root_node();
    match lang {
        Language::Rust => extract_rust_imports(root, source),
        Language::Python => extract_python_imports(root, source),
        Language::TypeScript | Language::JavaScript => extract_ts_imports(root, source),
        Language::Go => extract_go_imports(root, source),
        _ => Vec::new(), // Java, C, Cpp, Ruby: not yet implemented
    }
}

fn extract_rust_imports(node: tree_sitter::Node, source: &str) -> Vec<ImportStatement> {
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "use_declaration" {
            let line = child.start_position().row + 1;
            let text = child.utf8_text(source.as_bytes()).unwrap_or_default();
            // Remove `use ` prefix and `;` suffix
            let path_str = text
                .trim_start_matches("use ")
                .trim_start_matches("pub use ")
                .trim_end_matches(';')
                .trim();

            let is_wildcard = path_str.ends_with("::*");

            // Extract names from use tree
            let names = extract_rust_use_names(path_str);
            let path = path_str
                .split("::{")
                .next()
                .unwrap_or(path_str)
                .trim_end_matches("::*")
                .to_string();

            imports.push(ImportStatement {
                path,
                names,
                line,
                is_wildcard,
            });
        }
    }
    imports
}

fn extract_rust_use_names(path: &str) -> Vec<String> {
    // Handle `use std::collections::{HashMap, HashSet};`
    if let Some(brace_start) = path.find("::{") {
        let names_str = &path[brace_start + 3..];
        let names_str = names_str.trim_end_matches('}');
        return names_str
            .split(',')
            .map(|n| {
                let n = n.trim();
                // Handle `self` and `as Alias`
                n.split(" as ").next().unwrap_or(n).trim().to_string()
            })
            .filter(|n| !n.is_empty())
            .collect();
    }
    // Handle `use std::collections::HashMap;`
    if let Some(last) = path.rsplit("::").next() {
        if last != "*" {
            return vec![last.split(" as ").next().unwrap_or(last).trim().to_string()];
        }
    }
    Vec::new()
}

fn extract_python_imports(node: tree_sitter::Node, source: &str) -> Vec<ImportStatement> {
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_statement" => {
                // `import os.path`
                let line = child.start_position().row + 1;
                let text = child.utf8_text(source.as_bytes()).unwrap_or_default();
                let path = text.trim_start_matches("import ").trim();
                let names: Vec<String> = path
                    .split(',')
                    .map(|p| {
                        let p = p.trim();
                        p.split(" as ")
                            .next()
                            .unwrap_or(p)
                            .trim()
                            .rsplit('.')
                            .next()
                            .unwrap_or(p)
                            .to_string()
                    })
                    .collect();
                imports.push(ImportStatement {
                    path: path
                        .split(',')
                        .next()
                        .unwrap_or(path)
                        .split(" as ")
                        .next()
                        .unwrap_or(path)
                        .trim()
                        .to_string(),
                    names,
                    line,
                    is_wildcard: false,
                });
            }
            "import_from_statement" => {
                // `from os.path import join, exists`
                let line = child.start_position().row + 1;
                let text = child.utf8_text(source.as_bytes()).unwrap_or_default();
                let is_wildcard = text.contains("import *");

                // Parse "from MODULE import NAMES"
                let after_from = text.trim_start_matches("from ").trim();
                let parts: Vec<&str> = after_from.splitn(2, " import ").collect();
                let module = parts.first().map(|s| s.trim()).unwrap_or("").to_string();
                let names = if parts.len() > 1 {
                    parts[1]
                        .split(',')
                        .map(|n| {
                            n.trim()
                                .split(" as ")
                                .next()
                                .unwrap_or("")
                                .trim()
                                .to_string()
                        })
                        .filter(|n| !n.is_empty() && n != "*")
                        .collect()
                } else {
                    Vec::new()
                };

                imports.push(ImportStatement {
                    path: module,
                    names,
                    line,
                    is_wildcard,
                });
            }
            _ => {}
        }
    }
    imports
}

fn extract_ts_imports(node: tree_sitter::Node, source: &str) -> Vec<ImportStatement> {
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "import_statement" {
            let line = child.start_position().row + 1;
            let text = child.utf8_text(source.as_bytes()).unwrap_or_default();

            // Extract module path from string literal
            let path = text
                .split('\'')
                .nth(1)
                .or_else(|| text.split('"').nth(1))
                .unwrap_or("")
                .to_string();

            // Extract imported names
            let mut names = Vec::new();
            let is_wildcard = text.contains("* as ");

            if let Some(brace_start) = text.find('{') {
                if let Some(brace_end) = text.find('}') {
                    let inner = &text[brace_start + 1..brace_end];
                    for name in inner.split(',') {
                        let n = name.trim().split(" as ").next().unwrap_or("").trim();
                        if !n.is_empty() {
                            names.push(n.to_string());
                        }
                    }
                }
            }
            // Default import: `import Foo from '...'`
            if names.is_empty() && !is_wildcard {
                let after_import = text.trim_start_matches("import ").trim();
                if let Some(name) = after_import.split_whitespace().next() {
                    if name != "{" && name != "*" && name != "type" {
                        names.push(name.to_string());
                    }
                }
            }

            imports.push(ImportStatement {
                path,
                names,
                line,
                is_wildcard,
            });
        }
    }
    imports
}

fn extract_go_imports(node: tree_sitter::Node, source: &str) -> Vec<ImportStatement> {
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "import_declaration" {
            let line = child.start_position().row + 1;
            // Can be single: `import "fmt"` or grouped: `import ("fmt" \n "os")`
            let text = child.utf8_text(source.as_bytes()).unwrap_or_default();

            for import_line in text.lines() {
                let trimmed = import_line.trim();
                if trimmed == "import"
                    || trimmed == "import ("
                    || trimmed == ")"
                    || trimmed.is_empty()
                {
                    continue;
                }
                let path_str = trimmed
                    .trim_start_matches("import ")
                    .trim()
                    .trim_matches('"')
                    .trim_matches('(')
                    .trim_matches(')')
                    .trim()
                    .trim_matches('"');

                if path_str.is_empty() {
                    continue;
                }

                let name = path_str.rsplit('/').next().unwrap_or(path_str).to_string();
                imports.push(ImportStatement {
                    path: path_str.to_string(),
                    names: vec![name],
                    line,
                    is_wildcard: false,
                });
            }
        }
    }
    imports
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
        assert_eq!(detect_language(Path::new("app.py")), Some(Language::Python));
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

    // ─── New language tests ──────────────────────────────────────────────────

    #[test]
    fn detect_java_language() {
        assert_eq!(
            detect_language(Path::new("Main.java")),
            Some(Language::Java)
        );
    }

    #[test]
    fn detect_cpp_language() {
        assert_eq!(detect_language(Path::new("main.cpp")), Some(Language::Cpp));
        assert_eq!(detect_language(Path::new("main.cc")), Some(Language::Cpp));
        assert_eq!(
            detect_language(Path::new("header.hpp")),
            Some(Language::Cpp)
        );
    }

    #[test]
    fn detect_c_language() {
        assert_eq!(detect_language(Path::new("main.c")), Some(Language::C));
        assert_eq!(detect_language(Path::new("header.h")), Some(Language::C));
    }

    #[test]
    fn detect_ruby_language() {
        assert_eq!(detect_language(Path::new("app.rb")), Some(Language::Ruby));
    }

    #[test]
    fn extract_java_class_and_methods() {
        let source = r#"
public class UserService {
    private String name;

    public void setName(String name) {
        this.name = name;
    }

    public String getName() {
        return this.name;
    }
}
"#;
        let symbols = extract_symbols(source, Language::Java);

        let class_sym = symbols.iter().find(|s| s.name == "UserService");
        assert!(class_sym.is_some(), "symbols: {:?}", symbols);
        assert_eq!(class_sym.unwrap().kind, SymbolKind::Class);

        let set_method = symbols.iter().find(|s| s.name == "setName");
        assert!(set_method.is_some(), "symbols: {:?}", symbols);
        assert_eq!(set_method.unwrap().kind, SymbolKind::Method);

        let get_method = symbols.iter().find(|s| s.name == "getName");
        assert!(get_method.is_some(), "symbols: {:?}", symbols);
    }

    #[test]
    fn extract_cpp_functions_and_classes() {
        let source = r#"
#include <iostream>

class Calculator {
public:
    int add(int a, int b) {
        return a + b;
    }
};

int main() {
    Calculator calc;
    return 0;
}
"#;
        let symbols = extract_symbols(source, Language::Cpp);

        let class_sym = symbols.iter().find(|s| s.name == "Calculator");
        assert!(class_sym.is_some(), "symbols: {:?}", symbols);
        assert_eq!(class_sym.unwrap().kind, SymbolKind::Class);

        let main_sym = symbols.iter().find(|s| s.name == "main");
        assert!(main_sym.is_some(), "symbols: {:?}", symbols);
        assert_eq!(main_sym.unwrap().kind, SymbolKind::Function);
    }

    #[test]
    fn extract_ruby_class_and_methods() {
        let source = r#"
class UserController
  def index
    @users = User.all
  end

  def show
    @user = User.find(params[:id])
  end
end
"#;
        let symbols = extract_symbols(source, Language::Ruby);

        let class_sym = symbols.iter().find(|s| s.name == "UserController");
        assert!(class_sym.is_some(), "symbols: {:?}", symbols);
        assert_eq!(class_sym.unwrap().kind, SymbolKind::Class);

        let index_method = symbols.iter().find(|s| s.name == "index");
        assert!(index_method.is_some(), "symbols: {:?}", symbols);
        assert_eq!(index_method.unwrap().kind, SymbolKind::Method);

        let show_method = symbols.iter().find(|s| s.name == "show");
        assert!(show_method.is_some(), "symbols: {:?}", symbols);
    }

    // ─── Call graph extraction tests ────────────────────────────────────

    #[test]
    fn extract_calls_rust_function() {
        let source = r#"
fn process(items: &[Item]) -> Result<()> {
    let config = Config::load("default")?;
    let mut results = Vec::new();
    for item in items {
        let val = item.transform();
        results.push(val);
    }
    println!("done: {}", results.len());
    save_results(&results)?;
    Ok(())
}
"#;
        let calls = extract_calls(source, Language::Rust, 2, 12);
        assert!(!calls.is_empty(), "should find calls");

        let names: Vec<&str> = calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(
            names.contains(&"Config::load"),
            "should find Config::load: {:?}",
            names
        );
        assert!(
            names.contains(&"transform"),
            "should find item.transform: {:?}",
            names
        );
        assert!(
            names.contains(&"push"),
            "should find results.push: {:?}",
            names
        );
        assert!(
            names.contains(&"println!"),
            "should find println!: {:?}",
            names
        );
        assert!(
            names.contains(&"save_results"),
            "should find save_results: {:?}",
            names
        );

        // Check receiver on method calls
        let transform_call = calls.iter().find(|c| c.callee == "transform").unwrap();
        assert_eq!(transform_call.receiver.as_deref(), Some("item"));
    }

    #[test]
    fn extract_calls_python() {
        let source = r#"
def process_data(df):
    result = df.groupby("category").agg({"value": "sum"})
    print(f"Groups: {len(result)}")
    save_to_csv(result, "output.csv")
    return result
"#;
        let calls = extract_calls(source, Language::Python, 2, 6);
        let names: Vec<&str> = calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(
            names.contains(&"groupby"),
            "should find df.groupby: {:?}",
            names
        );
        assert!(names.contains(&"print"), "should find print: {:?}", names);
        assert!(
            names.contains(&"save_to_csv"),
            "should find save_to_csv: {:?}",
            names
        );
    }

    #[test]
    fn extract_calls_typescript() {
        let source = r#"
function handleRequest(req: Request): Response {
    const user = authenticateUser(req.headers);
    const data = db.query("SELECT * FROM users");
    console.log("processed", user.id);
    return new Response(JSON.stringify(data));
}
"#;
        let calls = extract_calls(source, Language::TypeScript, 2, 7);
        let names: Vec<&str> = calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(
            names.contains(&"authenticateUser"),
            "should find authenticateUser: {:?}",
            names
        );
        assert!(
            names.contains(&"query"),
            "should find db.query: {:?}",
            names
        );
        assert!(
            names.contains(&"log"),
            "should find console.log: {:?}",
            names
        );
    }

    #[test]
    fn extract_calls_go() {
        let source = r#"
func handleRequest(w http.ResponseWriter, r *http.Request) {
    body, err := ioutil.ReadAll(r.Body)
    if err != nil {
        http.Error(w, "bad request", 400)
        return
    }
    fmt.Fprintf(w, "OK: %d bytes", len(body))
}
"#;
        let calls = extract_calls(source, Language::Go, 2, 9);
        let names: Vec<&str> = calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(
            names.contains(&"ReadAll"),
            "should find ioutil.ReadAll: {:?}",
            names
        );
        assert!(
            names.contains(&"Error"),
            "should find http.Error: {:?}",
            names
        );
        assert!(
            names.contains(&"Fprintf"),
            "should find fmt.Fprintf: {:?}",
            names
        );
    }

    #[test]
    fn extract_calls_empty_function() {
        let source = "fn noop() {}\n";
        let calls = extract_calls(source, Language::Rust, 1, 1);
        assert!(calls.is_empty(), "should find no calls in empty function");
    }

    #[test]
    fn extract_calls_respects_line_range() {
        let source = r#"
fn first() {
    a();
}
fn second() {
    b();
}
"#;
        // Only analyze first function (lines 2-4)
        let calls = extract_calls(source, Language::Rust, 2, 4);
        let names: Vec<&str> = calls.iter().map(|c| c.callee.as_str()).collect();
        assert!(names.contains(&"a"), "should find a(): {:?}", names);
        assert!(!names.contains(&"b"), "should NOT find b(): {:?}", names);
    }

    // ─── Scope context tests ────────────────────────────────────────────

    #[test]
    fn scope_at_line_rust_nested() {
        let source = r#"
mod utils {
    pub struct Config {
        pub name: String,
    }

    impl Config {
        pub fn new(name: &str) -> Self {
            Config { name: name.to_string() }
        }
    }
}
"#;
        // Line 9 is inside Config::new, inside impl Config, inside mod utils
        let scope = scope_at_line(source, Language::Rust, 9);
        assert!(
            !scope.breadcrumbs.is_empty(),
            "should have scope breadcrumbs"
        );
        assert!(scope.symbol.is_some(), "should have innermost symbol");
        let inner = scope.symbol.unwrap();
        assert_eq!(inner.name, "new");
    }

    #[test]
    fn scope_at_line_outside_any_symbol() {
        let source = "// just a comment\nlet x = 1;\n";
        let scope = scope_at_line(source, Language::Rust, 1);
        assert!(
            scope.breadcrumbs.is_empty(),
            "should be empty for comment-only line"
        );
        assert!(scope.symbol.is_none());
    }

    #[test]
    fn scope_at_line_python_class_method() {
        let source = r#"
class UserService:
    def __init__(self, db):
        self.db = db

    def get_user(self, user_id):
        return self.db.query(user_id)
"#;
        let scope = scope_at_line(source, Language::Python, 7);
        assert!(scope.symbol.is_some());
        let sym = scope.symbol.unwrap();
        assert_eq!(sym.name, "get_user");
        // Breadcrumbs should include the class
        let crumbs_str = scope.breadcrumbs.join(" > ");
        assert!(
            crumbs_str.contains("UserService"),
            "breadcrumbs: {crumbs_str}"
        );
    }

    // ── extract_members tests ────────────────────────────────────────────────

    #[test]
    fn extract_members_rust_struct_fields() {
        let source = "pub struct Config {\n    pub name: String,\n    port: u16,\n}\n";
        let members = extract_members(source, Language::Rust, 1);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].name, "name");
        assert_eq!(members[0].kind, "field");
        assert!(members[0].visibility.contains("pub"));
        assert_eq!(members[1].name, "port");
        assert!(members[1].visibility.is_empty());
    }

    #[test]
    fn extract_members_rust_enum_variants() {
        let source = "enum Direction {\n    North,\n    South,\n    East,\n    West,\n}\n";
        let members = extract_members(source, Language::Rust, 1);
        assert_eq!(members.len(), 4);
        assert!(members.iter().all(|m| m.kind == "variant"));
        assert_eq!(members[0].name, "North");
        assert_eq!(members[3].name, "West");
    }

    #[test]
    fn extract_members_python_class() {
        let source =
            "class User:\n    name: str\n    age: int = 25\n    def greet(self):\n        pass\n";
        let members = extract_members(source, Language::Python, 1);
        assert!(
            members.len() >= 2,
            "should have at least name and greet: {:?}",
            members
        );
        let props: Vec<_> = members.iter().filter(|m| m.kind == "property").collect();
        assert!(!props.is_empty(), "should have properties");
        let methods: Vec<_> = members.iter().filter(|m| m.kind == "method").collect();
        assert!(!methods.is_empty(), "should have methods");
    }

    #[test]
    fn extract_members_empty_for_non_type() {
        let source = "fn main() {\n    println!(\"hello\");\n}\n";
        let members = extract_members(source, Language::Rust, 1);
        assert!(members.is_empty());
    }

    #[test]
    fn find_rust_impls_basic() {
        let source = "trait A {}\nstruct B;\nimpl A for B {}\n";
        let impls = find_rust_impls(source, "test.rs");
        assert_eq!(impls.len(), 1);
        assert_eq!(impls[0].trait_name, "A");
        assert_eq!(impls[0].type_name, "B");
        assert_eq!(impls[0].file, "test.rs");
    }

    #[test]
    fn find_rust_impls_skips_inherent() {
        let source = "struct Foo;\nimpl Foo {\n    fn new() -> Self { Self }\n}\n";
        let impls = find_rust_impls(source, "test.rs");
        assert!(impls.is_empty(), "inherent impls should not be listed");
    }

    #[test]
    fn find_rust_impls_multiple() {
        let source = r#"trait X {}
trait Y {}
struct Z;
impl X for Z {}
impl Y for Z {}
"#;
        let impls = find_rust_impls(source, "multi.rs");
        assert_eq!(impls.len(), 2);
    }

    // ─── Parser cache tests ─────────────────────────────────────────────

    #[test]
    fn cached_parse_returns_valid_tree() {
        let source = "fn main() {}\n";
        let tree = cached_parse(source, Language::Rust);
        assert!(tree.is_some(), "should parse simple Rust");
        let tree = tree.unwrap();
        let root = tree.root_node();
        assert_eq!(root.kind(), "source_file");
    }

    #[test]
    fn cached_parse_works_across_languages() {
        // Parse with different languages sequentially
        let rust_tree = cached_parse("fn main() {}\n", Language::Rust);
        assert!(rust_tree.is_some());

        let py_tree = cached_parse("def main(): pass\n", Language::Python);
        assert!(py_tree.is_some());

        let ts_tree = cached_parse("function main() {}\n", Language::TypeScript);
        assert!(ts_tree.is_some());

        let go_tree = cached_parse("package main\nfunc main() {}\n", Language::Go);
        assert!(go_tree.is_some());

        // Re-parse Rust to verify cache still works
        let rust_tree2 = cached_parse("struct Foo {}\n", Language::Rust);
        assert!(rust_tree2.is_some());
    }

    #[test]
    fn cached_parse_handles_invalid_source_gracefully() {
        // Empty source should still parse (tree-sitter is lenient)
        let tree = cached_parse("", Language::Rust);
        assert!(tree.is_some());
    }

    // ─── Import extraction tests ────────────────────────────────────────

    #[test]
    fn extract_rust_imports_simple() {
        let source = r#"
use std::collections::HashMap;
use std::io::{self, Read, Write};
use crate::utils::*;
pub use super::config::Config;
"#;
        let imports = extract_imports(source, Language::Rust);
        assert!(imports.len() >= 4, "found: {:?}", imports);

        let hashmap = imports
            .iter()
            .find(|i| i.names.contains(&"HashMap".to_string()));
        assert!(hashmap.is_some(), "should find HashMap import");
        assert_eq!(hashmap.unwrap().path, "std::collections::HashMap");

        let io = imports.iter().find(|i| i.path.contains("std::io"));
        assert!(io.is_some(), "should find io import");
        assert!(io.unwrap().names.contains(&"Read".to_string()));
        assert!(io.unwrap().names.contains(&"Write".to_string()));

        let wildcard = imports.iter().find(|i| i.is_wildcard);
        assert!(wildcard.is_some(), "should find wildcard import");
    }

    #[test]
    fn extract_python_imports_basic() {
        let source = r#"
import os
import os.path
from collections import OrderedDict, defaultdict
from typing import List, Optional
from module import *
"#;
        let imports = extract_imports(source, Language::Python);
        assert!(imports.len() >= 4, "found: {:?}", imports);

        let os_import = imports.iter().find(|i| i.path == "os");
        assert!(os_import.is_some(), "should find 'import os'");

        let collections = imports.iter().find(|i| i.path == "collections");
        assert!(collections.is_some(), "should find collections import");
        assert!(collections
            .unwrap()
            .names
            .contains(&"OrderedDict".to_string()));

        let wildcard = imports.iter().find(|i| i.is_wildcard);
        assert!(wildcard.is_some(), "should find wildcard import");
    }

    #[test]
    fn extract_typescript_imports_basic() {
        let source = r#"
import React from 'react';
import { useState, useEffect } from 'react';
import * as path from 'path';
import type { Config } from './config';
"#;
        let imports = extract_imports(source, Language::TypeScript);
        assert!(imports.len() >= 3, "found: {:?}", imports);

        let react = imports.iter().find(|i| {
            i.path == "react" && i.names.contains(&"React".to_string())
        });
        assert!(react.is_some(), "should find default React import");

        let hooks = imports.iter().find(|i| {
            i.path == "react" && i.names.contains(&"useState".to_string())
        });
        assert!(hooks.is_some(), "should find named React imports");

        let wildcard = imports.iter().find(|i| i.is_wildcard);
        assert!(wildcard.is_some(), "should find wildcard import");
    }

    #[test]
    fn extract_go_imports_basic() {
        let source = r#"
package main

import (
    "fmt"
    "os"
    "path/filepath"
)
"#;
        let imports = extract_imports(source, Language::Go);
        assert!(imports.len() >= 3, "found: {:?}", imports);

        let fmt_import = imports.iter().find(|i| i.path == "fmt");
        assert!(fmt_import.is_some(), "should find fmt import");
        assert!(fmt_import.unwrap().names.contains(&"fmt".to_string()));

        let filepath = imports.iter().find(|i| i.path == "path/filepath");
        assert!(filepath.is_some(), "should find path/filepath import");
        assert!(filepath
            .unwrap()
            .names
            .contains(&"filepath".to_string()));
    }

    #[test]
    fn extract_imports_unsupported_language_returns_empty() {
        let source = "class Foo { void bar() {} }";
        let imports = extract_imports(source, Language::Java);
        assert!(imports.is_empty());
    }

    #[test]
    fn extract_imports_empty_source() {
        let imports = extract_imports("", Language::Rust);
        assert!(imports.is_empty());
    }
}
