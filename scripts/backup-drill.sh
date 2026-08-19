#!/usr/bin/env bash
# Backup + restore drill (docs/20 M20.4, docs/22 §backups).
#
# "A backup you haven't restored is a hope, not a backup." This script is the drill: it dumps a
# database, restores it into a *different* one, and then checks the restored copy against the
# original on the numbers that matter — the ledger's balance, the entry count, and the account
# count. A drill that only checks that pg_restore exited zero proves that pg_restore exited zero.
#
#   scripts/backup-drill.sh                       # against the dev stack (just dev)
#   SOURCE_URL=... RESTORE_URL=... scripts/backup-drill.sh
#
# It refuses to run if the restore target holds data it did not create, because the whole point is
# to practise a restore, not to perform an accidental one.
set -euo pipefail

# The client tools have to be at least the server's major version, and a laptop's Homebrew
# postgres is routinely older than the container's. So the default is to run them *in* a container
# on the compose network — which is also how they would run in the environment this drill is
# rehearsing for. Set PANDAY_DRILL_LOCAL=1 to use the binaries on PATH instead.
DRILL_IMAGE="${DRILL_IMAGE:-postgres:17-alpine}"
DRILL_NETWORK="${DRILL_NETWORK:-panday-dev_default}"
DUMP_DIR="${DUMP_DIR:-$(mktemp -d -t panday-backup-XXXXXX)}"
DUMP_NAME="drill.dump"

if [ "${PANDAY_DRILL_LOCAL:-0}" = "1" ]; then
  pg() { "$@"; }
  SOURCE_URL="${SOURCE_URL:-postgres://panday:panday@127.0.0.1:5442/panday}"
else
  pg() { docker run --rm --network "$DRILL_NETWORK" -v "$DUMP_DIR:/dump" "$DRILL_IMAGE" "$@"; }
  # Inside the network, the database is reachable by service name on its own port.
  SOURCE_URL="${SOURCE_URL:-postgres://panday:panday@postgres:5432/panday}"
fi
RESTORE_DB="${RESTORE_DB:-panday_restore_drill}"
HOST_URL="${SOURCE_URL%/*}"
RESTORE_URL="${RESTORE_URL:-$HOST_URL/$RESTORE_DB}"

say() { printf '\n== %s\n' "$1"; }

say "1. dump"
# Custom format: it restores in parallel and, more importantly, restores *selectively* — which is
# what you want at 3am when one table is wrong and the rest of the database is fine.
pg pg_dump --format=custom --file="/dump/$DUMP_NAME" "$SOURCE_URL"
ls -lh "$DUMP_DIR/$DUMP_NAME" | awk '{print "   dump is " $5}'

say "2. record what the source says"
read -r src_accounts src_entries src_balance <<EOF
$(pg psql -tA -F' ' "$SOURCE_URL" -c "
  SELECT (SELECT count(*) FROM accounts),
         (SELECT count(*) FROM ledger_entries),
         (SELECT coalesce(sum(amount_micros),0) FROM ledger_entries)")
EOF
echo "   accounts=$src_accounts entries=$src_entries balance=$src_balance"

say "3. restore into $RESTORE_DB"
# Refuse to clobber. A drill that can destroy the thing it is rehearsing for is not a drill.
existing="$(pg psql -tA "$HOST_URL/postgres" -c \
  "SELECT count(*) FROM pg_database WHERE datname = '$RESTORE_DB'")"
if [ "$existing" != "0" ]; then
  tables="$(pg psql -tA "$RESTORE_URL" -c \
    "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'" 2>/dev/null || echo 0)"
  if [ "$tables" != "0" ] && [ "${FORCE:-0}" != "1" ]; then
    echo "   $RESTORE_DB already holds $tables tables — set FORCE=1 to drop and recreate" >&2
    exit 1
  fi
  pg psql -q "$HOST_URL/postgres" -c "DROP DATABASE $RESTORE_DB"
fi
pg psql -q "$HOST_URL/postgres" -c "CREATE DATABASE $RESTORE_DB"
pg pg_restore --dbname="$RESTORE_URL" --no-owner --no-privileges "/dump/$DUMP_NAME"

say "4. check the restored copy against the source"
read -r dst_accounts dst_entries dst_balance <<EOF
$(pg psql -tA -F' ' "$RESTORE_URL" -c "
  SELECT (SELECT count(*) FROM accounts),
         (SELECT count(*) FROM ledger_entries),
         (SELECT coalesce(sum(amount_micros),0) FROM ledger_entries)")
EOF
echo "   accounts=$dst_accounts entries=$dst_entries balance=$dst_balance"

fail=0
[ "$src_accounts" = "$dst_accounts" ] || { echo "   MISMATCH accounts" >&2; fail=1; }
[ "$src_entries"  = "$dst_entries"  ] || { echo "   MISMATCH ledger entries" >&2; fail=1; }
[ "$src_balance"  = "$dst_balance"  ] || { echo "   MISMATCH balance" >&2; fail=1; }

say "5. check the restored ledger is internally consistent"
# The balances summary table is derived; a restore that brought the entries and not the summary
# would look fine until the first query that trusted the summary.
drift="$(pg psql -tA "$RESTORE_URL" -c "
  SELECT count(*) FROM balances b
  WHERE b.balance_micros <> (SELECT coalesce(sum(amount_micros),0)
                             FROM ledger_entries l WHERE l.account_id = b.account_id)")"
if [ "$drift" != "0" ]; then
  echo "   $drift account(s) whose restored balance disagrees with their entries" >&2
  fail=1
else
  echo "   balances agree with entries"
fi

rm -rf "$DUMP_DIR"
if [ "$fail" != "0" ]; then
  echo
  echo "DRILL FAILED — the backup does not restore to the same numbers." >&2
  exit 1
fi

printf '\nDRILL PASSED — restored %s accounts, %s entries, balance %s\n' \
  "$dst_accounts" "$dst_entries" "$dst_balance"
