Requires Linux, Python 3.11+, bubblewrap and util-linux script.
Keep the matching code-mode host and hashed bwrap beside the Codex binary.
Use development builds for the candidate and its unmodified base.

Run scripts/context_workflow/check.py. Set --binary to the compiled
client, --out to a new results directory, --requirements to the original
managed requirements.toml, and --hook to its session.py. The requirements
must contain PreCompact and PostToolUse.

Run Python without optimization. --only takes a scenario-name regex.
Successful scenarios are skipped when repeated with identical inputs.
After a failure or changed inputs, use a new output directory.
Run long-stream on the last published fork release; it must fail
at the usage bound.

Check each scenario's result, requests, events and saved history; terminal
scenarios also save their screen. On the unmodified base,
lifecycle-native-direct must fail at the pause request count; default,
manual and guard must pass. Synthetic usage tests client control flow,
not token estimation, performance or other platforms.
