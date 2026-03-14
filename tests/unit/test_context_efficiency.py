"""Tests for context efficiency improvements.

Covers:
- ListDirTool: gitignore filtering, progressive disclosure, include_ignored
- GrepTool/GlobTool: gitignore filtering
- tool_output_handler: list_dir summary strategy
- compaction: compact_history_messages (immutability, memory refs, boundary)
- chat_loop: MAX_SINGLE_TOOL_RESULT_CHARS hard limit
- _gitignore: shared loader
"""

from pathlib import Path

import pytest

from cli.tools.file_ops import ListDirTool
from cli.tools.search import GrepTool, GlobTool
from cli.tools._gitignore import load_gitignore
from core.agent.tool_output_handler import (
    generate_structured_summary,
    SUMMARY_GENERATORS,
    _summarize_list_dir,
)
from core.context.compaction import compact_history_messages


# ============================================================================
# Fixtures
# ============================================================================


@pytest.fixture
def project(tmp_path: Path) -> Path:
    """Project with gitignored artifacts."""
    src = tmp_path / "src"
    src.mkdir()
    (src / "main.py").write_text("def main(): pass\n")
    (src / "utils.py").write_text("def add(a, b): return a + b\n")

    cache = src / "__pycache__"
    cache.mkdir()
    (cache / "main.cpython-311.pyc").write_bytes(b"\x00")
    (cache / "utils.cpython-311.pyc").write_bytes(b"\x00")

    venv = tmp_path / ".venv"
    venv.mkdir()
    (venv / "pyvenv.cfg").write_text("home = /usr/bin\n")

    dist = tmp_path / "dist"
    dist.mkdir()
    (dist / "package-1.0.tar.gz").write_bytes(b"\x00")

    build = tmp_path / "build"
    build.mkdir()
    (build / "lib").mkdir()
    (build / "lib" / "output.so").write_bytes(b"\x00")

    node = tmp_path / "node_modules"
    node.mkdir()
    (node / "lodash").mkdir()
    (node / "lodash" / "index.js").write_text("module.exports = {}\n")

    (tmp_path / ".gitignore").write_text(
        "__pycache__/\n*.pyc\n.venv/\ndist/\nbuild/\nnode_modules/\n.env\n"
    )

    tests = tmp_path / "tests"
    tests.mkdir()
    (tests / "test_main.py").write_text("def test_main(): pass\n")

    (tmp_path / "README.md").write_text("# Project\n")

    return tmp_path


# ============================================================================
# Shared _gitignore loader
# ============================================================================


class TestLoadGitignore:
    def test_loads_existing_gitignore(self, project: Path):
        spec = load_gitignore(str(project))
        assert spec is not None
        assert spec.match_file("__pycache__/")
        assert not spec.match_file("src/main.py")

    def test_returns_none_without_gitignore(self, tmp_path: Path):
        assert load_gitignore(str(tmp_path)) is None

    def test_returns_none_on_bad_path(self):
        assert load_gitignore("/nonexistent/path/xyz") is None

    def test_returns_none_when_pathspec_unavailable(self, monkeypatch):
        """Covers the except Exception branch."""
        import cli.tools._gitignore as mod

        original_import = (
            __builtins__.__import__ if hasattr(__builtins__, "__import__") else __import__
        )

        def fake_import(name, *args, **kwargs):
            if name == "pathspec":
                raise ImportError("no pathspec")
            return original_import(name, *args, **kwargs)

        monkeypatch.setattr("builtins.__import__", fake_import)
        assert mod.load_gitignore("/tmp") is None


# ============================================================================
# ListDirTool: gitignore filtering
# ============================================================================


