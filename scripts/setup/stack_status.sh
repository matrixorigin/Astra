# Status parsing and recovery-command rendering for stack-setup.sh.
# The caller provides python_cmd and stack_env and owns prompting.

print_stack_command() {
    local target="$1"
    printf 'make %s STACK_ENV=%q' "$target" "$stack_env"
}

print_stack_inspect_commands() {
    printf 'Inspect: '
    print_stack_command stack-status
    printf '    Logs: '
    print_stack_command stack-logs
    printf '\n'
}

parse_model_catalog_state() {
    local model_json="$1" model_summary
    active_model_count=0
    inactive_model_count=0
    active_model_names=""
    inactive_model_names=""

    model_summary="$("$python_cmd" -c '
import json
import sys

try:
    value = json.loads(sys.argv[1])
except (IndexError, json.JSONDecodeError):
    raise SystemExit(0)
items = value.get("items", []) if isinstance(value, dict) else value
if not isinstance(items, list):
    raise SystemExit(0)
active = []
inactive = []
for item in items:
    if not isinstance(item, dict):
        continue
    name = item.get("name")
    if not isinstance(name, str) or not name.strip():
        continue
    (active if item.get("is_active") is True else inactive).append(name.strip())
separator = chr(31)
print(separator.join((str(len(active)), str(len(inactive)), ", ".join(sorted(set(active))), ", ".join(sorted(set(inactive))))))
' "$model_json" 2>/dev/null || true)"
    if [[ -n "$model_summary" ]]; then
        IFS=$'\037' read -r active_model_count inactive_model_count active_model_names inactive_model_names <<< "$model_summary"
    fi
}
