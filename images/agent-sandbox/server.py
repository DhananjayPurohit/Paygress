#!/usr/bin/env python3
"""
Paygress agent-sandbox exec server.

Tiny HTTP server that lets a client (paygress-cli exec, the MCP
run_command tool, or any HTTP-capable agent) execute shell commands
inside this container and stream back the result.

Auth: HTTP Basic against EXEC_USER / EXEC_PASS env vars set by the
provider at container-start time. The provider knows these because
the consumer's spawn request ships them as ssh_username/ssh_password
(reused as exec credentials so users don't have a second secret to
manage).

Wire format:
    POST /exec   {command: str, timeout_secs?: int, working_dir?: str}
                 -> {stdout, stderr, exit_code, duration_ms, timed_out}
    GET  /health -> 200 OK ({"status": "ok", "workspace": "/workspace"})

Stdlib-only (http.server + base64 + subprocess + json) so it works
inside the nikolaik/python-nodejs base image with zero pip installs.
"""

import base64
import json
import os
import subprocess
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LISTEN_HOST = "0.0.0.0"
LISTEN_PORT = 8080
WORKSPACE = os.environ.get("WORKSPACE", "/workspace")
EXEC_USER = os.environ.get("EXEC_USER", "")
EXEC_PASS = os.environ.get("EXEC_PASS", "")
DEFAULT_TIMEOUT_SECS = 60
MAX_TIMEOUT_SECS = 1800  # 30 min hard cap so a runaway script can't DoS the lease


def expected_authorization() -> str:
    creds = f"{EXEC_USER}:{EXEC_PASS}".encode("utf-8")
    return "Basic " + base64.b64encode(creds).decode("ascii")


class ExecHandler(BaseHTTPRequestHandler):
    # Don't dump every request to stderr; the provider already
    # captures container logs and the auth header would be noisy.
    def log_message(self, format, *args):
        return

    def _json_response(self, status: int, body: dict) -> None:
        payload = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _check_auth(self) -> bool:
        if not EXEC_USER or not EXEC_PASS:
            # Refuse to start serving auth-protected endpoints if creds
            # weren't injected. Surfaces the misconfiguration loudly
            # rather than silently allowing anonymous exec.
            self._json_response(503, {"error": "exec credentials not configured"})
            return False
        got = self.headers.get("Authorization", "")
        if got != expected_authorization():
            self.send_response(401)
            self.send_header("WWW-Authenticate", 'Basic realm="paygress-exec"')
            self.end_headers()
            return False
        return True

    def do_GET(self):
        if self.path == "/health":
            # Health check stays unauthenticated so the provider can
            # liveness-probe the container without juggling creds.
            self._json_response(
                200,
                {"status": "ok", "workspace": WORKSPACE, "service": "paygress-exec"},
            )
            return
        self._json_response(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/exec":
            self._json_response(404, {"error": "not found"})
            return
        if not self._check_auth():
            return

        length = int(self.headers.get("Content-Length", "0"))
        try:
            body = json.loads(self.rfile.read(length).decode("utf-8"))
        except Exception as e:
            self._json_response(400, {"error": f"invalid json body: {e}"})
            return

        command = body.get("command")
        if not isinstance(command, str) or not command.strip():
            self._json_response(400, {"error": "`command` (non-empty string) is required"})
            return
        timeout_secs = int(body.get("timeout_secs") or DEFAULT_TIMEOUT_SECS)
        if timeout_secs <= 0 or timeout_secs > MAX_TIMEOUT_SECS:
            self._json_response(
                400,
                {
                    "error": f"timeout_secs must be in 1..={MAX_TIMEOUT_SECS}",
                },
            )
            return
        working_dir = body.get("working_dir") or WORKSPACE

        # Execute via bash -lc so the user can write idiomatic shell
        # (`cd && python script.py | tee out.log`). A wrapper shell is
        # the standard agent-sandbox shape — the alternative (parsing
        # argv into argc) blocks pipes/redirects and is hostile to
        # the LLM's natural output style.
        start = time.monotonic()
        timed_out = False
        try:
            result = subprocess.run(
                ["bash", "-lc", command],
                cwd=working_dir,
                capture_output=True,
                timeout=timeout_secs,
            )
            stdout = result.stdout.decode("utf-8", errors="replace")
            stderr = result.stderr.decode("utf-8", errors="replace")
            exit_code = result.returncode
        except subprocess.TimeoutExpired as e:
            timed_out = True
            stdout = (e.stdout or b"").decode("utf-8", errors="replace")
            stderr = (e.stderr or b"").decode("utf-8", errors="replace")
            exit_code = -1
        duration_ms = int((time.monotonic() - start) * 1000)

        self._json_response(
            200,
            {
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "duration_ms": duration_ms,
                "timed_out": timed_out,
            },
        )


def main():
    if not EXEC_USER or not EXEC_PASS:
        print(
            "warning: EXEC_USER/EXEC_PASS not set — /exec will return 503 until the provider injects credentials",
            file=sys.stderr,
        )
    os.makedirs(WORKSPACE, exist_ok=True)
    server = ThreadingHTTPServer((LISTEN_HOST, LISTEN_PORT), ExecHandler)
    print(f"paygress-exec listening on {LISTEN_HOST}:{LISTEN_PORT} (workspace: {WORKSPACE})", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
