Requires Linux, Python 3.11+, bubblewrap and util-linux script.
Keep the matching code-mode host and hashed bwrap beside the Codex binary.
Use development builds for the candidate and its unmodified base.

Run scripts/context_workflow/check.py. Set --binary to the compiled
client, --out to a new results directory, --requirements to the managed
requirements.toml rendered for the official client, and --hook to its
session.py. The requirements must contain PreToolUse, PreCompact and
PostToolUse. The suite retains the question guard and removes the two
context hooks in its isolated copies.

Run Python without optimization. --only takes a scenario-name regex.
Successful scenarios are skipped when repeated with identical inputs.
After a failure or changed inputs, use a new output directory.

Check each scenario's result, requests, events and saved history; terminal
scenarios also save their screen. On unmodified rust-v0.167.2, run
--only '^(lifecycle-native-direct$|default-|manual-|guard-)'.
lifecycle-native-direct must fail at the pause request count. The six
default, manual and guard checks must pass. Run all scenarios on the
integrated client, including the long-stream pause and restart checks.
Synthetic usage exercises client control flow.
