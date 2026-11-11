import http.server
import json
import queue
import subprocess
import threading


def progress(**fields):
    print(json.dumps(fields), flush=True)


def message(text):
    return {
        "type": "message",
        "id": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": text}],
    }


class Responses:
    def __init__(self, directory):
        self.steps = queue.Queue()
        self.requests = []
        self.directory = directory
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                body = json.loads(
                    self.rfile.read(int(self.headers["Content-Length"]))
                )
                title = set(
                    body.get("text", {})
                    .get("format", {})
                    .get("schema", {})
                    .get("properties", {})
                ) == {"title"}
                if title:
                    item, used = message('{"title":"Context test"}'), 100
                else:
                    fixture.requests.append({"path": self.path, "body": body})
                    fixture.save()
                    try:
                        item, used = fixture.steps.get_nowait()
                    except queue.Empty:
                        self.send_error(500, "Unexpected model request")
                        return
                if item == "overflow":
                    events = [
                        {
                            "type": "response.failed",
                            "response": {
                                "id": "overflow",
                                "status": "failed",
                                "error": {
                                    "code": "context_length_exceeded",
                                    "message": "Context is full",
                                },
                            },
                        }
                    ]
                else:
                    items = item if isinstance(item, list) else [item]
                    response = {
                        "id": "r" + str(len(fixture.requests)),
                        "object": "response",
                        "status": "completed",
                        "output": items,
                        "usage": {
                            "input_tokens": used,
                            "output_tokens": 10,
                            "total_tokens": used + 10,
                        },
                    }
                    events = [
                        {
                            "type": "response.output_item.done",
                            "output_index": index,
                            "item": item,
                        }
                        for index, item in enumerate(items)
                    ]
                    events.append(
                        {"type": "response.completed", "response": response}
                    )
                data = "".join(
                    "event: "
                    + event["type"]
                    + "\ndata: "
                    + json.dumps(event)
                    + "\n\n"
                    for event in events
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.server = http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0), Handler
        )
        threading.Thread(
            target=self.server.serve_forever, daemon=True
        ).start()

    def save(self):
        (self.directory / "requests.json").write_text(
            json.dumps(self.requests, indent=2)
        )

    def add(self, item, used=2000):
        self.steps.put((item, used))

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class App:
    def __init__(self, command, directory, env):
        self.log = (directory / "events.jsonl").open("a")
        self.stderr = (directory / "stderr.txt").open("a")
        self.process = subprocess.Popen(
            command,
            cwd=directory,
            env=env,
            text=True,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr,
        )
        self.messages = queue.Queue()
        self.events = []
        self.serial = 0
        threading.Thread(target=self.read, daemon=True).start()
        self.request(
            "initialize",
            {
                "clientInfo": {"name": "context_workflow", "version": "1"},
                "capabilities": {"experimentalApi": True},
            },
        )
        self.send("initialized", {})

    def read(self):
        for line in self.process.stdout:
            self.log.write(line)
            self.log.flush()
            self.messages.put(json.loads(line))
        self.messages.put({"eof": True})

    def send(self, method, params, request_id=None):
        value = {"method": method, "params": params}
        if request_id is not None:
            value["id"] = request_id
        self.process.stdin.write(json.dumps(value) + "\n")
        self.process.stdin.flush()

    def until(self, predicate):
        while True:
            value = self.messages.get(timeout=60)
            if value.get("eof"):
                raise RuntimeError("app-server exited; inspect stderr.txt")
            self.events.append(value)
            if "method" in value and "id" in value:
                raise AssertionError(("Unexpected server request", value))
            if predicate(value):
                return value

    def request(self, method, params):
        self.serial += 1
        self.send(method, params, self.serial)
        response = self.until(lambda value: value.get("id") == self.serial)
        assert "error" not in response, response
        return response["result"]

    def turn(self, thread, prompt):
        self.request(
            "turn/start",
            {"threadId": thread, "input": [{"type": "text", "text": prompt}]},
        )
        return self.until(
            lambda value: value.get("method") == "turn/completed"
        )["params"]["turn"]

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=30)
        self.process.stdout.close()
        self.stderr.close()
        self.log.close()


def mcp_server(stream, output):
    for line in stream:
        request = json.loads(line)
        if "id" not in request:
            continue
        method = request["method"]
        if method == "initialize":
            result = {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "context-fixture", "version": "1"},
            }
        elif method == "tools/list":
            result = {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo text",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        },
                    }
                ]
            }
        elif method == "tools/call":
            result = {
                "content": [
                    {
                        "type": "text",
                        "text": request["params"]["arguments"]["text"],
                    }
                ]
            }
        else:
            result = {}
        output.write(
            json.dumps(
                {"jsonrpc": "2.0", "id": request["id"], "result": result}
            )
            + "\n"
        )
        output.flush()


if __name__ == "__main__":
    import sys

    mcp_server(sys.stdin, sys.stdout)
