import argparse
import json
import os
from pathlib import Path
import re
import selectors
import shlex
import subprocess
import time

from context_workflow.fixture import Responses, message, progress


def run(binary, directory, mode):
    directory.mkdir(parents=True)
    home = directory / "home"
    home.mkdir()
    (home / "config.toml").write_text(
        "[projects." + json.dumps(str(directory))
        + "]\ntrust_level = 'trusted'\n"
    )
    fixture = Responses(directory)
    fixture.add(message("INPUT_ACCEPTED"))
    text = "x" * 1984 + "END_OF_PAYLOAD"
    framed = mode != "ordinary"
    payload = text.encode()
    if framed:
        payload = b"\x1b[200~" + payload + b"\x1b[201~"
    if mode == "focus-paste":
        payload = b"\x1b[I" + payload
    marker = (
        f"[Pasted Content {len(text)} chars]".encode()
        if framed else b"END_OF_PAYLOAD"
    )
    options = {
        "model": "gpt-6.1-sol",
        "model_provider": "fixture",
        "model_providers.fixture.name": "Offline input fixture",
        "model_providers.fixture.base_url": "http://127.0.0.1:"
        + str(fixture.server.server_port),
        "model_providers.fixture.wire_api": "responses",
        "model_providers.fixture.requires_openai_auth": False,
        "model_providers.fixture.supports_websockets": False,
        "model_providers.fixture.request_max_retries": 0,
        "features.apps": False,
        "features.hooks": False,
        "features.code_mode.enabled": False,
        "features.enable_request_compression": False,
        "check_for_update_on_startup": False,
        "sandbox_mode": "danger-full-access",
        "approval_policy": "never",
        "tui.disable_paste_burst": True,
    }
    command = [str(binary), "--no-alt-screen"] + [
        arg for key, value in options.items()
        for arg in ("-c", key + "=" + json.dumps(value))
    ]
    shell = "stty rows 40 cols 140; exec " + shlex.join(command)
    process = subprocess.Popen(
        ["script", "-q", "-e", "-f", "-c", shell,
         str(directory / "terminal.txt")],
        cwd=directory, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env={**os.environ, "CODEX_HOME": str(home),
             "TERM": "xterm-256color"},
    )
    poller = selectors.DefaultSelector()
    poller.register(process.stdout, selectors.EVENT_READ)
    output, pending = bytearray(), b""
    injected, submitted = False, False
    deadline = time.monotonic() + 30
    result = {"mode": mode, "characters": len(text), "passed": False}

    def send(data):
        process.stdin.write(data)
        process.stdin.flush()

    try:
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"TUI exited: {process.returncode}")
            for key, _ in poller.select(timeout=0.1):
                data = os.read(key.fd, 65536)
                output.extend(data)
                if not injected:
                    pending += data
                    for query, reply in (
                        (b"\x1b[6n", b"\x1b[1;1R"),
                        (b"\x1b[c", b"\x1b[?1;2c"),
                        (b"\x1b[?u", b"\x1b[?0u"),
                    ):
                        if query in pending:
                            send(reply * pending.count(query))
                            pending = pending.replace(query, b"")
                    pending = pending[-128:]
            visible = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", output)
            if not injected and (
                b"\x1b]7501;state=idle:app=codex\x1b\\" in output
            ):
                assert not fixture.requests
                output.clear()
                send(payload)
                injected = True
                result["injected"] = True
                deadline = time.monotonic() + 10
            elif injected and not submitted and marker in visible:
                assert not fixture.requests
                (directory / "before-enter.txt").write_bytes(visible)
                result["visible_before_enter"] = True
                progress(mode=mode, visible_before_enter=True)
                send(b"\r")
                submitted = True
                deadline = time.monotonic() + 20
            elif submitted and b"INPUT_ACCEPTED" in visible:
                assert len(fixture.requests) == 1, fixture.requests
                messages = [
                    item for item in fixture.requests[0]["body"]["input"]
                    if item.get("role") == "user"
                ]
                content = [
                    part["text"] for item in messages
                    for part in item["content"]
                    if part.get("type") == "input_text"
                ]
                assert content[-1] == text, content
                result.update(passed=True, requests=1, exact_payload=True)
                break
        else:
            raise TimeoutError(
                "waiting for " + (
                    "submission" if submitted
                    else "draft" if injected else "startup"
                )
            )
    except Exception as error:
        result["error"] = repr(error)
    finally:
        (directory / "result.json").write_text(
            json.dumps(result, indent=2) + "\n"
        )
        progress(**result)
        if process.poll() is None:
            send(b"\x03\x03\x04")
            process.wait(timeout=15)
        poller.close()
        process.stdin.close()
        process.stdout.close()
        fixture.close()
    return result["passed"]


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--mode", choices=(
        "ordinary", "paste", "focus-paste"))
    args = parser.parse_args()
    modes = [args.mode] if args.mode else ["ordinary", "paste", "focus-paste"]
    results = [run(args.binary.resolve(), args.out.resolve() / mode, mode)
               for mode in modes]
    raise SystemExit(0 if all(results) else 1)
