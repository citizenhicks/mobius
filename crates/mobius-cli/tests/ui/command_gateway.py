"""Start an isolated real gateway and one sleeping shell per client. Ctrl-C cleans up.

Usage: python3 command_gateway.py <empty evidence directory> <binary directory> [macos ios cli]
The clients open the session IDs in sessions.json and stop the processes listed in each command.pid.
"""

import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COMMAND = "echo $$ > command.pid; exec /bin/sleep 900"
CLIENTS = ("macos", "ios", "cli")


class Model(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = json.dumps(body.get("messages", []))
        with self.server.lock:
            client = next((c for c in CLIENTS if f"Exercise Stop for {c}" in messages), None)
            first = client and client not in self.server.started and body.get("tools")
            if first:
                self.server.started.add(client)
        delta = {"content": "Ready."}
        if first:
            delta = {"tool_calls": [{"index": 0, "id": "cmd000001", "function": {
                "name": "bash", "arguments": json.dumps({"command": COMMAND, "run_in_background": True})}}]}
        result = json.dumps({"choices": [{"delta": delta}]})
        response = f"data: {result}\n\ndata: [DONE]\n\n".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


def main(root, binaries, clients):
    root.mkdir(mode=0o700, exist_ok=True)
    state = root / "gateway"
    if state.exists():
        raise RuntimeError("use a fresh fixture directory")
    accounts = root / "gateway-accounts.json"
    env = {k: v for k, v in os.environ.items() if not k.startswith("MOBIUS_")}
    env.update(MOBIUS_GATEWAY_STATE_DIR=str(state), MOBIUS_GATEWAY_TOKEN_FILE=str(accounts),
               MOBIUS_GATEWAY_NO_MENU_BAR="1")
    gw = str(binaries / "mobius-gateway")
    subprocess.run([gw, "bootstrap"], env=env, check=True, stdout=subprocess.DEVNULL)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    endpoint = f"tcp://127.0.0.1:{port}"
    config = state / "gateway.toml"
    config.write_text(config.read_text().replace('listen = "127.0.0.1:8741"', f'listen = "127.0.0.1:{port}"'))
    accounts.write_text(accounts.read_text().replace("tcp://127.0.0.1:8741", endpoint))
    env["MOBIUS_GATEWAY_ENDPOINT"] = endpoint
    with ThreadingHTTPServer(("127.0.0.1", 0), Model) as model, (root / "gateway.log").open("w") as log:
        model.lock = threading.Lock()
        model.started = set()
        threading.Thread(target=model.serve_forever, daemon=True).start()
        gateway = subprocess.Popen([gw, "serve"], env=env, cwd=root, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 20
            while True:
                if gateway.poll() is not None:
                    raise RuntimeError("gateway exited, see gateway.log")
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=.2):
                        break
                except OSError:
                    if time.monotonic() > deadline:
                        raise
                    time.sleep(.1)
            subprocess.run([gw, "register-provider", "--provider", "mistral", "--model", "mistral-large-4-0",
                            "--base-url", f"http://127.0.0.1:{model.server_port}/v1", "--credential-stdin"],
                           input=b"loopback-test", env=env, check=True, stdout=subprocess.DEVNULL)
            sessions = {}
            stop_deadline = time.monotonic() + 885
            for client in clients:
                workspace = root / client
                workspace.mkdir()
                prompt = f"Exercise Stop for {client}"
                task = workspace / "task.txt"
                task.write_text(prompt)
                with (workspace / "seed.log").open("w") as seed_log:
                    subprocess.run([str(binaries / "mobius"), "run", "mobius", str(task)], env=env,
                                   cwd=workspace, stdout=seed_log, stderr=seed_log, check=True, timeout=45)
                marker = workspace / "command.pid"
                deadline = time.monotonic() + 10
                while not marker.exists() or not marker.stat().st_size:
                    if time.monotonic() > deadline:
                        raise RuntimeError(f"{client}: command never wrote its PID")
                    time.sleep(.05)
                pid = int(marker.read_text())
                os.kill(pid, 0)
                with sqlite3.connect(state / "checkpoints.sqlite3") as db:
                    [(session,)] = db.execute("SELECT session_id FROM sessions WHERE first_user_message = ?", (prompt,))
                sessions[client] = {"session": session, "pid": pid}
            (root / "sessions.json").write_text(json.dumps({"endpoint": endpoint, **sessions}, indent=2))
            if "ios" in clients:
                pairing = subprocess.check_output([gw, "pairing-code", "--json"], env=env)
                connection = root / "ios-connection.json"
                connection.touch(mode=0o600)
                connection.write_text(json.dumps({"endpoint": endpoint, "code": json.loads(pairing)["code"]}))
            for client, details in sessions.items():
                while True:
                    try:
                        os.kill(details["pid"], 0)
                    except ProcessLookupError:
                        break
                    if time.monotonic() >= stop_deadline:
                        raise RuntimeError(f"{client}: command was not stopped before its natural exit")
                    time.sleep(.1)
                print(f"{client}: PID {details['pid']} exited", flush=True)
            (root / "stopped.json").write_text(json.dumps(sessions, indent=2))
            signal.pause()
        finally:
            gateway.terminate()
            gateway.wait(timeout=15)


if __name__ == "__main__":
    try:
        main(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve(), sys.argv[3:] or CLIENTS)
    except KeyboardInterrupt:
        pass
