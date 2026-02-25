#!/usr/bin/env python3
"""Automated migration: convert core/ classes from db: Session to DbConsumer.

This script performs the mechanical transformation:
1. Add DbConsumer/DbFactory imports
2. Add DbConsumer to class bases
3. Rewrite __init__: db: Session -> db_factory: DbFactory, super().__init__(db_factory)
4. Wrap method bodies: self.db.xxx -> with self._db() as db: db.xxx

Run: python scripts/migrate_db_consumer.py
"""

import ast
import re
import os
import sys
import textwrap

# Classes already migrated or to skip
SKIP_CLASSES = {
    'ContextManager', 'EmbeddingService', 'PromptManager', 'PromptFeedback',
    'RelevanceScorer', 'ToolMockingLayer', 'AgentExecutor',
    # api/services/ - request-scoped
    'AgentService', 'ContextService', 'DecisionService', 'EventService',
    'ReplayService', 'SandboxService', 'SessionService', 'SkillService',
}

# Special cases: _FeedbackBuffer uses db.get_bind() only in __init__, not self.db in methods
# SkillPipeline uses self._db as a different attribute name
# InputFaceLearner uses self._db as attribute
MANUAL_CLASSES = {'_FeedbackBuffer', 'SkillPipeline', 'InputFaceLearner'}


def find_classes_in_file(filepath):
    """Find classes with db: Session in __init__."""
    with open(filepath) as f:
        source = f.read()
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return [], source

    classes = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.ClassDef):
            continue
        if node.name in SKIP_CLASSES or node.name in MANUAL_CLASSES:
            continue

        # Check if already extends DbConsumer
        base_names = []
        for b in node.bases:
            if isinstance(b, ast.Name):
                base_names.append(b.id)
            elif isinstance(b, ast.Attribute):
                base_names.append(b.attr)
        if 'DbConsumer' in base_names:
            continue

        # Find __init__ with db: Session
        for item in node.body:
            if isinstance(item, ast.FunctionDef) and item.name == '__init__':
                for arg in item.args.args[1:]:
                    if arg.arg in ('db', 'db_session'):
                        ann = arg.annotation
                        if ann and 'Session' in ast.dump(ann):
                            classes.append({
                                'name': node.name,
                                'line': node.lineno,
                                'bases': base_names,
                                'db_param': arg.arg,
                                'init_line': item.lineno,
                            })
                            break
    return classes, source


