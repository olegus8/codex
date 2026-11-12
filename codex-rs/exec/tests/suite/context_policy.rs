use anyhow::Result;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::process::Output;

async fn run_exec(json_output: bool, exhausted: bool) -> Result<Output> {
    let test = test_codex_exec();
    std::fs::write(
        test.home_path().join("config.toml"),
        concat!(
            "model_auto_compact_enabled = false\n",
            "model_context_pause_percent = 70\n",
            "model_context_window = 200000\n",
        ),
    )?;
    let server = responses::start_mock_server().await;
    let stream = if exhausted {
        responses::sse_failed("r1", "context_length_exceeded", "Context limit exceeded")
    } else {
        responses::sse(vec![
            responses::ev_response_created("r1"),
            responses::ev_assistant_message("m1", "Work saved"),
            responses::ev_completed_with_tokens("r1", 134_000),
        ])
    };
    let request = responses::mount_sse_once(&server, stream).await;
    let mut command = test.cmd_with_server(&server);
    command.arg("--skip-git-repo-check");
    if json_output {
        command.arg("--json");
    }
    let output = command.arg("Save my work").output()?;
    assert_eq!(request.requests().len(), 1);
    assert_eq!(output.status.success(), !exhausted, "{output:?}");
    Ok(output)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_policy_json_outcomes_are_distinct() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let paused = run_exec(/*json_output*/ true, /*exhausted*/ false).await?;
    let paused = String::from_utf8(paused.stdout)?;
    let completed: Value = serde_json::from_str(paused.lines().last().expect("turn event"))?;
    assert_eq!(completed["type"], "turn.completed");
    assert_eq!(completed["context_pause"]["threshold_percent"], 70);
    let usage = &completed["usage"]["context_window_usage"];
    assert_eq!(usage["context_window"], 190_000);
    assert_eq!(
        usage["used_tokens"],
        completed["context_pause"]["used_tokens"]
    );
    assert!(usage["used_tokens"].as_i64().expect("context usage") >= 134_000);

    let exhausted = run_exec(/*json_output*/ true, /*exhausted*/ true).await?;
    let exhausted = String::from_utf8(exhausted.stdout)?;
    let failed: Value = serde_json::from_str(exhausted.lines().last().expect("turn event"))?;
    assert_eq!(failed["type"], "turn.failed");
    assert_eq!(failed["context_pause"], Value::Null);
    assert!(
        failed["error"]["message"]
            .as_str()
            .expect("exhaustion message")
            .to_lowercase()
            .contains("context")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_policy_human_output_reports_pause_and_exhaustion() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let paused = run_exec(/*json_output*/ false, /*exhausted*/ false).await?;
    let stderr = String::from_utf8(paused.stderr)?;
    assert!(
        stderr.contains("Reached the 70% context pause threshold"),
        "{stderr}"
    );
    assert!(stderr.contains("context window:"), "{stderr}");
    assert_eq!(String::from_utf8(paused.stdout)?.trim(), "Work saved");

    let exhausted = run_exec(/*json_output*/ false, /*exhausted*/ true).await?;
    let stderr = String::from_utf8(exhausted.stderr)?;
    assert!(
        stderr.contains("context window: 190,000 of 190,000 tokens used"),
        "{stderr}"
    );
    assert!(!stderr.contains("context pause threshold"), "{stderr}");
    assert!(exhausted.stdout.is_empty(), "{:?}", exhausted.stdout);
    Ok(())
}
