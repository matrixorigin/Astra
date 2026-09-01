"""Dependency-free runtime contract for the Astra Harbor adapter."""

import ipaddress
import os
import shlex
from typing import Callable
from urllib.parse import urlsplit


_LOCAL_PROXY_BYPASSES = ("localhost", "127.0.0.1", "::1")
_PROXY_VARIABLES = (
    ("http_proxy", "HTTP_PROXY", "ASTRA_HARBOR_HTTP_PROXY"),
    ("https_proxy", "HTTPS_PROXY", "ASTRA_HARBOR_HTTPS_PROXY"),
    ("all_proxy", "ALL_PROXY", "ASTRA_HARBOR_ALL_PROXY"),
)

# Harbor's standard Terminal-Bench agent timeout is 900 seconds.  Astra must
# finish its own process and descendant cleanup before Harbor cancels the
# outer exec; otherwise verification can race a still-live container command.
# The value is configurable by the benchmark config, but the inner deadline is
# always derived from that one authority rather than having an independent
# 1200-second default.
DEFAULT_HARBOR_AGENT_TIMEOUT_SEC = 900
DEFAULT_CLEANUP_MARGIN_SEC = 60
DEFAULT_KILL_AFTER_SEC = 20
# Leave a small process-level cushion after Astra's own absolute one-shot
# deadline. Astra already reserves its full terminal-settlement budget
# internally (cancellation, root/background drain, and serialization); this
# outer cushion is only for the shell/tee handoff before coreutils `timeout`
# could send its signal.  Keeping a second 40-second terminal reserve here
# used to silently shorten every 900-second official task to 730 seconds of
# useful work (840 - 40 - 70), despite the CLI owning the 70-second terminal
# protocol.  Fifteen seconds still keeps the independently enforced
# `timeout` + kill-after bound strictly below Harbor's official deadline.
DEFAULT_PROCESS_CUSHION_SEC = 15


def _configured_value(
    get_env: Callable[[str], str | None],
    name: str,
) -> str | None:
    """Resolve an exact ``${NAME}`` config placeholder from the host env.

    Harbor passes agent configuration values through verbatim; it does not
    consistently expand shell-style placeholders.  Resolving only an exact,
    allowlisted placeholder keeps the config declarative without forwarding
    unrelated host secrets or putting credentials on the Harbor command line.
    Literal configured values continue to win, which preserves existing
    callers that inject an already-scoped value.
    """
    value = get_env(name)
    if value == f"${{{name}}}":
        return os.environ.get(name)
    return value


def _first_value(
    get_env: Callable[[str], str | None],
    names: tuple[str, ...],
) -> str | None:
    for name in names:
        if value := get_env(name):
            return value
    return None


def _validate_proxy(value: str, name: str) -> str:
    """Validate a container-facing proxy without exposing its value in errors."""
    if value != value.strip() or any(
        char.isspace() or ord(char) < 0x20 for char in value
    ):
        raise ValueError(f"{name} contains whitespace or control characters")

    try:
        parsed = urlsplit(value)
        hostname = parsed.hostname
        port = parsed.port
    except ValueError as error:
        raise ValueError(f"{name} is not a valid proxy URL") from error

    if parsed.scheme.lower() not in {"http", "https", "socks4", "socks5", "socks5h"}:
        raise ValueError(f"{name} must use an http(s) or supported socks scheme")
    if not hostname or parsed.username is not None or parsed.password is not None:
        raise ValueError(f"{name} must be a host-only proxy URL without credentials")
    if parsed.query or parsed.fragment or parsed.path not in ("", "/"):
        raise ValueError(f"{name} must not contain a path, query, or fragment")
    if port is not None and not (1 <= port <= 65535):
        raise ValueError(f"{name} has an invalid port")

    normalized_hostname = hostname.strip("[]").lower()
    if normalized_hostname in _LOCAL_PROXY_BYPASSES:
        raise ValueError(
            f"{name} points at a loopback proxy; use a container-reachable relay"
        )
    try:
        proxy_ip = ipaddress.ip_address(normalized_hostname)
    except ValueError:
        # Hostnames are expected here; only an IP parsing failure is benign.
        proxy_ip = None
    if proxy_ip is not None and proxy_ip.is_loopback:
        raise ValueError(
            f"{name} points at a loopback proxy; use a container-reachable relay"
        )

    return value


