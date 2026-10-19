#!/bin/sh
#
# scripts/deploy_local.sh — One-command local Soroban testnet setup
#
# Builds the subscription-vault contract, deploys it to a local Stellar
# quickstart network, creates test identities, wraps a test token, runs
# init as a smoke test, and exercises the full subscription lifecycle.
#
# Usage:
#   ./scripts/deploy_local.sh                       # full setup (Docker + deploy)
#   ./scripts/deploy_local.sh --no-docker            # skip Docker, use existing network
#   ./scripts/deploy_local.sh --skip-smoke           # skip the lifecycle smoke test
#   ./scripts/deploy_local.sh --allow-protocol-mismatch   # warn instead of fail on protocol mismatch
#   ./scripts/deploy_local.sh --help                 # print help and exit
#
# On re-run, the script re-uses any existing Docker container, CLI identity,
# and already-deployed token (stored in .deploy-state). If the contract is
# already initialized, init is skipped.
#
# Environment variables (optional):
#   STELLAR_CLI       Path to stellar/soroban CLI binary (auto-detected)
#   TOKEN_ADDR        Override token contract address (skip token deploy)
#   CONTRACT_DIR      Contract crate directory (default: contracts/subscription_vault)
#   NETWORK_NAME      Network alias (default: local-dev)
#   RPC_URL           Soroban RPC URL (default: http://localhost:8000/soroban/rpc)
#   NETWORK_PASSPHRASE (default: "Standalone Network ; February 2017")
#   QUICKSTART_IMAGE  Override the pinned quickstart image (see QUICKSTART_IMAGE below)
#   EXPECTED_PROTOCOL_VERSION
#                     Override the expected Soroban protocol version
#                     (default: major of soroban-sdk in the contract Cargo.toml)
#
# Exit codes:
#   0 — success
#   1 — dependency missing or configuration error
#   2 — build or deploy failure
#   3 — Soroban protocol version mismatch

set -eu

# ── Color helpers (POSIX-safe, fall back to plain text) ─────────────────────
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BLUE=''; BOLD=''; NC=''
fi

# ── Paths ───────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
STATE_FILE="${ROOT_DIR}/.deploy-state"

# ── Default configuration ───────────────────────────────────────────────────
CONTRACT_DIR="${CONTRACT_DIR:-${ROOT_DIR}/contracts/subscription_vault}"
CONTRACT_NAME="subscription_vault"
NETWORK_NAME="${NETWORK_NAME:-local-dev}"
RPC_URL="${RPC_URL:-http://localhost:8000/soroban/rpc}"
NETWORK_PASSPHRASE="${NETWORK_PASSPHRASE:-Standalone Network ; February 2017}"
ADMIN_IDENTITY="${ADMIN_IDENTITY:-admin-local}"
SUBSCRIBER_IDENTITY="${SUBSCRIBER_IDENTITY:-subscriber-local}"
MERCHANT_IDENTITY="${MERCHANT_IDENTITY:-merchant-local}"
TOKEN_DECIMALS=7
NETWORK_CONTAINER="stellabill-local"

# ── Pinned quickstart image (issue #216) ─────────────────────────────────────
# The local Soroban network MUST come from a known-good, immutable image.
# `stellar/quickstart:testing` is a MOVING tag — it is re-pushed whenever a new
# stellar release lands, so an unpinned pull can silently change the Soroban
# protocol version underneath a contract WASM that was compiled and tested
# against a different one. The symptom is a smoke test that fails for no
# visible reason, or worse, passes while exercising a different protocol.
#
# The pin below is tag *and* manifest digest, so the image is reproducible even
# if the tag is later re-pushed. Bump it deliberately, in a reviewed commit, and
# re-run the smoke test when you do.
#
# Digest: sha256:427069406fbbe2ecd091f75d5a9c1e588ac3108875dec6ab6e0e2ac9f76c311e
QUICKSTART_IMAGE="${QUICKSTART_IMAGE:-stellar/quickstart:v670-b1459.1-testing@sha256:427069406fbbe2ecd091f75d5a9c1e588ac3108875dec6ab6e0e2ac9f76c311e}"

# Expected Soroban protocol version. Defaults to the major version of
# soroban-sdk the contract is built against (see sdk_major_from_cargo), because
# a contract compiled for protocol N cannot be trusted on a network running a
# different protocol. Overridable for deliberate upgrades.
EXPECTED_PROTOCOL_VERSION="${EXPECTED_PROTOCOL_VERSION:-}"

# ── Flags ───────────────────────────────────────────────────────────────────
SKIP_SMOKE=false
NO_DOCKER=false
CLEANUP_CONTAINER=false
ALLOW_PROTOCOL_MISMATCH=false

# ── Cleanup trap ────────────────────────────────────────────────────────────
cleanup() {
    if [ "${CLEANUP_CONTAINER}" = "true" ]; then
        info "Cleaning up container '${NETWORK_CONTAINER}' ..."
        docker stop "${NETWORK_CONTAINER}" 2>/dev/null || true
        docker rm "${NETWORK_CONTAINER}" 2>/dev/null || true
        ok "Container stopped and removed."
    fi
}
trap cleanup EXIT INT TERM

