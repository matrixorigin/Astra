"""Project the admin loader's model-check error into a non-secret CI diagnostic."""

import json
from pathlib import Path
import re
import sys


CODES = {
    "model_probe_timeout",
    "model_probe_network",
    "model_probe_rate_limited",
    "model_probe_credentials_or_access",
    "model_probe_quota_exhausted",
    "model_probe_request_rejected",
    "model_probe_configuration",
    "model_probe_response",
    "model_probe_upstream",
}
RETRYABLE = {"model_probe_timeout", "model_probe_network", "model_probe_rate_limited"}


def classify(text, model):
    prefix = f"  model check failed for {model}: HTTP "
    for line in text.splitlines():
        if not line.startswith(prefix):
            continue
        match = re.fullmatch(r"(\d{3}): (\{.*\})", line[len(prefix):])
        if not match:
            continue
        try:
            body = json.loads(match.group(2))
        except ValueError:
            continue
        code = body.get("error_code") if isinstance(body, dict) else None
        if code in CODES:
            return code
    return "unknown"


if __name__ == "__main__":
    code = classify(Path(sys.argv[1]).read_text(errors="replace"), sys.argv[2])
    print(code)
    raise SystemExit(0 if code in RETRYABLE else 1)
