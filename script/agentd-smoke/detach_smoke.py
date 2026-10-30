"""Kills the GUI side mid-turn and checks the turn finished detached (.issues/036),
that a client reattaching mid-turn is told it is running and when it ends
(plan 037 P4), and that a later client replays it all.

With SMOKE_VIA_REMOTE_SERVER=1 the second argument is the remote server binary:
each client goes through `remote_server exec` like the GUI does, and attaches to
the agentd embedded in that binary (plan 037 P4 auto-deploy).

With SMOKE_SSH set as well (an ssh command prefix, e.g. "ssh my-host"), the
binaries, the fake model and the project live on that host, and each client is
a local ssh process running `remote_server exec` there. Killing a client then
drops a real SSH connection, the way closing the GUI machine does (plan 037 P3)."""
import json
import os
import shlex
import struct
import subprocess
import sys
import tempfile
import time

eval_cli, agentd, port = sys.argv[1], sys.argv[2], sys.argv[3]
fake_model_source = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_model.py")


class LocalHost:
    prefix = []

    def mkdtemp(self, prefix):
        # /tmp directly: macOS's default temp dir (/var/folders/...) is deep
        # enough that the agentd socket path exceeds SUN_LEN (104) and attach
        # refuses ("path must be shorter than SUN_LEN").
        return tempfile.mkdtemp(prefix=f"/tmp/{prefix}")

    def write(self, path, text):
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)

    def popen(self, argv, env, **kwargs):
        return subprocess.Popen(argv, env=dict(os.environ, **env), **kwargs)

    def run(self, argv, env):
        subprocess.run(argv, env=dict(os.environ, **env), capture_output=True)


class SshHost:
    def __init__(self, ssh):
        self.prefix = shlex.split(ssh)

    def shell(self, command, **kwargs):
        return subprocess.run([*self.prefix, command], check=True, capture_output=True, text=True,
                              encoding="utf-8", errors="replace", **kwargs)

    def mkdtemp(self, prefix):
        return self.shell(f"mktemp -d /tmp/{prefix}XXXXXX").stdout.strip()

    def write(self, path, text):
        self.shell(f"cat > {shlex.quote(path)}", input=text)

    def command(self, argv, env):
        assignments = [f"{key}={value}" for key, value in env.items()]
        return shlex.join(["env", *assignments, *argv])

    def popen(self, argv, env, **kwargs):
        return subprocess.Popen([*self.prefix, self.command(argv, env)], **kwargs)

    def run(self, argv, env):
        subprocess.run([*self.prefix, self.command(argv, env)], capture_output=True)


ssh = os.environ.get("SMOKE_SSH")
host = SshHost(ssh) if ssh else LocalHost()
workdir = host.mkdtemp("z036-")
host.write(os.path.join(workdir, "hello.txt"), "magic word: pineapple\n")
tools = host.mkdtemp("z036-tools-")
fake_model = os.path.join(tools, "fake_model.py")
with open(fake_model_source, encoding="utf-8") as f:
    host.write(fake_model, f.read())