# ── Help ────────────────────────────────────────────────────────────────────
usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

One-command local Soroban testnet setup for the subscription-vault contract.

Builds the contract, deploys to a local Stellar quickstart network,
creates test identities, wraps a test token (native XLM via SAC), and
calls init as a smoke test.

OPTIONS:
  --no-docker        Skip Docker container start; assume network is already running
  --skip-smoke       Skip the full subscription lifecycle smoke test
  --allow-protocol-mismatch
                     Warn (instead of exit 3) when the network's Soroban
                     protocol version differs from the expected one
  --help             Show this help message and exit

ENVIRONMENT VARIABLES:
  STELLAR_CLI       Path to stellar/soroban CLI (auto-detected if unset)
  TOKEN_ADDR        Pre-existing token contract address (skip token deploy)
  CONTRACT_DIR      Contract crate directory (default: contracts/subscription_vault)
  NETWORK_NAME      Network alias used by the CLI (default: local-dev)
  RPC_URL           Soroban RPC endpoint (default: http://localhost:8000/soroban/rpc)
  NETWORK_PASSPHRASE (default: "Standalone Network ; February 2017")
  QUICKSTART_IMAGE  Pinned quickstart image (tag@digest); see script header
  EXPECTED_PROTOCOL_VERSION
                     Expected Soroban protocol version (default: soroban-sdk major)
EOF
    exit 0
}

# ── Logging helpers ─────────────────────────────────────────────────────────
info()  { printf "${BLUE}[INFO]${NC}  %s\n" "$*"; }
ok()    { printf "${GREEN}[ OK ]${NC} %s\n" "$*"; }
warn()  { printf "${YELLOW}[WARN]${NC} %s\n" "$*" >&2; }
err()   { printf "${RED}[ERR ]${NC} %s\n" "$*" >&2; }
step()  { printf "\n${BLUE}==>${NC} ${BOLD}%s${NC}\n" "$*"; }
run()   {
    printf "${YELLOW}\$ %s${NC}\n" "$*"
    "$@"
}

# =============================================================================
# STEP 0 — Dependency checks
# =============================================================================
detect_cli() {
    if [ -n "${STELLAR_CLI:-}" ]; then
        CLI="$STELLAR_CLI"
    elif command -v stellar >/dev/null 2>&1; then
        CLI="$(command -v stellar)"
    elif command -v soroban >/dev/null 2>&1; then
        CLI="$(command -v soroban)"
    else
        printf '%s\n' \
            "ERROR: Neither 'stellar' nor 'soroban' CLI found in PATH." \
            "" \
            "Install the Soroban CLI:" \
            "  https://developers.stellar.org/docs/tools/soroban-cli/install" \
            "" \
            "Quick install (macOS/Linux):" \
            "  curl -fsSL https://github.com/stellar/stellar-cli/raw/main/install.sh | sh" \
            >&2
        exit 1
    fi
    CLI_BASENAME="$(basename "${CLI}")"

    # Detect CLI version: old `soroban` uses --name flag, new `stellar` uses positional
    if "${CLI}" network add --help 2>&1 | grep -qF -- "--name" 2>/dev/null; then
        CLI_OLD_STYLE=1
    else
        CLI_OLD_STYLE=0
    fi

    if ! "${CLI}" --version >/dev/null 2>&1; then
        err "CLI binary '${CLI}' does not run. Is it a valid executable?"
        exit 1
    fi
    ok "CLI: ${CLI} $("${CLI}" --version 2>&1 | head -1)"
}

check_rust() {
    if ! command -v rustc >/dev/null 2>&1; then
        printf '%s\n' \
            "ERROR: Rust (rustc) not found. Install from: https://rustup.rs/" \
            "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" \
            >&2
        exit 1
    fi
    if ! rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$'; then
        info "Adding wasm32-unknown-unknown target..."
        run rustup target add wasm32-unknown-unknown
    fi
    ok "Rust: $(rustc --version)"
}

check_curl() {
    if ! command -v curl >/dev/null 2>&1; then
        err "curl not found. Install curl to use this script."
        err "  apt-get install curl   # Debian/Ubuntu"
        err "  brew install curl      # macOS"
        exit 1
    fi
    ok "curl available."
}

check_docker() {
    if ! command -v docker >/dev/null 2>&1; then
        printf '%s\n' \
            "ERROR: Docker not found." \
            "" \
            "The local Soroban network requires a container running" \
            "the pinned stellar/quickstart image (see QUICKSTART_IMAGE)." \
            "" \
            "Install Docker: https://docs.docker.com/get-docker/" \
            "Or re-run with --no-docker if you already have a network." \
            >&2
        exit 1
    fi
    ok "Docker: $(docker --version 2>/dev/null)"
}

check_docker_daemon() {
    if ! docker info >/dev/null 2>&1; then
        printf '%s\n' \
            "ERROR: Docker daemon is not running." \
            "" \
            "The docker binary was found, but the daemon is not available." \
            "" \
            "Start the Docker daemon:" \
            "  • Linux: systemctl start docker  (or use your init system)" \
            "  • macOS: open -a Docker" \
            "  • Windows: Start Docker Desktop" \
            "" \
            "If you have a network running locally without Docker," \
            "use: ./scripts/deploy_local.sh --no-docker" \
            >&2
        exit 1
    fi
    ok "Docker daemon is running."
}

