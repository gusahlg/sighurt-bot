#!/usr/bin/env bash
# Keep Discord startup behind the local model's real readiness signal.
set -euo pipefail

HEALTH_URL=${SIGHURT_HEALTH_URL:-http://127.0.0.1:8088/healthz}
WAIT_SECONDS=${SIGHURT_LLM_WAIT_SECONDS:-180}
case "$WAIT_SECONDS" in
    '' | *[!0-9]*)
        echo "invalid SIGHURT_LLM_WAIT_SECONDS=$WAIT_SECONDS" >&2
        exit 2
        ;;
esac

deadline=$((SECONDS + WAIT_SECONDS))
while ((SECONDS < deadline)); do
    if curl --fail --silent --max-time 2 "$HEALTH_URL" >/dev/null; then
        echo "wait_for_llm: ready at $HEALTH_URL"
        exit 0
    fi
    sleep 1
done

echo "wait_for_llm: no healthy model at $HEALTH_URL after ${WAIT_SECONDS}s" >&2
exit 1
