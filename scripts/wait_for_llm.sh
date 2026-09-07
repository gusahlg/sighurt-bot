#!/usr/bin/env bash
# Keep Discord startup behind the *configured* model's readiness signal.
#
# If endpoint_url is localhost, bounce the 1.1B tensor-ash unit first (the
# classic server-mode stack). If the bot is pointed at the desktop 16B, do
# NOT restart sighurt-llm — that used to waste the GTX 1650 and made every
# bot restart wait on a brain Discord isn't even talking to.
set -euo pipefail

CONFIG="${SIGHURT_BOT_CONFIG:-$HOME/discord-bot/config.toml}"
endpoint="$(grep -E '^endpoint_url[[:space:]]*=' "$CONFIG" 2>/dev/null | head -1 | cut -d'"' -f2 || true)"
endpoint="${endpoint%/}"

if [ -n "${SIGHURT_HEALTH_URL:-}" ]; then
    HEALTH_URL=$SIGHURT_HEALTH_URL
elif [ -n "$endpoint" ]; then
    HEALTH_URL="$endpoint/healthz"
else
    HEALTH_URL=http://127.0.0.1:8088/healthz
fi

case "$HEALTH_URL" in
    *127.0.0.1*|*localhost*)
        systemctl --user restart sighurt-llm.service 2>/dev/null || true
        ;;
esac

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
