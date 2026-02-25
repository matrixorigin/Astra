#!/usr/bin/env python3
"""
Complete migration: convert core/ classes from db: Session to DbConsumer.

Phase 1: Rewrite class definitions and __init__
Phase 2: Wrap method bodies with self._db() as db:

Usage: python scripts/migrate_db_consumer_full.py [--dry-run]
"""

import ast
import re
import os
import sys

SKIP_CLASSES = {
    'ContextManager', 'EmbeddingService', 'PromptManager', 'PromptFeedback',
    'RelevanceScorer', 'ToolMockingLayer', 'AgentExecutor',
    'AgentService', 'ContextService', 'DecisionService', 'EventService',
    'ReplayService', 'SandboxService', 'SessionService', 'SkillService',
    '_FeedbackBuffer', 'SkillPipeline', 'InputFaceLearner',
}


def find_target_classes(tree, source):
    """Find classes with db: Session in __init__ that need migration."""
    classes = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.ClassDef):
            continue
        if node.name in SKIP_CLASSES:
            continue
        base_names = [
            b.id if isinstance(b, ast.Name) else
            (b.attr if isinstance(b, ast.Attribute) else '?')
            for b in node.bases
        ]
        if 'DbConsumer' in base_names:
            continue

        for item in node.body:
            if isinstance(item, ast.FunctionDef) and item.name == '__init__':
                for arg in item.args.args[1:]:
                    if arg.arg in ('db', 'db_session') and arg.annotation:
                        if 'Session' in ast.dump(arg.annotation):
                            classes.append({
                                'node': node,
                                'name': node.name,
                                'class_line': node.lineno,
                                'bases': base_names,
                                'db_param': arg.arg,
                                'init_node': item,
                            })
                            break
    return classes


