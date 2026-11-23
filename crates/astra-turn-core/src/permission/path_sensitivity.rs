//! Central path-sensitivity model for permission decisions.
//!
//! This module is the single policy boundary for deciding whether a path-like
//! token should trip the sensitive-path gate. Display redaction still lives in
//! `permission::redact`; call sites should not manually combine redaction,
//! sandbox, and internal-artifact rules.
//!
//! Shell-command scanning here is intentionally narrow: it only extracts
//! path-like operands that a command is about to read or write, then delegates
//! the actual sensitivity decision back to this module. Sandbox boundary
//! enforcement still owns full command validation.

use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
};

use astra_sandbox::{
    InternalPathKind, is_dangerous_file_path, is_sensitive_directory, is_sensitive_path,
};
use astra_services::SessionArtifactStore;
use serde_json::Value;

use crate::parallel_tool_exec::is_read_only_tool_with_args;
use crate::tool_argument_hints::{command_hint_from_args, path_hint_from_args};

use super::redact::matches_sensitive_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSensitivity {
    Normal,
    /// Reading this path can expose credentials or system-sensitive state.
    Sensitive,
    /// Mutating this path can alter persistent shell/git/editor/agent state,
    /// but read-only inspection is allowed unless the path is also sensitive.
    WriteSensitive,
    InternalArtifactReadOnly(InternalPathKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    List,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitivePathMatch {
    pub token: String,
    pub sensitivity: PathSensitivity,
    pub access: PathAccess,
    /// The preflight matched a rule without determining the operand's effect
    /// or the paths it can select.
    inferred: bool,
}

impl SensitivePathMatch {
    fn with_inferred_access(mut self, inferred: bool) -> Self {
        self.inferred |= inferred;
        self
    }

    /// Determine how precisely to describe a rejected Bash operand. The
    /// fallback can reject an expression without proving its shell role.
    fn explicit_shell_access(
        &self,
        command: &str,
        context: &mut PathSensitivityContext,
    ) -> Option<PathAccess> {
        if !self.inferred {
            return Some(self.access);
        }
        if self.access == PathAccess::Write {
            return astra_sandbox::has_literal_bash_write_redirect(command, &self.token)
                .then_some(PathAccess::Write);
        }
        let paths = astra_sandbox::parse_bash_literal_paths(command)?;
        if self.access == PathAccess::Read && paths.read_redirects.contains(&self.token) {
            return Some(PathAccess::Read);
        }
        paths.commands.iter().find_map(|argv| {
            let hit = read_only_argv_path_match(argv, context)??;
            (!hit.inferred && hit.token == self.token && hit.sensitivity == self.sensitivity)
                .then_some(hit.access)
        })
    }
}

/// Model-facing explanation for a local Bash protected-path rejection.
pub fn bash_protected_path_denial(command: &str) -> Option<String> {
    bash_protected_path_denial_in_context(command, &mut PathSensitivityContext::new(None))
}

/// Assess a shell command relative to the directory in which it will run.
pub fn bash_protected_path_denial_in_directory(command: &str, directory: &Path) -> Option<String> {
    let mut context = PathSensitivityContext::new(Some(directory));
    context.deny_tool_result_reads = true;
    bash_protected_path_denial_in_context(command, &mut context)
}

/// Enforce the tool-result reader boundary alongside an existing workspace sandbox.
pub fn bash_tool_result_denial_in_directory(command: &str, directory: &Path) -> Option<String> {
    let mut context = PathSensitivityContext::new(Some(directory));
    context.deny_tool_result_reads = true;
    context.ignore_write_sensitive = true;
    let hit = sensitive_path_match_for_shell_command_in_context(command, &mut context)?;
    (hit.sensitivity
        == PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult))
    .then(|| describe_bash_path_denial(command, &mut context, &hit))
    .flatten()
}

/// Identify a literal credential or system access that the Runner will reject,
/// without consulting the Server filesystem or guessing dynamic shell effects.
pub fn bash_known_forbidden_path_denial(command: &str, directory: &Path) -> Option<String> {
    let mut context = PathSensitivityContext::lexical(directory);
    let hit = sensitive_path_match_for_shell_command_in_context(command, &mut context)?;
    if hit.sensitivity != PathSensitivity::Sensitive
        || hit.explicit_shell_access(command, &mut context).is_none()
    {
        return None;
    }
    bash_protected_path_denial_in_context(command, &mut context)
}

/// Assess known shell operands and declared sources on the execution host.
pub fn bash_path_preflight(
    request: astra_tools::shell_ops::BashPreflightRequest<'_>,
) -> Option<String> {
    use astra_tools::shell_ops::BashPreflightRequest;

    match request {
        BashPreflightRequest::Command {
            command,
            directory,
            workspace_root,
            source_artifacts,
            workspace_write_admitted,
        } => {
            let mut context = PathSensitivityContext::new(Some(directory));
            context.deny_tool_result_reads = true;
            if workspace_write_admitted {
                context.approved_workspace = workspace_root.canonicalize().ok();
                context.approved_relative_writes =
                    astra_sandbox::bash_has_stable_working_directory(command);
            }
            bash_protected_path_denial_in_context(command, &mut context).or_else(|| {
                source_artifacts.and_then(|paths| source_artifact_denial(workspace_root, paths))
            })
        }
        BashPreflightRequest::SourceArtifacts {
            workspace_root,
            paths,
        } => source_artifact_denial(workspace_root, paths),
    }
}

fn source_artifact_denial(workspace_root: &Path, paths: &[String]) -> Option<String> {
    let mut context = PathSensitivityContext::new(Some(workspace_root));
    context.deny_tool_result_reads = true;
    for path in paths {
        if context.requires_gate(path, PathAccess::Read).is_some() {
            let token =
                astra_text_utils::credential_redaction::redact_credentials_for_display_bounded(
                    path, 1024,
                )
                .map(|(safe, _)| safe)
                .unwrap_or_else(|_| "<path expression omitted>".into());
            return Some(format!(
                "Sandbox: Source-artifact preflight blocked protected path {token:?}. The command was not run."
            ));
        }
    }
    None
}

fn bash_protected_path_denial_in_context(
    command: &str,
    context: &mut PathSensitivityContext,
) -> Option<String> {
    let hit = sensitive_path_match_for_shell_command_in_context(command, context)?;
    describe_bash_path_denial(command, context, &hit)
}

fn describe_bash_path_denial(
    command: &str,
    context: &mut PathSensitivityContext,
    hit: &SensitivePathMatch,
) -> Option<String> {
    let token = astra_text_utils::credential_redaction::redact_credentials_for_display_bounded(
        &hit.token, 1024,
    )
    .map(|(safe, _)| safe)
    .unwrap_or_else(|_| "<path expression omitted>".into());
    let rule = match hit.sensitivity {
        PathSensitivity::Sensitive => "a sensitive credential or system path",
        PathSensitivity::WriteSensitive => "write-sensitive app/runtime state",
        PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult) => {
            "an owned tool-result artifact. Use `introspect` with its artifact handle"
        }
        PathSensitivity::InternalArtifactReadOnly(_) => {
            "an internal runtime artifact path. Use `read_file` to read session artifacts"
        }
        PathSensitivity::Normal => return None,
    };
    let operand = match hit.explicit_shell_access(command, context) {
        Some(PathAccess::Read) => "the read operand",
        Some(PathAccess::List) => "the list operand",
        Some(PathAccess::Write) => "the write operand",
        None => {
            return Some(format!(
                "Sandbox: Protected-path preflight blocked path expression {token:?} under the rule for {rule}. This executor could not establish its effect or selected paths. The command was not run."
            ));
        }
    };
    Some(format!(
        "Sandbox: Protected-path preflight blocked {operand} {token:?} under the rule for {rule}. The command was not run."
    ))
}

pub fn classify_path_sensitivity(path: &str) -> PathSensitivity {
    PathSensitivityContext::new(None).classify(path).0
}

/// Filesystem facts and classifications are reused only within one decision.
/// A later decision resolves them again, so moved roots and symlinks are seen.
struct PathSensitivityContext {
    directory: Option<PathBuf>,
    sensitive_directory: bool,
    system_aliases: Vec<(PathBuf, PathBuf)>,
    approved_workspace: Option<PathBuf>,
    approved_relative_writes: bool,
    known_shell_effects_only: bool,
    ignore_write_sensitive: bool,
    lexical_only: bool,
    sessions_root: Option<PathBuf>,
    deny_tool_result_reads: bool,
    paths: HashMap<String, (PathSensitivity, Option<InternalPathKind>)>,
}

impl PathSensitivityContext {
    fn new(directory: Option<&Path>) -> Self {
        Self {
            directory: directory.map(Path::to_path_buf),
            sensitive_directory: directory.is_some_and(|directory| {
                let directory = directory.to_string_lossy();
                is_read_sensitive_path(&directory) || is_write_sensitive_path(&directory)
            }),
            system_aliases: directory
                .into_iter()
                .flat_map(|_| {
                    astra_sandbox::sensitive_path_rules()
                        .system_directories
                        .iter()
                })
                .filter_map(|path| {
                    let original = PathBuf::from(*path);
                    let canonical = canonicalize_existing_or_nearest(&original)?;
                    (canonical != original).then_some((canonical, original))
                })
                .collect(),
            approved_workspace: None,
            approved_relative_writes: false,
            known_shell_effects_only: false,
            ignore_write_sensitive: false,
            lexical_only: false,
            sessions_root: astra_services::local_session_artifact_store()
                .sessions_root()
                .canonicalize()
                .ok(),
            deny_tool_result_reads: false,
            paths: HashMap::new(),
        }
    }

    /// Project a Runner cwd without consulting the Server filesystem.
    fn lexical(directory: &Path) -> Self {
        Self {
            directory: Some(directory.to_path_buf()),
            sensitive_directory: false,
            system_aliases: Vec::new(),
            approved_workspace: None,
            approved_relative_writes: false,
            known_shell_effects_only: true,
            ignore_write_sensitive: false,
            lexical_only: true,
            sessions_root: None,
            deny_tool_result_reads: false,
            paths: HashMap::new(),
        }
    }

