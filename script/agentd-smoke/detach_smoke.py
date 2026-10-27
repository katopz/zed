"""Kills the GUI side mid-turn and checks the turn finished detached (.issues/036)."""
import json
import os
import subprocess
import sys
import tempfile
import time

eval_cli, agentd, port = sys.argv[1], sys.argv[2], sys.argv[3]
workdir = tempfile.mkdtemp(prefix="z036-")
with open(os.path.join(workdir, "hello.txt"), "w") as f:
    f.write("magic word: pineapple\n")
model = subprocess.Popen([sys.executable, "-I", os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_model.py"), port, os.path.basename(workdir)],
                         env=dict(os.environ, FAKE_DELAY="4"),
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
env = dict(os.environ, HOME=tempfile.mkdtemp(prefix="z036-home-"), FAKE_API_KEY="test",
           ZED_AGENTD_HOME=tempfile.mkdtemp(prefix="z036-agentd-"),
           ZED_OPENAI_COMPATIBLE_PROVIDERS=json.dumps({"fake": {
               "api_url": f"http://127.0.0.1:{port}/v1", "available_models": [{
                   "name": "m", "max_tokens": 32000, "capabilities": {
                       "tools": True, "images": False, "parallel_tool_calls": False,
                       "prompt_cache_key": False}}]}}))
command = [agentd, "attach", "--", eval_cli, "--acp", "--model", "fake/m"]


class Gui:
    def __init__(self):
        self.process = subprocess.Popen(command, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True, bufsize=1)
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
                return message, updates
            if message.get("method") == "session/update":
                updates.append(message["params"]["update"])


try:
    gui = Gui()
    gui.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    session_id = gui.call("session/new", {"cwd": workdir, "mcpServers": []})[0]["result"]["sessionId"]
    gui.send("session/prompt", {"sessionId": session_id, "prompt": [
        {"type": "text", "text": "Read hello.txt and reply with only the magic word."}]})
    time.sleep(1)
    gui.process.kill()
    print("GUI killed mid-turn; waiting for the detached turn")
    time.sleep(8)

    gui = Gui()
    gui.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    reply, updates = gui.call("session/load", {"sessionId": session_id, "cwd": workdir, "mcpServers": []})
    text = "".join(u["content"].get("text", "") for u in updates if u["sessionUpdate"] == "agent_message_chunk")
    statuses = [u.get("status") for u in updates if u["sessionUpdate"] == "tool_call_update"]
    print("replay after reattach:", repr(text), "| last tool status:", statuses[-1:] if statuses else None)
    print("PASS" if text.strip() == "pineapple" else "FAIL")
    gui.process.stdin.close()
    gui.process.wait(timeout=30)
finally:
    subprocess.run([agentd, "stop"], env=env, capture_output=True)
    model.kill()