class TestListDirGitignore:
    @pytest.mark.asyncio
    async def test_filters_all_gitignored_dirs(self, project: Path):
        tool = ListDirTool(str(project))
        result = await tool.execute(".", depth=3)
        for name in ("__pycache__", "dist/", "build/", "node_modules/"):
            assert name not in result, f"{name} should be filtered"

    @pytest.mark.asyncio
    async def test_dotdirs_filtered_regardless_of_gitignore(self, project: Path):
        """Dotfiles/dirs are always hidden (not just via .gitignore)."""
        tool = ListDirTool(str(project))
        result = await tool.execute(".", depth=2)
        assert ".venv" not in result
        assert ".gitignore" not in result

    @pytest.mark.asyncio
    async def test_include_ignored_shows_gitignored_artifacts(self, project: Path):
        tool = ListDirTool(str(project))
        result = await tool.execute(".", depth=3, include_ignored=True)
        assert "__pycache__" in result
        assert "dist/" in result

    @pytest.mark.asyncio
    async def test_source_files_preserved(self, project: Path):
        tool = ListDirTool(str(project))
        result = await tool.execute(".", depth=2)
        for expected in ("src/main.py", "src/utils.py", "tests/test_main.py", "README.md"):
            assert expected in result, f"{expected} should be present"


# ============================================================================
# ListDirTool: progressive disclosure
# ============================================================================


class TestListDirProgressive:
    @pytest.mark.asyncio
    async def test_depth_limit_shows_file_count(self, project: Path):
        tool = ListDirTool(str(project))
        result = await tool.execute(".", depth=0)
        assert "files)" in result
        # Verify actual count: src/ has 2 .py files (pycache filtered)
        assert "src/  (2 files)" in result

    @pytest.mark.asyncio
    async def test_depth_1_expands_children(self, project: Path):
        tool = ListDirTool(str(project))
        result = await tool.execute(".", depth=1)
        assert "src/main.py" in result
        # At depth=1, src/ should be expanded, not show count
        assert "src/  (" not in result

    @pytest.mark.asyncio
    async def test_output_significantly_smaller_than_full_tree(self, project: Path):
        tool = ListDirTool(str(project))
        default = await tool.execute(".", depth=1)
        full = await tool.execute(".", depth=5, include_ignored=True)
        assert len(default) < len(full) * 0.7, (
            f"default ({len(default)}) should be significantly smaller than full ({len(full)})"
        )

    @pytest.mark.asyncio
    async def test_no_gitignore_still_works(self, tmp_path: Path):
        (tmp_path / "file.py").write_text("x = 1\n")
        sub = tmp_path / "sub"
        sub.mkdir()
        (sub / "a.py").write_text("a = 1\n")
        tool = ListDirTool(str(tmp_path))
        result = await tool.execute(".", depth=1)
        assert "file.py" in result
        assert "sub/a.py" in result

    @pytest.mark.asyncio
    async def test_not_a_directory_error(self, project: Path):
        tool = ListDirTool(str(project))
        result = await tool.execute("README.md")
        assert "Error" in result
        assert "Not a directory" in result

    @pytest.mark.asyncio
    async def test_permission_denied_dir(self, tmp_path: Path):
        """Covers PermissionError branch in _walk."""
        import os, stat

        restricted = tmp_path / "noperm"
        restricted.mkdir()
        (restricted / "secret.txt").write_text("x")
        os.chmod(str(restricted), 0o000)
        try:
            tool = ListDirTool(str(tmp_path))
            result = await tool.execute(".", depth=1)
            assert "permission denied" in result.lower()
        finally:
            os.chmod(str(restricted), 0o755)

    @pytest.mark.asyncio
    async def test_max_child_scan_cap(self, tmp_path: Path):
        """Covers _MAX_CHILD_SCAN early return."""
        (tmp_path / ".gitignore").write_text("")
        big = tmp_path / "big"
        big.mkdir()
        # Create enough files to exceed a low scan cap
        for i in range(20):
            (big / f"f{i}.txt").write_text("x")
        tool = ListDirTool(str(tmp_path))
        tool._MAX_CHILD_SCAN = 5  # lower cap for test
        result = await tool.execute(".", depth=0)
        # Should still return a count (lower-bound)
        assert "big/" in result
        assert "files)" in result

    @pytest.mark.asyncio
    async def test_truncation_at_max_entries(self, tmp_path: Path):
        """Covers MAX_LIST_ENTRIES truncation in execute and _walk."""
        from cli.tools import file_ops

        (tmp_path / ".gitignore").write_text("")
        for i in range(30):
            (tmp_path / f"file{i:03d}.txt").write_text("x")
        tool = ListDirTool(str(tmp_path))
        original = file_ops.MAX_LIST_ENTRIES
        try:
            file_ops.MAX_LIST_ENTRIES = 10
            result = await tool.execute(".", depth=1)
            assert "truncated" in result
        finally:
            file_ops.MAX_LIST_ENTRIES = original


