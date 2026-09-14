import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sys
import traceback

from fixture import App, Responses, message, progress


def tool(call_id, transport, text):
    name, args = (
        "exec_command",
        {"cmd": "printf '" + text + "'", "max_output_tokens": 12000},
    )
    if transport.endswith("mcp"):
        name, args = "mcp__fixture__echo", {"text": text}
    if transport == "mcp":
        return {
            "type": "function_call",
            "id": call_id,
            "call_id": call_id,
            "namespace": "mcp__fixture",
            "name": "echo",
            "arguments": json.dumps({"text": text}),
        }
    if transport.startswith("code"):
        return {
            "type": "custom_tool_call",
            "id": call_id,
            "call_id": call_id,
            "name": "exec",
            "input": "text(await tools."
            + name
            + "("
            + json.dumps(args)
            + "));",
        }
    return {
        "type": "function_call",
        "id": call_id,
        "call_id": call_id,
        "name": name,
        "arguments": json.dumps(args),
    }


def inputs(request):
    return request["body"]["input"]


def retained(request, call_id, text):
    outputs = [
        item
        for item in inputs(request)
        if item.get("call_id") == call_id
        and item["type"]
        in ("function_call_output", "custom_tool_call_output")
    ]
    assert len(outputs) == 1, (call_id, outputs)
    assert text in json.dumps(outputs[0]), outputs[0]


def history_request(records):
    items = [
        row["payload"] for row in records if row["type"] == "response_item"
    ]
    return {"body": {"input": items}}


class Scenario:
    def __init__(
        self, args, name, transport="direct", hooks="native", auto=False
    ):
        self.directory = args.out / name
        self.directory.mkdir(parents=True)
        self.fixture = Responses(self.directory)
        self.transport = transport
        self.hooks = hooks
        self.app = None
        home = self.directory / "home"
        home.mkdir()
        (home / "config.toml").write_text(
            "[projects."
            + json.dumps(str(self.directory))
            + "]\ntrust_level = 'trusted'\n"
        )
        temporary = self.directory / "temp"
        temporary.mkdir()
        self.env = {
            **os.environ,
            "CODEX_HOME": str(home),
            "TMPDIR": str(temporary),
        }
        options = {
            "model": "gpt-6-astra",
            "model_provider": "fixture",
            "model_context_window": 200000,
            "model_auto_compact_token_limit": 180000,
            "model_context_pause_percent": 70,
            "model_providers.fixture.name": "Offline context fixture",
            "model_providers.fixture.base_url": "http://127.0.0.1:"
            + str(self.fixture.server.server_port),
            "model_providers.fixture.wire_api": "responses",
            "model_providers.fixture.requires_openai_auth": False,
            "model_providers.fixture.supports_websockets": False,
            "model_providers.fixture.request_max_retries": 0,
            "model_providers.fixture.stream_max_retries": 0,
            "features.enable_request_compression": False,
            "features.apps": False,
            "features.hooks": True,
            "features.code_mode.enabled": transport.startswith("code"),
            "features.token_budget": False,
            "check_for_update_on_startup": False,
            "sandbox_mode": "danger-full-access",
            "approval_policy": "never",
            "mcp_servers.fixture.command": sys.executable,
            "mcp_servers.fixture.args": [
                str(Path(__file__).with_name("fixture.py").resolve())
            ],
        }
        if not auto:
            options["model_auto_compact_enabled"] = False
        if hooks == "installed":
            options["tools.experimental_request_user_input.enabled"] = False
        config = self.directory / "system"
        config.mkdir()
        shutil.copyfile(args.hook, config / "session.py")
        requirements = args.requirements.read_text()
        for event in ("PreCompact", "PostToolUse"):
            start = requirements.index("[[hooks." + event + "]]")
            end = requirements.find("\n[[hooks.", start + 1)
            while end != -1 and requirements.startswith(
                "\n[[hooks." + event + ".", end
            ):
                end = requirements.find("\n[[hooks.", end + 1)
            requirements = requirements[:start] + (
                requirements[end:] if end != -1 else ""
            )
        (config / "requirements.toml").write_text(requirements)
        self.prefix = [
            args.bwrap,
            "--ro-bind",
            "/",
            "/",
            "--bind",
            str(self.directory),
            str(self.directory),
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/etc",
        ]
        for path in sorted(Path("/etc").iterdir()):
            if path.name != "codex" and path.exists():
                self.prefix += ["--ro-bind", str(path), str(path)]
        self.prefix += [
            "--ro-bind",
            str(config),
            "/etc/codex",
            "--",
            str(args.binary),
        ]
        self.options = [
            arg
            for key, value in options.items()
            for arg in ("-c", key + "=" + json.dumps(value))
        ]
        (self.directory / "options.json").write_text(
            json.dumps(options, indent=2)
        )

    def start(self, resume=None):
        self.app = App(
            self.prefix + ["app-server"] + self.options,
            self.directory,
            self.env,
        )
        requirements = self.app.request("configRequirements/read", {})
        hooks = requirements["requirements"]["hooks"]
        assert hooks["PreToolUse"], hooks
        assert not hooks.get("PreCompact") and not hooks.get("PostToolUse")
        method = "thread/resume" if resume else "thread/start"
        params = (
            {"threadId": resume}
            if resume
            else {"cwd": str(self.directory), "model": "gpt-6-astra"}
        )
        self.thread = self.app.request(method, params)["thread"]["id"]
        servers = self.app.request(
            "mcpServerStatus/list",
            {"threadId": self.thread, "detail": "toolsAndAuthOnly"},
        )
        server = next(s for s in servers["data"] if s["name"] == "fixture")
        assert server["runtimeStatus"] == "connected", server
        assert server["tools"] and server["toolsError"] is None, server
        return self.thread

    def turn(self, prompt, count, *, paused=False, failed=False):
        before = len(self.fixture.requests)
        turn = self.app.turn(self.thread, prompt)
        assert len(self.fixture.requests) - before == count, (
            prompt,
            count,
            len(self.fixture.requests) - before,
        )
        assert turn["status"] == ("failed" if failed else "completed"), turn
        assert bool(turn.get("contextPause")) == paused, turn
        if failed:
            assert turn["error"]["codexErrorInfo"] == "contextWindowExceeded"
        if paused:
            pause = turn["contextPause"]
            assert pause["contextWindow"] == 190000, pause
            assert pause["thresholdPercent"] == 70, pause
            assert pause["usedTokens"] >= 133000, pause
            updates = [
                event["params"]["tokenUsage"]["contextWindowUsage"]
                for event in self.app.events
                if event.get("method") == "thread/tokenUsage/updated"
            ]
            assert {
                "usedTokens": pause["usedTokens"],
                "contextWindow": 190000,
            } in updates, updates
        (self.directory / (str(before) + "-turn.json")).write_text(
            json.dumps(turn, indent=2)
        )
        progress(
            scenario=self.directory.name,
            prompt=prompt,
            status=turn["status"],
            paused=paused,
            requests=count,
        )
        return turn

    def history(self):
        if self.app:
            self.app.close()
            self.app = None
        records = [
            json.loads(line)
            for path in (self.directory / "home/sessions").rglob("*.jsonl")
            for line in path.read_text().splitlines()
        ]
        assert records, "Missing persisted history"
        return records

    def close(self):
        if self.app:
            self.app.close()
        self.fixture.close()