    fn classify(&mut self, path: &str) -> (PathSensitivity, Option<InternalPathKind>) {
        if let Some(result) = self.paths.get(path) {
            return *result;
        }
        if path.starts_with(
            astra_services::session_artifact_store::SESSION_TOOL_RESULT_ARTIFACT_URI_PREFIX,
        ) {
            return (
                PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult),
                None,
            );
        }
        let has_glob = contains_glob_meta(path);
        let expanded = expand_home_path(path);
        let relative_to_directory = expanded.is_relative() && self.directory.is_some();
        let resolved = match &self.directory {
            Some(directory) if expanded.is_relative() => directory.join(&expanded),
            _ => expanded,
        };
        let candidate = (!self.lexical_only
            && (self.directory.is_some() || self.sessions_root.is_some()))
        .then(|| canonicalize_existing_or_nearest(&resolved))
        .flatten();
        let lexical = normalize_lexical_path(&resolved);
        // Only a local execution host may resolve relative paths against the
        // invocation directory. A server's cwd is not an Edge workspace.
        let local_candidate = self
            .directory
            .as_ref()
            .and(candidate.as_ref())
            .filter(|candidate| {
                (relative_to_directory && self.sensitive_directory) || **candidate != lexical
            });
        let session = self.sessions_root.as_ref().and_then(|root| {
            let relative = candidate.as_ref()?.strip_prefix(root).ok()?;
            classify_session_relative_path(relative, has_glob)
        });
        let mut read_sensitive = is_read_sensitive_path(path)
            || (self.directory.is_some() && self.local_sensitive_path(&lexical, false))
            || local_candidate.is_some_and(|p| {
                self.local_sensitive_path(p, false) || is_read_sensitive_path(&p.to_string_lossy())
            });
        if read_sensitive && !self.lexical_only && self.directory.is_some() && resolved.is_dir() {
            read_sensitive = self.local_sensitive_path(&lexical, true)
                || local_candidate.is_some_and(|p| self.local_sensitive_path(p, true));
        }
        let sensitivity = if let Some(kind) = session.filter(|_| !has_glob) {
            PathSensitivity::InternalArtifactReadOnly(kind)
        } else if read_sensitive {
            PathSensitivity::Sensitive
        } else if ((is_write_sensitive_path(path)
            || (self.lexical_only && is_write_sensitive_path(&lexical.to_string_lossy())))
            && !is_agent_skill_content_path(&lexical.to_string_lossy()))
            || local_candidate.is_some_and(|p| {
                let resolved = p.to_string_lossy();
                is_write_sensitive_path(&resolved) && !is_agent_skill_content_path(&resolved)
            })
        {
            PathSensitivity::WriteSensitive
        } else {
            PathSensitivity::Normal
        };
        let result = (sensitivity, session.filter(|_| has_glob));
        self.paths.insert(path.to_owned(), result);
        result
    }

    fn local_sensitive_path(&self, path: &Path, directory: bool) -> bool {
        let sensitive = |path: &Path| {
            if directory {
                is_sensitive_directory(path)
            } else {
                is_sensitive_path(path)
            }
        };
        sensitive(path)
            || self.system_aliases.iter().any(|(alias, original)| {
                path.strip_prefix(alias)
                    .ok()
                    .is_some_and(|suffix| sensitive(&original.join(suffix)))
            })
    }

    fn requires_gate(&mut self, path: &str, access: PathAccess) -> Option<SensitivePathMatch> {
        let (mut sensitivity, listing) = self.classify(path);
        if matches!(access, PathAccess::List)
            && (self.lexical_only || self.directory.is_none())
            && sensitivity == PathSensitivity::Sensitive
            && !is_sensitive_directory(&expand_home_path(path))
        {
            // The Server cannot tell whether a filename-like label is an
            // ordinary directory; its execution host will resolve the path.
            sensitivity = PathSensitivity::Normal;
        }
        if let Some(kind) = listing
            && ((matches!(sensitivity, PathSensitivity::WriteSensitive)
                && matches!(access, PathAccess::List))
                || (self.deny_tool_result_reads
                    && kind == InternalPathKind::SessionToolResult
                    && matches!(access, PathAccess::Read | PathAccess::List)))
        {
            sensitivity = PathSensitivity::InternalArtifactReadOnly(kind);
        }
        let gated = match (sensitivity, access) {
            (PathSensitivity::Normal, _) => false,
            (PathSensitivity::Sensitive, _) => true,
            (PathSensitivity::WriteSensitive, PathAccess::Write) => !self.ignore_write_sensitive,
            (PathSensitivity::WriteSensitive, PathAccess::Read) if contains_glob_meta(path) => true,
            (PathSensitivity::WriteSensitive, PathAccess::Read | PathAccess::List) => false,
            (PathSensitivity::InternalArtifactReadOnly(_), PathAccess::Write) => true,
            (
                PathSensitivity::InternalArtifactReadOnly(kind),
                PathAccess::Read | PathAccess::List,
            ) => self.deny_tool_result_reads && kind == InternalPathKind::SessionToolResult,
        };
        gated.then(|| SensitivePathMatch {
            token: path.to_string(),
            sensitivity,
            access,
            inferred: matches!(access, PathAccess::Read) && contains_glob_meta(path),
        })
    }

    fn approved_workspace_write(&self, path: &str) -> bool {
        let Some(root) = self.approved_workspace.as_deref() else {
            return false;
        };
        if path.contains(['$', '`']) || contains_glob_meta(path) || path.starts_with('~') {
            return false;
        }
        let path = Path::new(path);
        let target = if path.is_absolute() {
            path.to_path_buf()
        } else if self.approved_relative_writes {
            let Some(directory) = self.directory.as_ref() else {
                return false;
            };
            directory.join(path)
        } else {
            return false;
        };
        astra_sandbox::canonicalize_parent_and_append(&target)
            .ok()
            .is_some_and(|resolved| resolved.starts_with(root))
    }
}

fn is_read_sensitive_path(path: &str) -> bool {
    let expanded = expand_home_path(path);
    is_sensitive_path(&expanded)
        || matches_sensitive_path(path)
        || matches_sensitive_path(&expanded.to_string_lossy())
}

fn is_write_sensitive_path(path: &str) -> bool {
    let expanded = expand_home_path(path);
    is_dangerous_file_path(path) || is_dangerous_file_path(&expanded.to_string_lossy())
}

fn is_agent_skill_content_path(path: &str) -> bool {
    if !path.contains(".astra") && !path.contains(".claude") {
        return false;
    }
    let expanded = expand_home_path(path);
    is_agent_skill_content_path_components(Path::new(path))
        || is_agent_skill_content_path_components(&expanded)
}

fn is_agent_skill_content_path_components(path: &Path) -> bool {
    let components = normal_component_strings(path);
    components.windows(2).any(|window| {
        matches!(window, [agent_dir, skills_dir]
            if matches!(agent_dir.as_str(), ".astra" | ".claude")
                && skills_dir == "skills")
    })
}

