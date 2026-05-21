# Justfile for the policast Compose stack.
#
# Install just:
#   macOS:    brew install just
#   Linux:    cargo install just   (or your distro's package manager)
#
# Usage:
#   just              # list available recipes
#   just up           # start sidecar
#   just demo         # run the DataFusion demo against the sidecar
#   just shell        # drop into an interactive Rust dev shell
#
# Env overrides work the same as with raw docker compose, for example:
#   just role=physician name='Dr. Smith' region= demo

set shell := ["bash", "-cu"]
set dotenv-load := true

# Principal knobs used by `demo`. Override on the command line:
#   just role=physician name='Dr. Smith' region= demo
role    := env_var_or_default("POLICAST_PRINCIPAL_ROLE", "analyst")
region  := env_var_or_default("POLICAST_PRINCIPAL_REGION", "us-east")
name    := env_var_or_default("POLICAST_PRINCIPAL_NAME", "")
id      := env_var_or_default("POLICAST_PRINCIPAL_ID", "alice@hospital.com")
table   := env_var_or_default("POLICAST_TABLE", "hospital.clinical.patients")

# Default recipe: list everything.
_default:
    @just --list --unsorted

# ---------------------------------------------------------------------------
# Bootstrap
# ---------------------------------------------------------------------------

# Copy docker/.env.example to .env if .env does not already exist.
init:
    @if [ ! -f .env ]; then \
        cp docker/.env.example .env; \
        echo "created .env from docker/.env.example"; \
    else \
        echo ".env already exists, leaving it alone"; \
    fi

# ---------------------------------------------------------------------------
# Sidecar lifecycle
# ---------------------------------------------------------------------------

# Start the resolver sidecar in the background and wait for it to be healthy.
up: init
    docker compose up -d sidecar
    @just _wait-healthy sidecar

# Stop and remove the sidecar container (keeps volumes).
down:
    docker compose down

# Stop everything including named volumes (cargo caches, etc).
clean:
    docker compose down -v

# Tail sidecar logs (ctrl-C to exit).
logs:
    docker compose logs -f sidecar

# Print current compose status.
ps:
    docker compose ps

# Curl the sidecar's /health endpoint from the host.
health:
    @curl -fsS http://127.0.0.1:{{ env_var_or_default("POLICAST_UC_PORT", "8765") }}/health && echo

# ---------------------------------------------------------------------------
# One-shot jobs
# ---------------------------------------------------------------------------

# Run the DataFusion demo end-to-end against the sidecar. Honors role/name/region.
demo: up
    POLICAST_PRINCIPAL_ROLE="{{ role }}" \
    POLICAST_PRINCIPAL_REGION="{{ region }}" \
    POLICAST_PRINCIPAL_NAME="{{ name }}" \
    POLICAST_PRINCIPAL_ID="{{ id }}" \
    POLICAST_TABLE="{{ table }}" \
    docker compose --profile demo run --rm datafusion-demo

# Shorthand: analyst in us-east.
#   Row filter on region, columns `ssn` + `diagnosis` are masked.
demo-analyst:
    just role=analyst region=us-east name= demo

# Shorthand: Dr. Smith the physician.
#   Row filter on `treating_physician`, no column masks.
demo-physician:
    just role=physician region= name='Dr. Smith' demo

# Shorthand: admin (no row filters, no column masks).
demo-admin:
    just role=admin region= name= id=admin@hospital.com demo

# SSN-masking contrast demo: run the same query as an admin, a physician,
# and an analyst back-to-back so you can see how the tag-scoped Cedar
# templates `column_mask_by_pii_tag` and `column_mask_by_phi_tag` (which
# expand over the `pii` / `phi` entries in the governance tag index —
# ssn is tagged pii, diagnosis is tagged phi) take effect based on role.
#
#   - admin     -> sees all columns unmasked and all rows
#   - physician -> sees SSN + diagnosis unmasked, but only their own patients
#   - analyst   -> sees `***` for SSN + diagnosis, only rows in their region
demo-ssn: up
    @echo
    @echo "================================================================="
    @echo " 1/3  admin      (no row filter, no column masks)"
    @echo "================================================================="
    @just demo-admin
    @echo
    @echo "================================================================="
    @echo " 2/3  physician  (row filter by treating_physician, no masks)"
    @echo "================================================================="
    @just demo-physician
    @echo
    @echo "================================================================="
    @echo " 3/3  analyst    (row filter by region, ssn + diagnosis masked)"
    @echo "================================================================="
    @just demo-analyst

