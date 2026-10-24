#!/usr/bin/env bash

set -euo pipefail

# Copyright (C) The Infumap Authors
# SPDX-License-Identifier: MIT
# See LICENSE.txt for the full license text.

readonly ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly RESTART_DELAY_SECS=5
CONFIG_ARGS=()
COMMAND=""
config_selected=0

child_pid=""
shutdown_requested=0

fail() {
    echo "Error: $1" >&2
    exit 1
}

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

resolve_bin() {
    local release_bin="$ROOT_DIR/target/release/imwebsearch"
    command_exists cargo || fail "cargo was not found on PATH. Install Rust to use run.sh."
    echo "Building imwebsearch (release)" >&2
    cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml" || fail "Could not build imwebsearch."
    [ -x "$release_bin" ] || fail "Expected binary at $release_bin after cargo build"
    printf '%s\n' "$release_bin"
}

terminate_child() {
    local pid="${1:-}"
    [ -n "$pid" ] || return 0
    kill -TERM "$pid" 2>/dev/null || true
}

wait_for_child_shutdown() {
    local pid="${1:-}"
    local attempt
    [ -n "$pid" ] || return 0

    for attempt in 1 2 3 4 5; do
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" 2>/dev/null || true
            return 0
        fi
        sleep 1
    done

    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

cleanup() {
    shutdown_requested=1
    terminate_child "$child_pid"
    wait_for_child_shutdown "$child_pid"
}

trap cleanup EXIT
# Exit through one cleanup path even when a signal interrupts wait or the
# restart delay. SIGTERM gives the server time to reap its owned Chrome tree;
# if SIGKILL is needed, the independent browser guardian finishes that cleanup.
trap 'exit 0' INT TERM

while [ "$#" -gt 0 ]; do
    case "$1" in
        --config)
            [ "$config_selected" -eq 0 ] || fail "Specify only one config path."
            [ "$#" -ge 2 ] && [ -n "$2" ] || fail "--config requires a file path."
            CONFIG_ARGS=(--config "$2")
            config_selected=1
            shift 2
            ;;
        --config=*)
            [ "$config_selected" -eq 0 ] || fail "Specify only one config path."
            selected_config="${1#--config=}"
            [ -n "$selected_config" ] || fail "--config requires a file path."
            CONFIG_ARGS=(--config "$selected_config")
            config_selected=1
            shift
            ;;
        --check-config|--setup-browser|--update-browser|--help|-h)
            [ -z "$COMMAND" ] || fail "Choose only one command."
            COMMAND="$1"
            shift
            ;;
        *) fail "Unknown option; use --help for usage." ;;
    esac
done

BIN="$(resolve_bin)"

if [ -n "$COMMAND" ]; then
    "$BIN" ${CONFIG_ARGS[@]+"${CONFIG_ARGS[@]}"} "$COMMAND"
    exit $?
fi

# Invalid configuration stops here, before setup or the restart loop.
"$BIN" ${CONFIG_ARGS[@]+"${CONFIG_ARGS[@]}"} --check-config

# Setup runs once per wrapper invocation, outside the server restart loop.
# Managed selection is persisted by setup; the server resolves that selection
# from the same TOML file without exporting or rewriting configuration.
if selected_browser="$("$BIN" ${CONFIG_ARGS[@]+"${CONFIG_ARGS[@]}"} --setup-browser)"; then
    echo "Browser selected: $selected_browser" >&2
else
    echo "Browser setup unavailable; search will start and affected fetches will report an error." >&2
fi

echo "Starting IMWebSearch MCP server"
echo "Binary: $BIN"
echo "Restart delay: ${RESTART_DELAY_SECS}s"

while true; do
    "$BIN" ${CONFIG_ARGS[@]+"${CONFIG_ARGS[@]}"} &
    child_pid="$!"

    set +e
    wait "$child_pid"
    exit_code=$?
    set -e
    child_pid=""

    if [ "$shutdown_requested" -eq 1 ]; then
        exit 0
    fi

    echo "imwebsearch exited with status ${exit_code}; restarting in ${RESTART_DELAY_SECS}s."
    sleep "$RESTART_DELAY_SECS"
done
