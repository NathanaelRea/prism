#!/usr/bin/env python3
"""Small offline OpenAI-compatible provider used by the demo capture."""

from __future__ import annotations

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def request_text(payload: dict[str, object]) -> str:
    messages = payload.get("messages", [])
    return "\n".join(str(message.get("content", "")) for message in messages if isinstance(message, dict)).lower()


def has_tool_result(payload: dict[str, object]) -> bool:
    return any(message.get("role") == "tool" for message in payload.get("messages", []) if isinstance(message, dict))


def reply_for(payload: dict[str, object]) -> tuple[str, str | None]:
    text = request_text(payload)
    if has_tool_result(payload):
        return ("The requested sandbox action completed successfully.", None)
    if "plan-ci.md phase" in text or "run the plan" in text:
        return ("I am applying the checkout CI fixture change and running its focused check.", "prism-demo-scenario apply-plan")
    if "plan-ci.md" in text or "ci/cd" in text or "ci workflow" in text:
        return ("I inspected the local CI fixture and am writing the executable plan.", "prism-demo-scenario write-plan")
    if "review" in text:
        return ("I am preparing the requested review repair.", "prism-demo-scenario repair review")
    if "ci" in text or "check" in text:
        return ("I am preparing the CI repair.", "prism-demo-scenario repair ci")
    return ("The offline Prism demo provider is ready.", None)


class Handler(BaseHTTPRequestHandler):
    server_version = "PrismDemoProvider/1"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def send_json(self, value: dict[str, object]) -> None:
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/v1/models":
            self.send_json({"object": "list", "data": [{"id": "prism-demo", "object": "model"}]})
            return
        self.send_error(404)

    def do_POST(self) -> None:
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(length) or b"{}")
        reply, command = reply_for(payload)
        tool_calls = []
        if command:
            tool_calls = [{"id": "call_prism_demo", "type": "function", "function": {"name": "bash", "arguments": json.dumps({"command": command})}}]
        completion = {
            "id": "chatcmpl-prism-demo",
            "object": "chat.completion",
            "created": int(time.time()),
            "model": "prism-demo",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": None if command else reply, "tool_calls": tool_calls}, "finish_reason": "tool_calls" if command else "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
        if not payload.get("stream"):
            self.send_json(completion)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        chunks = [
            {"id": completion["id"], "object": "chat.completion.chunk", "created": completion["created"], "model": "prism-demo", "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
        ]
        if command:
            chunks.append({"id": completion["id"], "object": "chat.completion.chunk", "created": completion["created"], "model": "prism-demo", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, **tool_calls[0]}]}, "finish_reason": None}]})
        else:
            chunks.append({"id": completion["id"], "object": "chat.completion.chunk", "created": completion["created"], "model": "prism-demo", "choices": [{"index": 0, "delta": {"content": reply}, "finish_reason": None}]})
        chunks.append({"id": completion["id"], "object": "chat.completion.chunk", "created": completion["created"], "model": "prism-demo", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls" if command else "stop"}]})
        for chunk in chunks:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")


if __name__ == "__main__":
    port = int(sys.argv[1])
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