def migrate_file(filepath, dry_run=False):
    """Full migration of a single file."""
    with open(filepath) as f:
        source = f.read()

    try:
        tree = ast.parse(source)
    except SyntaxError:
        return False, []

    classes = find_target_classes(tree, source)
    if not classes:
        return False, []

    lines = source.split('\n')

    # ── Phase 1: Add import ──
    has_import = 'from core.db_consumer import' in source
    import_insert_idx = 0
    if not has_import:
        for i, line in enumerate(lines):
            if line.startswith('from sqlalchemy') or line.startswith('import sqlalchemy'):
                import_insert_idx = i + 1
            elif line.startswith('from core.') and 'db_consumer' not in line:
                import_insert_idx = max(import_insert_idx, i + 1)
        if import_insert_idx == 0:
            for i, line in enumerate(lines):
                if line.startswith(('import ', 'from ')):
                    import_insert_idx = i + 1
        lines.insert(import_insert_idx, 'from core.db_consumer import DbConsumer, DbFactory')
        # All line numbers shift by 1
        offset = 1
    else:
        offset = 0

    # Re-parse with the import added
    new_source = '\n'.join(lines)
    tree = ast.parse(new_source)
    classes = find_target_classes(tree, new_source)
    if not classes:
        # Shouldn't happen, but safety check
        if not dry_run and new_source != source:
            with open(filepath, 'w') as f:
                f.write(new_source)
        return True, []

    # ── Phase 1b: Modify class definitions and __init__ ──
    # Process from bottom to top
    classes.sort(key=lambda c: c['class_line'], reverse=True)

    for cls in classes:
        node = cls['node']
        init = cls['init_node']
        cls_line_idx = node.lineno - 1

        # Add DbConsumer to bases
        line = lines[cls_line_idx]
        if cls['bases']:
            m = re.match(r'^(\s*class\s+' + re.escape(cls['name']) + r'\s*\()([^)]*)\)', line)
            if m:
                lines[cls_line_idx] = f"{m.group(1)}DbConsumer, {m.group(2).strip()}){line[m.end():]}"
        else:
            m = re.match(r'^(\s*class\s+' + re.escape(cls['name']) + r'\s*)(:\s*)$', line)
            if m:
                lines[cls_line_idx] = f"{m.group(1)}(DbConsumer):"
            else:
                m = re.match(r'^(\s*class\s+' + re.escape(cls['name']) + r'\s*)\(\s*\)(:\s*)$', line)
                if m:
                    lines[cls_line_idx] = f"{m.group(1)}(DbConsumer):"

        # Rewrite __init__ signature and body
        init_line_idx = init.lineno - 1

        # Find the full def line (may span multiple lines)
        def_end = init_line_idx
        paren_count = 0
        for i in range(init_line_idx, len(lines)):
            paren_count += lines[i].count('(') - lines[i].count(')')
            if paren_count <= 0:
                def_end = i
                break

        # Get the full def text
        def_lines = lines[init_line_idx:def_end + 1]
        def_text = '\n'.join(def_lines)

        # Replace db param
        db_param = cls['db_param']
        if db_param == 'db':
            def_text = re.sub(r'\bdb:\s*Session\s*\|\s*None\s*=\s*None\b', 'db_factory: DbFactory', def_text)
            def_text = re.sub(r'\bdb:\s*Session\b', 'db_factory: DbFactory', def_text)
        else:
            def_text = re.sub(r'\bdb_session:\s*Session\b', 'db_factory: DbFactory', def_text)

        lines[init_line_idx:def_end + 1] = def_text.split('\n')

        # Now handle __init__ body: replace self.db = db with super().__init__(db_factory)
        # Find body start
        body_nodes = init.body
        if not body_nodes:
            continue

        # Process init body lines
        body_start = body_nodes[0].lineno - 1
        body_end = init.end_lineno  # exclusive (1-indexed end)

        for i in range(body_start, body_end):
            if i >= len(lines):
                break
            line = lines[i]
            stripped = line.strip()

            # Replace self.db = db or next(get_db_session())
            if re.match(r'self\.(?:db|db_session)\s*=\s*(?:db|db_session)(?:\s+or\s+next\(get_db_session\(\)\))?$', stripped):
                indent = line[:len(line) - len(line.lstrip())]
                lines[i] = f"{indent}super().__init__(db_factory)"
            # Remove isinstance check
            elif 'isinstance' in stripped and 'Session' in stripped and 'raise TypeError' not in stripped:
                pass  # keep it, the raise is on next line
            elif stripped.startswith('if not isinstance') and 'Session' in stripped:
                lines[i] = ''  # blank it
                # Also blank the raise line
                if i + 1 < len(lines) and 'raise TypeError' in lines[i + 1]:
                    lines[i + 1] = ''
            # Remove _owns_session
            elif '_owns_session' in stripped:
                lines[i] = ''

    # ── Phase 2: Wrap method bodies ──
    # Re-parse
    new_source = '\n'.join(lines)
    try:
        tree = ast.parse(new_source)
    except SyntaxError as e:
        print(f"  SYNTAX ERROR after Phase 1 in {filepath}: {e}")
        if not dry_run:
            with open(filepath, 'w') as f:
                f.write(new_source)
        return True, [c['name'] for c in classes]

    lines = new_source.split('\n')

    # Find all methods in DbConsumer classes that use self.db.
    methods_to_wrap = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.ClassDef):
            continue
        base_names = [
            b.id if isinstance(b, ast.Name) else
            (b.attr if isinstance(b, ast.Attribute) else '?')
            for b in node.bases
        ]
        if 'DbConsumer' not in base_names:
            continue
        if node.name in SKIP_CLASSES:
            continue

        for item in node.body:
            if not isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            if item.name in ('__init__', '__enter__', '__exit__', 'close', '__del__'):
                continue
            if any(isinstance(d, ast.Name) and d.id == 'staticmethod' for d in item.decorator_list):
                continue

            # Check if method uses self.db.
            method_lines = lines[item.lineno - 1:item.end_lineno]
            method_text = '\n'.join(method_lines)
            if 'self.db.' not in method_text:
                continue
            if 'with self._db() as db:' in method_text:
                continue

            # Find real body start (skip docstring)
            body = item.body
            body_start_idx = 0
            if (body and isinstance(body[0], ast.Expr) and
                isinstance(body[0].value, ast.Constant) and
                isinstance(body[0].value.value, str)):
                body_start_idx = 1

            if body_start_idx >= len(body):
                continue

            real_body_start = body[body_start_idx].lineno - 1  # 0-indexed
            method_end = item.end_lineno  # 1-indexed, inclusive

            methods_to_wrap.append({
                'class_name': node.name,
                'method_name': item.name,
                'body_start': real_body_start,
                'method_end': method_end,
            })

    # Process from bottom to top
    methods_to_wrap.sort(key=lambda m: m['body_start'], reverse=True)

    for method in methods_to_wrap:
        bs = method['body_start']
        me = method['method_end']

        if bs >= len(lines):
            continue

        # Determine body indentation
        first_line = lines[bs]
        body_indent = len(first_line) - len(first_line.lstrip())
        indent_str = ' ' * body_indent

        # Build new lines: with self._db() as db: + indented body
        new_block = [f"{indent_str}with self._db() as db:"]
        for i in range(bs, me):
            line = lines[i]
            # Replace self.db. with db.
            line = line.replace('self.db.', 'db.')
            if line.strip():
                new_block.append(' ' * 4 + line)
            else:
                new_block.append(line)

        lines[bs:me] = new_block

    content = '\n'.join(lines)

    # ── Phase 3: Cleanup ──
    # Remove Phase 2 bridge comments
    content = re.sub(r'\s*# Phase 2 bridge:.*\n', '\n', content)

    # Remove unused Session import if no longer referenced
    # (Be conservative - only remove if Session is not used anywhere else)
    if 'Session' not in content.replace('from sqlalchemy.orm import Session', '').replace('DbFactory', ''):
        content = content.replace('from sqlalchemy.orm import Session\n', '')

    # Remove __enter__, __exit__, close(), __del__ methods from migrated classes
    # These are session ownership methods that DbConsumer replaces
    # (Only remove if they just do self.db.close() or similar)

    if not dry_run:
        with open(filepath, 'w') as f:
            f.write(content)

    return True, [c['name'] for c in classes]


def main():
    dry_run = '--dry-run' in sys.argv
    total_classes = 0
    total_files = 0

    for root, dirs, files in sorted(os.walk('core/')):
        for f in sorted(files):
            if not f.endswith('.py'):
                continue
            path = os.path.join(root, f)
            changed, class_names = migrate_file(path, dry_run)
            if changed:
                total_files += 1
                total_classes += len(class_names)
                action = "WOULD migrate" if dry_run else "Migrated"
                print(f"  {action} {path}: {', '.join(class_names)}")

    print(f"\n{'Would migrate' if dry_run else 'Migrated'}: {total_classes} classes in {total_files} files")


if __name__ == '__main__':
    main()