# =============================================================================
# STEP 1 — Build contract WASM
# =============================================================================
build_contract() {
    WASM_PATH="${ROOT_DIR}/target/wasm32-unknown-unknown/release/${CONTRACT_NAME}.wasm"
    if [ -f "${WASM_PATH}" ]; then
        info "Existing WASM found at ${WASM_PATH} — rebuilding."
    fi

    info "Building ${CONTRACT_NAME} WASM (release profile)..."
    run cargo build \
        --manifest-path "${CONTRACT_DIR}/Cargo.toml" \
        --release \
        --target wasm32-unknown-unknown

    if [ ! -f "${WASM_PATH}" ]; then
        # Try alternative path from workspace root
        WASM_PATH="${ROOT_DIR}/target/wasm32-unknown-unknown/release/${CONTRACT_NAME}.wasm"
    fi
    if [ ! -f "${WASM_PATH}" ]; then
        err "WASM not found after build. Searched: ${WASM_PATH}"
        ls -la "${ROOT_DIR}/target/wasm32-unknown-unknown/release/" 2>/dev/null || true
        exit 2
    fi
    WASM_SIZE="$(ls -lh "${WASM_PATH}" 2>/dev/null | awk '{print $5}')"
    [ -n "${WASM_SIZE}" ] || WASM_SIZE="unknown"
    ok "WASM built: ${WASM_PATH} (${WASM_SIZE})"
}

# =============================================================================
# STEP 2 — Start / verify local network
# =============================================================================

# Major version of `soroban-sdk` the contract is built against.
# Read from the contract Cargo.toml so the expected protocol version can never
# drift from the SDK the WASM was actually compiled with.
sdk_major_from_cargo() {
    manifest="${CONTRACT_DIR}/Cargo.toml"
    if [ ! -f "${manifest}" ]; then
        warn "Cannot read ${manifest}; cannot derive expected protocol version."
        return 0
    fi
    # First soroban-sdk dependency line, e.g. soroban-sdk = "22.0.0"
    sed -n 's/^[[:space:]]*soroban-sdk[[:space:]]*=[[:space:]]*"\([0-9][0-9.]*\)".*/\1/p' \
        "${manifest}" | head -1 | cut -d. -f1
}

# Query the Soroban protocol version advertised by the running network.
rpc_protocol_version() {
    curl -sS -m 15 -X POST \
        -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"getLedgerInfo"}' \
        "${RPC_URL}" 2>/dev/null | sed -n 's/.*"protocol_version"[[:space:]]*:[[:space:]]*"\{0,1\}\([0-9][0-9]*\)"\{0,1\}.*/\1/p' | head -1
}

# Fail loudly when the network's Soroban protocol version is not the one the
# contract was compiled and tested against. A mismatch means the smoke test
# below is exercising a protocol the contract was never validated on, which is
# exactly the "silent smoke-test failure" this guard prevents.
verify_protocol_version() {
    expected="${EXPECTED_PROTOCOL_VERSION}"
    if [ -z "${expected}" ]; then
        expected="$(sdk_major_from_cargo)"
    fi

    if [ -z "${expected}" ]; then
        warn "SKIP protocol check: no expected version (set EXPECTED_PROTOCOL_VERSION)."
        return 0
    fi

    actual="$(rpc_protocol_version)"
    if [ -z "${actual}" ]; then
        warn "SKIP protocol check: could not read protocol_version from ${RPC_URL}."
        return 0
    fi

    if [ "${actual}" = "${expected}" ]; then
        PROTOCOL_VERSION_REPORTED="${actual}"
        ok "Soroban protocol version: ${actual} (expected ${expected})."
        return 0
    fi

    # ── Mismatch ──
    PROTOCOL_VERSION_REPORTED="${actual}"
    err "==========================================================="
    err " SOROBAN PROTOCOL VERSION MISMATCH"
    err "==========================================================="
    err ""
    err "  Network reports : protocol ${actual}"
    err "  Contract expects : protocol ${expected}"
    err "  Pinned image     : ${QUICKSTART_IMAGE}"
    err ""
    err "  The contract WASM was compiled against protocol ${expected}"
    err "  but the local network is running protocol ${actual}."
    err "  A protocol change can alter host function semantics, so the"
    err "  smoke test below would not be validating this contract."
    err ""

    if [ "${ALLOW_PROTOCOL_MISMATCH}" = "true" ]; then
        warn "  --allow-protocol-mismatch given: continuing as a WARNING."
        warn "  Smoke-test results are NOT trustworthy under this mismatch."
        return 0
    fi

    err "  Fix by one of:"
    err "    1. Start a network matching the contract (remove the stale container):"
    err "         docker rm -f ${NETWORK_CONTAINER}"
    err "    2. Override the pinned image to one running protocol ${expected}:"
    err "         QUICKSTART_IMAGE=stellar/quickstart:<tag>@<digest> $0"
    err "    3. If the contract was intentionally upgraded, update"
    err "       contracts/subscription_vault/Cargo.toml (soroban-sdk), re-test,"
    err "       then re-pin QUICKSTART_IMAGE in this script."
    err "    4. To proceed anyway and inspect the failure:"
    err "         $0 --allow-protocol-mismatch"
    err ""
    exit 3
}