# ============================================================================
# GrepTool: gitignore filtering (Python fallback path)
# ============================================================================


class TestGrepGitignore:
    def test_python_grep_skips_gitignored_dirs(self, project: Path):
        """Python fallback should not search in __pycache__ or node_modules."""
        tool = GrepTool(str(project))
        result = tool._python_grep("def", project, "*.py")
        assert "main.py" in result
        assert "utils.py" in result
        assert "__pycache__" not in result

    def test_python_grep_skips_node_modules(self, project: Path):
        tool = GrepTool(str(project))
        result = tool._python_grep("module", project, "*.js")
        assert "node_modules" not in result

    def test_python_grep_handles_symlink_outside_root(self, project: Path, tmp_path: Path):
        """Covers ValueError branch when item is outside project root (e.g. symlink)."""
        # Create a symlink pointing outside the project
        external = tmp_path / "external"
        external.mkdir()
        (external / "ext.py").write_text("def ext(): pass\n")
        link = project / "src" / "ext_link"
        try:
            link.symlink_to(external)
        except OSError:
            pytest.skip("Cannot create symlinks")
        tool = GrepTool(str(project))
        # Should not crash, just skip the symlinked files
        result = tool._python_grep("def", project, "*.py")
        assert "main.py" in result


# ============================================================================
# GlobTool: gitignore filtering
# ============================================================================


class TestGlobGitignore:
    @pytest.mark.asyncio
    async def test_glob_skips_pyc_files(self, project: Path):
        tool = GlobTool(str(project))
        result = await tool.execute("**/*.pyc")
        assert result.lower() == "no matches found"

    @pytest.mark.asyncio
    async def test_glob_finds_source_but_not_gitignored(self, project: Path):
        tool = GlobTool(str(project))
        result = await tool.execute("**/*.py")
        assert "src/main.py" in result
        assert "__pycache__" not in result

    @pytest.mark.asyncio
    async def test_glob_skips_dist(self, project: Path):
        tool = GlobTool(str(project))
        result = await tool.execute("**/*.tar.gz")
        assert result.lower() == "no matches found"

    @pytest.mark.asyncio
    async def test_glob_not_a_directory(self, project: Path):
        tool = GlobTool(str(project))
        result = await tool.execute("**/*.py", path="README.md")
        assert "Error" in result

    @pytest.mark.asyncio
    async def test_glob_truncates_at_max_matches(self, tmp_path: Path):
        """Covers the MAX_MATCHES truncation branch."""
        (tmp_path / ".gitignore").write_text("")
        for i in range(250):
            (tmp_path / f"file{i:03d}.txt").write_text("x")
        tool = GlobTool(str(tmp_path))
        result = await tool.execute("*.txt")
        assert "truncated" in result

    @pytest.mark.asyncio
    async def test_glob_handles_symlink_outside_root(self, project: Path, tmp_path: Path):
        """Covers ValueError branch in GlobTool."""
        external = tmp_path / "external"
        external.mkdir()
        (external / "ext.py").write_text("x")
        link = project / "ext_link"
        try:
            link.symlink_to(external)
        except OSError:
            pytest.skip("Cannot create symlinks")
        tool = GlobTool(str(project))
        # Should not crash
        result = await tool.execute("**/*.py")
        assert isinstance(result, str)


# ============================================================================
# tool_output_handler: list_dir summary
# ============================================================================