def _bypass_items(value: str, name: str) -> list[str]:
    if any(char in value for char in "\r\n"):
        raise ValueError(f"{name} contains a newline")

    items: list[str] = []
    for raw_item in value.split(","):
        item = raw_item.strip()
        if not item:
            continue
        if any(char.isspace() for char in item) or item == "*":
            raise ValueError(f"{name} contains an unsafe bypass entry")
        items.append(item)
    return items


def _proxy_bypasses(
    get_env: Callable[[str], str | None],
    api_hostname: str | None,
) -> str:
    """Merge explicitly opted-in bypasses with the local Astra endpoints."""
    bypasses: list[str] = []
    seen: set[str] = set()

    # Ambient NO_PROXY can contain host-only routes that are unsafe to expose
    # to an untrusted task.  Inherit it only through an explicit harness key.
    configured = _first_value(
        get_env,
        ("ASTRA_HARBOR_NO_PROXY", "ASTRA_HARBOR_no_proxy"),
    )
    if configured:
        for item in _bypass_items(configured, "ASTRA_HARBOR_NO_PROXY"):
            key = item.lower()
            if key not in seen:
                bypasses.append(item)
                seen.add(key)

    for item in (*_LOCAL_PROXY_BYPASSES, api_hostname):
        if not item:
            continue
        key = item.lower()
        if key not in seen:
            bypasses.append(item)
            seen.add(key)

    return ",".join(bypasses)


def astra_runtime_env(get_env: Callable[[str], str | None]) -> dict[str, str]:
    """Build the allowlisted, secret-safe environment for `astra chat`."""
    runtime_env = {}
    # The access token is provisioned by the adapter as a private file during
    # install.  Never put its value in the environment mapping passed to
    # Docker exec: Harbor serializes that mapping as `-e NAME=value`, making a
    # credential visible in host process arguments.  The command builder reads
    # the file inside the task container immediately before starting Astra.
    for name in ("ASTRA_API_URL", "ASTRA_ACCESS_TOKEN_FILE"):
        if value := _configured_value(get_env, name):
            runtime_env[name] = value

    # Network access in task images is often available only through the host's
    # configured proxy.  Forward only the conventional proxy variables, and
    # normalize each pair so tools that prefer either case see the same value;
    # unrelated host environment values (including arbitrary secrets) stay out
    # of the task container.
    for lower_name, upper_name, explicit_name in _PROXY_VARIABLES:
        value = _first_value(get_env, (explicit_name, lower_name, upper_name))
        if value:
            value = _validate_proxy(value, explicit_name)
            runtime_env[lower_name] = value
            runtime_env[upper_name] = value

    api_url = runtime_env.get("ASTRA_API_URL")
    hostname = urlsplit(api_url).hostname if api_url else None
    if api_url:
        # The task container may need both to reach Astra on the Docker host
        # and to exercise services it starts on localhost.  Replacing the
        # bypass list with only the API hostname makes local curl/gRPC/HTTP
        # checks take the external proxy path and look like spurious 503s.
        # Ambient bypasses are intentionally not inherited; callers that need
        # additional entries must provide ASTRA_HARBOR_NO_PROXY explicitly.
        bypass = _proxy_bypasses(get_env, hostname)
        runtime_env["NO_PROXY"] = bypass
        runtime_env["no_proxy"] = bypass
    return runtime_env


def _positive_int(value: str | None, name: str, default: int) -> int:
    if value is None or not value.strip():
        return default
    try:
        parsed = int(value, 10)
    except ValueError as error:
        raise ValueError(f"{name} must be a positive integer") from error
    if parsed <= 0:
        raise ValueError(f"{name} must be a positive integer")
    return parsed


