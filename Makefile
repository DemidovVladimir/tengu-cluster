# Tengu Cluster — Makefile
#
# Quick start:  make setup && make up
# With GPU:     make up-gpu
# Full stack:   make up-full

.PHONY: help setup build up down up-gpu up-full up-cpu logs status \
        doctor clean pull native native-release

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

up: ## Start tengu (API backends only)
	$(COMPOSE) up -d

up-gpu: ## Start tengu + Ollama with NVIDIA GPU
	$(COMPOSE) --profile ollama-gpu up -d

up-cpu: ## Start tengu + Ollama (CPU only)
	$(COMPOSE) --profile ollama up -d

up-full: ## Start tengu + Ollama GPU + Qdrant
	$(COMPOSE) --profile full up -d

up-full-cpu: ## Start tengu + Ollama CPU + Qdrant
	$(COMPOSE) --profile full-cpu up -d

up-qdrant: ## Start tengu + Qdrant (no Ollama)
	$(COMPOSE) --profile qdrant up -d

down: ## Stop all services
	$(COMPOSE) --profile full --profile full-cpu --profile ollama --profile ollama-gpu --profile qdrant down

logs: ## Tail tengu logs
	$(COMPOSE) logs -f tengu

status: ## Show running services
	$(COMPOSE) ps

doctor: ## Run tengu diagnostics
	$(COMPOSE) exec tengu tengu doctor

clean: ## Stop all and remove volumes (destructive)
	@echo "This will delete all data volumes. Press Ctrl+C to cancel."
	@sleep 3
	$(COMPOSE) --profile full --profile full-cpu --profile ollama --profile ollama-gpu --profile qdrant down -v

# ── Native Build ────────────────────────────────────────────────
native: ## Build locally with cargo (debug)
	$(CARGO) build

native-release: ## Build locally with cargo (release, all features)
	$(CARGO) build --release

native-qdrant: ## Build locally with qdrant feature
	$(CARGO) build --release --features qdrant