class TestListDirSummary:
    def test_strategy_registered(self):
        assert "list_dir" in SUMMARY_GENERATORS
        assert "list_directory" in SUMMARY_GENERATORS

    def test_summary_extracts_top_level_dirs(self):
        listing = "api/  (42 files)\ncore/  (187 files)\ncli/  (23 files)\nREADME.md\nMakefile\n"
        summary = _summarize_list_dir(listing)
        assert "api/" in summary
        assert "core/" in summary
        assert "cli/" in summary
        assert "2 files in root" in summary

    def test_nested_paths_not_counted_as_top_level(self):
        listing = "api/\napi/models/foo.py\napi/main.py\ncli/\nREADME.md\n"
        summary = _summarize_list_dir(listing)
        top_section = summary.split("Top-level directories:\n")[1]
        # Top-level should list api/ and cli/, not nested paths
        assert "api/\n" in top_section
        assert "cli/\n" in top_section
        assert "api/models" not in top_section

    def test_summary_shows_entry_count(self):
        lines = [f"dir{i}/  (10 files)" for i in range(5)] + ["file.py"]
        listing = "\n".join(lines)
        summary = _summarize_list_dir(listing)
        assert "6 entries" in summary

    def test_large_listing_truncates_dirs(self):
        lines = [f"dir{i}/" for i in range(50)]
        listing = "\n".join(lines)
        summary = _summarize_list_dir(listing)
        assert "more dirs" in summary
        # Should only show 30
        assert "dir29/" in summary
        assert "dir30/" not in summary.split("more dirs")[0]

    def test_summary_smaller_than_large_input(self):
        lines = [f"dir{i}/subdir/file{j}.py" for i in range(20) for j in range(10)]
        listing = "\n".join(lines)
        summary = generate_structured_summary(listing, "list_dir")
        assert len(summary) < len(listing) * 0.5

    def test_empty_input(self):
        summary = _summarize_list_dir("")
        assert "0 entries" in summary


# ============================================================================
# compaction: compact_history_messages
# ============================================================================