def astra_inner_timeout(
    get_env: Callable[[str], str | None],
    *,
    outer_timeout: int,
    kill_after_sec: int = DEFAULT_KILL_AFTER_SEC,
) -> int:
    """Derive Astra's inner deadline from Harbor's authoritative deadline.

    Harbor does not expose the live Trial object to an installed agent, so the
    adapter config carries the same timeout value used by the Trial.  A
    bounded cleanup margin is mandatory; silently allowing an inner deadline
    beyond Harbor's cancellation boundary recreates the verifier race.
    """
    outer = outer_timeout
    if isinstance(outer, bool) or not isinstance(outer, int) or outer <= 0:
        raise ValueError("official Harbor agent timeout must be a positive integer")
    margin = _positive_int(
        get_env("ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS"),
        "ASTRA_HARBOR_CLEANUP_MARGIN_SECONDS",
        DEFAULT_CLEANUP_MARGIN_SEC,
    )
    if kill_after_sec < 0:
        raise ValueError("kill_after_sec must be non-negative")
    if margin <= kill_after_sec:
        raise ValueError(
            "cleanup margin must exceed the kill-after grace period"
        )
    inner = outer - margin
    if inner <= 0 or inner + kill_after_sec >= outer:
        raise ValueError(
            "Harbor agent timeout must leave room for Astra process cleanup"
        )
    return inner


def astra_chat_command(
    model_name: str,
    instruction: str,
    output_path: str,
    *,
    timeout_sec: int = DEFAULT_HARBOR_AGENT_TIMEOUT_SEC - DEFAULT_CLEANUP_MARGIN_SEC,
    kill_after_sec: int = DEFAULT_KILL_AFTER_SEC,
) -> str:
    """Build a bounded, quiescing task-container command.

    Harbor's outer cancellation is not guaranteed to reap a process that is
    already inside a Docker exec. Keep a small, configurable margin below
    Harbor's task timeout and let coreutils ``timeout`` terminate Astra before
    verification starts. The command remains task/provider neutral; this is a
    process-lifecycle guarantee, not a task policy.
    """
    if timeout_sec <= 0 or kill_after_sec < 0:
        raise ValueError("timeout_sec must be positive and kill_after_sec non-negative")
    internal_deadline_sec = timeout_sec - DEFAULT_PROCESS_CUSHION_SEC
    if internal_deadline_sec <= 0:
        raise ValueError("timeout_sec must leave room for Astra terminal finalization")
    return " ".join(
        [
            # Harbor can be invoked directly, without the Rust harness
            # preflight.  Refuse that run at the container boundary when the
            # server contract or credential was not wired at all.  `astra
            # health` is intentionally the only live probe here: it checks
            # core API/database readiness while allowing the server's normal
            # optional-component degradation semantics.  Do not probe a
            # model or interpret task-specific output before the agent starts.
            "test -n \"${ASTRA_API_URL:-}\" || { echo 'astra harbor preflight: ASTRA_API_URL is missing' >&2; exit 78; }",
            ";",
            "test -r \"${ASTRA_ACCESS_TOKEN_FILE:-}\" || { echo 'astra harbor preflight: Astra access-token file is missing' >&2; exit 78; }",
            ";",
            "ASTRA_ACCESS_TOKEN=\"$(cat \"$ASTRA_ACCESS_TOKEN_FILE\")\"",
            ";",
            "export ASTRA_ACCESS_TOKEN",
            ";",
            "test -n \"${ASTRA_ACCESS_TOKEN:-}\" || { echo 'astra harbor preflight: Astra access-token file is empty' >&2; exit 78; }",
            ";",
            "astra health >/dev/null || { echo 'astra harbor preflight: API is not ready' >&2; exit 78; }",
            ";",
            "timeout",
            "--signal=TERM",
            f"--kill-after={kill_after_sec}s",
            f"{timeout_sec}s",
            "astra chat",
            "--no-resume",
            "--json",
            "--stream-events",
            shlex.quote(f"{output_path}.events"),
            "--max-wall-time-seconds",
            str(internal_deadline_sec),
            "-y",
            # Harbor owns an ephemeral container dedicated to this task. Bypass
            # makes that container the authorized scope while Astra continues
            # to enforce catastrophic hard-denies.
            "--permission-mode",
            "bypass",
            "--model",
            shlex.quote(model_name),
            "--message",
            shlex.quote(instruction),
            "| tee",
            shlex.quote(output_path),
        ]
    )
