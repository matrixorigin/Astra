#!/usr/bin/env python3
"""Loopback contract tests for the interactive embedding preflight."""

from __future__ import annotations

import http.server
import json
import pathlib
import subprocess
import tempfile
import threading


REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
PROBE = REPO_ROOT / "scripts" / "setup" / "check_embedding.py"


class EmbeddingHandler(http.server.BaseHTTPRequestHandler):
    mode = "success"

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length))
        if self.path != "/v1/embeddings" or body.get("model") != "test-model":
            self.send_error(404)
            return
        if self.headers.get("Authorization") != "Bearer test-secret":
            self.send_error(401)
            return
        if self.mode == "redirect":
            self.send_response(307)
            self.send_header("Location", "https://example.invalid/v1/embeddings")
            self.end_headers()
            return
        if self.mode == "reject":
            payload = {"error": {"message": "rejected test-secret"}}
            encoded = json.dumps(payload).encode()
            self.send_response(401)
        elif self.mode == "non_finite":
            payload = {"data": [{"embedding": [float("nan")]}]}
            encoded = json.dumps(payload).encode()
            self.send_response(200)
        else:
            payload = {"data": [{"embedding": [0.1, 0.2, 0.3]}]}
            encoded = json.dumps(payload).encode()
            self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def log_message(self, _format: str, *_args: object) -> None:
        return


def write_env(path: pathlib.Path, base_url: str, dimension: int = 3) -> None:
    path.write_text(
        "\n".join(
            (
                "MEMORIA_EMBEDDING_PROVIDER=openai",
                f"MEMORIA_EMBEDDING_BASE_URL={base_url}",
                "MEMORIA_EMBEDDING_MODEL=test-model",
                f"MEMORIA_EMBEDDING_DIM={dimension}",
                "MEMORIA_EMBEDDING_API_KEY=test-secret",
            )
        )
        + "\n",
        encoding="utf-8",
    )


def run_probe(env_file: pathlib.Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["python3", str(PROBE), str(env_file), "--timeout", "2"],
        cwd=REPO_ROOT,
        check=False,
        capture_output=True,
        text=True,
    )


def main() -> int:
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), EmbeddingHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="astra-embedding-preflight-") as temp:
            env_file = pathlib.Path(temp) / "stack.env"
            base_url = f"http://127.0.0.1:{server.server_port}/v1"

            write_env(env_file, base_url)
            result = run_probe(env_file)
            assert result.returncode == 0, result.stderr
            assert "dimensions=3" in result.stdout

            write_env(env_file, base_url, dimension=4)
            result = run_probe(env_file)
            assert result.returncode == 1
            assert "configured 4, endpoint returned 3" in result.stderr

            EmbeddingHandler.mode = "reject"
            write_env(env_file, base_url)
            result = run_probe(env_file)
            assert result.returncode == 1
            assert "[redacted]" in result.stderr
            assert "test-secret" not in result.stderr

            EmbeddingHandler.mode = "redirect"
            result = run_probe(env_file)
            assert result.returncode == 1
            assert "cross-origin redirect" in result.stderr
            assert "test-secret" not in result.stderr

            EmbeddingHandler.mode = "non_finite"
            write_env(env_file, base_url, dimension=1)
            result = run_probe(env_file)
            assert result.returncode == 1
            assert "non-finite" in result.stderr
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    print("embedding preflight contract: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
