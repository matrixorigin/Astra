use tree_sitter::{Node, Parser, Tree};

use super::command::CommandRisk;

/// Parse a bash command string into a tree-sitter AST.
pub fn parse_bash(command: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_bash::language()).ok()?;
    parser.parse(command, None)
}

/// AST-level bash risk analysis.
///
/// This is intentionally conservative: it focuses on high-signal primitives
/// (pipelines, command substitutions, redirections, privilege escalation, network tools)
/// and avoids false positives from string literals.
pub fn analyze_bash_risks_ast(command: &str) -> Vec<CommandRisk> {
    let Some(tree) = parse_bash(command) else {
        return vec![];
    };
    let root = tree.root_node();
    let mut ctx = RiskCtx::new(command);
    visit_node(root, &mut ctx);
    ctx.into_risks()
}

struct RiskCtx<'a> {
    src: &'a str,
    hits: Vec<CommandRisk>,
}

impl<'a> RiskCtx<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, hits: vec![] }
    }

    fn push(&mut self, risk: CommandRisk) {
        if !self.hits.contains(&risk) {
            self.hits.push(risk);
        }
    }

    fn into_risks(self) -> Vec<CommandRisk> {
        self.hits
    }

    fn text(&self, n: Node<'_>) -> &'a str {
        n.utf8_text(self.src.as_bytes()).unwrap_or("")
    }
}

fn visit_node(node: Node<'_>, ctx: &mut RiskCtx<'_>) {
    // High-signal nodes we can reason about structurally.
    match node.kind() {
        // `$()` and legacy backticks.
        "command_substitution" | "old_command_substitution" => {
            ctx.push(CommandRisk::CommandSubstitution);
        }
        // `<(cmd)` / `>(cmd)`
        "process_substitution" => {
            ctx.push(CommandRisk::ProcessSubstitution);
        }
        // `|` pipeline
        "pipeline" => {
            analyze_pipeline(node, ctx);
        }
        // Any form of redirection (`>`, `>>`, `<`, `2>`, `<<EOF`, etc.)
        "redirected_statement" | "redirection" | "herestring_redirect" | "heredoc_redirect" => {
            analyze_redirection(node, ctx);
        }
        // A command invocation (simple_command includes assignments + command name).
        "command" | "simple_command" => {
            analyze_command_invocation(node, ctx);
        }
        _ => {}
    }

    // Recurse.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit_node(child, ctx);
    }
}

fn analyze_pipeline(node: Node<'_>, ctx: &mut RiskCtx<'_>) {
    // Heuristic: detect `curl|wget ... | sh|bash|zsh` without matching strings.
    // tree-sitter-bash represents pipelines as a sequence of commands.
    let mut cursor = node.walk();
    let mut commands = Vec::new();
    for child in node.children(&mut cursor) {
        if child.kind() == "command" || child.kind() == "simple_command" {
            if let Some(name) = command_name(child, ctx) {
                commands.push((name, child));
            }
        }
    }
    if commands.is_empty() {
        return;
    }

    let has_network = commands
        .iter()
        .any(|(n, _)| matches!(n.as_str(), "curl" | "wget" | "nc" | "ncat" | "netcat"));
    if has_network {
        ctx.push(CommandRisk::NetworkAccess);
    }

    // `curl ... | bash` / `wget ... | sh`
    let last = commands.last().map(|(n, _)| n.as_str()).unwrap_or("");
    if has_network && matches!(last, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh") {
        ctx.push(CommandRisk::RemoteCodeExecution);
    }
}

fn analyze_redirection(node: Node<'_>, ctx: &mut RiskCtx<'_>) {
    // We look for any redirection target that clearly escapes boundaries or hits sensitive paths.
    // Note: This is advisory; actual path access is enforced elsewhere.
    let txt = ctx.text(node).to_lowercase();
    if txt.contains("../") || txt.contains("..\\") {
        ctx.push(CommandRisk::PathTraversal);
    }
    for sensitive in &["/etc/", "/root/", "/var/log/", "/proc/", "/sys/"] {
        if txt.contains(sensitive) {
            ctx.push(CommandRisk::SensitivePathAccess(sensitive.to_string()));
            break;
        }
    }
    // Any `>` / `>>` / `2>` is a write primitive; mark it.
    if txt.contains('>') {
        ctx.push(CommandRisk::OutputRedirection);
    }
}

fn analyze_command_invocation(node: Node<'_>, ctx: &mut RiskCtx<'_>) {
    let Some(name) = command_name(node, ctx) else {
        return;
    };
    let lower = name.to_ascii_lowercase();

    // Privilege escalation primitives
    if matches!(lower.as_str(), "sudo" | "su" | "doas") {
        ctx.push(CommandRisk::PrivilegeEscalation);
    }

    // Network primitives (also caught via pipeline)
    if matches!(
        lower.as_str(),
        "curl" | "wget" | "nc" | "ncat" | "netcat" | "ssh" | "scp"
    ) {
        ctx.push(CommandRisk::NetworkAccess);
    }

    // Environment manipulation (export PATH/LD_*)
    if lower == "export" {
        let txt = ctx.text(node).to_lowercase();
        if txt.contains("path=") || txt.contains("ld_") {
            ctx.push(CommandRisk::EnvManipulation);
        }
    }

    // Process control
    if matches!(lower.as_str(), "kill" | "pkill" | "killall") {
        ctx.push(CommandRisk::ProcessControl);
    }

    // `eval ...` is a code-injection surface (esp. with substitutions)
    if lower == "eval" {
        ctx.push(CommandRisk::Eval);
    }
}

fn command_name(node: Node<'_>, ctx: &RiskCtx<'_>) -> Option<String> {
    // For both `command` and `simple_command`, the "name" is typically the first "word".
    // We intentionally ignore assignments (FOO=bar) by taking the first word that
    // looks like a bare identifier.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "word" {
            let w = ctx.text(child).trim();
            if w.is_empty() {
                continue;
            }
            // Strip leading command-prefixes like `command`, `builtin`? (rare)
            return Some(w.to_string());
        }
    }
    None
}