# Recompile Cedar policies -> examples/policies/manifest.json on the host.
compile:
    docker compose --profile tools run --rm compile

# ---------------------------------------------------------------------------
# Interactive / auxiliary services
# ---------------------------------------------------------------------------

# Drop into an interactive Rust dev shell with the workspace mounted at /workspace.
shell:
    docker compose --profile shell run --rm df-shell

# Run a one-off command inside the dev shell, e.g.:
#   just run "cargo test -p policast-uc"
run cmd:
    docker compose --profile shell run --rm df-shell bash -c "{{ cmd }}"

# Run the workspace's test suite inside the shell image.
test:
    just run "cargo test --workspace"

# Start the staged Unity Catalog OSS server (not yet wired to the sidecar).
uc-oss-up:
    docker compose --profile uc-oss up -d unitycatalog

# Stop the Unity Catalog OSS server.
uc-oss-down:
    docker compose --profile uc-oss stop unitycatalog

# Bring up the full UC-backed profile:
#   minio -> minio-init (bucket) -> unitycatalog -> uc-bootstrap
#   (seed governance delta tables) -> sidecar-uc-full (--backend uc-bootstrap)
uc-full-up:
    docker compose --profile uc-full up -d minio minio-init unitycatalog uc-bootstrap sidecar-uc-full
    @just _wait-healthy sidecar-uc-full

# Run the DataFusion demo against the uc-full sidecar.
uc-full-demo: uc-full-up
    POLICAST_UC_ENDPOINT=http://sidecar-uc-full:8765 docker compose --profile uc-full --profile demo run --rm datafusion-demo

# Tear down the uc-full services while leaving the base file-backed flow intact.
uc-full-down:
    docker compose --profile uc-full rm -sf sidecar-uc-full uc-bootstrap unitycatalog minio minio-init || true

# ---------------------------------------------------------------------------
# Image builds (explicit; compose builds on demand too)
# ---------------------------------------------------------------------------

# Build all three images.
build: build-sidecar build-demo build-shell

build-sidecar:
    DOCKER_BUILDKIT=1 docker build --target sidecar -t policast-sidecar:local -f docker/Dockerfile .

build-demo:
    DOCKER_BUILDKIT=1 docker build --target demo    -t policast-demo:local    -f docker/Dockerfile .

build-shell:
    DOCKER_BUILDKIT=1 docker build --target shell   -t policast-shell:local   -f docker/Dockerfile .

# Rebuild without cache (use when the lockfile / base image changed).
rebuild:
    DOCKER_BUILDKIT=1 docker build --no-cache --target sidecar -t policast-sidecar:local -f docker/Dockerfile .
    DOCKER_BUILDKIT=1 docker build --no-cache --target demo    -t policast-demo:local    -f docker/Dockerfile .
    DOCKER_BUILDKIT=1 docker build --no-cache --target shell   -t policast-shell:local   -f docker/Dockerfile .

# Run CI smoke against non-interactive just recipes.
ci-smoke:
    bash scripts/ci-smoke.sh

# Faster smoke subset for local iteration.
ci-smoke-quick:
    bash scripts/ci-smoke.sh --quick

# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

# Poll `docker compose ps` until the given service reports "healthy"
# (or up to ~30s).
_wait-healthy service:
    @echo -n "waiting for {{ service }} to be healthy"; \
    for i in $(seq 1 30); do \
        status=$(docker inspect --format='{{{{.State.Health.Status}}' "policast-{{ service }}" 2>/dev/null || echo missing); \
        if [ "$status" = "healthy" ]; then echo " ok"; exit 0; fi; \
        echo -n "."; sleep 1; \
    done; \
    echo; echo "service {{ service }} did not become healthy in time" >&2; exit 1
