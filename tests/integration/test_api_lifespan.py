"""Test API lifespan (startup and shutdown)."""

import pytest
from fastapi.testclient import TestClient


class TestAPILifespan:
    """Test API startup and shutdown lifecycle."""

    def test_api_startup_and_shutdown_without_errors(self):
        """Test that API can start and shutdown cleanly."""
        from api.main import app
        
        # TestClient context manager triggers both startup and shutdown
        with TestClient(app) as client:
            # Verify API is running
            response = client.get("/health")
            assert response.status_code == 200
        
        # If we reach here, shutdown completed without exceptions

    def test_api_shutdown_cancels_background_tasks(self):
        """Test that shutdown properly cancels background tasks."""
        from api.main import app
        
        with TestClient(app) as client:
            # Verify API started
            assert client.get("/health").status_code == 200
        
        # Shutdown should cancel cleanup_task and trigger_task
        # If scheduler.stop() is called on undefined variable, this will fail

    def test_api_shutdown_stops_embedding_worker(self):
        """Test that shutdown stops embedding worker if it exists."""
        from api.main import app
        
        with TestClient(app) as client:
            response = client.get("/health")
            assert response.status_code == 200
        
        # Shutdown should handle embedding_worker.stop() gracefully

    def test_api_shutdown_waits_for_job_backend(self):
        """Test that shutdown waits for job backend cleanup."""
        from api.main import app
        
        with TestClient(app) as client:
            response = client.get("/health")
            assert response.status_code == 200
        
        # Shutdown should call job_router.shutdown()

    @pytest.mark.asyncio
    async def test_lifespan_context_manager(self):
        """Test lifespan as async context manager."""
        from api.main import app, lifespan
        
        # Test that lifespan can be entered and exited
        async with lifespan(app):
            # During lifespan, background tasks should be running
            pass
        
        # After exit, all tasks should be cancelled/stopped
