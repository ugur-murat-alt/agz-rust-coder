"""Small bounded stdio client for the local, provider-free MCP benchmarks."""
import json
import os
from pathlib import Path
import selectors
import subprocess
import time


class McpSession:
    def __init__(self, command, *, cwd, env, stderr, timeout=90):
        self.stderr = Path(stderr)
        self.log = self.stderr.open("wb")
        self.process = subprocess.Popen(command, cwd=cwd, env=env,
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.log, bufsize=0)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.pending = bytearray()
        self.request_id = 0
        self.timeout = timeout

    def __enter__(self):
        try:
            self.request("initialize", {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "local-benchmark", "version": "1"},
            })
            self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
            return self
        except BaseException:
            self.close()
            raise

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, method, params):
        self.request_id += 1
        self.send({"jsonrpc": "2.0", "id": self.request_id,
                   "method": method, "params": params})
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            while b"\n" in self.pending:
                line, _, rest = self.pending.partition(b"\n")
                self.pending[:] = rest
                response = json.loads(line)
                if response.get("id") == self.request_id:
                    if "error" in response:
                        raise RuntimeError(str(response["error"])[:2000])
                    return response["result"], len(line)
            if not self.selector.select(max(0, deadline - time.monotonic())):
                break
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("MCP exited: " + self.stderr.read_text()[-2000:])
            self.pending.extend(chunk)
            if len(self.pending) > 8 * 1024 * 1024:
                raise RuntimeError("MCP response exceeded 8 MiB")
        raise TimeoutError(f"MCP request exceeded {self.timeout} seconds: {method}")

    def call(self, name, arguments):
        return self.request("tools/call", {"name": name, "arguments": arguments})

    def close(self):
        try:
            self.process.stdin.close()
        except BrokenPipeError:
            pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
        self.selector.close()
        self.process.stdout.close()
        self.log.close()

    def __exit__(self, *_):
        self.close()
