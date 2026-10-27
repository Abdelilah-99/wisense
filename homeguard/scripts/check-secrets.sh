#!/usr/bin/env bash
# Fails if a staged/tracked file under homeguard/ looks like it holds a secret:
# a Telegram bot token, a real MAC address, a non-placeholder password, RTSP
# credentials, or a committed .env file. Run before every commit (CI runs it
# too). Placeholders such as change-me, ${VAR} and {FRIGATE_VAR} are allowed.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

status=0
fail() { echo "check-secrets: $1" >&2; status=1; }

# Staged versions if there are any, otherwise what is tracked.
grep_files() { git grep --cached -nIE "$@" -- homeguard ':!homeguard/scripts/check-secrets.sh' || true; }

if git ls-files --cached -- 'homeguard/.env' 'homeguard/**/.env' | grep -q .; then
  fail ".env is tracked; it must stay local"
fi

hits=$(grep_files '[0-9]{8,10}:[A-Za-z0-9_-]{35}')
[ -n "$hits" ] && fail "possible Telegram bot token:"$'\n'"$hits"

# MAC addresses, except well-known non-addresses and documentation examples
# (02:00:00:00:00:xx, and 00:00:5e:00:53:xx reserved by RFC 7042).
hits=$(grep_files '([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}' \
  | grep -viE '(00:){5}00|(ff:){5}ff|aa:bb:cc:dd:ee:ff|02:00:00:00:00:[0-9a-f]{2}|00:00:5e:00:53:[0-9a-f]{2}' || true)
[ -n "$hits" ] && fail "possible real MAC address (use 02:00:00:00:00:xx or the RFC 7042 range 00:00:5e:00:53:xx in examples):"$'\n'"$hits"

# password/token/secret assignments whose value is not a placeholder.
hits=$(grep_files '(password|passwd|token|secret|pin)[A-Z_]*["'"'"']?[[:space:]]*[=:][[:space:]]*["'"'"']?[A-Za-z0-9]' \
  | grep -vE '^[^:]+:[0-9]+:[[:space:]]*(#|//)' \
  | grep -vE ':(ro|rw)$' \
  | grep -vE '\.rs:[0-9]+:.*(password|token|secret|pin)[a-z_]*: *(String|&|Option|Vec|bool|u[0-9]+)' \
  | grep -vE '\.rs:[0-9]+:[[:space:]]*(password|token|secret|pin)[a-z_]*: *[a-z_][a-z0-9_]*(\.[a-z_][a-z0-9_]*)*(\(\))?,?$' \
  | grep -viE 'change-?me|\$\{|\{FRIGATE_|<[a-z_ -]+>|example|placeholder|env::var|std::env|_env|secret_file|test|fn |let |\.rs:[0-9]+: *(//|///)' || true)
[ -n "$hits" ] && fail "possible hard-coded password/token:"$'\n'"$hits"

hits=$(grep_files 'rtsp://[^:@/ ]+:[^@/ ]+@' | grep -vE '\{FRIGATE_[A-Z_]+\}:\{FRIGATE_[A-Z_]+\}|user:pass@|USER:PASSWORD@' || true)
[ -n "$hits" ] && fail "possible RTSP credentials:"$'\n'"$hits"

[ "$status" -eq 0 ] && echo "check-secrets: ok"
exit "$status"