fn normal_component_strings(path: &Path) -> Vec<String> {
    normalize_lexical_path(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn classify_session_relative_path(
    relative: &Path,
    allow_glob_selectors: bool,
) -> Option<InternalPathKind> {
    if relative.as_os_str().is_empty() {
        return Some(InternalPathKind::SessionRoot);
    }
    let relative =
        astra_services::session_artifact_store::local_owner_sessions_relative_path(relative)?;
    let components = normal_components(relative)?;
    classify_session_relative_components(&components, allow_glob_selectors)
}

fn normal_components(path: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => out.push(value.to_string_lossy().to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn classify_session_relative_components(
    components: &[String],
    allow_glob_selectors: bool,
) -> Option<InternalPathKind> {
    match components {
        [] => Some(InternalPathKind::SessionRoot),
        [journal_file] if is_session_journal_file_name(journal_file) => {
            Some(InternalPathKind::SessionJournal)
        }
        [session_selector] if is_session_selector(session_selector, allow_glob_selectors) => {
            Some(InternalPathKind::SessionDirectory)
        }
        [session_selector, child]
            if is_session_selector(session_selector, allow_glob_selectors)
                && is_session_diagnostic_file(child) =>
        {
            Some(InternalPathKind::SessionDiagnostic)
        }
        [session_selector, dir] if is_session_selector(session_selector, allow_glob_selectors) => {
            session_artifact_dir_kind(dir)
        }
        [session_selector, dir, rest @ ..]
            if is_session_selector(session_selector, allow_glob_selectors)
                && rest
                    .iter()
                    .all(|component| allow_glob_selectors || !contains_glob_meta(component)) =>
        {
            session_artifact_dir_kind(dir)
        }
        _ => None,
    }
}

fn is_session_selector(component: &str, allow_glob_selectors: bool) -> bool {
    astra_services::session_journal::validate_session_id(component).is_ok()
        || (allow_glob_selectors && contains_glob_meta(component))
}

fn is_session_diagnostic_file(name: &str) -> bool {
    matches!(
        name,
        "workspace.yaml"
            | "conversation_log.jsonl"
            | "step_events.jsonl"
            | "session-memory.md"
            | "session-memory.meta.json"
    ) || (name.starts_with("llm_error_") && name.ends_with(".json"))
}

fn session_artifact_dir_kind(name: &str) -> Option<InternalPathKind> {
    match name {
        astra_services::session_artifact_store::LOCAL_SESSION_TOOL_RESULTS_DIR => {
            Some(InternalPathKind::SessionToolResult)
        }
        "step_checkpoints" | "checkpoints" | "file_checkpoints" => {
            Some(InternalPathKind::SessionDiagnostic)
        }
        _ => None,
    }
}

fn is_session_journal_file_name(file_name: &str) -> bool {
    let Some(session_id) = file_name.strip_suffix(".jsonl") else {
        return false;
    };
    astra_services::session_journal::validate_session_id(session_id).is_ok()
}

fn contains_glob_meta(path: &str) -> bool {
    path.chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
}

fn expand_home_path(path: &str) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(path);
    };

    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    if let Some(rest) = path.strip_prefix("$HOME/") {
        return home.join(rest);
    }
    if let Some(rest) = path.strip_prefix("${HOME}/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn canonicalize_existing_or_nearest(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }

    let mut current = path;
    let mut missing = Vec::new();

    loop {
        let file_name = current.file_name()?.to_os_string();
        missing.push(file_name);

        let parent = current.parent()?;
        current = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };

        if let Ok(mut canonical) = current.canonicalize() {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }
    }
}

pub fn path_requires_sensitive_gate(path: &str, access: PathAccess) -> Option<SensitivePathMatch> {
    PathSensitivityContext::new(None).requires_gate(path, access)
}

/// Inspect tool arguments for a path-like field or shell path operand and decide
/// whether it trips the sensitive-path gate.
pub fn sensitive_path_match_for_tool_args(
    tool_name: &str,
    args: &Value,
) -> Option<SensitivePathMatch> {
    let is_direct_read = is_read_only_tool_with_args(tool_name, Some(args));

    if let Some(path) = path_hint_from_args(args)
        && !path.is_empty()
    {
        let access = if is_direct_read {
            direct_read_access(tool_name)
        } else {
            PathAccess::Write
        };
        return path_requires_sensitive_gate(&path, access);
    }

    if crate::tool::categories::registry().is_shell(tool_name)
        && let Some(command) = command_hint_from_args(args)
    {
        return sensitive_path_match_for_shell_command(command);
    }

    None
}

pub fn sensitive_path_match_for_shell_command(command: &str) -> Option<SensitivePathMatch> {
    sensitive_path_match_for_shell_command_in_context(
        command,
        &mut PathSensitivityContext::new(None),
    )
}

/// Detect an established protected write using only the selected Runner cwd's
/// lexical label. Actual path resolution remains the Runner's responsibility.
pub fn bash_protected_write_candidate(command: &str, directory: &Path) -> bool {
    let mut context = PathSensitivityContext::lexical(directory);
    sensitive_path_match_for_shell_command_in_context(command, &mut context).is_some_and(|hit| {
        hit.sensitivity == PathSensitivity::WriteSensitive
            && hit.access == PathAccess::Write
            && (Path::new(&hit.token).is_absolute()
                || astra_sandbox::bash_has_stable_working_directory(command))
    })
}

fn sensitive_path_match_for_shell_command_in_context(
    command: &str,
    context: &mut PathSensitivityContext,
) -> Option<SensitivePathMatch> {
    let parsed = crate::permission::compound_command::tokenize_compound_command(command);
    let first = parsed.steps.iter().find_map(|step| {
        sensitive_path_match_for_shell_segment(&step.command, PathAccess::Write, context)
    });
    if first
        .as_ref()
        .is_some_and(|hit| hit.sensitivity != PathSensitivity::WriteSensitive)
    {
        return first;
    }

    // A protected write can be approved; an independent credential or system
    // access cannot. Inspect the remaining operands before offering approval,
    // including reads in the same segment as a write redirect.
    if first.is_some() {
        context.ignore_write_sensitive = true;
        let unwaived = parsed.steps.iter().find_map(|step| {
            sensitive_path_match_for_shell_segment(&step.command, PathAccess::Write, context)
        });
        context.ignore_write_sensitive = false;
        if unwaived.is_some() {
            return unwaived;
        }
    }

    let Some(proven_read) = read_only_shell_path_match(command, context) else {
        return first;
    };
    proven_read.or_else(|| {
        first?;
        // Only downgrade inferred access. Recheck the same segment as well:
        // a false write hit can precede a credential in a compound command.
        parsed.steps.iter().find_map(|step| {
            sensitive_path_match_for_shell_segment(&step.command, PathAccess::Read, context)
                .filter(|hit| hit.sensitivity != PathSensitivity::WriteSensitive)
        })
    })
}

fn read_only_shell_path_match(
    command: &str,
    context: &mut PathSensitivityContext,
) -> Option<Option<SensitivePathMatch>> {
    let commands = astra_sandbox::parse_bash_commands_for_path_analysis(command)?;
    let mut can_downgrade = true;
    for argv in &commands {
        match read_only_argv_path_match(argv, context) {
            Some(Some(hit)) => return Some(Some(hit)),
            Some(None) => {}
            None => can_downgrade = false,
        }
    }
    can_downgrade.then_some(None)
}

fn read_only_argv_path_match(
    argv: &[String],
    context: &mut PathSensitivityContext,
) -> Option<Option<SensitivePathMatch>> {
    if crate::cloud::approval_policy::bash_argv_approval_reason(argv).is_some() {
        return None;
    }
    let command_index = first_command_token_index(argv)?;
    let hit = known_shell_operand_match(argv, command_index, true, context)?;
    // The lexical command index can skip wrappers without resolving their
    // option values or working directory.
    let hit = hit.map(|hit| hit.with_inferred_access(command_index != 0));
    (command_index == 0 || hit.is_some()).then_some(hit)
}

fn git_path_match(
    args: &[String],
    context: &mut PathSensitivityContext,
) -> Option<Option<SensitivePathMatch>> {
    let command = git_subcommand_index(args)?;
    if args[command] == "config" {
        return git_config_file_path_match(&args[command + 1..], context);
    }
    if !matches!(args[command].as_str(), "show" | "log" | "diff") {
        return None;
    }
    let subcommand = args[command].as_str();
    let history = subcommand != "diff";
    let metadata_only = git_outputs_metadata_only(subcommand, &args[command + 1..]);
    let mut args = args[command + 1..].iter();
    let mut pathspec_only = false;
    let mut protected_pathspec = None;
    while let Some(arg) = args.next() {
        if !pathspec_only && arg.starts_with('-') {
            if arg == "--" {
                pathspec_only = true;
            } else if history
                && crate::cloud::approval_policy::GIT_HISTORY_VALUE_OPTIONS.contains(&arg.as_str())
            {
                args.next()?;
            }
            continue;
        }

        if !arg.starts_with(':')
            && let Some(hit) = context.requires_gate(arg, PathAccess::Read)
        {
            return Some(Some(hit.with_inferred_access(!pathspec_only)));
        }

        if let Some(path) = git_literal_pathspec(arg) {
            if let Some(hit) = context.requires_gate(path, PathAccess::Read) {
                return Some(Some(hit));
            }
            // A pathspec can select descendants in a historical tree.
            // Preserve the protected-state rule even for literal pathspecs.
            if !metadata_only {
                protected_pathspec = protected_pathspec.or_else(|| {
                    let mut hit = context.requires_gate(path, PathAccess::Write)?;
                    hit.access = PathAccess::Read;
                    Some(hit.with_inferred_access(true))
                });
            }
        }
        if !pathspec_only
            && !arg.starts_with(':')
            && let Some((_, path)) = arg.split_once(':')
            && let Some(hit) = context.requires_gate(path, PathAccess::Read)
        {
            return Some(Some(hit));
        }
        if !pathspec_only
            && let Some(path) = arg.strip_prefix(':')
            && !path.starts_with(['(', '/'])
        {
            let path = match path.as_bytes() {
                [b'0'..=b'3', b':', ..] => &path[2..],
                _ => path,
            };
            if let Some(hit) = context.requires_gate(path, PathAccess::Read) {
                return Some(Some(hit));
            }
        }
    }
    // Git's selected objects are unknown without inspecting the tree.
    // An argv scan cannot override an existing compound-command gate.
    if metadata_only {
        Some(None)
    } else {
        protected_pathspec.map(Some)
    }
}

fn git_subcommand_index(args: &[String]) -> Option<usize> {
    let mut index = 0;
    while let Some(argument) = args.get(index).map(String::as_str) {
        match argument {
            "--no-pager"
            | "--paginate"
            | "-p"
            | "-P"
            | "--bare"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--icase-pathspecs"
            | "--no-optional-locks"
            | "--no-lazy-fetch" => index += 1,
            "-C" | "-c" => index += 2,
            argument if argument.starts_with("--") && argument.contains('=') => index += 1,
            _ => return Some(index),
        }
    }
    None
}

fn git_config_file_path_match(
    args: &[String],
    context: &mut PathSensitivityContext,
) -> Option<Option<SensitivePathMatch>> {
    let access = git_config_file_access(args);
    let mut index = 0;
    while let Some(argument) = args.get(index).map(String::as_str) {
        if argument == "--" {
            break;
        }
        let long_option = argument
            .strip_prefix("--")
            .map(|option| option.split_once('=').map_or(option, |(name, _)| name));
        let path = if matches!(long_option, Some("file" | "fil")) {
            argument.split_once('=').map(|(_, path)| path).or_else(|| {
                index += 1;
                args.get(index).map(String::as_str)
            })
        } else if argument == "-f" {
            index += 1;
            args.get(index).map(String::as_str)
        } else {
            argument.strip_prefix("-f").filter(|path| !path.is_empty())
        };
        if let Some(path) = path
            && let Some(hit) = shell_operand_sensitive_path_match(path, access, context)
        {
            return Some(Some(hit));
        }
        index += 1;
    }
    Some(None)
}

fn git_config_file_access(args: &[String]) -> PathAccess {
    let mut index = 0;
    let mut positional_count = 0;
    let mut has_read_action = false;
    while let Some(argument) = args.get(index).map(String::as_str) {
        if argument == "--" {
            positional_count += args.len().saturating_sub(index + 1);
            break;
        }
        let long_option = argument
            .strip_prefix("--")
            .map(|option| option.split_once('=').map_or(option, |(name, _)| name));
        let matches_long_option = |options: &[&str]| {
            long_option.is_some_and(|option| options.iter().any(|name| name.starts_with(option)))
        };
        if matches!(long_option, Some("file" | "fil")) || argument == "-f" {
            if argument == "-f" || !argument.contains('=') {
                index += 1;
            }
        } else if matches_long_option(&[
            "add",
            "replace-all",
            "unset",
            "unset-all",
            "rename-section",
            "remove-section",
            "edit",
        ]) || argument == "-e"
        {
            return PathAccess::Write;
        } else if matches_long_option(&[
            "get",
            "get-all",
            "get-regexp",
            "get-urlmatch",
            "get-color",
            "get-colorbool",
            "list",
        ]) || argument == "-l"
        {
            has_read_action = true;
        } else if matches_long_option(&["type", "default", "blob"])
            || matches!(argument, "-t" | "--type" | "--default" | "--blob")
        {
            if !argument.contains('=') {
                index += 1;
            }
        } else if argument.starts_with("-f") && argument.len() > 2 {
            // `-fFILE` selects the config file without a separate argument.
        } else if !argument.starts_with('-') {
            positional_count += 1;
        }
        index += 1;
    }
    if !has_read_action && positional_count >= 2 {
        PathAccess::Write
    } else {
        PathAccess::Read
    }
}

fn git_outputs_metadata_only(subcommand: &str, args: &[String]) -> bool {
    if subcommand == "show" {
        return false;
    }
    // Git diff normally prints content. Require an explicit names-only
    // option and reject other output options, even in the same command.
    let mut metadata_only = subcommand == "log";
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "--" => return metadata_only && args.all(|path| git_literal_pathspec(path).is_some()),
            "--name-only" | "--name-status" => metadata_only = true,
            "--cached" | "--staged" | "--no-renames" | "--no-color" if subcommand == "diff" => {}
            "--oneline" | "-s" | "--no-patch" | "--all" | "--reverse" | "--no-merges"
            | "--merges" | "--first-parent" | "--full-history" | "--date-order"
            | "--topo-order" | "--decorate" | "--no-decorate" | "--pretty" | "--graph"
                if subcommand == "log" => {}
            value
                if subcommand == "log"
                    && value.split_once('=').is_some_and(|(name, format)| {
                        name == "--pretty"
                            || (matches!(name, "--format" | "--date") && !format.is_empty())
                    }) => {}
            value
                if subcommand == "log"
                    && crate::cloud::approval_policy::GIT_HISTORY_VALUE_OPTIONS
                        .contains(&value) =>
            {
                // Git handles some log switches before consuming revision values.
                if args.next().is_none_or(|value| value.starts_with('-')) {
                    return false;
                }
            }
            value
                if subcommand == "log"
                    && value.strip_prefix('-').is_some_and(|digits| {
                        !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_digit())
                    }) => {}
            value
                if !value.starts_with('-')
                    && !value.contains(':')
                    && git_literal_pathspec(value).is_some() => {}
            _ => return false,
        }
    }
    metadata_only
}

fn git_literal_pathspec(selector: &str) -> Option<&str> {
    let (path, literal) = if let Some(magic) = selector.strip_prefix(":(") {
        let (flags, path) = magic.split_once(')')?;
        if !flags
            .split(',')
            .all(|flag| matches!(flag, "top" | "literal"))
        {
            return None;
        }
        (path, flags.split(',').any(|flag| flag == "literal"))
    } else if let Some(path) = selector.strip_prefix(":/") {
        (path, false)
    } else if selector.starts_with(':') {
        return None;
    } else {
        (selector, false)
    };
    (!path.is_empty() && (literal || !contains_glob_meta(path))).then_some(path)
}

fn direct_read_access(tool_name: &str) -> PathAccess {
    match tool_name {
        "list_dir" | "glob" => PathAccess::List,
        _ => PathAccess::Read,
    }
}

fn sensitive_path_match_for_shell_segment(
    segment: &str,
    inferred_access: PathAccess,
    context: &mut PathSensitivityContext,
) -> Option<SensitivePathMatch> {
    let (tokens, redirects) = shell_tokenize_like_bash(segment);
    if let Some(hit) = redirection_sensitive_path_match(segment, &redirects, context) {
        return Some(hit.with_inferred_access(true));
    }

    let command_index = first_command_token_index(&tokens)?;
    if command_index == 0
        && literal_data_command(&tokens)
        && astra_sandbox::parse_bash_literal_paths(segment).is_some_and(|paths| {
            paths.commands.len() == 1
                && paths.commands[0] == tokens
                && paths.read_redirects.is_empty()
                && paths.write_redirects.is_empty()
        })
    {
        return None;
    }
    if let Some(hit) = known_shell_operand_match(&tokens, command_index, false, context) {
        return hit.map(|hit| {
            let literal_argv = command_index == 0
                && astra_sandbox::parse_bash_literal_paths(segment)
                    .is_some_and(|paths| paths.commands.len() == 1 && paths.commands[0] == tokens);
            hit.with_inferred_access(!literal_argv)
        });
    }
    if crate::cloud::approval_policy::bash_command_is_read_only(segment) {
        return None;
    }
    if context.known_shell_effects_only {
        return None;
    }
    // The generic fallback does not establish whether an operand is read or
    // written, so a write approval cannot waive its match.
    let approved_workspace = context.approved_workspace.take();
    let hit = generic_sensitive_path_match(&tokens, command_index + 1, inferred_access, context)
        .map(|hit| hit.with_inferred_access(true));
    context.approved_workspace = approved_workspace;
    hit
}

fn literal_data_command(argv: &[String]) -> bool {
    match argv {
        [command, ..] if matches!(command.as_str(), "basename" | "dirname") => true,
        [command, _] if command == "test" => true,
        _ => false,
    }
}

