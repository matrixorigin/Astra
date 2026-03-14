"""SecurityGuard — pre-execution static analysis via AST."""

import ast
from dataclasses import dataclass, field


@dataclass
class SecurityIssue:
    category: str  # "dangerous_import", "dangerous_call", "dangerous_attr", "syntax_error"
    description: str
    line: int


@dataclass
class SecurityVerdict:
    safe: bool
    issues: list[SecurityIssue] = field(default_factory=list)


# Modules that can escape the sandbox
DEFAULT_DENY_IMPORTS = frozenset(
    {
        "os",
        "subprocess",
        "sys",
        "shutil",
        "socket",
        "ctypes",
        "pickle",
        "multiprocessing",
        "http",
        "ftplib",
        "telnetlib",
        "signal",
        "importlib",
        "pathlib",
        "glob",
        "tempfile",
        "webbrowser",
        "code",
        "codeop",
        "compileall",
        "py_compile",
    }
)

# Safe for data work
DEFAULT_ALLOW_IMPORTS = frozenset(
    {
        "json",
        "math",
        "datetime",
        "re",
        "collections",
        "itertools",
        "functools",
        "typing",
        "dataclasses",
        "decimal",
        "statistics",
        "csv",
        "io",
        "hashlib",
        "uuid",
        "string",
        "textwrap",
        "enum",
        "copy",
        "operator",
        "numbers",
        "fractions",
        "random",
        "bisect",
        "heapq",
        "array",
        "pprint",
    }
)

# Dangerous builtins
DANGEROUS_CALLS = frozenset(
    {
        "exec",
        "eval",
        "compile",
        "__import__",
        "globals",
        "locals",
        "getattr",
        "setattr",
        "delattr",
        "breakpoint",
        "exit",
        "quit",
        "open",  # File I/O — should use DB instead
    }
)

# Dangerous attribute access patterns (bypass vectors)
DANGEROUS_ATTRS = frozenset(
    {
        "__builtins__",
        "__subclasses__",
        "__bases__",
        "__mro__",
        "__class__",
        "__globals__",
        "__code__",
        "__func__",
        "__self__",
        "__dict__",
        "__init_subclass__",
    }
)

# Dangerous names used as identifiers (not calls)
DANGEROUS_NAMES = frozenset(
    {
        "__builtins__",
        "__loader__",
        "__spec__",
    }
)


class SecurityGuard:
    """Pre-execution static analysis. Rejects dangerous code before it reaches any runtime.

    This is defense-in-depth. The primary security boundary is:
    - SubprocessRuntime: rlimit + timeout + tmpdir
    - DockerRuntime: container isolation + gVisor
    - DataContext: DB user permissions (read-only user can't write)

    AST analysis catches obvious dangerous patterns early, before code reaches the runtime.
    It is NOT a complete sandbox — determined attackers can bypass AST analysis.
    """

    def __init__(
        self,
        deny_imports: set[str] | None = None,
        allow_imports: set[str] | None = None,
    ):
        self.deny_imports = deny_imports or set(DEFAULT_DENY_IMPORTS)
        self.allow_imports = allow_imports or set(DEFAULT_ALLOW_IMPORTS)

    def analyze(
        self,
        code: str,
        language: str = "python",
        extra_allowed: list[str] | None = None,
    ) -> SecurityVerdict:
        if language != "python":
            return SecurityVerdict(
                safe=False,
                issues=[
                    SecurityIssue(
                        "unsupported", f"Language {language} not supported for analysis", 0
                    )
                ],
            )

        try:
            tree = ast.parse(code)
        except SyntaxError as e:
            return SecurityVerdict(
                safe=False, issues=[SecurityIssue("syntax_error", str(e), e.lineno or 0)]
            )

        allowed = self.allow_imports | set(extra_allowed or [])
        issues: list[SecurityIssue] = []

        for node in ast.walk(tree):
            # Check imports
            if isinstance(node, ast.Import):
                for alias in node.names:
                    root = alias.name.split(".")[0]
                    self._check_import(root, node.lineno, allowed, issues)

            elif isinstance(node, ast.ImportFrom):
                if node.module:
                    root = node.module.split(".")[0]
                    self._check_import(root, node.lineno, allowed, issues)

            # Check dangerous calls
            elif isinstance(node, ast.Call):
                name = self._get_call_name(node)
                if name in DANGEROUS_CALLS:
                    issues.append(
                        SecurityIssue(
                            "dangerous_call",
                            f"Call to '{name}' is not allowed",
                            node.lineno,
                        )
                    )

            # Check dangerous attribute access (__builtins__, __subclasses__, etc.)
            elif isinstance(node, ast.Attribute):
                if node.attr in DANGEROUS_ATTRS:
                    issues.append(
                        SecurityIssue(
                            "dangerous_attr",
                            f"Access to '{node.attr}' is not allowed",
                            node.lineno,
                        )
                    )

            # Check dangerous name references
            elif isinstance(node, ast.Name):
                if node.id in DANGEROUS_NAMES:
                    issues.append(
                        SecurityIssue(
                            "dangerous_name",
                            f"Reference to '{node.id}' is not allowed",
                            node.lineno,
                        )
                    )

        return SecurityVerdict(safe=len(issues) == 0, issues=issues)

    def _check_import(
        self,
        module: str,
        line: int,
        allowed: set[str],
        issues: list[SecurityIssue],
    ) -> None:
        if module in self.deny_imports:
            issues.append(
                SecurityIssue(
                    "dangerous_import",
                    f"Import '{module}' is blocked",
                    line,
                )
            )
        elif module not in allowed:
            # Unknown module — not in deny or allow list. Allow it but could be stricter.
            pass

    @staticmethod
    def _get_call_name(node: ast.Call) -> str:
        if isinstance(node.func, ast.Name):
            return node.func.id
        if isinstance(node.func, ast.Attribute):
            return node.func.attr
        return ""
