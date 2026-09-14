Requires Linux, Python 3.11+, bubblewrap and util-linux script.
Keep the matching code-mode host and hashed bwrap beside the Codex binary.
Use a development build for the candidate and its unmodified base.

python3 scripts/context_workflow/check.py \
  --binary /path/to/codex --out /path/to/new-results \
  --requirements /path/to/requirements.toml --hook /path/to/session.py

Pass the installation's existing required hooks and their session script.
Each fixture mounts a private /etc/codex with only PreCompact and
PostToolUse removed. Other requirements remain. The installed variant
also disables the synchronous question tool; both variants exercise the
required question guard. No host configuration is edited.

The loopback Responses fixture reports synthetic usage in a 200000-token
window, with 190000 usable. MCP echoes fixture text. No model calls are
billed. TUI checks use a real PTY; app-server checks use public JSON-RPC.

Results, requests, events, terminal output and session history are saved
per scenario. Repeating a successful scenario skips it; changed inputs
are refused. After a failure, use a new output directory. --only takes
a scenario-name regex. Python assertions must remain enabled.

On the unmodified base, lifecycle-native-direct must fail at the pause
request count. The default, manual and guard scenarios must still pass.
These checks do not establish real-model token estimation, performance,
macOS or Windows behavior, or readiness to replace an installed client.
