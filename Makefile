# mo-dev-agent Makefile
# Inspired by MatrixOne's development workflow

.PHONY: help
help:
	@echo "mo-dev-agent Development Commands"
	@echo "=================================="
	@echo ""
	@echo "Environment Setup:"
	@echo "  make setup              - Initial project setup (copy .env, install deps)"
	@echo "  make install            - Install Python dependencies"
	@echo ""
	@echo "Development Environment:"
	@echo "  make dev-up             - Start MatrixOne + Redis"
	@echo "  make dev-down           - Stop all services"
	@echo "  make dev-restart        - Restart all services"
	@echo "  make dev-logs           - Show all logs (tail -f)"
	@echo "  make dev-logs-db        - Show MatrixOne logs"
	@echo "  make dev-ps             - Show service status"
	@echo "  make dev-clean          - Stop and remove all data (WARNING: destructive!)"
	@echo ""
	@echo "Database:"
	@echo "  make db-init            - Initialize database schema"
	@echo "  make db-connect         - Connect to MatrixOne CLI"
	@echo "  make db-reset           - Reset database (drop + recreate)"
	@echo ""
	@echo "Testing:"
	@echo "  make test               - Run all tests"
	@echo "  make test-unit          - Run unit tests"
	@echo "  make test-integration   - Run integration tests"
	@echo "  make test-e2e           - Run end-to-end tests"
	@echo ""
	@echo "Code Quality:"
	@echo "  make lint               - Run linters"
	@echo "  make format             - Format code"
	@echo ""
	@echo "Examples:"
	@echo "  make setup && make dev-up && make db-init  # First time setup"
	@echo "  make dev-up && make test                   # Daily development"

# ============================================================================
# Environment Setup
# ============================================================================

.PHONY: setup
setup:
	@echo "Setting up mo-dev-agent development environment..."
	@if [ ! -f .env ]; then \
		cp .env.example .env; \
		echo "✅ Created .env file (please review and customize)"; \
	else \
		echo "⚠️  .env already exists, skipping"; \
	fi
	@echo ""
	@$(MAKE) install
	@echo ""
	@echo "✅ Setup complete! Next steps:"
	@echo "   1. Review and customize .env file"
	@echo "   2. Run: make dev-up"
	@echo "   3. Run: make db-init"

.PHONY: install
install:
	@echo "Installing Python dependencies..."
	@if command -v poetry >/dev/null 2>&1; then \
		poetry install; \
	else \
		pip install -e .; \
	fi

# ============================================================================
# Development Environment
# ============================================================================

.PHONY: dev-up
dev-up:
	@echo "Starting mo-dev-agent development environment..."
	@cd infra && docker compose --profile dev up -d
	@echo ""
	@echo "✅ Services started!"
	@echo "   MatrixOne: mysql -h127.0.0.1 -P6001 -uroot -p111"
	@echo "   Redis:     redis-cli -h 127.0.0.1 -p 6379"
	@echo ""
	@echo "Next: make db-init (to initialize database schema)"

.PHONY: dev-down
dev-down:
	@echo "Stopping mo-dev-agent services..."
	@cd infra && docker compose --profile dev down

.PHONY: dev-restart
dev-restart:
	@echo "Restarting mo-dev-agent services..."
	@cd infra && docker compose --profile dev restart

.PHONY: dev-logs
dev-logs:
	@cd infra && docker compose --profile dev logs -f

.PHONY: dev-logs-db
dev-logs-db:
	@cd infra && docker compose logs -f matrixone

.PHONY: dev-ps
dev-ps:
	@cd infra && docker compose --profile dev ps

.PHONY: dev-clean
dev-clean:
	@echo "⚠️  WARNING: This will delete all data!"
	@printf "Are you sure? [y/N] "; \
	read REPLY; \
	if [ "$$REPLY" = "y" ] || [ "$$REPLY" = "Y" ]; then \
		cd infra && docker compose --profile dev down -v; \
		echo "✅ All data removed"; \
	else \
		echo "Cancelled"; \
	fi

.PHONY: dev-init
dev-init: dev-up db-init
	@echo "✅ Development environment ready!"

# ============================================================================
# Database
# ============================================================================

.PHONY: db-init
db-init:
	@echo "Initializing database schema..."
	@bash infra/scripts/init-db.sh

.PHONY: db-connect
db-connect:
	@echo "Connecting to MatrixOne..."
	@echo ""
	@mysql -h127.0.0.1 -P6001 -uroot -p111 2>/dev/null || \
	mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl 2>/dev/null || \
	mysql -h127.0.0.1 -P6001 -uroot -p111 --skip_ssl 2>/dev/null || \
	(echo "❌ Connection failed. Please try manually:" && \
	 echo "   mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl   # or" && \
	 echo "   mysql -h127.0.0.1 -P6001 -uroot -p111 --skip_ssl   # (underscore)" && \
	 exit 1)

.PHONY: db-reset
db-reset:
	@echo "⚠️  WARNING: This will drop and recreate the database!"
	@printf "Are you sure? [y/N] "; \
	read REPLY; \
	if [ "$$REPLY" = "y" ] || [ "$$REPLY" = "Y" ]; then \
		if [ -f .env ]; then \
			export $$(cat .env | grep -v '^#' | xargs); \
			DB_NAME=$${MATRIXONE_DATABASE:-dev_agent}; \
		else \
			DB_NAME=dev_agent; \
		fi; \
		mysql -h127.0.0.1 -P6001 -uroot -p111 -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;" 2>/dev/null || \
		mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;" 2>/dev/null || \
		mysql -h127.0.0.1 -P6001 -uroot -p111 --skip_ssl -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;"; \
		$(MAKE) db-init; \
		echo "✅ Database reset complete"; \
	else \
		echo "Cancelled"; \
	fi

# ============================================================================
# Testing
# ============================================================================

.PHONY: test
test:
	@echo "Running all tests..."
	@pytest tests/ -v

.PHONY: test-unit
test-unit:
	@echo "Running unit tests..."
	@pytest tests/unit/ -v

.PHONY: test-integration
test-integration:
	@echo "Running integration tests..."
	@pytest tests/integration/ -v

.PHONY: test-e2e
test-e2e:
	@echo "Running end-to-end tests..."
	@pytest tests/e2e/ -v

# ============================================================================
# Code Quality
# ============================================================================

.PHONY: lint
lint:
	@echo "Running linters..."
	@ruff check .
	@mypy sdk/ core/

.PHONY: format
format:
	@echo "Formatting code..."
	@ruff format .