# Verify a pre-existing container is running the pinned image. Reusing a
# container started from a different image would bypass the pin entirely.
verify_container_image() {
    running_image="$(docker inspect -f '{{.Config.Image}}' "${NETWORK_CONTAINER}" 2>/dev/null || true)"
    [ -n "${running_image}" ] || return 0

    if [ "${running_image}" = "${QUICKSTART_IMAGE}" ]; then
        return 0
    fi

    err "Container '${NETWORK_CONTAINER}' is running a different image:"
    err "  running : ${running_image}"
    err "  expected: ${QUICKSTART_IMAGE}"
    err ""
    err "Reusing it would bypass the pinned image and the protocol check."
    err "Recreate it with:"
    err "  docker rm -f ${NETWORK_CONTAINER} && $0"
    exit 3
}

ensure_network() {
    if [ "${NO_DOCKER}" = "true" ]; then
        info "Skipping Docker (--no-docker). Checking network reachability..."
        wait_for_rpc 10
        verify_protocol_version
        return 0
    fi

    if docker inspect "${NETWORK_CONTAINER}" >/dev/null 2>&1; then
        verify_container_image
        container_running="$(docker inspect -f '{{.State.Running}}' "${NETWORK_CONTAINER}" 2>/dev/null)"
        if [ "${container_running}" = "true" ]; then
            info "Docker container '${NETWORK_CONTAINER}' already running."
            wait_for_rpc 30
            verify_protocol_version
            return 0
        else
            info "Container '${NETWORK_CONTAINER}' exists but is stopped. Starting..."
            run docker start "${NETWORK_CONTAINER}"
            wait_for_rpc 30
            verify_protocol_version
            return 0
        fi
    fi

    info "Starting Stellar quickstart container (detached)..."
    info "  image: ${QUICKSTART_IMAGE}"
    # shellcheck disable=SC2086
    run docker run -d \
        --name "${NETWORK_CONTAINER}" \
        -p 8000:8000 \
        "${QUICKSTART_IMAGE}" \
        --standalone \
        --enable-soroban

    # Track that we started it so cleanup trap stops it
    CLEANUP_CONTAINER=true

    wait_for_rpc 60
    verify_protocol_version
    ok "Local Stellar network ready at ${RPC_URL}"
}

wait_for_rpc() {
    max_seconds="${1:-30}"
    max_tries=$((max_seconds / 2))
    info "Waiting for RPC at ${RPC_URL} (up to ${max_seconds}s)..."
    i=0
    while [ "${i}" -lt "${max_tries}" ]; do
        if curl -sf "${RPC_URL}" >/dev/null 2>&1; then
            ok "RPC is ready."
            return 0
        fi
        sleep 2
        i=$((i + 1))
    done
    err "RPC at ${RPC_URL} not available after ${max_seconds}s."
    err "Check: docker logs ${NETWORK_CONTAINER}"
    exit 1
}

# =============================================================================
# STEP 3 — Configure CLI network
# =============================================================================
configure_network() {
    network_exists=0
    "${CLI}" network ls 2>/dev/null | grep -qF "${NETWORK_NAME}" && network_exists=1

    if [ "${network_exists}" = "1" ]; then
        info "Network '${NETWORK_NAME}' already configured. Skipping."
        return 0
    fi

    info "Adding network '${NETWORK_NAME}'..."
    if [ "${CLI_OLD_STYLE}" = "1" ]; then
        run "${CLI}" network add \
            --name "${NETWORK_NAME}" \
            --rpc-url "${RPC_URL}" \
            --network-passphrase "${NETWORK_PASSPHRASE}"
    else
        run "${CLI}" network add \
            "${NETWORK_NAME}" \
            --rpc-url "${RPC_URL}" \
            --network-passphrase "${NETWORK_PASSPHRASE}"
    fi
    ok "Network '${NETWORK_NAME}' configured."
}

# =============================================================================
# STEP 4 — Create and fund identities
# =============================================================================
ensure_identity() {
    label="$1"
    ident_exists=0
    "${CLI}" keys ls 2>/dev/null | grep -qF "${label}" && ident_exists=1

    if [ "${ident_exists}" = "1" ]; then
        if [ "${ALLOW_IDENTITY_REUSE:-false}" = "true" ]; then
            warn "Identity '${label}' already exists — reusing (--allow-identity-reuse)."
        else
            err "Identity '${label}' already exists in the CLI keystore."
            err "Reusing a stale identity can cause unexpected auth failures."
            err "Fix by one of:"
            err "  1. Remove it:  ${CLI} keys rm ${label}"
            err "  2. Use a unique suffix:  ADMIN_IDENTITY=${label}-$$ $0"
            err "  3. Explicitly allow reuse:  $0 --allow-identity-reuse"
            exit 1
        fi
    else
        info "Generating identity '${label}'..."
        run "${CLI}" keys generate "${label}"
        ok "Identity '${label}' created."
    fi

    addr="$("${CLI}" keys address "${label}" 2>/dev/null || true)"
    if [ -z "${addr}" ]; then
        err "Failed to retrieve address for '${label}'."
        exit 1
    fi
    echo "${addr}"
}

