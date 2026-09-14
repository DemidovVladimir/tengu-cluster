# Tengu Cluster — Makefile
#
# Quick start:    make setup && make up
# With memory:    make up-memory      (Postgres + pgvector agentic memory, image built with postgres_memory)
# Native build:   make native / make native-memory

.PHONY: help setup build up down up-memory logs status \
        doctor clean pull native native-release native-memory

COMPOSE := docker compose
CARGO   := cargo

# ── Help ────────────────────────────────────────────────────────
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

# ── Setup ───────────────────────────────────────────────────────
setup: ## Create .env and config.toml from examples
	@test -f .env || (cp .env.example .env && echo "Created .env — edit it to add API keys")
	@test -f config.toml || (cp config.example.toml config.toml && echo "Created config.toml")
	@echo "Ready. Run: make up"

# ── Docker ──────────────────────────────────────────────────────
build: ## Build tengu Docker image
	$(COMPOSE) build

pull: ## Pull latest base images
	$(COMPOSE) pull

up: ## Start tengu (OpenRouter + Telegram)
	$(COMPOSE) up -d

up-memory: ## Start tengu + Postgres/pgvector agentic memory (rebuilds image with postgres_memory)
	TENGU_FEATURES=openrouter,telegram,postgres_memory $(COMPOSE) --profile postgres-memory up -d --build

down: ## Stop all services
	$(COMPOSE) --profile postgres-memory down

logs: ## Tail tengu logs
	$(COMPOSE) logs -f tengu

status: ## Show running services
	$(COMPOSE) ps

doctor: ## Run tengu diagnostics
	$(COMPOSE) exec tengu tengu doctor

clean: ## Stop all and remove volumes (destructive)
	@echo "This will delete all data volumes. Press Ctrl+C to cancel."
	@sleep 3
	$(COMPOSE) --profile postgres-memory down -v

# ── Native Build ────────────────────────────────────────────────
native: ## Build locally with cargo (debug, default features)
	$(CARGO) build

native-release: ## Build locally with cargo (release, all features)
	$(CARGO) build --release --all-features

native-memory: ## Build locally with the postgres_memory feature
	$(CARGO) build --release --features postgres_memory