/// Outer `None` means the command has no path-operand model here.
fn known_shell_operand_match(
    tokens: &[String],
    command_index: usize,
    require_known: bool,
    context: &mut PathSensitivityContext,
) -> Option<Option<SensitivePathMatch>> {
    if require_known && literal_data_command(&tokens[command_index..]) {
        return Some(None);
    }
    let command = shell_basename(&tokens[command_index]);
    let args_start = command_index + 1;

    Some(match command {
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" => {
            return grep_like_sensitive_path_match(
                tokens,
                args_start,
                command,
                require_known,
                context,
            );
        }
        "find" => {
            let hit = find_sensitive_path_match(tokens, args_start, context);
            // These scans cover explicit operands, not every expression or
            // descendant the command may read.
            return hit.map(Some).or_else(|| (!require_known).then_some(None));
        }
        "git" => return git_path_match(&tokens[args_start..], context),
        "sed" | "cp" | "install" | "rsync" | "rm" | "rmdir" | "unlink" | "mv" | "touch"
        | "mkdir" | "chmod" | "chown" | "chgrp" | "truncate" | "tee" => {
            let arguments: Vec<_> = tokens[args_start..].iter().map(String::as_str).collect();
            let roles = astra_sandbox::bash_command_path_operands(command, &arguments)?;
            let complete = roles.complete;
            for path in &roles.reads {
                if let Some(hit) =
                    shell_operand_sensitive_path_match(path, PathAccess::Read, context)
                {
                    return Some(Some(hit.with_inferred_access(!complete)));
                }
            }
            for pattern in &roles.read_selectors {
                if let Some(hit) =
                    shell_operand_sensitive_path_match(pattern, PathAccess::Read, context)
                {
                    return Some(Some(hit.with_inferred_access(true)));
                }
            }
            if !roles.writes_known && context.known_shell_effects_only {
                return None;
            }
            for backup in astra_sandbox::bash_command_backup_write_paths(command, &roles) {
                if let Some(hit) =
                    shell_operand_sensitive_path_match(&backup, PathAccess::Write, context)
                {
                    return Some(Some(hit.with_inferred_access(true)));
                }
            }
            let saved = if roles.writes_known {
                None
            } else {
                context.approved_workspace.take()
            };
            let hit = roles.writes.into_iter().find_map(|path| {
                shell_operand_sensitive_path_match(path, PathAccess::Write, context)
            });
            if saved.is_some() {
                context.approved_workspace = saved;
            }
            return hit
                .map(|hit| Some(hit.with_inferred_access(!complete)))
                .or_else(|| (!require_known || complete).then_some(None));
        }
        "ls" | "ll" => {
            return generic_read_operand_match(
                &tokens[args_start..],
                command,
                PathAccess::List,
                require_known,
                context,
            );
        }
        "sort" | "tree" | "date" | "uniq" | "cut" | "paste" | "nl" | "column" | "fmt" | "fold"
        | "expand" | "wc" | "file" => {
            let paths =
                crate::cloud::approval_policy::shell_command_paths(command, &tokens[args_start..])?;
            let positional_access = if command == "tree" && !paths.positionals_are_reads {
                PathAccess::List
            } else {
                PathAccess::Read
            };
            for path in paths.positionals {
                if let Some(hit) =
                    shell_operand_sensitive_path_match(path, positional_access, context)
                {
                    return Some(Some(hit));
                }
            }
            for path in paths.reads {
                if let Some(hit) =
                    shell_operand_sensitive_path_match(path, PathAccess::Read, context)
                {
                    return Some(Some(hit));
                }
            }
            for path in paths.writes {
                if let Some(hit) =
                    shell_operand_sensitive_path_match(path, PathAccess::Write, context)
                {
                    return Some(Some(hit));
                }
            }
            return Some(None);
        }
        "cat" | "head" | "tail" | "less" | "more" | "stat" | "du" | "basename" | "dirname"
        | "realpath" | "readlink" | "test" => {
            return generic_read_operand_match(
                &tokens[args_start..],
                command,
                PathAccess::Read,
                require_known,
                context,
            );
        }
        "echo" | "printf" | "pwd" | "true" | "false" | "sleep" | "whoami" | "id" | "uname"
        | "hostname" | "env" | "printenv" => None,
        _ => return None,
    })
}

fn first_command_token_index(tokens: &[String]) -> Option<usize> {
    let mut idx = 0;
    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if token.is_empty() || looks_like_shell_assignment(token) {
            idx += 1;
            continue;
        }

        let command = shell_basename(token);
        match command {
            "command" | "builtin" | "exec" | "nohup" | "noglob" | "nocorrect" | "sudo" | "doas"
            | "time" => {
                idx += 1;
                continue;
            }
            "env" => {
                idx += 1;
                while idx < tokens.len() {
                    let env_token = tokens[idx].as_str();
                    if looks_like_shell_assignment(env_token) {
                        idx += 1;
                        continue;
                    }
                    if env_token.starts_with('-') {
                        idx += 1;
                        continue;
                    }
                    break;
                }
                continue;
            }
            _ => return Some(idx),
        }
    }
    None
}

fn redirection_sensitive_path_match(
    segment: &str,
    redirects: &[(String, PathAccess)],
    context: &mut PathSensitivityContext,
) -> Option<SensitivePathMatch> {
    for (token, access) in redirects {
        let access = *access;
        let token = token.as_str();
        let proven_write = access == PathAccess::Write
            && shell_path_operand(token)
                .is_some_and(|path| astra_sandbox::has_literal_bash_write_redirect(segment, &path));
        if context.known_shell_effects_only && access == PathAccess::Write && !proven_write {
            continue;
        }
        let saved = if access == PathAccess::Write && !proven_write {
            context.approved_workspace.take()
        } else {
            None
        };
        let hit = shell_operand_sensitive_path_match(token, access, context);
        if saved.is_some() {
            context.approved_workspace = saved;
        }
        if let Some(hit) = hit {
            return Some(hit);
        }
    }
    None
}

fn redirection_access(token: &str, target: &str) -> Option<PathAccess> {
    let without_fd = token.trim_start_matches(|ch: char| ch.is_ascii_digit());
    match without_fd {
        ">" | ">>" | ">|" | "<>" => Some(PathAccess::Write),
        "<" => Some(PathAccess::Read),
        ">&" if target != "-" && !target.bytes().all(|byte| byte.is_ascii_digit()) => {
            Some(PathAccess::Write)
        }
        _ => None,
    }
}

fn generic_read_operand_match(
    args: &[String],
    command: &str,
    access: PathAccess,
    require_known: bool,
    context: &mut PathSensitivityContext,
) -> Option<Option<SensitivePathMatch>> {
    // Skipping an option in the lexical scan does not prove that it has no
    // path operands. Only certify positionals and known numeric limits.
    let path_args = match args {
        [flag, count, rest @ ..]
            if matches!(command, "head" | "tail")
                && flag == "-n"
                && !count.is_empty()
                && count.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            rest
        }
        _ => args,
    };
    let known = !matches!(command, "basename" | "dirname" | "test")
        && path_args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .all(|arg| arg == "-" || !arg.starts_with('-'));
    if let Some(hit) = generic_sensitive_path_match(args, 0, access, context) {
        return Some(Some(hit.with_inferred_access(!require_known || !known)));
    }
    (!require_known || known).then_some(None)
}

fn generic_sensitive_path_match(
    tokens: &[String],
    start: usize,
    access: PathAccess,
    context: &mut PathSensitivityContext,
) -> Option<SensitivePathMatch> {
    let mut stop_parsing_flags = false;
    let mut idx = start;
    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if !stop_parsing_flags && token == "--" {
            stop_parsing_flags = true;
            idx += 1;
            continue;
        }
        if !stop_parsing_flags && token.starts_with('-') {
            idx += 1;
            continue;
        }
        if let Some(hit) = shell_operand_sensitive_path_match(token, access, context) {
            return Some(hit);
        }
        idx += 1;
    }
    None
}

fn grep_like_sensitive_path_match(
    tokens: &[String],
    start: usize,
    command: &str,
    require_known: bool,
    context: &mut PathSensitivityContext,
) -> Option<Option<SensitivePathMatch>> {
    let mut positionals = Vec::new();
    let mut recursive = matches!(command, "rg" | "ag" | "ack");
    let mut explicit_pattern = false;
    let mut options_known = true;
    let mut args = tokens[start..].iter().map(String::as_str);
    let mut positional_only = false;
    while let Some(arg) = args.next() {
        if !positional_only && arg == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && arg.starts_with('-') && arg != "-" {
            match grep_option(command, arg, &mut recursive) {
                Some(Some((kind, attached))) => {
                    let value = attached.or_else(|| args.next())?;
                    // GNU grep accepts abbreviated actions. Only exact known
                    // non-traversing actions can rule out descendant reads.
                    recursive |= matches!(kind, GrepFlagValue::Directories)
                        && !matches!(value, "read" | "skip");
                    explicit_pattern |=
                        matches!(kind, GrepFlagValue::Pattern | GrepFlagValue::PatternFile);
                    if matches!(
                        kind,
                        GrepFlagValue::PatternFile
                            | GrepFlagValue::File
                            | GrepFlagValue::PathSelector
                    ) && !(command == "rg"
                        && matches!(kind, GrepFlagValue::PathSelector)
                        && value.starts_with('!'))
                        && let Some(hit) =
                            shell_operand_sensitive_path_match(value, PathAccess::Read, context)
                    {
                        return Some(Some(hit.with_inferred_access(
                            !options_known || matches!(kind, GrepFlagValue::PathSelector),
                        )));
                    }
                }
                Some(None) => {}
                None if is_gnu_grep(command) => {
                    let hit = grep_ambiguous_operand_match(&tokens[start..], command, context);
                    return hit.map(Some).or_else(|| (!require_known).then_some(None));
                }
                None => options_known = false,
            }
            continue;
        }

        positionals.push(arg);
    }

    // GNU grep accepts options after operands. Determine the search roots only
    // after consuming options and separating an implicit pattern from paths.
    let skip_pattern = usize::from(!explicit_pattern && !positionals.is_empty());
    let roots = &positionals[skip_pattern..];
    if let Some(hit) = roots
        .iter()
        .find_map(|path| shell_operand_sensitive_path_match(path, PathAccess::Read, context))
    {
        return Some(Some(hit.with_inferred_access(!options_known)));
    }

    if recursive {
        if let Some(hit) = roots
            .iter()
            .find_map(|path| grep_sensitive_root(path, context))
        {
            return Some(Some(hit));
        }
        // Recursive search can open unnamed descendants.
        if require_known {
            return None;
        }
    }
    (!require_known || options_known).then_some(None)
}

fn grep_ambiguous_operand_match(
    args: &[String],
    command: &str,
    context: &mut PathSensitivityContext,
) -> Option<SensitivePathMatch> {
    // An unmodelled flag can change arity and traversal. No argument role
    // inferred in the main parser is reliable enough to exempt a path here.
    args.iter()
        .map(String::as_str)
        .flat_map(|arg| {
            let attached = if arg.starts_with("--") {
                arg.split_once('=').map(|(_, value)| value)
            } else {
                match grep_option(command, arg, &mut false) {
                    Some(Some((GrepFlagValue::PatternFile, value))) => value,
                    _ => None,
                }
            };
            std::iter::once(arg).chain(attached)
        })
        .find_map(|path| {
            shell_operand_sensitive_path_match(path, PathAccess::Read, context)
                .or_else(|| grep_sensitive_root(path, context))
        })
        .map(|hit| hit.with_inferred_access(true))
}

fn grep_sensitive_root(
    path: &str,
    context: &mut PathSensitivityContext,
) -> Option<SensitivePathMatch> {
    let approved_workspace = context.approved_workspace.take();
    let hit = shell_operand_sensitive_path_match(path, PathAccess::Write, context);
    context.approved_workspace = approved_workspace;
    let mut hit = hit.filter(|hit| hit.sensitivity == PathSensitivity::WriteSensitive)?;
    // The protected root may contain credentials. The operation is a read.
    hit.access = PathAccess::Read;
    Some(hit.with_inferred_access(true))
}

fn is_gnu_grep(command: &str) -> bool {
    matches!(command, "grep" | "egrep" | "fgrep")
}

#[derive(Clone, Copy)]
enum GrepFlagValue {
    Pattern,
    PatternFile,
    File,
    PathSelector,
    Data,
    Directories,
}

