# Shell & Permission Security Improvements

Layered security model adapted to the existing Rust codebase.

## Changes Summary

### 1. Permission/Approval System (VERY HIGH — Security)

**File: `rust/crates/astra-cli/src/cli/permission_manager.rs`**

- **Rule-based permissions**: Added `PermissionRule` with glob-style matching (`Bash(git commit:*)`)
- **Settings persistence**: `PermissionSettings` loads/saves from `.kiro/permissions.json`
- **Layered evaluation order**:
  1. Deny rules (bypass-immune — checked even with auto_approve)
  2. Session overrides
  3. Git safety checks (bypass-immune)
  4. Dangerous file path checks (bypass-immune)
  5. Execute decision (deny/allowlist/ask)
  6. Persistent allow rules
  7. Auto-approve mode
  8. Interactive prompt
- **New constructor**: `with_project()` loads rules from project directory
- **New method**: `add_allow_rule()` persists rules to disk

### 2. Git Commit/Push Safety (HIGH — Safety)

**New file: `rust/crates/runtime/src/tool_sandbox/git_safety.rs`**

Validates git commands before execution:
- **Commit message injection**: Blocks `$()`, backticks, `${}` in double-quoted `-m` messages
- **Argument injection**: Blocks commit messages starting with `-`
- **Hook-skip blocking**: Flags `--no-verify`, `--no-gpg-sign`, `--no-signoff`
- **Force push detection**: Flags `--force`, `-f`, `--force-with-lease`
- **Compound cd+git blocking**: Prevents `cd /evil && git status` (bare repo attack vector)
- **Git config injection**: Blocks `git -c` (arbitrary config = code execution via core.fsmonitor)
- **Exec path manipulation**: Blocks `--exec-path` and `--config-env`
- **Commit amend detection**: Flags `--amend` for explicit approval
- **Bare repo detection**: `is_bare_git_repo()` checks for HEAD + objects/ + refs/ without .git/HEAD

### 3. Shell Output Streaming (HIGH — UX)

**File: `rust/crates/astra-cli/src/edge_tools/shell.rs`**

- **`run_command_streaming()`**: New streaming execution function alongside existing `run_command_with_cleanup()`
  - Reads stdout/stderr incrementally via background threads
  - Calls `on_output` callback for real-time display
  - Output truncation at 30K chars with `[output truncated]` marker
  - Returns `StreamingResult` with output, exit_code, and backgrounded flag

### 4. Timeout Auto-Backgrounding (MEDIUM — UX)

**File: `rust/crates/astra-cli/src/edge_tools/shell.rs`**

- **Auto-backgrounding**: When `allow_background=true` and timeout hits, the process is detached instead of killed
- **Size watchdog**: Background thread monitors process, kills after 30 minutes max
- **Graceful degradation**: Falls back to hard kill when `allow_background=false`

### 5. Security Hardening (MEDIUM — Hardening)

**New file: `rust/crates/runtime/src/tool_sandbox/shell_hardening.rs`**

- **Extglob disable**: `shopt -u extglob || setopt NO_EXTENDED_GLOB` (bash/zsh compatible)
  - Prevents malicious filename expansion after security validation
- **IFS reset**: `IFS=$' \t\n'` prevents word-splitting attacks
- **Stdin redirect**: `< /dev/null` prevents commands from reading spawn's stdin pipe
  - Smart: skips for heredocs, existing redirects, and pipe commands
- **Secret env scrubbing**: `scrub_secrets_from_env()` removes API keys, tokens, passwords
  - Known list: ANTHROPIC_API_KEY, OPENAI_API_KEY, AWS_SECRET_ACCESS_KEY, etc.
  - Heuristic: removes any var containing SECRET, TOKEN, PASSWORD, PRIVATE_KEY
  - GitHub Actions: removes INPUT_* prefix variants
- **Dangerous file paths**: `DANGEROUS_FILE_PATHS` constant + `is_dangerous_file_path()` check
  - .git/, .bashrc, .zshrc, .ssh/, .kiro/, .vscode/, etc.

**File: `rust/crates/runtime/src/tool_sandbox/command.rs`**

- `wrap_command_with_limits()` now applies shell hardening preamble in Standard+ modes
- `filter_environment()` now scrubs secrets in Standard+ modes

## Test Coverage

| Area | Tests | Status |
|------|-------|--------|
| Git safety | 12 tests | ✅ All pass |
| Shell hardening | 12 tests | ✅ All pass |
| Permission rules | 10 new tests | ✅ All pass |
| Existing tool_sandbox | 89 tests | ✅ All pass |
| Existing permission_manager | 23 tests | ✅ All pass |
| Full runtime suite | 2312 tests | ✅ All pass |
| Full CLI suite | 852 tests | ✅ All pass |

## Architecture

The implementation follows a layered security model:

```
Module Mapping
─────────────────────────────────────────────────────────────
permission_manager.rs   — PermissionRule + PermissionSettings
git_safety.rs           — validate_git_command, is_bare_git_repo
shell_hardening.rs      — build_hardened_command, scrub_secrets_from_env, DANGEROUS_FILE_PATHS
shell.rs                — run_command_streaming, size_watchdog
```