fund_identity() {
    addr="$1"
    # Derive friendbot URL from RPC_URL
    case "${RPC_URL}" in
        */soroban/rpc)
            FRIENDBOT_URL="${RPC_URL%/soroban/rpc}/friendbot"
            ;;
        *)
            FRIENDBOT_URL="http://localhost:8000/friendbot"
            ;;
    esac

    friendbot_output=""
    friendbot_output="$(curl -sf "${FRIENDBOT_URL}?addr=${addr}" 2>&1 || true)"
    if echo "${friendbot_output}" | grep -q '"hash"' 2>/dev/null; then
        ok "Funded ${addr}"
    else
        warn "Friendbot response (may already be funded) for ${addr}"
    fi
}

# =============================================================================
# STEP 5 — Deploy or reuse a test token
# =============================================================================
resolve_token() {
    if [ -n "${TOKEN_ADDR:-}" ]; then
        info "Using TOKEN_ADDR from environment: ${TOKEN_ADDR}"
        return 0
    fi

    if [ -f "${STATE_FILE}" ]; then
        . "${STATE_FILE}"
        if [ -n "${SAVED_TOKEN_ADDR:-}" ]; then
            TOKEN_ADDR="${SAVED_TOKEN_ADDR}"
            info "Reusing token from previous deploy: ${TOKEN_ADDR}"
            return 0
        fi
    fi

    info "Deploying test token (native XLM via Stellar Asset Contract)..."
    token_output=$("${CLI}" lab token wrap \
        --network "${NETWORK_NAME}" \
        --source "${ADMIN_IDENTITY}" \
        --asset "native" \
        2>&1 || true)

    TOKEN_ADDR=$(echo "${token_output}" | grep -o 'C[A-Z0-9]\{55\}' | head -1)

    if [ -z "${TOKEN_ADDR}" ]; then
        err "Failed to obtain test token contract address."
        err ""
        err "Diagnosis:"
        err "  1. Is the local network running and reachable?"
        err "  2. Does the admin identity have XLM balance?"
        err "  3. Was the token already wrapped? Set TOKEN_ADDR=<addr> to reuse."
        err ""
        err "  ${CLI} lab token wrap --network ${NETWORK_NAME} --source ${ADMIN_IDENTITY} --asset native --verbose"
        exit 2
    fi
    ok "Test token deployed at: ${TOKEN_ADDR}"
}

# =============================================================================
# STEP 6 — Deploy the subscription-vault contract
# =============================================================================
deploy_vault() {
    info "Deploying ${CONTRACT_NAME} contract..."

    # Install WASM to network
    info "Installing WASM..."
    WASM_HASH=$("${CLI}" contract install \
        --network "${NETWORK_NAME}" \
        --source "${ADMIN_IDENTITY}" \
        --wasm "${WASM_PATH}" \
        2>&1 || true)

    if [ -z "${WASM_HASH}" ]; then
        err "WASM install failed. Check network connectivity and admin key."
        exit 2
    fi
    ok "WASM installed: ${WASM_HASH}"

    # Deploy contract instance
    info "Deploying contract instance..."
    CONTRACT_ID=$("${CLI}" contract deploy \
        --network "${NETWORK_NAME}" \
        --source "${ADMIN_IDENTITY}" \
        --wasm-hash "${WASM_HASH}" \
        2>&1 || true)

    if [ -z "${CONTRACT_ID}" ]; then
        # Fallback: some CLI versions need --wasm directly
        CONTRACT_ID=$("${CLI}" contract deploy \
            --network "${NETWORK_NAME}" \
            --source "${ADMIN_IDENTITY}" \
            --wasm "${WASM_PATH}" \
            2>&1 || true)
    fi

    if [ -z "${CONTRACT_ID}" ]; then
        err "Contract deployment failed."
        exit 2
    fi

    # Persist both token and contract ID atomically
    printf "SAVED_TOKEN_ADDR='%s'\nSAVED_CONTRACT_ID='%s'\n" \
      "${TOKEN_ADDR}" "${CONTRACT_ID}" > "${STATE_FILE}" 2>/dev/null || true

    ok "Contract deployed at: ${CONTRACT_ID}"
}

# =============================================================================
# STEP 7 — Initialize the contract
# =============================================================================
init_contract() {
    info "Initializing contract..."
    info "  token=${TOKEN_ADDR}"
    info "  admin=${ADMIN_ADDR}"
    info "  min_topup=10000000"
    info "  grace_period=86400 (1 day)"

    INIT_RESULT=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --source "${ADMIN_IDENTITY}" \
        --id "${CONTRACT_ID}" \
        -- \
        init \
        --token "${TOKEN_ADDR}" \
        --token_decimals "${TOKEN_DECIMALS}" \
        --admin "${ADMIN_ADDR}" \
        --min_topup 10000000 \
        --grace_period 86400 \
        2>&1) || {
        if echo "${INIT_RESULT}" | grep -qi "already\|AlreadyInitialized\|was already" 2>/dev/null; then
            warn "Contract already initialized (re-run). Skipping."
            return 0
        fi
        err "Init failed: ${INIT_RESULT}"
        exit 2
    }

    ok "Contract initialized successfully."
}

