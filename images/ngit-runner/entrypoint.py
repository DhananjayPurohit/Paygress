#!/usr/bin/env python3
"""ngit-runner entrypoint — one-shot CI pipeline executor.

Reads NGIT_REPO + NGIT_COMMIT from env, clones the repo at that
commit, parses NGIT_PIPELINE_PATH (default `.ngit/ci.yml`), runs each
step in sequence, and exits with overall pass/fail. While running,
serves a JSON status document on NGIT_STATUS_PORT (default 8080) so
the bridge daemon (or a human via host-port tunnel) can poll
progress and read the final result without needing to scrape stdout.

This is intentionally NOT the place where Nostr result events get
published — that lives in the bridge daemon (next step in the
roadmap). Keeping the runner protocol-agnostic at the result layer
means a non-Nostr caller can use the same image just as well.
"""

from __future__ import annotations

import http.server
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError:
    print("ngit-runner: PyYAML missing from image — rebuild", file=sys.stderr)
    sys.exit(2)


WORKSPACE = Path("/workspace")
REPO_DIR = WORKSPACE / "repo"


# ---------- status document (shared state with HTTP server) ----------

_status_lock = threading.Lock()
_status: dict[str, Any] = {
    "state": "starting",  # starting | cloning | running | passed | failed | error
    "started_at": time.time(),
    "finished_at": None,
    "steps": [],          # list of {name, exit_code, started_at, finished_at}
    "current_step": None,
    "error": None,
}


def update_status(**changes: Any) -> None:
    with _status_lock:
        _status.update(changes)


def append_step(step: dict[str, Any]) -> None:
    with _status_lock:
        _status["steps"].append(step)


def status_snapshot() -> dict[str, Any]:
    with _status_lock:
        return json.loads(json.dumps(_status))


class StatusHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802 — stdlib hook name
        if self.path != "/status":
            self.send_response(404)
            self.end_headers()
            return
        body = json.dumps(status_snapshot(), indent=2).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        # Quiet the default access log — pipeline stdout is the
        # signal, status polling is noise.
        return


def start_status_server() -> None:
    port = int(os.environ.get("NGIT_STATUS_PORT", "8080"))
    server = http.server.ThreadingHTTPServer(("0.0.0.0", port), StatusHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    print(f"ngit-runner: status server listening on :{port}/status")


# ---------- pipeline execution ----------


def fail(reason: str, exit_code: int = 1) -> None:
    print(f"ngit-runner: {reason}", file=sys.stderr)
    update_status(state="error", error=reason, finished_at=time.time())
    # Let the status server be polled briefly before exit so the
    # bridge sees the error doc rather than a connection refused.
    time.sleep(2)
    sys.exit(exit_code)


def require_env(name: str) -> str:
    value = os.environ.get(name, "").strip()
    if not value:
        fail(f"required env var {name} is empty")
    return value


def run_and_stream(cmd: list[str], cwd: Path | None = None) -> int:
    """Run a command, stream output to stdout, return exit code."""
    print(f"+ {' '.join(cmd)}")
    proc = subprocess.Popen(
        cmd,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    assert proc.stdout is not None
    for line in proc.stdout:
        sys.stdout.write(line)
        sys.stdout.flush()
    return proc.wait()


def clone_repo(repo: str, commit: str) -> None:
    update_status(state="cloning")
    if REPO_DIR.exists():
        fail(f"workspace dirty: {REPO_DIR} already exists (image misconfigured)")
    rc = run_and_stream(["git", "clone", "--depth", "50", repo, str(REPO_DIR)])
    if rc != 0:
        fail(f"git clone failed (exit {rc})", exit_code=rc)
    rc = run_and_stream(["git", "checkout", commit], cwd=REPO_DIR)
    if rc != 0:
        # Shallow clone may not have the commit — refetch with full history.
        rc = run_and_stream(["git", "fetch", "--unshallow"], cwd=REPO_DIR)
        if rc != 0:
            fail(f"git fetch --unshallow failed (exit {rc})", exit_code=rc)
        rc = run_and_stream(["git", "checkout", commit], cwd=REPO_DIR)
        if rc != 0:
            fail(f"git checkout {commit} failed (exit {rc})", exit_code=rc)


def load_pipeline(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        fail(f"pipeline file {path} not found in repo")
    try:
        doc = yaml.safe_load(path.read_text())
    except yaml.YAMLError as e:
        fail(f"pipeline file is not valid YAML: {e}")
    if not isinstance(doc, dict) or "steps" not in doc:
        fail("pipeline must be a mapping with a top-level `steps:` list")
    steps = doc["steps"]
    if not isinstance(steps, list) or not steps:
        fail("`steps:` must be a non-empty list")
    for i, step in enumerate(steps):
        if not isinstance(step, dict) or "run" not in step:
            fail(f"step {i}: missing required `run:` field")
    return steps


def run_pipeline(steps: list[dict[str, Any]]) -> int:
    update_status(state="running")
    for i, step in enumerate(steps):
        name = step.get("name", f"step-{i}")
        run = step["run"]
        if not isinstance(run, str):
            fail(f"step {name}: `run` must be a shell string")
        started = time.time()
        update_status(current_step=name)
        print(f"\n=== step {i}: {name} ===")
        rc = run_and_stream(["sh", "-c", run], cwd=REPO_DIR)
        finished = time.time()
        append_step({
            "name": name,
            "exit_code": rc,
            "started_at": started,
            "finished_at": finished,
        })
        if rc != 0:
            update_status(
                state="failed",
                current_step=None,
                finished_at=finished,
            )
            return rc
    update_status(state="passed", current_step=None, finished_at=time.time())
    return 0


def main() -> int:
    start_status_server()
    repo = require_env("NGIT_REPO")
    commit = require_env("NGIT_COMMIT")
    pipeline_path = os.environ.get("NGIT_PIPELINE_PATH", ".ngit/ci.yml")

    print(f"ngit-runner: repo={repo} commit={commit} pipeline={pipeline_path}")
    clone_repo(repo, commit)
    steps = load_pipeline(REPO_DIR / pipeline_path)
    rc = run_pipeline(steps)
    # Hold open briefly so the bridge can poll /status one last time
    # before the container exits and the host-port becomes unreachable.
    time.sleep(5)
    return rc


if __name__ == "__main__":
    sys.exit(main())
