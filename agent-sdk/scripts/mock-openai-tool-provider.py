"""Deterministic local OpenAI-compatible provider for client golden-path tests."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import argparse
import json
import threading


class Handler(BaseHTTPRequestHandler):
    calls = 0
    lock = threading.Lock()

    def log_message(self, _format, *_args):
        return

    def do_POST(self):  # noqa: N802 - BaseHTTPRequestHandler API
        if not self.path.endswith("/chat/completions"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        with self.lock:
            type(self).calls += 1
            call = type(self).calls
        if call == 1:
            chunks = [
                {
                    "choices": [
                        {
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "mock-write-call",
                                        "function": {
                                            "name": "write_file",
                                            "arguments": json.dumps(
                                                {
                                                    "path": "cli-golden-output.txt",
                                                    "content": "cli golden content\n",
                                                },
                                                ensure_ascii=False,
                                            ),
                                        },
                                    }
                                ]
                            }
                        }
                    ]
                }
            ]
        else:
            chunks = [
                {"choices": [{"delta": {"content": "golden path complete"}}]},
                {
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2,
                    },
                },
            ]
        payload = "".join(f"data: {json.dumps(chunk, ensure_ascii=False)}\n\n" for chunk in chunks)
        payload += "data: [DONE]\n\n"
        body = payload.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
