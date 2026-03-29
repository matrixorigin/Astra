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
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::C | Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
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
pub fn extract_calls(source: &str, lang: Language, start_line: usize, end_line: usize) -> Vec<CallSite> {
    let mut parser = tree_sitter::Parser::new();
    let language = match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::TypeScript | Language::JavaScript => {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::C | Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
    };
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut calls = Vec::new();
    collect_calls(tree.root_node(), source, start_line, end_line, lang, &mut calls);
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
        Language::TypeScript | Language::JavaScript => kind == "call_expression" || kind == "new_expression",
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
        assert_eq!(
            detect_language(Path::new("main.cpp")),
            Some(Language::Cpp)
        );
        assert_eq!(
            detect_language(Path::new("main.cc")),
            Some(Language::Cpp)
        );
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
        assert_eq!(
            detect_language(Path::new("app.rb")),
            Some(Language::Ruby)
        );
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
        assert!(names.contains(&"Config::load"), "should find Config::load: {:?}", names);
        assert!(names.contains(&"transform"), "should find item.transform: {:?}", names);
        assert!(names.contains(&"push"), "should find results.push: {:?}", names);
        assert!(names.contains(&"println!"), "should find println!: {:?}", names);
        assert!(names.contains(&"save_results"), "should find save_results: {:?}", names);

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
        assert!(names.contains(&"groupby"), "should find df.groupby: {:?}", names);
        assert!(names.contains(&"print"), "should find print: {:?}", names);
        assert!(names.contains(&"save_to_csv"), "should find save_to_csv: {:?}", names);
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
        assert!(names.contains(&"authenticateUser"), "should find authenticateUser: {:?}", names);
        assert!(names.contains(&"query"), "should find db.query: {:?}", names);
        assert!(names.contains(&"log"), "should find console.log: {:?}", names);
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
        assert!(names.contains(&"ReadAll"), "should find ioutil.ReadAll: {:?}", names);
        assert!(names.contains(&"Error"), "should find http.Error: {:?}", names);
        assert!(names.contains(&"Fprintf"), "should find fmt.Fprintf: {:?}", names);
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
        assert!(!scope.breadcrumbs.is_empty(), "should have scope breadcrumbs");
        assert!(scope.symbol.is_some(), "should have innermost symbol");
        let inner = scope.symbol.unwrap();
        assert_eq!(inner.name, "new");
    }

    #[test]
    fn scope_at_line_outside_any_symbol() {
        let source = "// just a comment\nlet x = 1;\n";
        let scope = scope_at_line(source, Language::Rust, 1);
        assert!(scope.breadcrumbs.is_empty(), "should be empty for comment-only line");
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
        assert!(crumbs_str.contains("UserService"), "breadcrumbs: {crumbs_str}");
    }
}
