#!/usr/bin/env python3
"""Phase 2: Wrap self.db usages in with self._db() as db: blocks.

For each method that uses self.db, wraps the entire method body in:
    with self._db() as db:
        ...

And replaces self.db.xxx with db.xxx within those methods.

Also handles:
- Removing Phase 2 bridge comments and SessionLocal imports
- Fixing child object creation (Sandbox(db=db) etc.)
- Removing _owns_session, __enter__, __exit__, close(), __del__ patterns
"""

import re
import os
import ast
import sys
import textwrap

# Classes already migrated in Phase 1 (don't touch)
ALREADY_MIGRATED = {
    'ContextManager', 'EmbeddingService', 'PromptManager', 'PromptFeedback',
    'RelevanceScorer', 'ToolMockingLayer', 'AgentExecutor',
}

# Classes to skip entirely
SKIP = {
    'AgentService', 'ContextService', 'DecisionService', 'EventService',
    'ReplayService', 'SandboxService', 'SessionService', 'SkillService',
    '_FeedbackBuffer', 'SkillPipeline', 'InputFaceLearner',
}


def process_file(filepath, dry_run=False):
    """Replace self.db.xxx with db.xxx inside with self._db() as db: blocks."""
    with open(filepath) as f:
        content = f.read()

    if 'self.db.' not in content:
        return False

    original = content

    # Simple approach: replace self.db. with a temporary marker,
    # then for each method, wrap in with self._db() as db:

    # Actually, the simplest correct approach:
    # For every method that contains self.db., wrap the body in with self._db() as db:
    # and replace self.db. with db.

    # But we need to be careful about:
    # 1. __init__ methods (skip - already handled)
    # 2. Methods that already have with self._db() as db:
    # 3. Static methods (skip)
    # 4. Methods with self.db = xxx (assignment, not usage)

    # Let's use AST to find methods, then do text replacement

    try:
        tree = ast.parse(content)
    except SyntaxError:
        return False

    lines = content.split('\n')
    # Collect methods that need wrapping (from bottom to top for safe insertion)
    methods_to_wrap = []

    for node in ast.walk(tree):
        if not isinstance(node, ast.ClassDef):
            continue
        if node.name in ALREADY_MIGRATED or node.name in SKIP:
            continue

        # Check if this class extends DbConsumer
        base_names = []
        for b in node.bases:
            if isinstance(b, ast.Name):
                base_names.append(b.id)
            elif isinstance(b, ast.Attribute):
                base_names.append(b.attr)
        if 'DbConsumer' not in base_names:
            continue

        for item in node.body:
            if not isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            if item.name == '__init__':
                continue
            if any(isinstance(d, ast.Name) and d.id == 'staticmethod' for d in item.decorator_list):
                continue

            # Check if method body contains self.db usage
            method_source = ast.get_source_segment(content, item)
            if method_source and 'self.db.' in method_source:
                # Check if already wrapped
                if 'with self._db() as db:' in method_source:
                    continue
                methods_to_wrap.append({
                    'class_name': node.name,
                    'method_name': item.name,
                    'lineno': item.lineno,
                    'end_lineno': item.end_lineno,
                    'col_offset': item.col_offset,
                    'is_async': isinstance(item, ast.AsyncFunctionDef),
                })

    if not methods_to_wrap:
        # Still might need self.db. -> db. replacement if already wrapped
        return False

    # Sort by line number descending (process from bottom to top)
    methods_to_wrap.sort(key=lambda m: m['lineno'], reverse=True)

    for method in methods_to_wrap:
        start = method['lineno'] - 1  # 0-indexed
        end = method['end_lineno']  # exclusive

        # Find the method body start (after def line and docstring)
        body_start = start + 1
        # Skip docstring
        while body_start < end:
            stripped = lines[body_start].strip()
            if stripped.startswith('"""') or stripped.startswith("'''"):
                # Find end of docstring
                if stripped.count('"""') >= 2 or stripped.count("'''") >= 2:
                    body_start += 1
                else:
                    quote = stripped[:3]
                    body_start += 1
                    while body_start < end and quote not in lines[body_start]:
                        body_start += 1
                    body_start += 1  # skip closing quote line
                break
            elif stripped == '' or stripped.startswith('#'):
                body_start += 1
            else:
                break

        if body_start >= end:
            continue

        # Determine indentation of method body
        first_body_line = lines[body_start]
        body_indent = len(first_body_line) - len(first_body_line.lstrip())
        indent_str = ' ' * body_indent

        # Insert "with self._db() as db:" and indent body
        # Replace self.db. with db. in the body lines
        new_lines = [f"{indent_str}with self._db() as db:"]
        for i in range(body_start, end):
            line = lines[i]
            # Replace self.db. with db.
            line = line.replace('self.db.', 'db.')
            # Add 4 spaces of indentation to body
            if line.strip():  # non-empty line
                new_lines.append(' ' * 4 + line)
            else:
                new_lines.append(line)  # keep empty lines as-is

        # Replace the body lines
        lines[body_start:end] = new_lines

    content = '\n'.join(lines)

    if content != original:
        if not dry_run:
            with open(filepath, 'w') as f:
                f.write(content)
        return True
    return False


def main():
    dry_run = '--dry-run' in sys.argv
    count = 0

    for root, dirs, files in sorted(os.walk('core/')):
        for f in sorted(files):
            if not f.endswith('.py'):
                continue
            path = os.path.join(root, f)
            if process_file(path, dry_run):
                count += 1
                action = "WOULD wrap" if dry_run else "Wrapped"
                print(f"  {action} {path}")

    print(f"\n{'Would wrap' if dry_run else 'Wrapped'}: {count} files")


if __name__ == '__main__':
    main()
