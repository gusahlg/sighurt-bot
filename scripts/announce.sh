#!/usr/bin/env bash
# Post an announcement to Discord AS SuperSighurt.
#
# Reads DISCORD_TOKEN from the bot's .env and sends a message to the
# announcement channel via the Discord REST API. Used to tell the community
# about each shipped improvement (commit + short feature summary) so they can
# test it — their feedback is the point.
#
# Usage:
#   scripts/announce.sh "message text"
#   scripts/announce.sh < message.txt          # read body from stdin
#   CHANNEL_ID=<id> scripts/announce.sh "..."   # override target channel
set -euo pipefail
cd "$(dirname "$0")/.."

ENV_FILE="${ENV_FILE:-.env}"
# Sig's main/most-active channel — where the regulars actually hang out.
CHANNEL_ID="${CHANNEL_ID:-1405453469032120340}"

if [ ! -f "$ENV_FILE" ]; then
    echo "announce: $ENV_FILE not found (run from the bot repo, or set ENV_FILE)" >&2
    exit 1
fi
TOKEN="$(grep -E '^DISCORD_TOKEN=' "$ENV_FILE" | head -1 | cut -d= -f2-)"
if [ -z "$TOKEN" ]; then
    echo "announce: DISCORD_TOKEN missing from $ENV_FILE" >&2
    exit 1
fi

if [ "$#" -ge 1 ]; then
    BODY="$1"
else
    BODY="$(cat)"
fi
if [ -z "${BODY//[[:space:]]/}" ]; then
    echo "announce: empty message body" >&2
    exit 1
fi

# Discord hard-limits a message to 2000 chars.
BODY="${BODY:0:1990}"

# Build the JSON payload with whatever's available (jq preferred; python3 or
# perl as fallbacks) so this runs on the bare server PATH too.
if command -v jq >/dev/null 2>&1; then
    payload="$(jq -n --arg c "$BODY" '{content:$c}')"
elif command -v python3 >/dev/null 2>&1; then
    payload="$(BODY="$BODY" python3 -c 'import json,os;print(json.dumps({"content":os.environ["BODY"]}))')"
else
    payload="$(BODY="$BODY" perl -MJSON::PP -e 'print encode_json({content=>$ENV{BODY}})')"
fi

http_code="$(curl -sS -o /tmp/announce-resp.json -w '%{http_code}' \
    -X POST "https://discord.com/api/v10/channels/${CHANNEL_ID}/messages" \
    -H "Authorization: Bot ${TOKEN}" \
    -H "Content-Type: application/json" \
    -d "$payload")"

if [ "$http_code" = "200" ] || [ "$http_code" = "201" ]; then
    echo "announce: posted to channel ${CHANNEL_ID} (HTTP ${http_code})"
else
    echo "announce: FAILED (HTTP ${http_code})" >&2
    cat /tmp/announce-resp.json >&2
    exit 1
fi
