"""Scripted OpenAI-compatible streaming endpoint for the .issues/036 smokes."""
import json
import os
import tempfile
import time
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root = sys.argv[2]  # worktree root name, e.g. z036-abc
log = open(os.path.join(tempfile.gettempdir(), "agentd-smoke-model.log"), "w", encoding="utf-8")


def chunk(delta, finish=None):
    return {"id": "c", "object": "chat.completion.chunk", "model": "m",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


def plan(body):
    messages = body.get("messages", [])
    if not body.get("tools"):
        return [chunk({"role": "assistant", "content": "Smoke thread"}), chunk({}, "stop")]
    last = messages[-1]
    if last["role"] == "tool":
        time.sleep(float(os.environ.get("FAKE_DELAY", "0")))
        content = json.dumps(last.get("content", ""))
        word = "pineapple" if "pineapple" in content else "OK"
        return [chunk({"role": "assistant", "content": word[:3]}),
                chunk({"content": word[3:]}), chunk({}, "stop")]
    text = json.dumps(last.get("content", ""))
    if "Edit hello.txt" in text:
        name, args = "edit_file", {"path": f"{root}/hello.txt", "edits": [
            {"old_text": "pineapple", "new_text": "mango"}]}
    elif "hello.txt" in text:
        name, args = "read_file", {"path": f"{root}/hello.txt"}
    else:
        name, args = "terminal", {"command": "printf done > out.txt", "cd": root}
    call = {"index": 0, "id": f"call_{name}", "type": "function",
            "function": {"name": name, "arguments": json.dumps(args)}}
    return [chunk({"role": "assistant", "tool_calls": [call]}), chunk({}, "tool_calls")]


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        events = plan(body)
        log.write(json.dumps({"model": body.get("model"),
                              "last": body.get("messages", [{}])[-1].get("role"),
                              "tools": bool(body.get("tools")), "events": len(events)}) + "\n")
        log.flush()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for event in events:
            self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")

    def log_message(self, *args):
        pass


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
