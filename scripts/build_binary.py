#!/usr/bin/env python3
"""Build trust-mem-lite into a single-file binary using PyInstaller.

Usage:
    pip install pyinstaller
    python scripts/build_binary.py

Output:
    dist/trustmem              (single-file binary)

The binary bundles the CLI + MCP server. No Python or pip needed at runtime.
Only exception: local embedding requires a separate `pip install sentence-transformers`.
"""
import platform
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Entry point: the CLI that also spawns the MCP server
ENTRY = ROOT / "cli" / "mo_memory_cli.py"

# Packages to collect (PyInstaller misses some with lazy imports)
HIDDEN_IMPORTS = [
    "mo_memory_mcp",
    "mo_memory_mcp.server",
    "mo_memory_mcp.schema",
    "mcp.server",
    "mcp.server.stdio",
    "mcp.shared",
    "mcp.types",
    "pymysql",
    "pymysql.cursors",
    "sqlalchemy.dialects.mysql",
    "sqlalchemy.dialects.mysql.pymysql",
    "pydantic",
    "httpx",
    "matrixone",
    "matrixone.sqlalchemy_ext",
]

# Data files: steering rule templates
DATAS = [
    (str(ROOT / "mo_memory_mcp" / "templates"), "mo_memory_mcp/templates"),
]

# Packages to exclude (not needed, saves ~50MB+)
EXCLUDES = [
    "cryptography",
    "bcrypt",
    "numpy",
    "scipy",
    "torch",
    "sentence_transformers",
    "transformers",
    "PIL",
    "matplotlib",
    "tkinter",
    "unittest",
    "xmlrpc",
    "pydoc",
]


def build() -> None:
    arch = platform.machine()
    system = platform.system().lower()
    name = f"trustmem-{system}-{arch}"

    cmd = [
        sys.executable, "-m", "PyInstaller",
        "--onefile",
        "--name", name,
        "--strip",
        "--noconfirm",
    ]

    for mod in HIDDEN_IMPORTS:
        cmd += ["--hidden-import", mod]

    for src, dst in DATAS:
        cmd += ["--add-data", f"{src}:{dst}"]

    for exc in EXCLUDES:
        cmd += ["--exclude-module", exc]

    # Add project root to path so imports resolve
    cmd += ["--paths", str(ROOT)]

    cmd.append(str(ENTRY))

    print(f"Building {name}...")
    print(f"  Entry: {ENTRY}")
    print(f"  Command: {' '.join(cmd[:6])}...")
    subprocess.check_call(cmd, cwd=ROOT)

    out = ROOT / "dist" / name
    size_mb = out.stat().st_size / 1024 / 1024
    print(f"\n✅ Built: {out}")
    print(f"   Size: {size_mb:.1f} MB")
    print(f"\n   Test: ./dist/{name} --help")


if __name__ == "__main__":
    build()