# =============================================================================
# STEP 8 — Verify deployment
# =============================================================================
verify_deployment() {
    step "Verifying deployment..."

    ADMIN_RESPONSE=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --source "${ADMIN_IDENTITY}" \
        --id "${CONTRACT_ID}" \
        -- \
        get_admin \
        2>&1) || { warn "get_admin failed."; return; }

    ADMIN_GOT=$(echo "${ADMIN_RESPONSE}" | grep -o 'G[A-Z0-9]\{55\}' | head -1)
    if [ -n "${ADMIN_GOT}" ]; then
        if [ "${ADMIN_GOT}" = "${ADMIN_ADDR}" ]; then
            ok "Admin verified: ${ADMIN_GOT}"
        else
            warn "Admin mismatch: expected ${ADMIN_ADDR}, got ${ADMIN_GOT}"
        fi
    else
        info "Admin response: ${ADMIN_RESPONSE}"
    fi

    VERSION_RESPONSE=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --source "${ADMIN_IDENTITY}" \
        --id "${CONTRACT_ID}" \
        -- \
        version \
        2>&1) || true

    if echo "${VERSION_RESPONSE}" | grep -q '^[0-9]' 2>/dev/null; then
        ok "Contract version: ${VERSION_RESPONSE}"
    fi
}

# =============================================================================
# STEP 9 — Full subscription lifecycle smoke test
# =============================================================================

# Merchant's on-chain (wallet) token balance, used to prove that
# withdraw_merchant_funds actually moved funds to the merchant.
merchant_token_balance() {
    "${CLI}" lab token balance \
        --network "${NETWORK_NAME}" \
        --asset "native" \
        --address "${MERCHANT_ADDR}" 2>/dev/null | tr -dc '0-9' | head -c 40
}

# Merchant's withdrawable balance held inside the vault (not yet withdrawn).
merchant_vault_balance() {
    out=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --id "${CONTRACT_ID}" \
        -- \
        get_merchant_balance \
        --merchant "${MERCHANT_ADDR}" \
        2>&1 || true)
    # The CLI echoes the i128 result on its own line; take the last
    # standalone integer so echoed argument values are not mistaken for it.
    echo "${out}" | grep -oE '^[0-9]+$' | tail -1
}

