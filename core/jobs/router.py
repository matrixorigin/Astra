"""Job router — selects backend based on environment and requirements."""

import os
from pathlib import Path

from core.jobs.backend import JobBackend, JobRequirements
from core.jobs.local import LocalJobBackend
from core.logging_config import get_logger

logger = get_logger(__name__)


class JobRouter:
    """Auto-detect available backends and route jobs."""

    def __init__(self) -> None:
        self.backends: dict[str, JobBackend] = {"local": LocalJobBackend()}
        self._detect_optional_backends()

    def _detect_optional_backends(self) -> None:
        if os.getenv("RAY_ADDRESS"):
            try:
                from core.jobs.ray_backend import RayJobBackend

                self.backends["ray"] = RayJobBackend(address=os.environ["RAY_ADDRESS"])
                logger.info("Ray backend available")
            except ImportError:
                pass

        if os.getenv("KUBERNETES_SERVICE_HOST") or Path("~/.kube/config").expanduser().exists():
            try:
                from core.jobs.k8s_backend import K8sJobBackend

                self.backends["k8s"] = K8sJobBackend()
                logger.info("K8s backend available")
            except ImportError:
                pass

    def select(self, requirements: JobRequirements) -> JobBackend:
        if requirements.gpu_required:
            for name in ("ray", "k8s", "local"):
                if name in self.backends:
                    return self.backends[name]
        return self.backends["local"]

    async def shutdown(self) -> None:
        """Graceful shutdown — propagate to all backends."""
        for backend in self.backends.values():
            await backend.shutdown()