def lifecycle(case):
    case.start()
    fixture = case.fixture
    fixture.add(tool("below", case.transport, "BELOW_TOOL"), 120000)
    fixture.add(message("BELOW_DONE"), 120000)
    case.turn("Below threshold", 2)
    retained(fixture.requests[1], "below", "BELOW_TOOL")
    fixture.add(tool("pause", case.transport, "PAUSE_TOOL"), 134000)
    case.turn("Cross the pause threshold", 1, paused=True)
    saved = case.app.request(
        "thread/read", {"threadId": case.thread, "includeTurns": True}
    )
    assert (
        sum(bool(t.get("contextPause")) for t in saved["thread"]["turns"])
        == 1
    )
    case.app.close()
    case.start(resume=case.thread)
    fixture.add(tool("ordinary", case.transport, "ORDINARY_TOOL"), 140000)
    fixture.add(message("ORDINARY_DONE"), 140000)
    case.turn("Continue ordinary work", 2)
    retained(fixture.requests[3], "pause", "PAUSE_TOOL")
    case.app.close()
    case.start(resume=case.thread)
    fixture.add(tool("handoff", case.transport, "HANDOFF_TOOL"), 185000)
    fixture.add(message("HANDOFF_DONE"), 185000)
    case.turn("Write a handoff after restart", 2)
    retained(fixture.requests[5], "ordinary", "ORDINARY_TOOL")
    fixture.add(tool("full", case.transport, "FULL_TOOL"), 191000)
    case.turn("Reach the usable limit", 1, failed=True)
    case.turn("Keep this submitted input", 0, failed=True)
    records = case.history()
    assert not any(row["type"] == "compacted" for row in records)
    for call_id in ("below", "pause", "ordinary", "handoff", "full"):
        retained(history_request(records), call_id, call_id.upper() + "_TOOL")
    assert "Keep this submitted input" in json.dumps(records)
    for request in fixture.requests:
        assert request["path"] == "/responses", request["path"]
        assert not any(
            item["type"] == "compaction" for item in inputs(request)
        )


def overflow(case):
    case.start()
    case.fixture.add(message("BEFORE_OVERFLOW"), 120000)
    case.turn("Keep this history", 1)
    case.fixture.add("overflow")
    case.turn("Provider reports exhaustion", 1, failed=True)
    case.turn("Preserve follow-up", 0, failed=True)
    records = case.history()
    assert "BEFORE_OVERFLOW" in json.dumps(records)
    assert not any(row["type"] == "compacted" for row in records)