/// Outer `None` means the option's operand effect is unknown.
fn grep_option<'a>(
    command: &str,
    arg: &'a str,
    recursive: &mut bool,
) -> Option<Option<(GrepFlagValue, Option<&'a str>)>> {
    use GrepFlagValue::{Data, Directories, File, PathSelector, Pattern, PatternFile};
    let gnu = is_gnu_grep(command);
    let ripgrep = command == "rg";
    if let Some(long) = arg.strip_prefix("--") {
        let (name, attached) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (long, None),
        };
        let kind = match name {
            "regexp" => Pattern,
            "match" if command == "ack" => Pattern,
            "file" if gnu || ripgrep => PatternFile,
            "exclude-from" if gnu => File,
            "ignore-file" if ripgrep => File,
            "directories" if gnu => Directories,
            "recursive" | "dereference-recursive" if gnu => {
                *recursive = true;
                return Some(None);
            }
            "include" if gnu => PathSelector,
            "binary-files" | "devices" | "label" | "exclude" | "exclude-dir"
            | "group-separator"
                if gnu =>
            {
                Data
            }
            "glob" | "iglob" if ripgrep => PathSelector,
            "color" | "colors" | "encoding" | "replace" if ripgrep => Data,
            "after-context" | "before-context" | "context" | "max-count" => Data,
            "ignore-case" | "invert-match" | "line-number" | "with-filename" | "no-filename"
            | "fixed-strings" | "count" | "quiet" | "word-regexp" | "line-regexp"
            | "byte-offset" | "only-matching" | "color" => {
                return Some(None);
            }
            "colour"
            | "extended-regexp"
            | "basic-regexp"
            | "perl-regexp"
            | "no-ignore-case"
            | "null-data"
            | "no-messages"
            | "silent"
            | "line-buffered"
            | "text"
            | "files-without-match"
            | "files-with-matches"
            | "initial-tab"
            | "null"
            | "no-group-separator"
            | "binary"
                if gnu =>
            {
                return Some(None);
            }
            _ => return None,
        };
        return Some(Some((kind, attached)));
    }

    let short = arg.strip_prefix('-')?;
    for (index, flag) in short.char_indices() {
        let kind = match flag {
            'e' => Pattern,
            'f' if gnu || ripgrep => PatternFile,
            'f' if command == "ag" => continue,
            'A' | 'B' | 'C' | 'm' => Data,
            'd' if gnu => Directories,
            'D' if gnu => Data,
            'r' | 'R' if gnu => {
                *recursive = true;
                continue;
            }
            'E'
            | 'P'
            | 'G'
            | 'a'
            | 'H'
            | 'h'
            | 'L'
            | 's'
            | 'b'
            | 'I'
            | 'T'
            | 'Z'
            | 'z'
            | 'U'
            | 'V'
            | '0'..='9'
                if gnu =>
            {
                continue;
            }
            'g' if ripgrep => PathSelector,
            'E' | 'r' if ripgrep => Data,
            'b' | 'H' | 'N' | 'P' | 's' | 'S' | 'u' | 'U' if ripgrep => continue,
            'i' | 'v' | 'n' | 'F' | 'c' | 'q' | 'w' | 'x' | 'l' | 'o' => continue,
            _ => return None,
        };
        // A value-taking letter consumes the rest of the cluster as data.
        if !gnu && !ripgrep && (!matches!(kind, Pattern | PatternFile) || short.len() != 1) {
            return None;
        }
        let rest = &short[index + flag.len_utf8()..];
        return Some(Some((kind, (!rest.is_empty()).then_some(rest))));
    }
    Some(None)
}

fn find_sensitive_path_match(
    tokens: &[String],
    start: usize,
    context: &mut PathSensitivityContext,
) -> Option<SensitivePathMatch> {
    let mut idx = start;
    let mut options_done = false;
    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if !options_done && token == "--" {
            options_done = true;
            idx += 1;
            continue;
        }
        if !options_done && token == "-files0-from" {
            let path = tokens.get(idx + 1)?;
            if let Some(hit) = shell_operand_sensitive_path_match(path, PathAccess::Read, context) {
                return Some(hit);
            }
            idx += 2;
            continue;
        }
        if !options_done && token.starts_with('-') {
            break;
        }
        if let Some(hit) = shell_operand_sensitive_path_match(token, PathAccess::Read, context) {
            return Some(hit.with_inferred_access(true));
        }
        idx += 1;
    }
    None
}

fn shell_operand_sensitive_path_match(
    token: &str,
    access: PathAccess,
    context: &mut PathSensitivityContext,
) -> Option<SensitivePathMatch> {
    let path = shell_path_operand(token)?;
    let hit = context.requires_gate(&path, access)?;
    if access == PathAccess::Write
        && hit.sensitivity == PathSensitivity::WriteSensitive
        && context.approved_workspace_write(&path)
    {
        return None;
    }
    Some(hit.with_inferred_access(token.contains(['$', '`']) || contains_glob_meta(token)))
}

fn shell_path_operand(token: &str) -> Option<String> {
    let trimmed = token
        .trim_matches(|ch| matches!(ch, '"' | '\''))
        .trim_matches(|ch| matches!(ch, '(' | ')' | ',' | ';'));
    if trimmed.is_empty()
        || trimmed == "-"
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn shell_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn looks_like_shell_assignment(token: &str) -> bool {
    let Some((key, value)) = token.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && !value.is_empty()
        && key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && key
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

/// Split word arguments from unquoted redirection operators and their targets.
fn shell_tokenize_like_bash(input: &str) -> (Vec<String>, Vec<(String, PathAccess)>) {
    enum Token {
        Word(String),
        Redirect(String),
    }
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut plain_number = true;
    let mut chars = input.chars().peekable();
    let mut in_double_quote = false;
    let mut in_single_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                started = true;
                plain_number = false;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                started = true;
                plain_number = false;
            }
            '\\' if !in_single_quote => {
                if let Some(next) = chars.next()
                    && !matches!(next, '\n' | '\r')
                {
                    current.push(next);
                    started = true;
                    plain_number = false;
                }
            }
            '>' | '<' if !in_double_quote && !in_single_quote => {
                let mut op = String::new();
                if started && plain_number && !current.is_empty() {
                    op.push_str(&current);
                    current.clear();
                } else if started {
                    tokens.push(Token::Word(std::mem::take(&mut current)));
                }
                started = false;
                plain_number = true;
                op.push(ch);
                if chars.peek().is_some_and(|next| *next == ch) {
                    op.push(chars.next().unwrap());
                }
                let operator = op.trim_start_matches(|ch: char| ch.is_ascii_digit());
                if (operator == "<<" && chars.peek() == Some(&'<'))
                    || (operator == "<" && chars.peek() == Some(&'>'))
                    || (operator == ">" && chars.peek() == Some(&'|'))
                {
                    op.push(chars.next().unwrap());
                }
                if chars.peek().is_some_and(|next| *next == '&') {
                    op.push(chars.next().unwrap());
                }
                tokens.push(Token::Redirect(op));
            }
            c if c.is_whitespace() && !in_double_quote && !in_single_quote => {
                if started {
                    tokens.push(Token::Word(std::mem::take(&mut current)));
                }
                started = false;
                plain_number = true;
            }
            _ => {
                current.push(ch);
                started = true;
                plain_number &= ch.is_ascii_digit() && !in_single_quote && !in_double_quote;
            }
        }
    }

    if started {
        tokens.push(Token::Word(current));
    }

    let mut words = Vec::new();
    let mut redirects = Vec::new();
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            Token::Word(word) => words.push(word),
            Token::Redirect(op) => {
                if matches!(tokens.peek(), Some(Token::Word(_)))
                    && let Some(Token::Word(target)) = tokens.next()
                    && let Some(access) = redirection_access(&op, &target)
                {
                    redirects.push((target, access));
                }
            }
        }
    }
    (words, redirects)
}

