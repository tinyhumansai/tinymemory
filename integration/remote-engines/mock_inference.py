"""Deterministic OpenAI-compatible inference double for live engine wiring tests."""

import hashlib
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def schema_value(schema):
    if not isinstance(schema, dict):
        return None
    if "enum" in schema and schema["enum"]:
        return schema["enum"][0]
    schema_type = schema.get("type")
    if schema_type == "object" or "properties" in schema:
        properties = schema.get("properties", {})
        return {name: schema_value(value) for name, value in properties.items()}
    if schema_type == "array":
        return []
    if schema_type in ("number", "integer"):
        return 0
    if schema_type == "boolean":
        return False
    return "tinymemory"


def embedding(text, dimensions):
    digest = hashlib.sha256(text.encode()).digest()
    vector = [0.0] * dimensions
    for index, byte in enumerate(digest):
        vector[index % dimensions] += (byte + 1) / 256.0
    return vector


class Handler(BaseHTTPRequestHandler):
    def log_message(self, _format, *_args):
        return

    def send_json(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/health":
            self.send_json(200, {"status": "ok"})
        elif self.path.endswith("/models"):
            self.send_json(200, {"object": "list", "data": []})
        else:
            self.send_json(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        request = json.loads(self.rfile.read(length) or b"{}")
        if self.path.endswith("/embeddings"):
            inputs = request.get("input", [])
            if isinstance(inputs, str):
                inputs = [inputs]
            dimensions = int(request.get("dimensions") or 1536)
            data = [
                {
                    "object": "embedding",
                    "index": index,
                    "embedding": embedding(str(text), dimensions),
                }
                for index, text in enumerate(inputs)
            ]
            self.send_json(
                200,
                {
                    "object": "list",
                    "data": data,
                    "model": request.get("model", "test"),
                    "usage": {"prompt_tokens": 1, "total_tokens": 1},
                },
            )
            return
        if self.path.endswith("/chat/completions"):
            response_format = request.get("response_format", {})
            schema = response_format.get("json_schema", {}).get("schema", {})
            content = json.dumps(schema_value(schema) if schema else {})
            self.send_json(
                200,
                {
                    "id": "tinymemory-test",
                    "object": "chat.completion",
                    "created": 0,
                    "model": request.get("model", "test"),
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": content},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2,
                    },
                },
            )
            return
        self.send_json(404, {"error": "not found"})


ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
