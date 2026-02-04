#!/bin/bash
# MkDocs documentation server management script

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
PID_FILE="$PROJECT_DIR/.mkdocs.pid"
LOG_FILE="$PROJECT_DIR/.mkdocs.log"
PORT="${MKDOCS_PORT:-8000}"

start() {
    if [ -f "$PID_FILE" ]; then
        pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            echo "Server already running (PID: $pid)"
            echo "URL: http://localhost:$PORT/heliosdb-lite/"
            return 1
        fi
        rm -f "$PID_FILE"
    fi

    # Check if port is in use
    if ss -tuln | grep -q ":$PORT "; then
        echo "Error: Port $PORT is already in use"
        return 1
    fi

    cd "$PROJECT_DIR" || exit 1

    echo "Starting MkDocs server on port $PORT..."
    nohup python3 -m mkdocs serve --dev-addr=0.0.0.0:$PORT > "$LOG_FILE" 2>&1 &
    pid=$!
    echo $pid > "$PID_FILE"

    # Wait for server to start (build takes ~50 seconds)
    echo "Building documentation (this may take up to 60 seconds)..."
    for i in {1..60}; do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "Error: Server process died. Check $LOG_FILE"
            rm -f "$PID_FILE"
            return 1
        fi
        if ss -tuln | grep -q ":$PORT "; then
            echo "Server started (PID: $pid)"
            echo "URL: http://localhost:$PORT/heliosdb-lite/"
            echo "Log: $LOG_FILE"
            return 0
        fi
        sleep 1
    done

    echo "Error: Server failed to start within 60s. Check $LOG_FILE"
    echo "Process may still be building. Check with: $0 status"
    return 1
}

stop() {
    if [ ! -f "$PID_FILE" ]; then
        echo "No PID file found. Server may not be running."
        # Try to find and kill anyway
        pkill -f "mkdocs serve" 2>/dev/null && echo "Killed mkdocs process"
        return 0
    fi

    pid=$(cat "$PID_FILE")
    if kill -0 "$pid" 2>/dev/null; then
        echo "Stopping server (PID: $pid)..."
        kill "$pid"
        sleep 1
        if kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid"
        fi
        echo "Server stopped"
    else
        echo "Server not running (stale PID file)"
    fi
    rm -f "$PID_FILE"
}

status() {
    if [ -f "$PID_FILE" ]; then
        pid=$(cat "$PID_FILE")
        if kill -0 "$pid" 2>/dev/null; then
            echo "Server running (PID: $pid)"
            echo "URL: http://localhost:$PORT/heliosdb-lite/"
            echo "Log: $LOG_FILE"
            echo ""
            echo "Recent log:"
            tail -5 "$LOG_FILE" 2>/dev/null
            return 0
        fi
        echo "Server not running (stale PID file)"
        rm -f "$PID_FILE"
        return 1
    fi
    echo "Server not running"
    return 1
}

logs() {
    if [ -f "$LOG_FILE" ]; then
        tail -${1:-50} "$LOG_FILE"
    else
        echo "No log file found"
    fi
}

case "$1" in
    start)
        start
        ;;
    stop)
        stop
        ;;
    restart)
        stop
        sleep 1
        start
        ;;
    status)
        status
        ;;
    logs)
        logs "$2"
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|status|logs [lines]}"
        echo ""
        echo "Commands:"
        echo "  start   - Start the MkDocs server in background"
        echo "  stop    - Stop the running server"
        echo "  restart - Restart the server"
        echo "  status  - Show server status"
        echo "  logs    - Show recent log output (default: 50 lines)"
        echo ""
        echo "Environment:"
        echo "  MKDOCS_PORT - Server port (default: 8000)"
        exit 1
        ;;
esac
