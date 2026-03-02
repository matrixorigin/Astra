FROM python:3.11-slim

WORKDIR /app

# Optional extras, e.g. "local-embedding" for sentence-transformers.
# Usage: docker build --build-arg INSTALL_EXTRAS="local-embedding" .
ARG INSTALL_EXTRAS=""

# Install system dependencies
RUN apt-get update && apt-get install -y \
    gcc \
    && rm -rf /var/lib/apt/lists/*

# --- Dependency layer (cached unless pyproject.toml changes) ---
# Copy dependency definition + minimal package stubs so pip can resolve
# the project as a package without the full source tree.
# Trailing "/" ensures Docker creates the directory and copies the file into it.
# poetry.lock is copied to bust Docker cache when transitive deps change,
# even if pyproject.toml version ranges haven't changed.
COPY pyproject.toml poetry.lock ./
COPY core/__init__.py core/
COPY api/__init__.py api/
COPY config/__init__.py config/
RUN if [ -n "$INSTALL_EXTRAS" ]; then \
        pip install --no-cache-dir ".[$INSTALL_EXTRAS]"; \
    else \
        pip install --no-cache-dir .; \
    fi

# --- Application layer (changes frequently) ---
COPY . .

# Create non-root user
RUN useradd -m -u 1000 appuser && chown -R appuser:appuser /app
USER appuser

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8000/health', timeout=2)"

CMD ["python", "-m", "uvicorn", "api.main:app", "--host", "0.0.0.0", "--port", "8000"]