def large_result(case):
    case.start()
    text = "LARGE_BEGIN" + "abcdef0123456789" * 1800 + "LARGE_END"
    case.fixture.add(tool("large", case.transport, text), 131000)
    case.turn("Large result crosses threshold", 1, paused=True)
    case.fixture.add(message("LARGE_HANDOFF"), 150000)
    case.turn("Handoff retains large output", 1)
    retained(case.fixture.requests[1], "large", "LARGE_END")
    assert not any(row["type"] == "compacted" for row in case.history())


def independent(case):
    first = None
    for index in range(2):
        case.start()
        assert case.thread != first
        first = case.thread
        case.fixture.add(tool("independent", "direct", "INDEPENDENT"), 134000)
        case.turn("Pause this independent session", 1, paused=True)
        case.app.close()
        case.app = None
    records = case.history()
    assert (
        sum(
            bool(row.get("payload", {}).get("context_pause"))
            for row in records
        )
        == 2
    )


def compaction(case, *, manual):
    case.start()
    case.fixture.add(
        message("BEFORE_COMPACT"), 185000 if not manual else 2000
    )
    case.turn("Before compaction", 1)
    case.fixture.add(message("REQUESTED_SUMMARY"), 1000)
    if manual:
        case.app.request("thread/compact/start", {"threadId": case.thread})
        case.app.until(lambda value: value.get("method") == "turn/completed")
    else:
        case.fixture.add(message("AFTER_COMPACT"), 2000)
        case.turn("Automatic compaction still defaults on", 2)
    records = case.history()
    assert any(row["type"] == "compacted" for row in records), records[-4:]


def guard(case):
    case.start()
    case.fixture.add(
        {
            "type": "function_call",
            "id": "guard",
            "call_id": "guard",
            "name": "request_user_input"
            + ("_async" if case.hooks == "installed" else ""),
            "arguments": json.dumps(
                {
                    "questions": [
                        {
                            "header": "Guard",
                            "id": "guard",
                            "question": "Must be blocked",
                            "options": [
                                {"label": "Yes", "description": "Yes"},
                                {"label": "No", "description": "No"},
                            ],
                        }
                    ]
                }
            ),
        }
    )
    case.fixture.add(message("GUARD_PRESERVED"))
    case.turn("Exercise retained question guard", 2)
    retained(case.fixture.requests[1], "guard", "Question menus are disabled")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--requirements", type=Path, required=True)
    parser.add_argument("--hook", type=Path, required=True)
    parser.add_argument("--bwrap", default="bwrap")
    parser.add_argument("--only")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    identity = {
        str(path): hashlib.file_digest(path.open("rb"), "sha256").hexdigest()
        for path in [
            args.binary,
            args.requirements,
            args.hook,
            *sorted(Path(__file__).parent.glob("*.py")),
        ]
    }
    manifest = args.out / "identity.json"
    if manifest.exists():
        assert json.loads(manifest.read_text()) == identity, "Inputs changed"
    else:
        manifest.write_text(json.dumps(identity, indent=2))
    cases = []
    for hooks in ("native", "installed"):
        from terminal import terminal

        cases.append(("terminal-" + hooks, terminal, {"hooks": hooks}))
        for transport in ("direct", "code", "mcp", "code-mcp"):
            cases.append(
                (
                    f"lifecycle-{hooks}-{transport}",
                    lifecycle,
                    {"hooks": hooks, "transport": transport},
                )
            )
        cases.extend(
            [
                ("independent-" + hooks, independent, {"hooks": hooks}),
                ("overflow-" + hooks, overflow, {"hooks": hooks}),
                (
                    "large-" + hooks,
                    large_result,
                    {"hooks": hooks, "transport": "mcp"},
                ),
                ("guard-" + hooks, guard, {"hooks": hooks}),
                (
                    "manual-" + hooks,
                    lambda c: compaction(c, manual=True),
                    {"hooks": hooks},
                ),
                (
                    "default-" + hooks,
                    lambda c: compaction(c, manual=False),
                    {"hooks": hooks, "auto": True},
                ),
            ]
        )
    results = []
    for name, check, options in cases:
        if args.only and not re.search(args.only, name):
            continue
        saved = args.out / name / "result.json"
        if saved.exists() and json.loads(saved.read_text())["passed"]:
            results.append(json.loads(saved.read_text()))
            continue
        if saved.parent.exists():
            raise RuntimeError(f"Use a fresh output directory for {name}")
        case = Scenario(args, name, **options)
        result = {"name": name, "passed": False}
        try:
            check(case)
            result["passed"] = True
        except Exception:
            result["error"] = traceback.format_exc()
        finally:
            case.close()
        saved.write_text(json.dumps(result, indent=2))
        results.append(result)
        print(json.dumps(result), flush=True)
    report = {
        "binary": str(args.binary),
        "sha256": identity[str(args.binary)],
        "results": results,
    }
    (args.out / "report.json").write_text(json.dumps(report, indent=2))
    return int(not results or not all(r["passed"] for r in results))


if __name__ == "__main__":
    sys.exit(main())
