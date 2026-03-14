"""Doctor — diagnostic checks for mo-agent CLI."""

import platform
import sys

from rich.console import Console
from rich.table import Table


def run_doctor(console: Console, client=None) -> list[tuple[str, bool, str]]:
    """Run diagnostic checks and print results. Returns list of (name, passed, detail)."""
    checks: list[tuple[str, bool, str]] = []

    # Python version
    v = sys.version_info
    ok = v >= (3, 11)
    checks.append(("Python ≥ 3.11", ok, f"{v.major}.{v.minor}.{v.micro}"))

    # rich importable
    try:
        import rich
        from importlib.metadata import version as pkg_version

        checks.append(("rich", True, pkg_version("rich")))
    except ImportError:
        checks.append(("rich", False, "not installed"))

    # prompt_toolkit importable
    try:
        import prompt_toolkit
        from importlib.metadata import version as pkg_version

        checks.append(("prompt_toolkit", True, pkg_version("prompt_toolkit")))
    except ImportError:
        checks.append(("prompt_toolkit", False, "not installed"))

    # API reachable
    if client:
        try:
            client.ensure_authenticated()
            checks.append(("API reachable", True, client.base_url))
        except Exception:
            # Try a simple connection test
            try:
                import httpx

                r = httpx.get(f"{client.base_url}/health", timeout=3)
                checks.append(("API reachable", r.status_code < 500, client.base_url))
            except Exception:
                checks.append(("API reachable", False, client.base_url))
    else:
        checks.append(("API reachable", False, "no client"))

    # Auth status
    if client:
        try:
            result = client.ensure_authenticated()
            checks.append(("Authenticated", result is True, ""))
        except Exception:
            checks.append(("Authenticated", False, ""))
    else:
        checks.append(("Authenticated", False, "no client"))

    # Project detection
    try:
        from pathlib import Path
        import subprocess

        git_result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            timeout=3,
        )
        if git_result.returncode == 0:
            checks.append(("Git repo", True, git_result.stdout.strip()))
        else:
            checks.append(("Git repo", False, "not a git repo"))
    except Exception:
        checks.append(("Git repo", False, "git not found"))

    # Print table
    t = Table(title="mo-agent doctor", show_header=True, border_style="bright_black")
    t.add_column("Check", style="cyan")
    t.add_column("Status", width=4)
    t.add_column("Detail", style="dim")
    for name, passed, detail in checks:
        icon = "[green]✓[/green]" if passed else "[red]✗[/red]"
        t.add_row(name, icon, detail)
    console.print(t)

    all_ok = all(ok for _, ok, _ in checks)
    if all_ok:
        console.print("\n[green]✓ Everything looks good![/green]")
    else:
        console.print("\n[yellow]Some checks failed. See above.[/yellow]")

    return checks
