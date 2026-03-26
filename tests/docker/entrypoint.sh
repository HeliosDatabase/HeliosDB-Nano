#!/bin/bash
set -e

# HeliosDB Nano HA Entrypoint Script
# Configures and starts the database based on role

# Default values
: "${HELIOSDB_DATA_DIR:=/data}"
: "${HELIOSDB_LOG_LEVEL:=info}"
: "${HELIOSDB_ROLE:=primary}"
: "${HELIOSDB_PORT:=5432}"
: "${HELIOSDB_REPL_PORT:=5433}"
: "${HELIOSDB_HTTP_PORT:=8080}"
: "${HELIOSDB_SYNC_MODE:=async}"

# Set RUST_LOG based on HELIOSDB_LOG_LEVEL
export RUST_LOG="${HELIOSDB_LOG_LEVEL}"

echo "Starting HeliosDB Nano node..."
echo "  Role: $HELIOSDB_ROLE"
echo "  Node ID: ${HELIOSDB_NODE_ID:-auto}"
echo "  Data dir: $HELIOSDB_DATA_DIR"
echo "  Sync mode: $HELIOSDB_SYNC_MODE"

# Generate node ID if not provided
if [ -z "$HELIOSDB_NODE_ID" ]; then
    HELIOSDB_NODE_ID=$(cat /proc/sys/kernel/random/uuid)
    echo "  Generated Node ID: $HELIOSDB_NODE_ID"
fi

# Wait for primary to be available (for standbys)
wait_for_primary() {
    local host=$1
    local port=${2:-5433}
    local max_attempts=${3:-30}
    local attempt=1

    echo "Waiting for primary at $host:$port..."

    while [ $attempt -le $max_attempts ]; do
        if nc -z "$host" "$port" 2>/dev/null; then
            echo "Primary is available at $host:$port"
            return 0
        fi
        echo "  Attempt $attempt/$max_attempts - Primary not yet available"
        sleep 2
        attempt=$((attempt + 1))
    done

    echo "ERROR: Primary not available after $max_attempts attempts"
    return 1
}

# Wait for primary if needed (called before build_command for standbys)
prepare_standby() {
    if [ -z "$HELIOSDB_PRIMARY_HOST" ]; then
        echo "ERROR: HELIOSDB_PRIMARY_HOST is required for standby role"
        exit 1
    fi

    PRIMARY_HOST=$(echo "$HELIOSDB_PRIMARY_HOST" | cut -d: -f1)
    PRIMARY_PORT=$(echo "$HELIOSDB_PRIMARY_HOST" | cut -d: -f2)
    PRIMARY_PORT=${PRIMARY_PORT:-5433}
    wait_for_primary "$PRIMARY_HOST" "$PRIMARY_PORT"
}

# Build command based on role
build_command() {
    local cmd="heliosdb-nano start"
    cmd="$cmd --data-dir $HELIOSDB_DATA_DIR"
    cmd="$cmd --listen 0.0.0.0"
    cmd="$cmd --port $HELIOSDB_PORT"
    cmd="$cmd --http-port $HELIOSDB_HTTP_PORT"
    # Note: Use RUST_LOG env var for logging instead of --log-level

    case "$HELIOSDB_ROLE" in
        primary)
            cmd="$cmd --replication-role primary"
            cmd="$cmd --replication-port $HELIOSDB_REPL_PORT"
            if [ -n "$HELIOSDB_STANDBYS" ]; then
                cmd="$cmd --standby-hosts $HELIOSDB_STANDBYS"
            fi
            if [ -n "$HELIOSDB_OBSERVER_HOSTS" ]; then
                cmd="$cmd --observer-hosts $HELIOSDB_OBSERVER_HOSTS"
            fi
            cmd="$cmd --sync-mode $HELIOSDB_SYNC_MODE"
            ;;
        standby)
            cmd="$cmd --replication-role standby"
            cmd="$cmd --primary-host $HELIOSDB_PRIMARY_HOST"
            cmd="$cmd --replication-port $HELIOSDB_REPL_PORT"
            cmd="$cmd --sync-mode $HELIOSDB_SYNC_MODE"
            ;;
        observer)
            cmd="$cmd --replication-role observer"
            cmd="$cmd --replication-port $HELIOSDB_REPL_PORT"
            if [ -n "$HELIOSDB_PRIMARY_HOST" ]; then
                cmd="$cmd --primary-host $HELIOSDB_PRIMARY_HOST"
            fi
            ;;
        proxy)
            # Run the proxy instead of the database
            cmd="heliosdb-proxy"
            cmd="$cmd --listen 0.0.0.0:$HELIOSDB_PORT"
            cmd="$cmd --admin-listen 0.0.0.0:9090"
            if [ -n "$HELIOSDB_PRIMARY_HOST" ]; then
                cmd="$cmd --primary $HELIOSDB_PRIMARY_HOST"
            fi
            if [ -n "$HELIOSDB_STANDBYS" ]; then
                IFS=',' read -ra STANDBY_ARRAY <<< "$HELIOSDB_STANDBYS"
                for standby in "${STANDBY_ARRAY[@]}"; do
                    cmd="$cmd --standby $standby"
                done
            fi
            ;;
        *)
            echo "ERROR: Unknown role: $HELIOSDB_ROLE"
            exit 1
            ;;
    esac

    echo "$cmd"
}

# Handle signals
trap 'echo "Received SIGTERM, shutting down..."; kill -TERM $PID 2>/dev/null' SIGTERM
trap 'echo "Received SIGINT, shutting down..."; kill -INT $PID 2>/dev/null' SIGINT

# Check if custom command provided
if [ "$1" = "heliosdb-nano" ] || [ "$1" = "heliosdb-proxy" ]; then
    # Prepare standby nodes (wait for primary) - must be done before build_command
    if [ "$HELIOSDB_ROLE" = "standby" ]; then
        prepare_standby
    fi

    # Build the command based on environment
    CMD=$(build_command)
    echo "Executing: $CMD"

    # Execute and capture PID
    exec $CMD &
    PID=$!

    # Wait for the process
    wait $PID
    EXIT_CODE=$?
    echo "Process exited with code $EXIT_CODE"
    exit $EXIT_CODE
else
    # Pass through custom command
    exec "$@"
fi