smoke_test() {
    step "Smoke test: full subscription lifecycle..."

    # 9a. Fund subscriber with native tokens (best-effort)
    info "Minting test tokens to subscriber..."
    if "${CLI}" lab token mint \
        --network "${NETWORK_NAME}" \
        --source "${ADMIN_IDENTITY}" \
        --asset "native" \
        --amount "100000000000" \
        --to "${SUBSCRIBER_ADDR}" 2>/dev/null; then
        ok "Tokens minted to subscriber."
    else
        warn "Token mint not available on standalone. Friendbot may have funded XLM."
    fi

    # 9b. Check subscriber balance
    SUB_BALANCE=$("${CLI}" lab token balance \
        --network "${NETWORK_NAME}" \
        --asset "native" \
        --address "${SUBSCRIBER_ADDR}" 2>/dev/null || echo "unknown")
    info "Subscriber balance: ${SUB_BALANCE}"

    # 9c. Create subscription
    info "Creating subscription..."
    info "  subscriber=${SUBSCRIBER_ADDR}"
    info "  merchant=${MERCHANT_ADDR}"
    info "  amount=1000000 (0.01 token)"
    info "  interval=86400s (1 day)"

    SUB_CREATE_RESULT=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --source "${SUBSCRIBER_IDENTITY}" \
        --id "${CONTRACT_ID}" \
        -- \
        create_subscription \
        --subscriber "${SUBSCRIBER_ADDR}" \
        --merchant "${MERCHANT_ADDR}" \
        --amount 1000000 \
        --interval_seconds 86400 \
        --usage_enabled false \
        --lifetime_cap 100000000 \
        --expires_at 9999999999 \
        2>&1 || true)

    SUB_ID=$(echo "${SUB_CREATE_RESULT}" | grep -oE '[0-9]+' | head -1)
    if [ -z "${SUB_ID}" ]; then
        warn "Subscription creation failed or returned unexpected: ${SUB_CREATE_RESULT}"
        warn "Skipping remaining smoke test steps."
        return
    fi
    ok "Subscription created with ID: ${SUB_ID}"

    # 9d. Deposit funds
    info "Depositing 50000000 tokens to subscription ${SUB_ID}..."
    DEPOSIT_RESULT=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --source "${SUBSCRIBER_IDENTITY}" \
        --id "${CONTRACT_ID}" \
        -- \
        deposit_funds \
        --subscription_id "${SUB_ID}" \
        --subscriber "${SUBSCRIBER_ADDR}" \
        --amount 50000000 \
        2>&1 || true)

    if echo "${DEPOSIT_RESULT}" | grep -qi "error"; then
        warn "Deposit failed: ${DEPOSIT_RESULT}"
    else
        ok "Deposit succeeded."
    fi

    # 9e. Query subscription state
    QUERY_RESULT=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --id "${CONTRACT_ID}" \
        -- \
        get_subscription \
        --subscription_id "${SUB_ID}" \
        2>&1 || true)

    if echo "${QUERY_RESULT}" | grep -qi "error"; then
        warn "Query failed: ${QUERY_RESULT}"
    else
        ok "Subscription query succeeded."
    fi

    # 9f. Charge the subscription (admin-only)
    info "Charging subscription ${SUB_ID}..."
    CHARGE_RESULT=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --source "${ADMIN_IDENTITY}" \
        --id "${CONTRACT_ID}" \
        -- \
        charge_subscription \
        --subscription_id "${SUB_ID}" \
        2>&1 || true)

    if echo "${CHARGE_RESULT}" | grep -qi "error"; then
        warn "Charge failed: ${CHARGE_RESULT}"
        warn "This may be expected: interval may not have elapsed yet, or insufficient balance."
    else
        ok "Charge succeeded."
    fi

    # 9g. Earn merchant balance, then withdraw it.
    #
    # The interval charge above is expected to fail on a fresh subscription
    # (interval has not elapsed), so it cannot be relied on to produce merchant
    # earnings. `charge_one_off` is merchant-authorised and debits prepaid
    # balance immediately, so it gives the withdrawal path real earnings to
    # move — which is what this step is here to regression-test.
    info "Creating merchant earnings via charge_one_off (100000)..."
    ONE_OFF_RESULT=$("${CLI}" contract invoke \
        --network "${NETWORK_NAME}" \
        --source "${MERCHANT_IDENTITY}" \
        --id "${CONTRACT_ID}" \
        -- \
        charge_one_off \
        --subscription_id "${SUB_ID}" \
        --merchant "${MERCHANT_ADDR}" \
        --amount 100000 \
        2>&1 || true)

    if echo "${ONE_OFF_RESULT}" | grep -qi "error"; then
        warn "charge_one_off failed: ${ONE_OFF_RESULT}"
        warn "Cannot verify withdrawal without earnings. Skipping 9h/9i."
    else
        ok "charge_one_off succeeded."

        # 9h. Withdraw merchant funds
        MERCHANT_BAL_BEFORE=$(merchant_vault_balance)
        MERCHANT_TOKEN_BEFORE=$(merchant_token_balance)
        info "Merchant vault balance before:  ${MERCHANT_BAL_BEFORE:-unknown}"
        info "Merchant token balance before: ${MERCHANT_TOKEN_BEFORE:-unknown}"

        WITHDRAW_AMOUNT="${MERCHANT_BAL_BEFORE:-0}"
        if [ "${WITHDRAW_AMOUNT}" = "0" ]; then
            warn "No withdrawable merchant balance. Skipping withdrawal check."
        else
            info "Withdrawing ${WITHDRAW_AMOUNT} to merchant..."
            WITHDRAW_RESULT=$("${CLI}" contract invoke \
                --network "${NETWORK_NAME}" \
                --source "${MERCHANT_IDENTITY}" \
                --id "${CONTRACT_ID}" \
                -- \
                withdraw_merchant_funds \
                --merchant "${MERCHANT_ADDR}" \
                --amount "${WITHDRAW_AMOUNT}" \
                2>&1 || true)

            if echo "${WITHDRAW_RESULT}" | grep -qi "error"; then
                err "withdraw_merchant_funds FAILED: ${WITHDRAW_RESULT}"
                err "A regression in the merchant withdrawal path would not be"
                err "caught by the rest of this smoke test — treat as a failure."
            else
                ok "withdraw_merchant_funds succeeded."

                # 9i. Assert the merchant's token balance actually increased
                MERCHANT_BAL_AFTER=$(merchant_vault_balance)
                MERCHANT_TOKEN_AFTER=$(merchant_token_balance)
                info "Merchant vault balance after:   ${MERCHANT_BAL_AFTER:-unknown}"
                info "Merchant token balance after:  ${MERCHANT_TOKEN_AFTER:-unknown}"

                if [ -z "${MERCHANT_TOKEN_BEFORE}" ] || [ -z "${MERCHANT_TOKEN_AFTER}" ]; then
                    warn "Could not read merchant token balances; withdrawal"
                    warn "invoked successfully but the balance delta is unverified."
                elif [ "${MERCHANT_TOKEN_AFTER}" -gt "${MERCHANT_TOKEN_BEFORE}" ] 2>/dev/null; then
                    ok "Merchant token balance increased: ${MERCHANT_TOKEN_BEFORE} -> ${MERCHANT_TOKEN_AFTER}"
                else
                    err "Merchant token balance did NOT increase: ${MERCHANT_TOKEN_BEFORE} -> ${MERCHANT_TOKEN_AFTER}"
                    err "withdraw_merchant_funds returned success but moved no funds."
                fi

                if [ "${MERCHANT_BAL_AFTER}" = "0" ] 2>/dev/null; then
                    ok "Vault merchant balance fully drained."
                elif [ -n "${MERCHANT_BAL_AFTER}" ] && [ "${MERCHANT_BAL_AFTER}" -lt "${WITHDRAW_AMOUNT}" ] 2>/dev/null; then
                    warn "Vault merchant balance is lower than before (${MERCHANT_BAL_AFTER} < ${WITHDRAW_AMOUNT})."
                fi
            fi
        fi
    fi

    ok "Smoke test complete."
}

