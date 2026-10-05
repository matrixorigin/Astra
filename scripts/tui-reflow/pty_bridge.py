"""JSON transport for a real Astra process attached to a controlling PTY."""

import base64
import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios


def resize(fd, columns, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))


binary, root = sys.argv[1:]
pid, fd = pty.fork()
if pid == 0:
    resize(0, 100, 30)
    os.chdir(root)
    # Explicit local roots and a fake gateway handoff keep this independent of
    # developer credentials, configuration, model access and an Astra server.
    env = {
        "PATH": os.environ["PATH"],
        "TERM": "xterm-256color",
        "ASTRA_ACCESS_TOKEN": "resize-test-not-a-real-token",
        "ASTRA_API_URL": "http://127.0.0.1:9",
        "ASTRA_LOCAL_STATE_ROOT": root + "/state",
        "ASTRA_CLI_CREDENTIALS_DIR": root + "/credentials",
        "MOI_AUTH_DIR": root + "/auth",
        "MOI_AGENT_CALL": "1",
    }
    # Shell output preceding Astra must survive, including native reflow.
    os.write(1, b"RESIZE_HISTORY_SENTINEL " + b"history " * 20 + b"\r\n")
    os.execve(binary, [binary, "--api-url", env["ASTRA_API_URL"],
                       "--profile", "resize-test", "--model", "resize-model",
                       "--bare", "--no-instructions", "interactive"], env)

pending_commands = bytearray()
running = True
try:
    while running:
        ready, _, _ = select.select([fd, sys.stdin], [], [], 1)
        if fd in ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            print(json.dumps({"data": base64.b64encode(data).decode()}), flush=True)
        if sys.stdin in ready:
            # select observes the fd, not TextIO's hidden read-ahead buffer.
            # Decode every complete command from the same raw read in order.
            data = os.read(sys.stdin.fileno(), 65536)
            if not data:
                break
            pending_commands.extend(data)
            while b"\n" in pending_commands:
                line, _, rest = pending_commands.partition(b"\n")
                pending_commands = bytearray(rest)
                command = json.loads(line)
                if "resize" in command:
                    resize(fd, *command["resize"])
                elif "input" in command:
                    os.write(fd, command["input"].encode())
                elif "stop" in command:
                    running = False
                    break

finally:
    try:
        os.kill(pid, signal.SIGHUP)
    except ProcessLookupError:
        pass
    os.close(fd)
    os.waitpid(pid, 0)
