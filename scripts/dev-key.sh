#!/usr/bin/env bash
# Mint a dev account + API key, and print how to use it (docs/22 M22.1).
#
# Idempotent by intent: it creates a *new* account each time it is run with no PANDAY_DEV_ACCOUNT
# set. Reusing a stale account id from a database that was reset is worse than making a new one —
# the failure would be a 401 with no explanation.
set -euo pipefail

: "${PANDAY_DATABASE_URL:?PANDAY_DATABASE_URL must be set}"

account="${PANDAY_DEV_ACCOUNT:-$(cargo run --quiet -p panday-platform -- account dev)}"
key="$(cargo run --quiet -p panday-platform -- issue-key "$account" laptop models,sessions 2>/dev/null)"

cat <<EOF

  account   $account
  key       $key

  export PANDAY_API_KEY=$key
  curl -s http://127.0.0.1:8088/v1/chat/completions \\
    -H "Authorization: Bearer \$PANDAY_API_KEY" \\
    -H 'Content-Type: application/json' \\
    -d '{"model":"auto","messages":[{"role":"user","content":"hi"}]}'

  The key is shown once. Run \`just dev\` again for another.

EOF
