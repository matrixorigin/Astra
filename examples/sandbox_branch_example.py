"""Sandbox and Branch Example."""

from core.sandbox import Branch, Sandbox
from sdk import Database, GitForData

db = Database()
git = GitForData(db=db)

print("=== Sandbox Usage Example ===\n")

sandbox = Sandbox(db=db)

# 1. Create sandbox
print("1. Create sandbox")
sandbox.create("sandbox_exp1")

# 2. List sandboxes with filtering
print("\n2. List sandboxes")
all_sandboxes = sandbox.list()
print(f"   All: {[s['name'] for s in all_sandboxes if 'exp' in s['name']]}")

exp_sandboxes = sandbox.list(pattern="%exp%")
print(f"   Filtered (exp): {[s['name'] for s in exp_sandboxes]}")

# 3. Use sandbox (switch to sandbox database)
print("\n3. Use sandbox - Method 1: Switch database")
sandbox.use("sandbox_exp1")
count = db.fetchone("SELECT COUNT(*) as count FROM conversation_events")["count"]
print(f"   Querying in sandbox: {count} events")
sandbox.use("dev_agent")  # Switch back

# 4. Use sandbox - Method 2: Explicit database name
print("\n4. Use sandbox - Method 2: Explicit name")
count = db.fetchone("SELECT COUNT(*) as count FROM sandbox_exp1.conversation_events")["count"]
print(f"   Querying with full name: {count} events")

# 5. Add specific table to sandbox
print("\n5. Add table to sandbox")
db.execute("CREATE DATABASE sandbox_exp2")
sandbox.add_table("sandbox_exp2", "conversation_events")
tables = sandbox.list_tables("sandbox_exp2")
print(f"   Tables in sandbox_exp2: {tables}")

# 6. Sandbox info
print("\n6. Sandbox info")
info = sandbox.info("sandbox_exp1")
print(f"   Name: {info['name']}")
print(f"   Tables: {info['tables']}")
print(f"   Details: {info['details'][:2]}")  # Show first 2 tables

# 7. Checkpoint and restore
print("\n7. Checkpoint and restore")
sandbox.checkpoint("sandbox_exp1", "before_changes")
print("   Created checkpoint: before_changes")

# Make changes
db.execute("DROP TABLE IF EXISTS sandbox_exp1.conversation_events")
print("   Dropped table")

# Restore
sandbox.restore("sandbox_exp1", "before_changes")
tables = sandbox.list_tables("sandbox_exp1")
print(f"   Restored! Tables: {len(tables)}")

# 8. Cleanup
print("\n8. Cleanup")
sandbox.delete("sandbox_exp1")
sandbox.delete("sandbox_exp2")
git.drop_snapshot("sandbox_exp1_before_changes")
print("   Cleaned up")

print("\n=== Branch Example ===\n")

branch = Branch(db=db)

print("1. Create base table")
db.execute("DROP TABLE IF EXISTS events")
db.execute("CREATE TABLE events (id INT, msg VARCHAR(50), PRIMARY KEY(id))")
db.execute("INSERT INTO events VALUES (1, 'event1'), (2, 'event2')")

print("2. Create branch")
branch.create("events_exp", "events")

print("3. Work in branch")
db.execute("INSERT INTO events_exp VALUES (3, 'event3')")

print("4. Diff and merge")
diff = branch.diff("events_exp", "events", output="count")
print(f"   Diff: {diff}")
branch.merge("events_exp", "events", on_conflict="accept")

print("5. Cleanup")
branch.delete("events_exp")
db.execute("DROP TABLE events")

print("\n✅ Done!")
print("\n=== Key Takeaways ===")
print("1. Use sandbox.use(name) to switch database context")
print("2. Or use explicit names: sandbox_name.table_name")
print("3. Use sandbox.list(pattern='%exp%') to filter sandboxes")
print("4. Use sandbox.checkpoint() and restore() for versioning")
print("5. All operations are zero-copy (instant, no storage overhead)")
