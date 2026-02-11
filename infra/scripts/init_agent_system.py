#!/usr/bin/env python3
"""Initialize agent configuration database and RBAC roles.

Usage:
    python3 infra/scripts/init_agent_system.py

    # Or with custom connection
    python3 infra/scripts/init_agent_system.py --host localhost --port 6001 --user root --password 111
"""

import argparse
import sys
from pathlib import Path

import pymysql
from pymysql.cursors import DictCursor

# Add project root to path
project_root = Path(__file__).parent.parent.parent
sys.path.insert(0, str(project_root))

from core.logging_config import get_logger

logger = get_logger(__name__)


def run_sql_file(conn, sql_file: Path, autocommit: bool = False) -> bool:
    """Execute SQL file."""
    try:
        logger.info(f"Executing {sql_file.name}...")

        # Set autocommit mode
        original_autocommit = conn.get_autocommit()
        conn.autocommit(autocommit)

        with open(sql_file) as f:
            sql_content = f.read()

        # Remove comments
        lines = []
        for line in sql_content.split("\n"):
            # Remove line comments
            if "--" in line:
                line = line[: line.index("--")]
            line = line.strip()
            if line:
                lines.append(line)

        sql_content = " ".join(lines)

        # Split by semicolon and execute each statement
        statements = [s.strip() for s in sql_content.split(";") if s.strip()]

        cursor = conn.cursor()

        for i, stmt in enumerate(statements):
            if not stmt:
                continue

            try:
                logger.debug(f"Executing statement {i + 1}/{len(statements)}: {stmt[:80]}...")
                cursor.execute(stmt)
                if not autocommit:
                    conn.commit()
                logger.debug(f"✓ Statement {i + 1} executed successfully")
            except Exception as e:
                # Some statements might fail if already exists
                logger.warning(f"Statement {i + 1} warning: {e}")
                logger.debug(f"Failed statement: {stmt[:200]}")

        cursor.close()

        # Restore original autocommit mode
        conn.autocommit(original_autocommit)

        logger.info(f"✅ {sql_file.name} executed successfully")
        return True

    except Exception as e:
        logger.error(f"❌ Failed to execute {sql_file.name}: {e}")
        import traceback

        logger.error(traceback.format_exc())
        return False


def verify_tables(conn) -> bool:
    """Verify that tables were created."""
    try:
        cursor = conn.cursor()
        cursor.execute("SHOW TABLES FROM agent_config")
        tables = [row["Tables_in_agent_config"] for row in cursor.fetchall()]
        cursor.close()

        expected_tables = ["model_registry", "skills_registry", "api_tokens", "audit_logs"]

        for table in expected_tables:
            if table in tables:
                logger.info(f"✅ Table {table} exists")
            else:
                logger.error(f"❌ Table {table} not found")
                return False

        return True

    except Exception as e:
        logger.error(f"❌ Table verification failed: {e}")
        return False


def verify_roles(conn) -> bool:
    """Verify that roles were created."""
    try:
        cursor = conn.cursor()
        cursor.execute("SELECT role_name FROM mo_catalog.mo_role WHERE role_name LIKE 'mo_agent_%'")

        roles = [row["role_name"] for row in cursor.fetchall()]
        cursor.close()

        if "mo_agent_admin" in roles:
            logger.info("✅ Role mo_agent_admin exists")
        else:
            logger.warning("⚠️  Role mo_agent_admin not found")

        if "mo_agent_user" in roles:
            logger.info("✅ Role mo_agent_user exists")
        else:
            logger.warning("⚠️  Role mo_agent_user not found")

        return True

    except Exception as e:
        logger.error(f"❌ Role verification failed: {e}")
        return False


def main():
    parser = argparse.ArgumentParser(description="Initialize agent configuration system")
    parser.add_argument("--host", default="localhost", help="Database host")
    parser.add_argument("--port", type=int, default=6001, help="Database port")
    parser.add_argument("--user", default="root", help="Database user")
    parser.add_argument("--password", default="111", help="Database password")
    parser.add_argument("--skip-rbac", action="store_true", help="Skip RBAC initialization")

    args = parser.parse_args()

    logger.info("=" * 60)
    logger.info("Agent Configuration System Initialization")
    logger.info("=" * 60)

    # Connect to database
    try:
        conn = pymysql.connect(
            host=args.host,
            port=args.port,
            user=args.user,
            password=args.password,
            charset="utf8mb4",
            cursorclass=DictCursor,
        )
        logger.info(f"✅ Connected to MatrixOne at {args.host}:{args.port}")
    except Exception as e:
        logger.error(f"❌ Failed to connect to database: {e}")
        return 1

    try:
        # Get script directory
        script_dir = Path(__file__).parent

        # Step 1: Initialize agent_config database
        logger.info("\n📦 Step 1: Creating agent_config database and tables...")
        if not run_sql_file(conn, script_dir / "init-agent-config.sql"):
            return 1

        # Step 2: Verify tables
        logger.info("\n🔍 Step 2: Verifying tables...")
        if not verify_tables(conn):
            return 1

        # Step 3: Initialize RBAC roles
        if not args.skip_rbac:
            logger.info("\n🔐 Step 3: Creating RBAC roles...")
            # RBAC commands need autocommit mode
            if not run_sql_file(conn, script_dir / "init-rbac.sql", autocommit=True):
                logger.warning(
                    "⚠️  RBAC initialization had warnings (this is normal if roles already exist)"
                )

            # Step 4: Verify roles
            logger.info("\n🔍 Step 4: Verifying roles...")
            verify_roles(conn)

        logger.info("\n" + "=" * 60)
        logger.info("✅ Agent configuration system initialized successfully!")
        logger.info("=" * 60)
        logger.info("\nNext steps:")
        logger.info("1. Create admin user:")
        logger.info("   CREATE USER admin IDENTIFIED BY 'password';")
        logger.info("   GRANT mo_agent_admin TO admin;")
        logger.info("\n2. Create regular user:")
        logger.info("   CREATE USER alice IDENTIFIED BY 'password';")
        logger.info("   GRANT mo_agent_user TO alice;")
        logger.info("\n3. Start using mo-admin CLI:")
        logger.info("   mo-admin model add gpt-4 openai --scope global")

        return 0

    except Exception as e:
        logger.error(f"❌ Initialization failed: {e}")
        return 1
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
