"""Sandbox for isolated experiments.

Provides isolated environments for testing and experimentation.
"""

from datetime import datetime
from typing import Optional

from sdk.database import Database
from sdk.git_for_data import GitForData


class Sandbox:
    """Sandbox for isolated experiments.
    
    Creates isolated environments using MatrixOne snapshots for
    safe experimentation without affecting production data.
    """

    def __init__(self, db: Optional[Database] = None) -> None:
        """Initialize sandbox manager.
        
        Args:
            db: Database instance. If None, creates a new one.
        """
        self.db = db or Database()
        self.git = GitForData(db)

    def create_sandbox(
        self, sandbox_name: str, description: str = "", base_snapshot: Optional[str] = None
    ) -> dict:
        """Create a new sandbox environment.
        
        Args:
            sandbox_name: Name for the sandbox
            description: Optional description
            base_snapshot: Optional base snapshot to start from
            
        Returns:
            dict: Sandbox metadata
        """
        if base_snapshot:
            # Restore from base snapshot first
            self.git.restore_from_snapshot(base_snapshot)

        # Create sandbox snapshot
        snapshot = self.git.create_time_point_sandbox(sandbox_name, description)

        return {
            "sandbox_name": sandbox_name,
            "timestamp": snapshot.get("timestamp"),
            "description": description,
            "base_snapshot": base_snapshot,
            "status": "active",
        }

    def enter_sandbox(self, sandbox_name: str) -> None:
        """Enter a sandbox environment.
        
        Args:
            sandbox_name: Name of the sandbox to enter
            
        Note:
            This restores the database to the sandbox state.
            Create a checkpoint before entering if you want to return.
        """
        self.git.restore_from_snapshot(sandbox_name)

    def exit_sandbox(self, return_to_snapshot: str) -> None:
        """Exit sandbox and return to a previous state.
        
        Args:
            return_to_snapshot: Snapshot to return to
        """
        self.git.restore_from_snapshot(return_to_snapshot)

    def delete_sandbox(self, sandbox_name: str) -> None:
        """Delete a sandbox.
        
        Args:
            sandbox_name: Name of the sandbox to delete
        """
        self.git.drop_snapshot(sandbox_name)

    def list_sandboxes(self) -> list[dict]:
        """List all available sandboxes.
        
        Returns:
            list[dict]: List of sandboxes
        """
        # All snapshots can be used as sandboxes
        return self.git.list_snapshots()

    def run_experiment(
        self,
        experiment_name: str,
        experiment_fn: callable,
        cleanup: bool = True,
    ) -> dict:
        """Run an isolated experiment in a sandbox.
        
        Args:
            experiment_name: Name for the experiment
            experiment_fn: Function to execute in sandbox
            cleanup: Whether to cleanup sandbox after experiment
            
        Returns:
            dict: Experiment results
            
        Example:
            >>> def my_experiment():
            ...     # Run some operations
            ...     return {"result": "success"}
            >>> 
            >>> sandbox = Sandbox()
            >>> result = sandbox.run_experiment("test_exp", my_experiment)
        """
        # Create checkpoint before experiment (with unique timestamp)
        timestamp = str(int(datetime.now().timestamp()))
        checkpoint_name = f"before_{experiment_name}_{timestamp}".lower()
        self.git.create_snapshot(checkpoint_name)

        # Create sandbox (sanitize name)
        sandbox_name = f"sandbox_{experiment_name}_{timestamp}".lower()
        self.create_sandbox(sandbox_name, f"Experiment: {experiment_name}")

        try:
            # Run experiment
            result = experiment_fn()

            return {
                "experiment_name": experiment_name,
                "sandbox_name": sandbox_name,
                "status": "success",
                "result": result,
            }

        except Exception as e:
            return {
                "experiment_name": experiment_name,
                "sandbox_name": sandbox_name,
                "status": "failed",
                "error": str(e),
            }

        finally:
            # Restore to checkpoint
            self.git.restore_from_snapshot(checkpoint_name)

            # Cleanup
            if cleanup:
                self.delete_sandbox(sandbox_name)
                self.git.drop_snapshot(checkpoint_name)