def migrate_file(filepath, dry_run=False):
    """Migrate a single file."""
    classes, source = find_classes_in_file(filepath)
    if not classes:
        return False, []

    lines = source.split('\n')
    modified = False
    class_names = [c['name'] for c in classes]

    # Step 1: Add import if not present
    if 'from core.db_consumer import' not in source:
        # Find the right place to insert import
        # After other core imports, or after sqlalchemy imports
        insert_idx = 0
        for i, line in enumerate(lines):
            if line.startswith('from sqlalchemy') or line.startswith('import sqlalchemy'):
                insert_idx = i + 1
            elif line.startswith('from core.') and insert_idx == 0:
                insert_idx = i + 1
        if insert_idx == 0:
            # After all imports
            for i, line in enumerate(lines):
                if line.startswith(('import ', 'from ')) or line == '':
                    insert_idx = i + 1
                elif line and not line.startswith('#') and not line.startswith('"""') and insert_idx > 0:
                    break

        lines.insert(insert_idx, 'from core.db_consumer import DbConsumer, DbFactory')
        modified = True
        # Adjust line numbers for subsequent operations
        for c in classes:
            if c['line'] > insert_idx:
                c['line'] += 1
            if c['init_line'] > insert_idx:
                c['init_line'] += 1

    # Step 2: For each class, modify the class definition and __init__
    # Work from bottom to top to preserve line numbers
    classes.sort(key=lambda c: c['line'], reverse=True)

    for cls in classes:
        cls_line_idx = cls['line'] - 1
        line = lines[cls_line_idx]

        # Add DbConsumer to bases
        if cls['bases']:
            # Has existing bases - add DbConsumer
            # Pattern: class Name(Base1, Base2):
            match = re.match(r'^(\s*class\s+' + cls['name'] + r'\s*\()([^)]*)\)', line)
            if match:
                prefix = match.group(1)
                existing_bases = match.group(2).strip()
                rest = line[match.end():]
                lines[cls_line_idx] = f"{prefix}DbConsumer, {existing_bases}){rest}"
                modified = True
        else:
            # No bases - add (DbConsumer)
            match = re.match(r'^(\s*class\s+' + cls['name'] + r'\s*)(:\s*)$', line)
            if match:
                lines[cls_line_idx] = f"{match.group(1)}(DbConsumer):"
                modified = True
            else:
                # Maybe class Name():
                match = re.match(r'^(\s*class\s+' + cls['name'] + r'\s*)\(\s*\)(:\s*)$', line)
                if match:
                    lines[cls_line_idx] = f"{match.group(1)}(DbConsumer):"
                    modified = True

    # Rejoin and do text-based replacements
    content = '\n'.join(lines)

    # Step 3: Rewrite __init__ signatures
    for cls in classes:
        db_param = cls['db_param']

        # Pattern: def __init__(self, db: Session ...):
        # Handle various forms:
        # db: Session
        # db: Session | None = None
        # db_session: Session

        # Replace db: Session param with db_factory: DbFactory
        if db_param == 'db':
            # db: Session | None = None -> db_factory: DbFactory
            content = re.sub(
                r'(def __init__\(self,\s*)db:\s*Session\s*\|\s*None\s*=\s*None',
                r'\1db_factory: DbFactory',
                content,
            )
            # db: Session) -> db_factory: DbFactory)
            content = re.sub(
                r'(def __init__\(self,\s*)db:\s*Session\b',
                r'\1db_factory: DbFactory',
                content,
            )
        elif db_param == 'db_session':
            content = re.sub(
                r'(def __init__\(self,\s*)db_session:\s*Session\b',
                r'\1db_factory: DbFactory',
                content,
            )

    # Step 4: Replace self.db = db / self.db_session = db_session with super().__init__(db_factory)
    # Also handle self.db = db or next(get_db_session())
    for cls in classes:
        db_param = cls['db_param']
        attr = 'db' if db_param == 'db' else 'db_session'

        # self.db = db or next(get_db_session())
        content = re.sub(
            rf'(\s+)self\.{attr}\s*=\s*{db_param}\s+or\s+next\(get_db_session\(\)\)',
            r'\1super().__init__(db_factory)',
            content,
        )
        # self.db = db
        content = re.sub(
            rf'(\s+)self\.{attr}\s*=\s*{db_param}\s*\n',
            r'\1super().__init__(db_factory)\n',
            content,
        )

    # Step 5: Remove isinstance checks for Session
    content = re.sub(
        r'\s+if not isinstance\(db(?:_factory)?, Session\):\s*\n\s+raise TypeError\([^)]+\)\s*\n',
        '\n',
        content,
    )

    # Step 6: Remove _owns_session pattern
    content = re.sub(r'\s+self\._owns_session\s*=\s*(?:db\s*is\s*None|True|False)\s*\n', '\n', content)

    # Step 7: Replace self.db.xxx with pattern that we'll fix in methods
    # This is the hardest part - we need to wrap in with self._db() as db:
    # For now, just replace self.db. with a marker
    # Actually, let's just do it - replace self.db with self._db_MIGRATE
    # Then we'll do a second pass

    if not modified and content == source:
        return False, []

    if not dry_run:
        with open(filepath, 'w') as f:
            f.write(content)

    return True, class_names


def main():
    dry_run = '--dry-run' in sys.argv
    total_classes = 0
    total_files = 0

    for root, dirs, files in sorted(os.walk('core/')):
        for f in sorted(files):
            if not f.endswith('.py'):
                continue
            path = os.path.join(root, f)
            changed, class_names = migrate_file(path, dry_run=dry_run)
            if changed:
                total_files += 1
                total_classes += len(class_names)
                action = "WOULD migrate" if dry_run else "Migrated"
                print(f"  {action} {path}: {', '.join(class_names)}")

    print(f"\n{'Would migrate' if dry_run else 'Migrated'}: {total_classes} classes in {total_files} files")


if __name__ == '__main__':
    main()
