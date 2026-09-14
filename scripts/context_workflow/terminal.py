import json
import os
import re
import selectors
import shlex
import subprocess
import time

from fixture import message, progress


def terminal(case):
    from check import history_request, retained, tool

    fixture = case.fixture
    for call_id, used, answer in (
        ("below", 120000, "BELOW_DONE"),
        ("pause", 134000, None),
        ("ordinary", 140000, "ORDINARY_DONE"),
        ("handoff", 185000, "HANDOFF_DONE"),
        ("full", 191000, None),
    ):
        fixture.add(tool(call_id, "direct", call_id.upper() + "_TOOL"), used)
        if answer:
            fixture.add(message(answer), used)
    command = (
        case.prefix
        + ["--no-alt-screen"]
        + case.options
        + [
            "-c",
            "disable_paste_burst=true",
            "-c",
            'tui.status_line=["context-remaining"]',
            "Below threshold",
        ]
    )
    process = subprocess.Popen(
        [
            "script",
            "-q",
            "-e",
            "-f",
            "-c",
            "stty rows 40 cols 140; exec " + shlex.join(command),
            str(case.directory / "terminal.txt"),
        ],
        cwd=case.directory,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        env={**case.env, "TERM": "xterm-256color"},
    )
    poller = selectors.DefaultSelector()
    poller.register(process.stdout, selectors.EVENT_READ)
    output = bytearray()
    pending = b""
    stages = [
        (b"BELOW_DONE", b"Cross the threshold\r", 2),
        (b"Paused at the 70% context threshold", b"Continue ordinary work\r", 3),
        (b"ORDINARY_DONE", b"Write a handoff\r", 5),
        (b"HANDOFF_DONE", b"Reach exhaustion\r", 7),
        (b"Context exhausted. History is preserved.", b"\x04", 8),
    ]
    stage = 0
    deadline = time.monotonic() + 90
    next_quit = None
    try:
        while process.poll() is None:
            if time.monotonic() > deadline:
                process.stdin.write(b"\x1b\x04\x04")
                process.stdin.flush()
                process.wait(timeout=15)
                raise TimeoutError(f"Terminal stage {stage} did not finish")
            for key, _ in poller.select(timeout=0.1):
                data = os.read(key.fd, 65536)
                if not data:
                    continue
                output.extend(data)
                pending += data
                for query, reply in (
                    (b"\x1b[6n", b"\x1b[1;1R"),
                    (b"\x1b[c", b"\x1b[?1;2c"),
                    (b"\x1b[?u", b"\x1b[?0u"),
                ):
                    if query in pending:
                        process.stdin.write(reply * pending.count(query))
                        process.stdin.flush()
                        pending = pending.replace(query, b"")
                pending = pending[-128:]
            visible = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", output)
            if stage < len(stages) and stages[stage][0] in visible:
                marker, prompt, count = stages[stage]
                assert len(fixture.requests) == count, (stage, count)
                process.stdin.write(prompt)
                process.stdin.flush()
                stage += 1
                progress(
                    scenario=case.directory.name,
                    rendered=marker.decode(),
                    requests=count,
                )
                if stage == len(stages):
                    next_quit = time.monotonic() + 2
            elif next_quit and time.monotonic() >= next_quit:
                process.stdin.write(b"\x04")
                process.stdin.flush()
                next_quit = time.monotonic() + 2
        assert process.returncode == 0, process.returncode
        assert stage == len(stages), stage
        for index, call_id in ((1, "below"), (3, "pause"), (5, "ordinary")):
            retained(fixture.requests[index], call_id, call_id.upper() + "_TOOL")
        records = case.history()
        assert (
            sum(bool(row.get("payload", {}).get("context_pause")) for row in records)
            == 1
        )
        retained(history_request(records), "full", "FULL_TOOL")
        assert not any(row["type"] == "compacted" for row in records)
        assert b"of 190,000 tokens used" in visible
        (case.directory / "screen.txt").write_bytes(visible)
    finally:
        poller.close()
        process.stdin.close()
        process.stdout.close()
