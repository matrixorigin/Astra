#!/usr/bin/env python3
"""Clean up test databases created by parallel testing."""

from matrixone import Client


def cleanup_test_databases():
    """Clean up all worker test databases."""
    client = Client(host="localhost", port=6001, user="root", password="111", database="mo_catalog")

    # Find all worker databases
    result = client.execute('SHOW DATABASES LIKE "test_dev_agent_v3_gw%"')
    worker_dbs = [row[0] for row in result]

    if not worker_dbs:
        print("No worker databases found to clean up.")
        client._engine.dispose()
        return

    print(f"Found {len(worker_dbs)} worker databases to clean up:")
    for db in worker_dbs:
        print(f"  {db}")

    # Clean up each database
    for db in worker_dbs:
        try:
            client.execute(f"DROP DATABASE IF EXISTS `{db}`")
            print(f"✓ Dropped {db}")
        except Exception as e:
            print(f"✗ Failed to drop {db}: {e}")

    client._engine.dispose()
    print("Database cleanup completed.")


if __name__ == "__main__":
    cleanup_test_databases()
