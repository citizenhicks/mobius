"""Exercise the real gateway dashboard and provider editor in an isolated tmux terminal.

Usage: python3 gateway_settings.py <empty evidence directory> <directory containing mobius-gateway>
Requires Python 3.11+ and tmux. All provider addresses and credentials belong to this fixture.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import hashlib
import json
import os
from pathlib import Path
import shlex
import re
import subprocess
import sys
import tempfile
import time
import tomllib

from command_gateway import bootstrap, wait_for_listener


class Terminal:
    def __init__(self, socket_path, env, root, binary):
        self.socket_path = str(socket_path)
        self.env = env
        self.root = root
        self.binary = binary
        self.trace = []

    def command(self, *args, check=True):
        return subprocess.run(["tmux", "-S", self.socket_path, "-f", "/dev/null", *args],
                              env=self.env, capture_output=True, check=check, text=True).stdout

    def launch(self, *args):
        self.trace.append({"launch": args})
        self.command("new-session", "-d", "-s", "ui", "-x", "162", "-y", "65",
                     "-c", str(self.root), shlex.join([self.binary, *args]))

    def wait(self, label):
        deadline = time.monotonic() + 10
        while True:
            screen = self.command("capture-pane", "-p", "-t", "ui")
            if label in screen:
                self.trace.append({"observed": label, "screen": screen.rstrip()})
                return screen
            if time.monotonic() > deadline:
                self.trace.append({"missing": label, "screen": screen.rstrip()})
                raise AssertionError(f"Missing {label!r}. See terminal-trace.json")
            time.sleep(.03)

    def key(self, *keys):
        self.trace.append({"keys": keys})
        self.command("send-keys", "-t", "ui", *keys)

    def type(self, text):
        self.trace.append({"text": text})
        self.command("send-keys", "-t", "ui", "-l", text)

    def close(self):
        self.command("kill-session", "-t", "ui")


def settings(ui, config):
    ui.launch()
    ui.wait("i dictation")
    ui.key("i")
    screen = ui.wait("whisper-1")
    assert all(label in screen for label in ("Automatic", "On device", "Live", "After Stop"))
    ui.key("Down", "Enter")
    ui.wait("i dictation")
    assert config()["default_dictation"] == {"on_device": {}}
    ui.key("n")
    ui.wait("Bot question")
    ui.key("Down", "Enter")
    ui.wait("enter apply")
    ui.key("C-u")
    ui.type("Which Bot should review this?")
    ui.key("Enter")
    ui.wait("←→ adjust")
    ui.key(*["Down"] * 6, "Enter")
    ui.wait("i dictation")
    assert config()["auto_routing"]["bot_question"] == "Which Bot should review this?"
    ui.close()


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        if self.path != "/v1/custom/chat":
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.calls.append(body["model"])
        response = b'data: {"choices":[{"index":0,"delta":{"content":"ready"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


def provider_models(ui, config, calls):
    ui.launch("provider")
    ui.wait("A2Agent")
    ui.key("Up", "Enter")
    ui.wait("reuse the gateway credential")
    ui.key("Enter")
    screen = ui.wait("Decision models")
    assert all(label in screen for label in ("Language models", "Dictation models", "Image models",
                                               "Voice models", "Unavailable"))
    decision = screen.split("Decision models", 1)[1]
    assert "Unavailable" in decision.split("Save", 1)[0]
    ui.close()

    ui.launch("provider")
    ui.wait("A2Agent")
    ui.key("Enter")
    ui.wait("reuse the gateway credential")
    ui.key("Enter")
    ui.wait("Decision models")
    ui.key("Enter")
    ui.wait("API format")
    ui.key("Right")
    ui.wait("Chat Completions")
    ui.key("Down", "Enter")
    ui.wait("ctrl+u")
    ui.key("C-u")
    ui.type("custom/chat")
    ui.key("Enter")
    ui.wait("custom/chat")
    ui.key(*["Down"] * 5, "Enter")
    ui.wait("ctrl+u")
    ui.key("C-u")
    ui.type("low, high")
    ui.key("Enter")
    ui.wait("low, high")
    ui.key("Escape")
    ui.wait("Decision models")
    ui.key("Down", "Enter")
    before = ui.wait("After Stop")
    assert re.search(r"Transcription +Live", before)
    ui.key("Right")
    live = ui.wait("Realtime transcription")
    assert len(re.findall(r"Transcription +Live", live)) == 2 and "After Stop" not in live
    ui.key("Left")
    ui.wait("After Stop")
    ui.key("Down", "Enter")
    ui.wait("ctrl+u")
    ui.key("C-u")
    ui.type("/audio/transcriptions")
    ui.key("Enter")
    native = ui.wait("/audio/transcriptions")
    assert re.search(r"Transcription +Live", native) and "After Stop" in native
    ui.key("Escape")
    ui.wait("Decision models")
    ui.key("Up", "t")
    ui.wait("Provider test passed")
    assert calls == ["fixture-chat", "fixture-chat"], calls
    assert not config()["configured_providers"]["fixture"].get("group_endpoints")
    ui.key("BTab", "Enter")

    deadline = time.monotonic() + 10
    expected = [{"kind": "language", "api": "chat_completions", "path": "custom/chat"}]
    while True:
        provider = config()["configured_providers"]["fixture"]
        if provider.get("group_endpoints") == expected:
            assert [v["id"] for v in provider["models"][0]["reasoning"]] == ["low", "high"]
            return
        if time.monotonic() > deadline:
            raise AssertionError("Provider editor did not persist the selected group API and path")
        time.sleep(.03)


def main(root, binaries):
    os.umask(0o077)
    state, binary, env, _, port = bootstrap(root, binaries)
    env.update(HOME=str(root), TERM="xterm-256color")
    with tempfile.TemporaryDirectory(prefix="mobius-ui-", dir="/tmp") as socket_dir, (root / "gateway.log").open("w") as log:
        ui = Terminal(Path(socket_dir) / "tmux.sock", env, root, binary)
        gateway = subprocess.Popen([binary, "serve"], env=env, cwd=root, stdout=log, stderr=log)
        upstream = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        upstream.calls = []
        worker = threading.Thread(target=upstream.serve_forever, daemon=True)
        worker.start()
        try:
            wait_for_listener(gateway, port)
            for provider, instance, model, extra in [
                ("responses", "fixture", "fixture-chat", ["--dictation-model-id", "gpt-live-transcribe",
                                                          "--dictation-model-id", "whisper-1"]),
                ("a2agent", "a2agent", "fixture-a2a", []),
            ]:
                subprocess.run([binary, "register-provider", "--provider", provider, "--instance", instance,
                                "--model", model, "--base-url", f"http://127.0.0.1:{upstream.server_port}/v1", "--credential-stdin",
                                *extra], env=env, input=b"fixture-only", check=True, capture_output=True)
            def config():
                return tomllib.loads((state / "gateway.toml").read_text())
            settings(ui, config)
            provider_models(ui, config, upstream.calls)
        finally:
            upstream.shutdown()
            upstream.server_close()
            worker.join()
            ui.command("kill-server", check=False)
            gateway.terminate()
            gateway.wait(timeout=10)
            with open(binary, "rb") as source:
                digest = hashlib.file_digest(source, "sha256").hexdigest()
            evidence = {"binary": binary, "sha256": digest, "actions": ui.trace}
            (root / "terminal-trace.json").write_text(json.dumps(evidence, indent=2))
    print("Passed gateway settings and provider model groups")


if __name__ == "__main__":
    main(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
