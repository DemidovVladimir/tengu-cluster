# Tengu Cluster — Makefile
#
# Quick start:    make setup && make tor && cargo run -- chat      (native tengu, Tor proxy in Docker)
# Docker:         make up [SANDBOX=<name>]   (`tengu telegram` on ./config.toml or sandboxes/<name>/config.toml)
# With memory:    make up-memory          (+ Postgres/pgvector, image built with postgres_memory)
#
# SANDBOX=<name> mounts sandboxes/<name>/config.toml as the container's config
# (TENGU_CONFIG) instead of ./config.toml. Pass the same SANDBOX to every
# make target of that stack (NETWORK is derived from it).
# NETWORK defaults to that config's `[egress] network` (tor unless it says
# "open"). tor uses docker-compose.tor.yml: tengu on an internal network whose
# only exit is the Arti + lyrebird-rs container; open skips the override.
# Override with NETWORK=tor|open only if you know the config agrees.
# LYREBIRD_RS_DIR=<dir or git URL> overrides where lyrebird-rs is built from
# (default ../lyrebird-rs). `make tor*` targets drive the standalone proxy
# stack (deploy/tor/compose.yml) for native tengu; `make up` runs its own
# `tor` service inside the tengu compose project.

.PHONY: help setup build up up-memory chat down down-all logs status doctor clean pull check-config \
        tor tor-down tor-logs tor-bridges native native-release native-memory

SANDBOX ?=
CONFIG_FILE := $(if $(SANDBOX),sandboxes/$(SANDBOX)/config.toml,config.toml)
export TENGU_CONFIG_FILE := $(CONFIG_FILE)
NETWORK ?= $(if $(shell grep -Eqs '^[[:space:]]*network[[:space:]]*=[[:space:]]*"open"' $(CONFIG_FILE) && echo y),open,tor)
LYREBIRD_RS_DIR ?= ../lyrebird-rs
# Compose resolves relative build contexts against deploy/tor/, so hand it an
# absolute path (git URLs pass through).
export LYREBIRD_RS_SRC := $(if $(findstring ://,$(LYREBIRD_RS_DIR)),$(LYREBIRD_RS_DIR),$(abspath $(LYREBIRD_RS_DIR)))

ifeq ($(NETWORK),tor)
COMPOSE := docker compose -f docker-compose.yml -f docker-compose.tor.yml
else
COMPOSE := docker compose -f docker-compose.yml
endif
TOR_COMPOSE := docker compose -f deploy/tor/compose.yml
CARGO   := cargo

# ── Help ────────────────────────────────────────────────────────
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

# ── Setup ───────────────────────────────────────────────────────
setup: ## Create .env and config.toml from examples
	@test -f .env || (cp .env.example .env && echo "Created .env — edit it to add API keys")
	@test -f config.toml || (cp config.example.toml config.toml && echo "Created config.toml")
	@echo "Ready. Run: make tor && cargo run -- chat   (or: make up)"

# ── Tor proxy (deploy/tor: Arti + lyrebird-rs) ──────────────────
tor: ## Start the standalone Tor proxy on 127.0.0.1:9050 for native tengu (needs lyrebird-rs at $(LYREBIRD_RS_DIR))
	$(TOR_COMPOSE) up -d --build --wait

tor-down: ## Stop the standalone Tor proxy (the `make up` stack has its own `tor`: use `make down`)
	$(TOR_COMPOSE) down

tor-logs: ## Tail Arti + lyrebird-rs logs of the standalone proxy (`make up` stack: docker compose logs tor)
	$(TOR_COMPOSE) logs -f tor

tor-bridges: ## Print current Tor Browser bridge lines (obfs4 + snowflake) for deploy/tor/arti.toml
	@$(LYREBIRD_RS_DIR)/tools/arti-e2e/bridges.sh obfs4
	@$(LYREBIRD_RS_DIR)/tools/arti-e2e/bridges.sh snowflake

# ── Docker tengu ────────────────────────────────────────────────
build: ## Build the tengu image (and the Tor image under NETWORK=tor)
	$(COMPOSE) build

pull: ## Pull latest base images
	$(COMPOSE) pull

check-config: ## Fail unless the config `make up` would mount exists
	@test -f $(CONFIG_FILE) || { echo "$(CONFIG_FILE) not found — run \`make setup\` or pass SANDBOX=<name>"; exit 1; }
	@echo "config: $(CONFIG_FILE)  network: $(NETWORK)"

up: check-config ## Start tengu (Telegram) on ./config.toml, or SANDBOX=<name>; network from its [egress]
	$(COMPOSE) up -d --build

up-memory: check-config ## Start tengu + Postgres/pgvector agentic memory (image rebuilt with postgres_memory)
	TENGU_FEATURES=openrouter,telegram,postgres_memory $(COMPOSE) --profile postgres-memory up -d --build

chat: check-config ## Interactive TUI in a throwaway container (same SANDBOX / NETWORK wiring as `make up`)
	$(COMPOSE) run --rm tengu chat

down: ## Stop all services
	$(COMPOSE) --profile postgres-memory down --remove-orphans

down-all: ## Stop + remove every tengu container/network (either NETWORK, any SANDBOX, standalone `make tor`); keeps volumes
	docker compose -f docker-compose.yml -f docker-compose.tor.yml --profile postgres-memory down --remove-orphans
	$(TOR_COMPOSE) down --remove-orphans

logs: ## Tail tengu logs
	$(COMPOSE) logs -f tengu

status: ## Show running services
	$(COMPOSE) ps

doctor: ## Run tengu diagnostics inside the container (live Tor-exit check under NETWORK=tor)
	$(COMPOSE) exec tengu tengu doctor $(if $(filter tor,$(NETWORK)),--tor,)

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
