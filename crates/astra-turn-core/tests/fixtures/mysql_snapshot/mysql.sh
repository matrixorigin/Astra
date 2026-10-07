#!/bin/sh
# Offline adapter fixture. Record SQL only, never credentials or connection args.
while [ "$#" -gt 0 ]; do
  if [ "$1" = '-e' ]; then shift; sql=$1; break; fi
  shift
done
fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$sql" >> "$fixture_dir/sql.log"
case "$sql" in
  *'SELECT current_account_name() AS name'*)
    printf '+------+\n| name |\n+------+\n| sys  |\n+------+\n'
    exit 0 ;;
  'CREATE SNAPSHOT '*) failure=fail_capture ;;
  'RESTORE ACCOUNT '*) failure=fail_restore ;;
  'DROP SNAPSHOT '*) failure=fail_drop ;;
  *) failure=fail_query ;;
esac
if [ -f "$fixture_dir/$failure" ]; then
  printf 'injected %s\n' "$failure" >&2
  exit 1
fi
printf 'Query OK, 1 row affected\n'