pub fn sensitive_path_token_for_tool_args(tool_name: &str, args: &Value) -> Option<String> {
    sensitive_path_match_for_tool_args(tool_name, args).map(|hit| hit.token)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn create_current_session_artifact() -> (
        tempfile::TempDir,
        astra_services::session_journal::JournalDirGuard,
        std::path::PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);
        let artifact_path = astra_services::local_session_artifact_store()
            .session_path("session-1", "tool-results/call_abc.txt")
            .unwrap();
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "child output").unwrap();
        (temp, guard, artifact_path)
    }

    fn create_current_session_journal() -> (
        tempfile::TempDir,
        astra_services::session_journal::JournalDirGuard,
        std::path::PathBuf,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join("sessions");
        let guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);
        let journal_path = astra_services::local_session_artifact_store()
            .journal_path("550e8400-e29b-41d4-a716-446655440000")
            .unwrap();
        std::fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        std::fs::write(&journal_path, "{}\n").unwrap();
        (temp, guard, journal_path)
    }

    #[test]
    fn read_only_compound_commands_use_operand_access() {
        for command in [
            r#"for p in .claude/settings.json; do stat "$p" | head -n 1; done"#,
            r#"{ stat .claude/settings.json; cat .claude/README.md; }"#,
            r#"{ stat -- .claude/settings.json; cat .claude/README.md; }"#,
            r#"if true; then stat .claude/settings.json; fi"#,
            r#"git log --grep .env -- README.md"#,
            r#"git log --author "example:.env" -- README.md"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
        for command in [
            r#"for c in a123; do git show "${c}:.claude/.env"; done"#,
            r#"for p in .env; do cat "$p" | head -n 1; done"#,
            r#"select p in .claude/settings.json; do stat "$p"; done"#,
            r#"{ stat .claude/settings.json; rm .claude/settings.json; }"#,
            r#"{ stat .claude/settings.json; grep -e example .env; }"#,
            r#"{ stat .claude/settings.json; grep -f examples.txt .env; }"#,
            r#"for p in .claude; do unknown_command "$p"; done"#,
            r#"for c in a123; do git show "$c" -- .claude > .claude/settings.json; done"#,
            r#"for c in a123; do git diff --output=.claude/settings.json "$c" -- .claude; done"#,
            r#"for p in .claude; do rm -rf "$p"; done"#,
            r#"for PATH in ./project-bin; do git show HEAD -- .claude; done"#,
            r#"for c in a123; do git diff "$c" HEAD -- .ssh/name:with-colon; done"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_some(),
                "{command}"
            );
        }
        assert!(
            sensitive_path_match_for_shell_command(r#"for p in .env; do date -f "$p"; done"#)
                .is_some()
        );
    }

    #[test]
    fn file_operand_options_preserve_read_write_and_literal_roles() {
        for command in [
            "sort .env",
            "sort --random-source=.env README.md",
            "sort --random-sou=.env README.md",
            "sort --files0-from=.env",
            "sort --files0-from .env",
            "sort --files=.env",
            "sort -o ordinary .env",
            "tree -H . --hintro=.env",
            "tree -H . --houtro .env",
            "tree --gitfile=.env .",
            "tree --infofile .env .",
            "tree --fromfile .env",
            "tree --fromtabfile .env",
            "find -files0-from .env",
            "find . -files0-from .env",
            "git config --file=.env --list",
            "git config --file .env --list",
            "git config --fil=.env --list",
            "git config -f .env --list",
            "git config -f.env --list",
            "git -C . config --fil=.env --list",
            "git -c core.pager=cat config --fil=.env --list",
            "git --git-dir=. config --fil=.env --list",
            "git --work-tree=. config --fil=.env --list",
            "git --no-replace-objects config --fil=.env --list",
            "uniq .env",
            "uniq -s 2 .env",
            "cut -c 1- .env",
            "paste -d : .env",
            "paste --delim .env .env",
            "paste --delim=.env .env",
            "paste ~/.aws/credentials",
            "nl -ba .env",
            "nl ~/.ssh/id_rsa",
            "column -t .env",
            "fmt -w 80 .env",
            "fold -w80 .env",
            "expand -t 4 .env",
            "cut -c1- -- .env",
            "cut --only-delimited -c 1- .env",
            "column --table .env",
            "wc --files0-from=.env",
            "wc --files0-from .env",
            "wc --files=.env",
            "file --magic-file=.env README.md",
            "file -bm.env README.md",
            "file -f.env",
            "file --files-from=.env",
            "file --magic-file=ordinary:.env README.md",
            "cat '>' .env",
            "grep '' .env",
            "grep '>' .env",
            "grep '2'>output .env",
            "date -f .env",
            "date -r.env",
            "date -uf.env",
            "date --file=.env",
            "date --fi=.env",
            "date --fi .env",
            "date --reference .env",
            "date --ref .env",
            "date --debug --file=.env",
            "date -Iseconds --file=.env",
        ] {
            let hit = sensitive_path_match_for_shell_command(command)
                .expect("known file operand must remain protected");
            assert_eq!(hit.sensitivity, PathSensitivity::Sensitive, "{command}");
            assert_eq!(hit.access, PathAccess::Read, "{command}");
        }
        for command in [
            "sort -k .env README.md",
            "sort -rk.env README.md",
            "sort --field-separator .env README.md",
            "sort --output=ordinary --key .env README.md",
            "sort -k .env -- README.md",
            "tree -H .env .",
            "tree -T .env .",
            "tree -I .env .",
            "tree -H --fromfile .",
            "find -D tree .",
            "git config --list",
            "git config --file=.claude/settings.json --list",
            "cut -d .env README.md",
            "cut -O .env -c 1- README.md",
            "paste -d .env README.md",
            "paste --delim .env README.md",
            "paste --delim=.env README.md",
            "paste -d ~/.aws/credentials README.md",
            "fmt --pre .env README.md",
            "nl --number-sep .env README.md",
            "column --sep .env README.md",
            "column --tree-id .env README.md",
            "column -p .env README.md",
            "column -n .env README.md",
            "nl -s .env README.md",
            "uniq -s .env README.md",
            "tr .env x",
            "date -d .env",
            "date -d --file=.env",
            "date --date .env",
            "date -s --file=.env",
            "date -- --file=.env",
            "wc --total=.env README.md",
            "file -F .env README.md",
            "printf '%s\\n' '>' .env | head -n 2",
            "printf '%s\\n' \\> .env",
            "grep -F '>' README.md",
            "printf '%s' '2'>output",
            "printf '%s' ok 2>&1",
            "cat <<< .env",
            "cat 3<<< .env",
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "literal option value was treated as a file: {command}"
            );
        }
        assert!(
            sensitive_path_match_for_shell_command("tree -o.env .").is_none(),
            "tree requires its -o path as a separate argument"
        );

        for command in [
            "sort -T .claude README.md",
            "sort -rT.claude README.md",
            "sort --temp=.claude README.md",
            "sort -o.env README.md",
            "sort -ro.env README.md",
            "sort --output=.env README.md",
            "sort --out .env README.md",
            "tree -o .env .",
            "tree -oF .env .",
        ] {
            let hit = sensitive_path_match_for_shell_command(command)
                .expect("file output must remain a write operand");
            assert_eq!(hit.access, PathAccess::Write, "{command}");
        }
        for command in [
            "git config --file=.claude/settings.json --add synthetic.key marker",
            "git config --file=.claude/settings.json synthetic.key marker",
            "git config --file=.claude/settings.json --edit",
        ] {
            let hit = sensitive_path_match_for_shell_command(command)
                .expect("Git config file writes must remain protected");
            assert_eq!(
                hit.sensitivity,
                PathSensitivity::WriteSensitive,
                "{command}"
            );
            assert_eq!(hit.access, PathAccess::Write, "{command}");
        }
        let file_list = sensitive_path_match_for_shell_command("find -files0-from .env")
            .expect("find's file list is a direct read operand");
        assert!(!file_list.inferred);
        let positional = sensitive_path_match_for_shell_command("find -- -files0-from .env")
            .expect("find's post-`--` path is positional");
        assert!(positional.inferred);
        for command in [
            "printf x >| .env",
            "printf x 2>| .env",
            "cat <> .env",
            "printf x >&.env",
        ] {
            let hit = sensitive_path_match_for_shell_command(command)
                .expect("actual file redirection must remain protected");
            assert_eq!(hit.sensitivity, PathSensitivity::Sensitive, "{command}");
            assert_eq!(hit.access, PathAccess::Write, "{command}");
        }
    }

    #[test]
    fn file_option_paths_in_compounds_remain_checked() {
        for command in [
            "{ file --magic-file=.env .claude/README.md; }",
            "{ file -m.env .claude/README.md; }",
            "{ wc --files0-from=.env; stat .claude/README.md; }",
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_some(),
                "{command}"
            );
        }
        assert!(
            sensitive_path_match_for_shell_command(
                "{ stat .claude/README.md; wc --files0-from=items; }"
            )
            .is_none()
        );
        let hit = sensitive_path_match_for_shell_command(
            "{ stat .claude/README.md; wc --files0-from=items; cat .env; }",
        )
        .expect("keep confirmed path matches after an unmodelled option");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
    }

    #[test]
    fn git_history_preserves_protected_pathspecs_and_concrete_credentials() {
        assert!(!git_outputs_metadata_only(
            "log",
            &["--grep".to_string(), "--".to_string(),]
        ));
        for command in [
            r#"git log --oneline -- .claude"#,
            r#"git log --graph --oneline -- .claude"#,
            r#"git log --format=%s -- .claude"#,
            r#"git log --pretty=format:%h -- .claude"#,
            r#"git log --date=iso --format=%ad -- .claude"#,
            r#"git diff --name-only HEAD~1 HEAD -- .claude"#,
            r#"{ git log -3 --oneline HEAD -- .claude; }"#,
            r#"git show ':/release marker'"#,
            r#"git show ':0:.claude/README'"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
        for command in [
            r#"{ git show HEAD -- ':(top).pgpass' .claude/README; }"#,
            r#"git show HEAD -- ':(literal,top).pgpass'"#,
            r#"git diff HEAD -- ':/.pgpass'"#,
            r#"git show ':/.pgpass'"#,
            r#"git show ':.pgpass'"#,
            r#"git show ':0:.pgpass'"#,
            r#"{ git show HEAD -- ':(glob)**/*.txt' ':(top).pgpass'; }"#,
        ] {
            let hit =
                sensitive_path_match_for_shell_command(command).expect("protected literal path");
            assert_eq!(hit.sensitivity, PathSensitivity::Sensitive, "{command}");
            assert_eq!(hit.access, PathAccess::Read, "{command}");
        }
        for command in [
            r#"git log --oneline -p -- .claude"#,
            r#"git diff -p HEAD -- .claude"#,
            r#"git diff --name-only -p HEAD -- .claude"#,
            r#"git log --patch --oneline -- .claude"#,
            r#"git show HEAD -- .claude"#,
            r#"{ git show HEAD -- .claude; }"#,
            r#"{ git show HEAD -- ':(literal,top).claude'; }"#,
            r#"for c in a123; do git show "$c" -- .claude | head -n 5; done"#,
            r#"{ git show HEAD -- ':(glob)**/*.txt' .claude/README; }"#,
            r#"{ git show HEAD ':!*.txt' .claude/README; }"#,
            r#"{ git show 'HEAD:docs/*.txt' .claude/README; }"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn grep_path_access_distinguishes_pattern_values_and_file_operands() {
        for command in [
            r#"grep -ne.env README.md"#,
            r#"grep -A2e.env README.md"#,
            r#"grep -- .env README.md"#,
            r#"grep -e < /dev/null .env README.md"#,
            r#"grep -b -3 --include='*.md' .env README.md"#,
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
        for command in [
            r#"for name in env; do grep -ne. ".$name" README.md; done"#,
            r#"for name in env; do grep -e . ".$name" README.md; done"#,
            r#"for name in env; do grep -f ".$name" README.md; done"#,
            r#"grep -nf.env README.md"#,
            r#"grep --reg -- -nf.env README.md"#,
            r#"grep -n .env -e pattern"#,
            r#"rg -ne. .env README.md"#,
            r#"rg --ignore-file=.env pattern README.md"#,
            r#"for name in env; do rg --no-ignore pattern README.md; cat ".$name"; done"#,
        ] {
            let hit = sensitive_path_match_for_shell_command(command)
                .unwrap_or_else(|| panic!("credential read must gate: {command}"));
            assert_eq!(hit.sensitivity, PathSensitivity::Sensitive, "{command}");
            assert_eq!(hit.access, PathAccess::Read, "{command}");
        }
    }

    #[test]
    fn recursive_grep_protects_search_roots() {
        for command in [
            r#"for p in .claude; do grep -r needle "$p"; done"#,
            "grep -re needle .claude",
            "grep needle .claude --recursive",
            "grep --directories=recurse needle .claude",
            "grep -d recurse needle .claude",
            "grep -d rec needle .claude",
            "grep -dr needle .claude",
            "grep needle .claude --directories=rec",
            "grep --direct=rec needle .claude",
            "grep --rec .claude -e needle",
            "grep .claude --direct=rec --reg -e",
            "grep -d future-action needle .claude",
            "rg needle .claude",
            "ag needle .claude",
            "ag -f needle .claude",
            "ack needle .claude",
            "ack --match=needle .claude",
            "ack --match needle .claude",
        ] {
            let hit = sensitive_path_match_for_shell_command(command).expect(command);
            assert_eq!(
                hit.sensitivity,
                PathSensitivity::WriteSensitive,
                "{command}"
            );
            assert_eq!(hit.access, PathAccess::Read, "{command}");
        }
        for command in [
            "grep needle .claude/README.md",
            "grep -d read needle .claude",
            "grep --directories=skip needle .claude",
            "grep -e .claude README.md",
            "grep -e needle -- README.md -r",
            "rg -r .claude needle README.md",
            "rg needle src",
        ] {
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn classifies_internal_tool_results_as_read_only_artifacts() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&artifact_path),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult)
        );
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Read).is_none());
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Write).is_some());
    }

    #[test]
    fn bash_tool_result_reads_use_the_owner_scoped_reader() {
        let (temp, _guard, artifact) = create_current_session_artifact();
        let read = format!("cat '{}'", artifact.display());
        let data = format!("printf '%s' '{}'", artifact.display());
        assert!(bash_tool_result_denial_in_directory(&data, temp.path()).is_none());
        let error = bash_tool_result_denial_in_directory(&read, temp.path()).unwrap();
        assert!(error.contains("owned tool-result artifact"), "{error}");
        assert!(error.contains("introspect"), "{error}");
        let glob = format!("cat '{}'/call_*", artifact.parent().unwrap().display());
        assert!(bash_tool_result_denial_in_directory(&glob, temp.path()).is_some());
        let handle = "artifact://session/tool-result/result_example";
        assert!(
            bash_tool_result_denial_in_directory(&format!("printf '%s' '{handle}'"), temp.path())
                .is_none()
        );
        assert!(
            bash_tool_result_denial_in_directory(&format!("cat '{handle}'"), temp.path()).is_some()
        );

        let source = artifact
            .strip_prefix(temp.path())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let denial = bash_path_preflight(astra_tools::shell_ops::BashPreflightRequest::Command {
            command: "true",
            directory: temp.path(),
            workspace_root: temp.path(),
            source_artifacts: Some(&[source]),
            workspace_write_admitted: false,
        });
        assert!(
            denial.is_some(),
            "declared sources must use the same ownership boundary"
        );
    }

    #[cfg(unix)]
    #[test]
    fn each_decision_rechecks_artifact_aliases() {
        use std::os::unix::fs::symlink;

        let (temp, _guard, artifact) = create_current_session_artifact();
        let alias = temp.path().join("alias");
        let ordinary = temp.path().join("ordinary");
        std::fs::write(&ordinary, "ordinary file").unwrap();
        symlink(&artifact, &alias).unwrap();

        let command = format!("cat {} {}", alias.display(), alias.display());
        assert_eq!(sensitive_path_match_for_shell_command(&command), None);
        let write = format!("touch {}", alias.display());
        assert!(matches!(
            sensitive_path_match_for_shell_command(&write).map(|hit| hit.sensitivity),
            Some(PathSensitivity::InternalArtifactReadOnly(
                InternalPathKind::SessionToolResult
            ))
        ));

        std::fs::remove_file(&alias).unwrap();
        symlink(&ordinary, &alias).unwrap();
        assert_eq!(sensitive_path_match_for_shell_command(&write), None);
    }

    #[test]
    fn bash_preflight_distinguishes_sensitive_operands_from_literal_data() {
        use astra_tools::shell_ops::BashPreflightRequest;

        let dir = tempfile::tempdir().unwrap();
        let preflight = |command| {
            bash_path_preflight(BashPreflightRequest::Command {
                command,
                directory: dir.path(),
                workspace_root: dir.path(),
                source_artifacts: None,
                workspace_write_admitted: false,
            })
        };
        for command in [
            "sed -e 's/x/y/' -e '1r credentials.db' ordinary",
            "rsync --filter 'merge credentials.db' source ordinary",
            "rsync -r --filter '+ .env' --filter '- *' docs/ out/",
            "rsync -r --include .env --exclude '*' docs/ out/",
            "rsync -r --include=.env --exclude='*' docs/ out/",
            "cp --suffix .env README.md output.txt",
            "cp -S.env README.md output.txt",
            "rsync -b --suffix=.env source output",
            "sed -i .env -e 's/a/b/' ordinary",
        ] {
            assert!(preflight(command).is_some(), "{command}");
        }
        for command in [
            "grep -F .env README.md",
            "echo .env | head -n 1",
            "grep -F /etc/example README.md",
            "rg -F .env src",
            "rg -g '!*.env' needle docs",
            "rg --iglob='!*.env' needle docs",
            "grep -r -F .env docs",
            "printf '%s\\n' .env > output.txt",
            "cp --suffix .claude README.md output.txt",
            "rsync --suffix .env source output",
            "sed -e 's/.env/example/' README.md",
            "rsync --filter '- .env' source output",
            "rsync --exclude .env source output",
            "rsync --exclude=.env source output",
            "echo .env; cp input output",
            "echo .env; cat README.md",
        ] {
            assert!(preflight(command).is_none(), "{command}");
        }
        assert!(preflight("awk 'BEGIN { print \"/etc/example\" }'").is_none());
        assert!(preflight("echo .env | xargs cat").is_none());
        let Some(message) = preflight("echo .env; cat .ENV") else {
            panic!("the actual credential read must be denied");
        };
        assert!(message.contains("blocked the read operand"), "{message}");
        assert!(message.contains(".ENV"), "{message}");
    }

    #[test]
    fn selectors_and_derived_backup_paths_have_uncertain_diagnostics() {
        use astra_tools::shell_ops::BashPreflightRequest;

        let dir = tempfile::tempdir().unwrap();
        for command in [
            "rsync -r --include=.env --exclude='*' docs/ out/",
            "rsync -r --filter '+ .env' docs/ out/",
            "cp -S.env README.md output.txt",
            "cp -Senv docs/example. out/",
            "mv -Senv docs/example. out/",
            "sed -i.env 's/x/y/' .claude/settings.json",
            "sed --in-place=.env 's/x/y/' .claude/settings.json",
        ] {
            let result = bash_path_preflight(BashPreflightRequest::Command {
                command,
                directory: dir.path(),
                workspace_root: dir.path(),
                source_artifacts: None,
                workspace_write_admitted: true,
            });
            assert!(
                matches!(result, Some(ref message) if message.contains("could not establish")),
                "{command}: {result:?}"
            );
        }
    }

    #[test]
    fn scoped_bash_write_candidate_projects_known_writes_from_the_runner_cwd() {
        for command in [
            "printf approved > .claude/settings.json",
            "touch .claude/settings.json",
            "chmod 600 .claude/settings.json",
            "cp README.md .claude/settings.json",
            "cp -t .claude README.md LICENSE",
            "cp --suffix -tfoo README.md .claude/settings.json",
            "touch .claude/a .claude/b",
            "sed -e 's/a/b/' -i .claude/settings.json",
            "git config --file=.claude/settings.json --add synthetic.key marker",
            "sort -o.claude/settings.json README.md",
            "sort --output=.claude/settings.json README.md",
            "tree -o .claude/settings.json .",
            "sed -i -es/a/b/ .claude/settings.json",
            "sed -i bak 's/a/b/' .claude/settings.json",
            "sed -i'.claude/*' 's/a/b/' ordinary",
            "touch .claude/a; printf done",
        ] {
            assert!(
                bash_protected_write_candidate(command, Path::new("/workspace")),
                "{command}"
            );
        }
        for command in [
            "cd nested; printf approved > .claude/settings.json",
            "printf approved > .claude/.ENV",
            "unknown-command .claude/settings.json",
            "bash -c 'touch .claude/settings.json'",
            "touch .claude/$name",
            "sed --unknown rules .claude/settings.json",
            "sed --unknown -i bak 's/a/b/' .claude/settings.json",
            "sed -f rules.sed -i .claude/settings.json",
            "sed --file=rules.sed --in-place .claude/settings.json",
            "sed -i.bak '1r credentials.db' .claude/settings.json",
            "printf approved > .claude/settings.json; cat .env",
            "sort --output=.claude/settings.json .env",
            "cat .env > .claude/settings.json",
            "touch .claude/settings.json; chmod 600 /etc/passwd",
        ] {
            assert!(
                !bash_protected_write_candidate(command, Path::new("/workspace")),
                "{command}"
            );
        }
        assert!(bash_protected_write_candidate(
            "printf approved > settings.json",
            Path::new("/workspace/.claude")
        ));
        assert!(!bash_protected_write_candidate(
            "printf approved > settings.json",
            Path::new("/workspace/src")
        ));
    }

    #[test]
    fn directory_operands_do_not_depend_on_the_process_working_directory() {
        use astra_tools::shell_ops::BashPreflightRequest;

        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("service.envoy");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("README.md"), "ordinary").unwrap();
        for directory in ["output.env", "cache.pem"] {
            std::fs::create_dir(root.path().join(directory)).unwrap();
        }
        std::fs::create_dir_all(root.path().join("fixtures/etc")).unwrap();
        std::fs::create_dir(root.path().join("service.aws")).unwrap();
        std::fs::create_dir(root.path().join(".aws")).unwrap();
        for file in [
            "fixtures/etc/passwd",
            "service.aws/credentials",
            ".aws/credentials",
        ] {
            std::fs::write(root.path().join(file), "synthetic").unwrap();
        }
        std::fs::write(root.path().join(".env"), "synthetic").unwrap();
        for command in [
            "ls service.envoy",
            "cat service.envoy/README.md",
            "ls output.env",
            "ls cache.pem",
            "cat fixtures/etc/passwd",
            "cat service.aws/credentials",
        ] {
            assert!(
                bash_protected_path_denial_in_directory(command, root.path()).is_none(),
                "{command}"
            );
            assert!(
                sensitive_path_match_for_shell_command(command).is_none(),
                "{command}"
            );
        }
        assert!(bash_protected_path_denial_in_directory("cat .env", root.path()).is_some());
        assert!(
            bash_protected_path_denial_in_directory("cat .aws/credentials", root.path()).is_some()
        );
        let result = bash_path_preflight(BashPreflightRequest::Command {
            command: "cat fixtures/etc/passwd",
            directory: root.path(),
            workspace_root: root.path(),
            source_artifacts: None,
            workspace_write_admitted: false,
        });
        assert!(result.is_none(), "{result:?}");
    }

    #[test]
    fn workspaces_under_root_and_log_directories_are_not_blanket_denied() {
        for directory in ["/root/project", "/var/log/project"] {
            let directory = Path::new(directory);
            assert!(bash_protected_path_denial_in_directory("cat README.md", directory).is_none());
            assert!(bash_protected_path_denial_in_directory("cat .env", directory).is_some());
        }
    }

    #[test]
    fn literal_string_arguments_do_not_become_file_accesses() {
        use astra_tools::shell_ops::BashPreflightRequest;

        let root = tempfile::tempdir().unwrap();
        for command in ["test .env && printf yes", "basename .env", "dirname .env"] {
            let result = bash_path_preflight(BashPreflightRequest::Command {
                command,
                directory: root.path(),
                workspace_root: root.path(),
                source_artifacts: None,
                workspace_write_admitted: false,
            });
            assert!(result.is_none(), "{command}: {result:?}");
        }
        for command in ["test -f .env", "basename .env < .env", "cat .env"] {
            assert!(
                bash_protected_path_denial_in_directory(command, root.path()).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn known_forbidden_effects_are_identified_across_compound_commands() {
        let directory = Path::new("/workspace");
        for command in [
            "touch .claude/a; cat .env",
            "cat .env > .claude/a",
            "touch .claude/a; printf x < .env",
        ] {
            let denial = bash_known_forbidden_path_denial(command, directory);
            assert!(denial.is_some(), "{command}: {denial:?}");
        }
        for command in [
            "touch .claude/a; cat \"$file\"",
            "touch .claude/a; cat --help .env",
            "touch .claude/a; printf .env",
            "touch .claude/a; test .env",
            "touch .claude/a; basename .env",
        ] {
            assert!(
                bash_known_forbidden_path_denial(command, directory).is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn approved_workspace_writes_do_not_waive_reads_or_outside_paths() {
        use astra_tools::shell_ops::BashPreflightRequest;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".claude")).unwrap();
        std::fs::create_dir(outside.path().join(".claude")).unwrap();
        let check = |command: &str| {
            bash_path_preflight(BashPreflightRequest::Command {
                command,
                directory: root.path(),
                workspace_root: root.path(),
                source_artifacts: None,
                workspace_write_admitted: true,
            })
        };
        assert!(check("touch .claude/a .claude/b").is_none());
        assert!(check("sort --output=.claude/settings.json README.md").is_none());
        assert!(check("tree -o .claude/settings.json .").is_none());
        assert!(
            check("git config --file=.claude/settings.json --add synthetic.key marker").is_none()
        );
        let denied_read = check("sort --output=.claude/settings.json .env")
            .expect("an approved workspace write must not waive a protected input");
        assert!(
            denied_read.contains("blocked the read operand"),
            "{denied_read}"
        );
        for command in [
            "sort --output=.claude/settings.json --files0-from=.env".to_string(),
            "tree -o .claude/settings.json -H . --hintro=.env".to_string(),
            "git config --file=.claude/settings.json --add synthetic.key marker; cat .env"
                .to_string(),
            "touch .claude/a; grep -r needle .claude".to_string(),
            "touch .claude/a; git show HEAD -- .claude".to_string(),
            format!(
                "chmod 600 {}/.claude/settings.json",
                outside.path().display()
            ),
            "sed -f credentials.db -i .claude/settings.json".to_string(),
            "sed --file=credentials.db -i .claude/settings.json".to_string(),
            "sed -fcredentials.db -i .claude/settings.json".to_string(),
            "sed -i --unknown script .claude/settings.json".to_string(),
            "sed -i.bak '1r credentials.db' .claude/settings.json".to_string(),
            "rsync --files-from=credentials.db source .claude/output".to_string(),
            "rsync --filter='merge credentials.db' source .claude/output".to_string(),
            "touch --reference=credentials.db .claude/settings.json".to_string(),
            "chmod --reference=credentials.db .claude/settings.json".to_string(),
            "install -s --strip-program=credentials.db source .claude/settings.json".to_string(),
        ] {
            assert!(check(&command).is_some(), "{command}");
        }
        // A shell may change directory after launch; this preflight does not
        // claim to resolve subsequent relative operands against that state.
        assert!(check("cd .claude; touch settings.json").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn bash_path_preflight_uses_the_invocation_directory_and_resolves_aliases() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(dir.path().join(".env"), "synthetic").unwrap();
        symlink("../.env", nested.join("alias")).unwrap();
        let denial = bash_protected_path_denial_in_directory("cat alias", &nested).unwrap();
        assert!(denial.contains("sensitive credential"), "{denial}");
        assert!(bash_protected_path_denial_in_directory("cat alias", dir.path()).is_none());

        let sensitive_directory = dir.path().join(".ssh");
        std::fs::create_dir(&sensitive_directory).unwrap();
        std::fs::write(sensitive_directory.join("config"), "synthetic").unwrap();
        assert!(
            bash_protected_path_denial_in_directory("cat config", &sensitive_directory).is_some()
        );
        std::fs::create_dir(dir.path().join("output.env")).unwrap();
        symlink("output.env", dir.path().join("directory.pem")).unwrap();
        assert!(bash_protected_path_denial_in_directory("ls directory.pem", dir.path()).is_none());
        symlink(".ssh", dir.path().join("ordinary-directory")).unwrap();
        assert!(
            bash_protected_path_denial_in_directory("ls ordinary-directory", dir.path()).is_some()
        );

        // A lexical system path remains protected even when the system
        // directory itself is a symlink on this host.
        for (directory, command) in [
            (Path::new("/tmp"), "cat ../etc/hosts"),
            (Path::new("/"), "cat etc/hosts"),
        ] {
            assert!(
                bash_protected_path_denial_in_directory(command, directory).is_some(),
                "{directory:?}: {command}"
            );
        }
        let canonical_tmp = Path::new("/tmp").canonicalize().unwrap();
        assert!(
            bash_protected_path_denial_in_directory("cat ../etc/hosts", &canonical_tmp).is_some()
        );
        assert!(bash_protected_path_denial_in_directory("cat /dev/null", &canonical_tmp).is_none());

        let skills = dir.path().join(".claude/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(dir.path().join(".claude/settings.json"), "{}").unwrap();
        symlink("../settings.json", skills.join("settings-link")).unwrap();
        assert!(
            bash_protected_path_denial_in_directory(
                "touch .claude/skills/settings-link",
                dir.path()
            )
            .is_some()
        );
        assert!(
            bash_protected_path_denial_in_directory("touch .claude/skills/ordinary.md", dir.path())
                .is_none()
        );
    }

    #[test]
    fn classifies_current_session_journal_as_read_only_artifact() {
        let (_temp, _guard, journal_path) = create_current_session_journal();
        let journal_path = journal_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&journal_path),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionJournal)
        );
        assert!(path_requires_sensitive_gate(&journal_path, PathAccess::Read).is_none());
        assert!(path_requires_sensitive_gate(&journal_path, PathAccess::Write).is_some());

        let grep_args = serde_json::json!({
            "pattern": "str_replace|str replace",
            "path": journal_path.to_string()
        });
        assert_eq!(sensitive_path_match_for_tool_args("grep", &grep_args), None);
    }

    #[test]
    fn session_root_and_diagnostics_are_read_only_auto_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_root = temp.path().join(".astra/sessions");
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let store = astra_services::local_session_artifact_store();
        let session_dir = store.session_dir(session_id).unwrap();
        let checkpoint_dir = session_dir.join("step_checkpoints");
        let checkpoint = checkpoint_dir.join("000001-heavy.json");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(store.journal_path(session_id).unwrap(), "{}\n").unwrap();
        std::fs::write(session_dir.join("workspace.yaml"), "session_id: test\n").unwrap();
        std::fs::write(&checkpoint, "{}\n").unwrap();

        assert_eq!(
            classify_path_sensitivity(&sessions_root.to_string_lossy()),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionRoot)
        );
        assert_eq!(
            classify_path_sensitivity(&session_dir.to_string_lossy()),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionDirectory)
        );
        assert_eq!(
            classify_path_sensitivity(&session_dir.join("workspace.yaml").to_string_lossy()),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionDiagnostic)
        );
        assert_eq!(
            classify_path_sensitivity(&checkpoint.to_string_lossy()),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionDiagnostic)
        );

        let list_args = serde_json::json!({ "path": sessions_root.to_string_lossy().to_string() });
        assert_eq!(
            sensitive_path_match_for_tool_args("list_dir", &list_args),
            None
        );
        assert_eq!(sensitive_path_match_for_tool_args("glob", &list_args), None);

        assert_eq!(
            sensitive_path_match_for_shell_command(&format!(
                "ls -lt {} | head -20",
                sessions_root.display()
            )),
            None
        );
        assert_eq!(
            sensitive_path_match_for_shell_command(&format!(
                "ls -d {}/*/ | tail -10",
                sessions_root.display()
            )),
            None
        );
        assert_eq!(
            sensitive_path_match_for_shell_command(&format!(
                "ls -lt {}/*-heavy.json | head -3",
                checkpoint_dir.display()
            )),
            None
        );

        let content_glob = sensitive_path_match_for_shell_command(&format!(
            "cat {}/*-heavy.json",
            checkpoint_dir.display()
        ))
        .expect("content reads through globs should still gate");
        assert_eq!(content_glob.access, PathAccess::Read);
        assert_eq!(content_glob.sensitivity, PathSensitivity::WriteSensitive);

        let write_hit =
            path_requires_sensitive_gate(&checkpoint.to_string_lossy(), PathAccess::Write)
                .expect("writes to diagnostics must gate");
        assert_eq!(
            write_hit.sensitivity,
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionDiagnostic)
        );
    }

    #[test]
    fn arbitrary_astra_tool_results_are_not_permission_internal_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let artifact_path = temp
            .path()
            .join(".astra/sessions/session-1/tool-results/call_abc.txt");
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, "child output").unwrap();
        let artifact_path = artifact_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&artifact_path),
            PathSensitivity::WriteSensitive
        );
        assert!(path_requires_sensitive_gate(&artifact_path, PathAccess::Read).is_none());
        let hit = path_requires_sensitive_gate(&artifact_path, PathAccess::Write).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::WriteSensitive);
    }

    #[test]
    fn arbitrary_astra_session_journals_are_not_session_journals() {
        let temp = tempfile::tempdir().unwrap();
        let journal_path = temp
            .path()
            .join(".astra/sessions/550e8400-e29b-41d4-a716-446655440000.jsonl");
        std::fs::create_dir_all(journal_path.parent().unwrap()).unwrap();
        std::fs::write(&journal_path, "{}\n").unwrap();
        let journal_path = journal_path.to_string_lossy();

        assert_eq!(
            classify_path_sensitivity(&journal_path),
            PathSensitivity::WriteSensitive
        );
        assert!(path_requires_sensitive_gate(&journal_path, PathAccess::Read).is_none());
        let hit = path_requires_sensitive_gate(&journal_path, PathAccess::Write).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::WriteSensitive);
    }

    #[test]
    fn hidden_project_paths_are_not_application_state() {
        let home = dirs::home_dir().unwrap();
        for path in [
            "~/.cache/projects/app/src/main.rs".to_string(),
            "~/.codex/reviews/workspace/service.conf".to_string(),
            home.join(".codex/reviews/workspace/service.conf")
                .display()
                .to_string(),
            "~/.xxx/logs/session.log".to_string(),
        ] {
            assert_eq!(
                classify_path_sensitivity(&path),
                PathSensitivity::Normal,
                "{path}"
            );
            assert!(path_requires_sensitive_gate(&path, PathAccess::Write).is_none());
        }
        assert!(
            sensitive_path_match_for_shell_command(
                "printf value > ~/.cache/projects/app/service.conf"
            )
            .is_none()
        );
        for path in [
            "~/.ssh/id_rsa",
            "~/.aws/credentials",
            "~/.xxx/.env",
            "~/.astra/permissions.json",
        ] {
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Write).is_some(),
                "{path}"
            );
        }
    }

    #[test]
    #[serial_test::serial(path_sensitivity_home)]
    fn agent_skill_content_is_editable_hidden_app_content() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("HOME", temp.path());
        let absolute_astra = temp
            .path()
            .join(".astra/skills/code-review/SKILL.md")
            .to_string_lossy()
            .to_string();

        for path in [
            ".astra/skills/code-review/SKILL.md",
            "./.claude/skills/code-review/SKILL.md",
            "~/.astra/skills/code-review/SKILL.md",
            "$HOME/.claude/skills/code-review/SKILL.md",
            absolute_astra.as_str(),
        ] {
            assert_eq!(
                classify_path_sensitivity(path),
                PathSensitivity::Normal,
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Write).is_none(),
                "{path}"
            );

            let write_args = serde_json::json!({ "path": path, "content": "# Skill\n" });
            assert_eq!(
                sensitive_path_match_for_tool_args("write_file", &write_args),
                None,
                "{path}"
            );
        }
    }

    #[test]
    #[serial_test::serial(path_sensitivity_home)]
    fn agent_skill_credentials_and_control_files_remain_sensitive() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = EnvGuard::set("HOME", temp.path());

        for path in [
            "~/.astra/skills/code-review/.env",
            "~/.claude/skills/code-review/credentials.json",
        ] {
            assert_eq!(
                classify_path_sensitivity(path),
                PathSensitivity::Sensitive,
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Read).is_some(),
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Write).is_some(),
                "{path}"
            );
        }

        for path in [
            "~/.astra/permissions.json",
            "~/.astra/config.toml",
            "~/.claude/settings.json",
        ] {
            assert_eq!(
                classify_path_sensitivity(path),
                PathSensitivity::WriteSensitive,
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Read).is_none(),
                "{path}"
            );
            assert!(
                path_requires_sensitive_gate(path, PathAccess::Write).is_some(),
                "{path}"
            );
        }
    }

    #[test]
    fn shell_writes_to_agent_skill_content_do_not_trip_sensitive_gate() {
        assert_eq!(
            sensitive_path_match_for_shell_command(
                "cat > ~/.astra/skills/code-review/SKILL.md <<'EOF'\n# Skill\nEOF"
            ),
            None
        );
        assert_eq!(
            sensitive_path_match_for_shell_command(
                "mkdir -p .claude/skills/code-review && touch .claude/skills/code-review/SKILL.md"
            ),
            None
        );

        let hit = sensitive_path_match_for_shell_command("cat ~/.claude/skills/code-review/.env")
            .expect("credential file under a skill must still gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(hit.access, PathAccess::Read);
    }

    #[test]
    fn credential_files_remain_read_sensitive() {
        for path in [
            "~/.ssh/id_rsa",
            "~/.aws/credentials",
            "~/.xxx/.env",
            "~/.yyy/credentials.json",
            "~/.zzz/token.pem",
            ".env",
            "config/secrets.toml",
        ] {
            let hit = path_requires_sensitive_gate(path, PathAccess::Read).expect("gate");
            assert_eq!(hit.sensitivity, PathSensitivity::Sensitive, "{path}");
        }

        let shell_read = sensitive_path_match_for_shell_command("cat ~/.xxx/.env")
            .expect("credential-shaped files under hidden app state should still gate");
        assert_eq!(shell_read.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(shell_read.access, PathAccess::Read);
    }

    #[test]
    fn per_session_jsonl_files_are_not_session_journals() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let session_dir = artifact_path
            .parent()
            .and_then(std::path::Path::parent)
            .expect("session dir");
        let messages_path = session_dir.join("messages.jsonl");
        std::fs::write(&messages_path, "{}\n").unwrap();
        let messages_path = messages_path.to_string_lossy();

        assert!(!matches!(
            classify_path_sensitivity(&messages_path),
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionJournal)
        ));
    }

    #[test]
    fn sensitive_path_match_for_tool_args_detects_shell_file_operands() {
        let args = serde_json::json!({
            "command": "cat ~/.ssh/id_rsa"
        });
        let hit = sensitive_path_match_for_tool_args("bash", &args).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(hit.access, PathAccess::Read);
    }

    #[test]
    fn sensitive_path_match_for_tool_args_ignores_grep_pattern_text() {
        let args = serde_json::json!({
            "command": r#"grep -n "~/.ssh/id_rsa\|credentials.json" crates/astra-cli/src/edge_tools/shell.rs"#
        });
        assert_eq!(sensitive_path_match_for_tool_args("bash", &args), None);
    }

    #[test]
    fn sensitive_path_match_for_tool_args_detects_grep_file_operand() {
        let args = serde_json::json!({
            "command": "grep -n needle ~/.ssh/id_rsa"
        });
        let hit = sensitive_path_match_for_tool_args("bash", &args).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
        assert_eq!(hit.access, PathAccess::Read);
    }

    #[test]
    fn sensitive_path_match_for_shell_command_allows_internal_artifact_reads() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy().to_string();
        let command =
            format!("cat {artifact_path} | python3 -c 'import sys; print(sys.stdin.read())'");

        assert_eq!(sensitive_path_match_for_shell_command(&command), None);
    }

    #[test]
    fn sensitive_path_match_for_shell_command_gates_internal_artifact_writes() {
        let (_temp, _guard, artifact_path) = create_current_session_artifact();
        let artifact_path = artifact_path.to_string_lossy().to_string();
        let command = format!("rm -f {artifact_path}");

        let hit = sensitive_path_match_for_shell_command(&command).expect("gate");
        assert_eq!(
            hit.sensitivity,
            PathSensitivity::InternalArtifactReadOnly(InternalPathKind::SessionToolResult)
        );
        assert_eq!(hit.access, PathAccess::Write);
    }

    #[test]
    fn sensitive_path_match_for_tool_args_uses_structured_path_field() {
        let args = serde_json::json!({ "path": "~/.ssh/id_rsa" });
        let hit = sensitive_path_match_for_tool_args("read_file", &args).expect("gate");
        assert_eq!(hit.sensitivity, PathSensitivity::Sensitive);
    }
}