# =============================================================================
# SUMMARY
# =============================================================================
print_summary() {
    cat <<EOF

${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}
${BOLD}          Deployment Summary${NC}
${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}

  Contract ID:  ${BOLD}${CONTRACT_ID}${NC}
  Token:        ${TOKEN_ADDR}
  Admin:        ${ADMIN_ADDR}
  Subscriber:   ${SUBSCRIBER_ADDR}
  Merchant:     ${MERCHANT_ADDR}
  Network:      ${NETWORK_NAME}
  RPC URL:      ${RPC_URL}
  Image:        ${QUICKSTART_IMAGE}
  Protocol:     ${PROTOCOL_VERSION_REPORTED:-unknown} (expected ${EXPECTED_PROTOCOL_VERSION:-derived from soroban-sdk})

  State saved:  ${STATE_FILE}

  Quick reference:
    ${CLI_BASENAME} contract invoke \\
      --network ${NETWORK_NAME} \\
      --source ${ADMIN_IDENTITY} \\
      --id ${CONTRACT_ID} \\
      -- \\
      version

    ${CLI_BASENAME} contract invoke \\
      --network ${NETWORK_NAME} \\
      --id ${CONTRACT_ID} \\
      -- \\
      get_admin

  To clean up:
    docker stop ${NETWORK_CONTAINER} && docker rm ${NETWORK_CONTAINER}
    ${CLI_BASENAME} keys rm ${ADMIN_IDENTITY}
    ${CLI_BASENAME} keys rm ${SUBSCRIBER_IDENTITY}
    ${CLI_BASENAME} keys rm ${MERCHANT_IDENTITY}
    ${CLI_BASENAME} network rm ${NETWORK_NAME}
    rm -f ${STATE_FILE}

${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}
EOF
}

# =============================================================================
# MAIN
# =============================================================================
main() {
    # Parse arguments
    for arg in "$@"; do
        case "${arg}" in
            --help|-h) usage ;;
            --no-docker) NO_DOCKER=true ;;
            --skip-smoke) SKIP_SMOKE=true ;;
            --allow-protocol-mismatch) ALLOW_PROTOCOL_MISMATCH=true ;;
            --allow-identity-reuse) ALLOW_IDENTITY_REUSE=true ;;
            --*) warn "Unknown option: ${arg}" ;;
        esac
    done

    # ── Banner ──
    cat <<'EOF'
╔══════════════════════════════════════════════════════════════╗
║  Stellabill — Local Soroban Deploy Script                    ║
║  subscription_vault contract                                 ║
╚══════════════════════════════════════════════════════════════╝
EOF

    # ── Step 0: Dependency checks ──
    step "0/9 — Checking dependencies"
    detect_cli
    check_rust
    check_curl
    if [ "${NO_DOCKER}" = "false" ]; then
        check_docker
        check_docker_daemon
    else
        info "Skipping Docker check (--no-docker)."
    fi

    # ── Step 1: Build ──
    step "1/9 — Building contract WASM"
    build_contract

    # ── Step 2: Network ──
    step "2/9 — Starting local network"
    ensure_network

    # ── Step 3: CLI network config ──
    step "3/9 — Configuring CLI network"
    configure_network

    # ── Step 4: Identities ──
    step "4/9 — Creating identities"
    ADMIN_ADDR="$(ensure_identity "${ADMIN_IDENTITY}")"
    SUBSCRIBER_ADDR="$(ensure_identity "${SUBSCRIBER_IDENTITY}")"
    MERCHANT_ADDR="$(ensure_identity "${MERCHANT_IDENTITY}")"
    ok "Admin:      ${ADMIN_ADDR}"
    ok "Subscriber: ${SUBSCRIBER_ADDR}"
    ok "Merchant:   ${MERCHANT_ADDR}"

    # ── Step 5: Fund ──
    step "5/9 — Funding identities"
    fund_identity "${ADMIN_ADDR}"
    fund_identity "${SUBSCRIBER_ADDR}"
    fund_identity "${MERCHANT_ADDR}"

    # ── Step 6: Token ──
    step "6/9 — Resolving test token"
    resolve_token

    # ── Step 7: Deploy contract ──
    step "7/9 — Deploying subscription-vault contract"
    deploy_vault

    # ── Step 8: Init + verify ──
    step "8/9 — Initializing and verifying contract"
    init_contract
    verify_deployment

    # ── Step 9: Smoke test ──
    if [ "${SKIP_SMOKE}" = "true" ]; then
        step "9/9 — Smoke test skipped (--skip-smoke)"
    else
        step "9/9 — Running full subscription lifecycle smoke test"
        smoke_test
    fi

    # ── Done ──
    print_summary
    ok "All done."
}

main "$@"
