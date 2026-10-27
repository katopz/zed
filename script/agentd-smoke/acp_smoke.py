"""Drives `eval-cli --acp` over stdio as an ACP v1 client (.issues/036 smoke)."""
import atexit
import json
import os
import subprocess
import sys
import tempfile

binary, model, port = sys.argv[1], sys.argv[2], sys.argv[3]
workdir = tempfile.mkdtemp(prefix="z036-")
model_server = subprocess.Popen([sys.executable, "-I", os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_model.py"), port,
                                 os.path.basename(workdir)],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
atexit.register(model_server.kill)
with open(os.path.join(workdir, "hello.txt"), "w") as f:
    f.write("magic word: pineapple\n")

home = tempfile.mkdtemp(prefix="z036-home-")
# A throwaway HOME: "always allow" answers must never reach the real settings file.
env = dict(os.environ, HOME=home, FAKE_API_KEY="test", ZED_OPENAI_COMPATIBLE_PROVIDERS=json.dumps({
    "fake": {"api_url": f"http://127.0.0.1:{port}/v1", "available_models": [{
        "name": "m", "max_tokens": 32000,
        "capabilities": {"tools": True, "images": False, "parallel_tool_calls": False,
                         "prompt_cache_key": False}}]}}))
agentd = os.environ.get("SMOKE_AGENTD")
if agentd:
    env["ZED_AGENTD_HOME"] = tempfile.mkdtemp(prefix="z036-agentd-")
command = [binary, "--acp", "--model", model]
if agentd:
    command = [agentd, "attach", "--"] + command
agent = subprocess.Popen(
    command,
    env=env,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=open(os.path.join(tempfile.gettempdir(), "agentd-smoke-agent.err"), "w"),
    text=True,
    bufsize=1,
)
next_id = 0


def send(message):
    agent.stdin.write(json.dumps(message) + "\n")
    agent.stdin.flush()


def request(method, params):
    global next_id
    next_id += 1
    request_id = next_id
    send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
    updates, permissions = [], []
    while True:
        line = agent.stdout.readline()
        if not line:
            raise SystemExit(f"agent closed stdout during {method}")
        message = json.loads(line)
        if message.get("id") == request_id and "method" not in message:
            return message, updates, permissions
        if message.get("method") == "session/update":
            updates.append(message["params"]["update"])
        elif message.get("method") == "session/request_permission":
            options = message["params"]["options"]
            permissions.append([option["kind"] for option in options])
            allow = next(o for o in options if o["kind"] == "allow_always") if any(o["kind"] == "allow_always" for o in options) else next(o for o in options if o["kind"].startswith("allow"))
            send({"jsonrpc": "2.0", "id": message["id"],
                  "result": {"outcome": {"outcome": "selected", "optionId": allow["optionId"]}}})
        else:
            print("other:", line.strip()[:200])


def summarize(updates):
    text = "".join(u["content"].get("text", "") for u in updates
                   if u["sessionUpdate"] == "agent_message_chunk")
    tools = [(u["sessionUpdate"], u.get("title"), u.get("status")) for u in updates
             if u["sessionUpdate"] in ("tool_call", "tool_call_update")]
    return text.strip(), tools


reply, _, _ = request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
print("initialize:", reply.get("result") or reply.get("error"))
reply, _, _ = request("session/new", {"cwd": workdir, "mcpServers": []})
print("session/new:", reply.get("result") or reply.get("error"))
session_id = reply["result"]["sessionId"]

reply, updates, permissions = request("session/prompt", {"sessionId": session_id, "prompt": [
    {"type": "text", "text": "Read hello.txt with your read tool and reply with only the magic word."}]})
text, tools = summarize(updates)
print("prompt 1:", reply.get("result") or reply.get("error"))
print("  text:", text, "| tools:", tools, "| permissions:", permissions)

reply, updates, permissions = request("session/prompt", {"sessionId": session_id, "prompt": [
    {"type": "text", "text": "Run a command that writes done to out.txt, then reply with only OK."}]})
text, tools = summarize(updates)
print("prompt 2:", reply.get("result") or reply.get("error"))
print("  text:", text, "| tools:", tools, "| permissions:", permissions)
out = os.path.join(workdir, "out.txt")
print("  out.txt:", open(out).read() if os.path.exists(out) else None)

agent.stdin.close()
print("exit code:", agent.wait(timeout=60))
if agentd:
    # Second GUI: reattach through the daemon and replay the journaled session.
    agent = subprocess.Popen(command, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=open(os.path.join(tempfile.gettempdir(), "agentd-smoke-agent2.err"), "w"), text=True, bufsize=1)
    reply, _, _ = request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    print("reattach initialize loadSession:", reply["result"]["agentCapabilities"].get("loadSession"))
    reply, _, _ = request("session/list", {})
    print("session/list:", [s["sessionId"] == session_id for s in reply["result"]["sessions"]])
    reply, updates, _ = request("session/load", {"sessionId": session_id, "cwd": workdir, "mcpServers": []})
    text, tools = summarize(updates)
    print("session/load:", "error" not in reply, "| replayed text:", text, "| tool updates:", len(tools))
    agent.stdin.close()
    agent.wait(timeout=60)
    subprocess.run([agentd, "stop"], env=env, capture_output=True)
print("settings written under temp HOME:", os.path.exists(os.path.join(home, ".config/zed/settings.json")))
model_server.kill()
