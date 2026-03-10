FROM python:3.11-slim

WORKDIR /app

ARG INSTALL_EXTRAS=""

RUN apt-get update && apt-get install -y gcc && rm -rf /var/lib/apt/lists/*

# Dependency layer — cached unless pyproject.toml changes
COPY pyproject.toml poetry.lock README.md ./
# Minimal stubs so pip can resolve the package
COPY core/__init__.py core/
COPY api/__init__.py api/
COPY config/__init__.py config/
COPY mo_memory_mcp/__init__.py mo_memory_mcp/
COPY trustmem_cloud_v1/__init__.py trustmem_cloud_v1/
COPY cli/__init__.py cli/
RUN if [ -n "$INSTALL_EXTRAS" ]; then \
        pip install --no-cache-dir ".[$INSTALL_EXTRAS]"; \
    else \
        pip install --no-cache-dir .; \
    fi

# Application layer
COPY core/ core/
COPY mo_memory_mcp/ mo_memory_mcp/
COPY trustmem_cloud_v1/ trustmem_cloud_v1/
# Shared files used by core/ and trustmem_cloud_v1/
COPY api/__init__.py api/__init__.py
COPY api/_model_types.py api/_model_types.py
COPY api/base.py api/base.py

RUN useradd -m -u 1000 appuser && chown -R appuser:appuser /app
USER appuser

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8000/health', timeout=2)"

CMD ["python", "-m", "uvicorn", "trustmem_cloud_v1.api.main:app", "--host", "0.0.0.0", "--port", "8000"]