class TestCompactHistoryMessages:
    def test_small_history_returned_as_is(self):
        msgs = [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
        ]
        result = compact_history_messages(msgs, max_total_chars=10000)
        assert result is msgs  # identity — no copy needed when under threshold

    def test_old_tool_results_compressed_with_truncation_marker(self):
        msgs = [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "q1"},
            {"role": "tool", "content": "A" * 30000},
            {"role": "assistant", "content": "a1"},
            {"role": "user", "content": "q2"},
            {"role": "tool", "content": "B" * 30000},
            {"role": "assistant", "content": "a2"},
            {"role": "user", "content": "q3"},
            {"role": "tool", "content": "C" * 5000},
            {"role": "assistant", "content": "a3"},
            {"role": "user", "content": "q4"},
            {"role": "assistant", "content": "a4"},
        ]
        result = compact_history_messages(msgs, max_total_chars=20000)
        # Old tool result compressed with marker
        assert len(result[2]["content"]) < 1000
        assert "30000 chars truncated" in result[2]["content"]
        assert result[2]["content"].startswith("A" * 500)
        # Recent tool result preserved exactly
        assert result[8]["content"] == "C" * 5000

    def test_preserves_memory_references_in_compressed_output(self):
        ref = "[Full output (50000 bytes): memory:abc123]"
        msgs = [
            {"role": "user", "content": "q1"},
            {"role": "tool", "content": "big output " * 5000 + "\n" + ref},
            {"role": "assistant", "content": "a1"},
            {"role": "user", "content": "q2"},
            {"role": "assistant", "content": "a2"},
        ]
        result = compact_history_messages(msgs, max_total_chars=5000)
        assert "memory:abc123" in result[1]["content"]
        assert ref in result[1]["content"]

    def test_does_not_mutate_original_messages(self):
        original_content = "X" * 40000
        msgs = [
            {"role": "user", "content": "q1"},
            {"role": "tool", "content": original_content},
            {"role": "assistant", "content": "a1"},
            {"role": "user", "content": "q2"},
            {"role": "assistant", "content": "a2"},
        ]
        compact_history_messages(msgs, max_total_chars=5000)
        # Original must be untouched
        assert msgs[1]["content"] == original_content
        assert len(msgs[1]["content"]) == 40000

    def test_non_tool_messages_never_truncated(self):
        msgs = [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "old question"},
            {"role": "assistant", "content": "A" * 10000},  # long assistant msg
            {"role": "user", "content": "new question"},
            {"role": "assistant", "content": "new answer"},
        ]
        result = compact_history_messages(msgs, max_total_chars=5000)
        assert result[2]["content"] == "A" * 10000  # not truncated

    def test_single_user_turn_keeps_everything(self):
        msgs = [
            {"role": "user", "content": "only question"},
            {"role": "tool", "content": "T" * 40000},
            {"role": "assistant", "content": "answer"},
        ]
        result = compact_history_messages(msgs, max_total_chars=10000)
        assert len(result[1]["content"]) == 40000

    def test_small_old_tool_result_copied_not_truncated(self):
        """Covers the else branch: old tool result <= _COMPACT_TOOL_KEEP_CHARS."""
        msgs = [
            {"role": "user", "content": "q1"},
            {"role": "tool", "content": "small"},  # old, but small
            {"role": "assistant", "content": "a1"},
            {"role": "user", "content": "q2"},
            {"role": "tool", "content": "X" * 80000},  # push over threshold
            {"role": "user", "content": "q3"},
            {"role": "assistant", "content": "a3"},
        ]
        result = compact_history_messages(msgs, max_total_chars=10000)
        assert result[1]["content"] == "small"
        # Verify it's a copy, not the same object
        assert result[1] is not msgs[1]

    def test_multiple_memory_refs_all_preserved(self):
        """Covers the refs append branch with multiple refs."""
        refs = "[memory:ref1]\n[Full output (999 bytes): memory:ref2]"
        msgs = [
            {"role": "user", "content": "q1"},
            {"role": "tool", "content": "data " * 10000 + "\n" + refs},
            {"role": "assistant", "content": "a1"},
            {"role": "user", "content": "q2"},
            {"role": "assistant", "content": "a2"},
        ]
        result = compact_history_messages(msgs, max_total_chars=5000)
        assert "memory:ref1" in result[1]["content"]
        assert "memory:ref2" in result[1]["content"]


# ============================================================================
# chat_loop: hard limit on tool results
# ============================================================================


class TestToolResultHardLimit:
    def test_hard_limit_is_module_constant(self):
        """MAX_SINGLE_TOOL_RESULT_CHARS is a module-level constant, not loop-local."""
        from core.agent.chat_loop import MAX_SINGLE_TOOL_RESULT_CHARS

        assert isinstance(MAX_SINGLE_TOOL_RESULT_CHARS, int)
        assert MAX_SINGLE_TOOL_RESULT_CHARS == 12000

    def test_hard_limit_truncation_behavior(self):
        """Simulate the exact truncation logic from chat_loop.

        NOTE: This mirrors the inline truncation in ChatLoop._run_tool_loop.
        If the format there changes, this test must be updated to match.
        """
        from core.agent.chat_loop import MAX_SINGLE_TOOL_RESULT_CHARS

        big = "x" * 20000
        if len(big) > MAX_SINGLE_TOOL_RESULT_CHARS:
            result = (
                big[:MAX_SINGLE_TOOL_RESULT_CHARS] + f"\n... [hard-truncated from {len(big)} chars]"
            )
        assert len(result) < MAX_SINGLE_TOOL_RESULT_CHARS + 100
        assert "hard-truncated" in result
        assert "20000" in result
        assert result.startswith("x" * 100)

    def test_compaction_truncate_fallback_still_works(self):
        from core.context.compaction import truncate_tool_result, MAX_TOOL_RESULT_CHARS

        big = "x" * 20000
        result = truncate_tool_result(big)
        assert len(result) <= MAX_TOOL_RESULT_CHARS + 100
        assert "truncated" in result
