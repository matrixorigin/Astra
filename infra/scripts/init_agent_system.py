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

# Add project root to path
project_root = Path(__file__).parent.parent.parent
sys.path.insert(0, str(project_root))

from sdk import Database
from core.logging_config import get_logger

logger = get_logger(__name__)


def run_sql_file(db: Database, sql_file: Path) -> bool:
    """Execute SQL file."""
    try:
        logger.info(f"Executing {sql_file.name}...")
        
        with open(sql_file, 'r') as f:
            sql_content = f.read()
        
        # Split by semicolon and execute each statement
        statements = [s.strip() for s in sql_content.split(';') if s.strip()]
        
        for stmt in statements:
            # Skip comments
            if stmt.startswith('--'):
                continue
            
            try:
                result = db.execute(stmt)
                if result:
                    logger.debug(f"Statement executed: {stmt[:50]}...")
            except Exception as e:
                # Some statements like GRANT might fail if already exists
                logger.warning(f"Statement warning: {e}")
        
        logger.info(f"✅ {sql_file.name} executed successfully")
        return True
        
    except Exception as e:
        logger.error(f"❌ Failed to execute {sql_file.name}: {e}")
        return False


def verify_tables(db: Database) -> bool:
    """Verify that tables were created."""
    try:
        tables = ['model_registry', 'skills_registry', 'api_tokens', 'audit_logs']
        
        for table in tables:
            result = db.fetchone(
                "SELECT COUNT(*) as cnt FROM agent_config.%s" % table
            )
            if result:
                logger.info(f"✅ Table agent_config.{table} exists")
            else:
                logger.error(f"❌ Table agent_config.{table} not found")
                return False
        
        return True
        
    except Exception as e:
        logger.error(f"❌ Table verification failed: {e}")
        return False


def verify_roles(db: Database) -> bool:
    """Verify that roles were created."""
    try:
        result = db.fetchall(
            "SELECT role_name FROM mo_catalog.mo_role WHERE role_name LIKE 'mo_agent_%'"
        )
        
        roles = [row['role_name'] for row in result]
        
        if 'mo_agent_admin' in roles:
            logger.info("✅ Role mo_agent_admin exists")
        else:
            logger.warning("⚠️  Role mo_agent_admin not found")
        
        if 'mo_agent_user' in roles:
            logger.info("✅ Role mo_agent_user exists")
        else:
            logger.warning("⚠️  Role mo_agent_user not found")
        
        return True
        
    except Exception as e:
        logger.error(f"❌ Role verification failed: {e}")
        return False


def main():
    parser = argparse.ArgumentParser(description='Initialize agent configuration system')
    parser.add_argument('--host', default='localhost', help='Database host')
    parser.add_argument('--port', type=int, default=6001, help='Database port')
    parser.add_argument('--user', default='root', help='Database user')
    parser.add_argument('--password', default='111', help='Database password')
    parser.add_argument('--skip-rbac', action='store_true', help='Skip RBAC initialization')
    
    args = parser.parse_args()
    
    logger.info("=" * 60)
    logger.info("Agent Configuration System Initialization")
    logger.info("=" * 60)
    
    # Connect to database
    try:
        db = Database(
            host=args.host,
            port=args.port,
            user=args.user,
            password=args.password
        )
        logger.info(f"✅ Connected to MatrixOne at {args.host}:{args.port}")
    except Exception as e:
        logger.error(f"❌ Failed to connect to database: {e}")
        return 1
    
    # Get script directory
    script_dir = Path(__file__).parent
    
    # Step 1: Initialize agent_config database
    logger.info("\n📦 Step 1: Creating agent_config database and tables...")
    if not run_sql_file(db, script_dir / 'init-agent-config.sql'):
        return 1
    
    # Step 2: Verify tables
    logger.info("\n🔍 Step 2: Verifying tables...")
    if not verify_tables(db):
        return 1
    
    # Step 3: Initialize RBAC roles
    if not args.skip_rbac:
        logger.info("\n🔐 Step 3: Creating RBAC roles...")
        if not run_sql_file(db, script_dir / 'init-rbac.sql'):
            logger.warning("⚠️  RBAC initialization had warnings (this is normal if roles already exist)")
        
        # Step 4: Verify roles
        logger.info("\n🔍 Step 4: Verifying roles...")
        verify_roles(db)
    else:
        logger.info("\n⏭️  Skipping RBAC initialization")
    
    # Summary
    logger.info("\n" + "=" * 60)
    logger.info("✅ Agent configuration system initialized successfully!")
    logger.info("=" * 60)
    logger.info("\nNext steps:")
    logger.info("1. Grant roles to users:")
    logger.info("   GRANT mo_agent_user TO alice;")
    logger.info("   GRANT mo_agent_admin TO admin;")
    logger.info("\n2. Start using the agent:")
    logger.info("   mo-agent chat")
    logger.info("\n3. Manage configurations:")
    logger.info("   mo-admin model add gpt-4 openai --scope global")
    
    return 0


if __name__ == '__main__':
    sys.exit(main())