model = host.popen(["python3" if ssh else sys.executable, "-I", fake_model, port, os.path.basename(workdir)], {"FAKE_DELAY": "4"},
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
test_env = {"HOME": host.mkdtemp("z036-home-"), "FAKE_API_KEY": "test",
            "ZED_AGENTD_HOME": host.mkdtemp("z036-agentd-"),
            "ZED_OPENAI_COMPATIBLE_PROVIDERS": json.dumps({"fake": {
                "api_url": f"http://127.0.0.1:{port}/v1", "available_models": [{
                    "name": "m", "max_tokens": 32000, "capabilities": {
                        "tools": True, "images": False, "parallel_tool_calls": False,
                        "prompt_cache_key": False}}]}})}
agent = [eval_cli, "--acp", "--model", "fake/m"]
via_server = os.environ.get("SMOKE_VIA_REMOTE_SERVER") == "1"
assert via_server or not ssh, "SMOKE_SSH needs SMOKE_VIA_REMOTE_SERVER=1: the GUI reaches a host through remote_server exec"
if via_server:
    agentd_cli = [agentd, "agentd"]
    command = [*host.prefix, shlex.join([agentd, "exec"])] if ssh else [agentd, "exec"]
    # The GUI's shim takes this branch when `remote_server exec` exports its own path.
    # Over SSH the test environment travels in the descriptor, as the GUI's does.
    descriptor = json.dumps({"program": "sh", "args": [
        "-c", 'exec "$ZED_AGENTD_REMOTE_SERVER" agentd attach "$@"', "shim", "--", *agent],
        "env": test_env if ssh else {}, "working_dir": None}).encode()
    preamble = struct.pack("<I", len(descriptor)) + descriptor
else:
    agentd_cli = [agentd]
    command = [agentd, "attach", "--", *agent]
    preamble = b""
env = dict(os.environ, **test_env)
# Over SSH the clients are local ssh processes; the test environment is on the host.
client_env = dict(os.environ) if ssh else env

class Gui:
    def __init__(self):
        self.process = subprocess.Popen(command, env=client_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True, bufsize=1, encoding="utf-8", errors="replace")
        self.process.stdin.buffer.write(preamble)
        self.process.stdin.buffer.flush()
        self.next_id = 0

    def send(self, method, params):
        self.next_id += 1
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.next_id,
                                             "method": method, "params": params}) + "\n")
        self.process.stdin.flush()
        return self.next_id

    def call(self, method, params):
        request_id = self.send(method, params)
        updates = []
        while True:
            message = json.loads(self.process.stdout.readline())
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    raise RuntimeError(f"{method} failed: {message['error']}")
                return message, updates
            if message.get("method") == "session/update":
                updates.append(message["params"]["update"])

    def read_until_turn_state(self, running):
        """Live updates up to agentd's `_zed-agentd/turn_state` with `running`."""
        updates = []
        while True:
            message = json.loads(self.process.stdout.readline())
            if message.get("method") == "session/update":
                updates.append(message["params"]["update"])
            elif message.get("method") == "_zed-agentd/turn_state":
                assert message["params"] == {"sessionId": session_id, "running": running}, message
                return updates


def text_of(updates):
    return "".join(u["content"].get("text", "") for u in updates if u["sessionUpdate"] == "agent_message_chunk")


try:
    gui = Gui()
    gui.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    session_id = gui.call("session/new", {"cwd": workdir, "mcpServers": []})[0]["result"]["sessionId"]
    gui.send("session/prompt", {"sessionId": session_id, "prompt": [
        {"type": "text", "text": "Read hello.txt and reply with only the magic word."}]})
    time.sleep(1)
    gui.process.kill()
    print("GUI killed mid-turn")

    # Reattach while the turn still runs: agentd must say so, then say when it ends.
    gui = Gui()
    gui.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    gui.call("session/load", {"sessionId": session_id, "cwd": workdir, "mcpServers": []})
    gui.read_until_turn_state(True)
    live = gui.read_until_turn_state(False)
    mid_turn_ok = text_of(live).strip() == "pineapple"
    print("mid-turn reattach: running=true, then running=false after", repr(text_of(live)))
    gui.process.kill()

    gui = Gui()
    gui.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    reply, updates = gui.call("session/load", {"sessionId": session_id, "cwd": workdir, "mcpServers": []})
    text = text_of(updates)
    statuses = [u.get("status") for u in updates if u["sessionUpdate"] == "tool_call_update"]
    print("replay after reattach:", repr(text), "| last tool status:", statuses[-1:] if statuses else None)
    print("PASS" if mid_turn_ok and text.strip() == "pineapple" else "FAIL")
    gui.process.stdin.close()
    gui.process.wait(timeout=30)
finally:
    host.run([*agentd_cli, "stop"], test_env)
    model.kill()
    if ssh:
        # Killing the local ssh does not stop the model it started on the host.
        host.run(["pkill", "-f", fake_model], {})
        host.run(["rm", "-rf", workdir, tools, test_env["HOME"], test_env["ZED_AGENTD_HOME"]], {})
