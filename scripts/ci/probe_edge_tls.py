#!/usr/bin/env python3
"""Exercise a built Edge's TLS initialization and certificate rejection offline.

Run after the joint CLI/Edge build. A self-signed loopback endpoint must receive
a TLS handshake and be rejected without a panic, directly and via CONNECT.
This is not a successful-registration or live-provider acceptance test.
"""

import argparse
import concurrent.futures
import os
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile


def probe(binary: Path, root: Path, proxy: bool) -> None:
    workspace = root / ("proxy-workspace" if proxy else "direct-workspace")
    workspace.mkdir()
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(root / "cert.pem", root / "key.pem")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(15)
        port = listener.getsockname()[1]

        def serve():
            stream, _ = listener.accept()
            with stream:
                stream.settimeout(10)
                if proxy:
                    headers = bytearray()
                    while not headers.endswith(b"\r\n\r\n"):
                        data = stream.recv(1)
                        if not data or len(headers) > 16384:
                            raise AssertionError("missing bounded CONNECT request")
                        headers.extend(data)
                    assert headers.startswith(b"CONNECT edge-probe.invalid:443 ")
                    stream.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                # A real TLS ClientHello distinguishes certificate rejection
                # from a pre-handshake CryptoProvider panic.
                assert stream.recv(1, socket.MSG_PEEK) == b"\x16", "no TLS ClientHello"
                try:
                    with context.wrap_socket(stream, server_side=True):
                        raise AssertionError("Edge accepted an untrusted certificate")
                except ssl.SSLError:
                    return

        env = {key: value for key, value in os.environ.items()
               if key.lower() not in {"https_proxy", "http_proxy", "all_proxy", "no_proxy"}
               and not key.startswith("ASTRA_")}
        env["ASTRA_LOCAL_STATE_ROOT"] = str(root / ("proxy-state" if proxy else "direct-state"))
        if proxy:
            env["HTTPS_PROXY"] = f"http://127.0.0.1:{port}"
        url = "wss://edge-probe.invalid/edge/ws" if proxy else f"wss://127.0.0.1:{port}/edge/ws"
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            server = executor.submit(serve)
            result = subprocess.run(
                [str(binary), "--server-url", url, "--edge-id", "tls-release-probe",
                 "--token", "offline-test-token", "--workspace-dir", str(workspace),
                 "--reconnect", "false"],
                env=env, capture_output=True, text=True, timeout=20,
            )
            server.result(timeout=15)
        output = result.stdout + result.stderr
        assert result.returncode != 0, "untrusted certificate must fail"
        assert "panicked" not in output, output
        assert "certificate" in output.lower(), output
        print(f"Edge TLS {'CONNECT' if proxy else 'direct'}: untrusted certificate rejected")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="edge-tls-probe-") as directory:
        root = Path(directory)
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
             "-keyout", str(root / "key.pem"), "-out", str(root / "cert.pem"),
             "-days", "1", "-subj", "/CN=edge-probe.invalid"],
            check=True, capture_output=True,
        )
        for proxy in (False, True):
            probe(args.binary.resolve(), root, proxy)


if __name__ == "__main__":
    main()
