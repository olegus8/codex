use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use test_case::test_case;

async fn turn(test: &TestCodex, text: &str) -> Result<()> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        if let EventMsg::Error(error) = event {
            panic!("unexpected error: {error:?}");
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

fn reasoning_ids(request: &ResponsesRequest) -> Vec<String> {
    request
        .input()
        .iter()
        .filter(|item| item["type"] == "reasoning")
        .map(|item| item["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test_case(1, &["rs_past"]; "after one failure")]
#[test_case(2, &[]; "after two failures")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_server_errors_omit_past_reasoning(
    failures: usize,
    kept: &[&str],
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_reasoning_item("rs_past", &[], &["thought"]),
            ev_assistant_message("m1", "Done"),
            ev_completed("r1"),
        ]),
    )
    .await;
    let mut failed = Vec::new();
    for index in 0..failures {
        let failure = sse_failed(
            &format!("f{index}"),
            "server_error",
            "An error occurred while processing your request.",
        );
        failed.push(mount_sse_once(&server, failure).await);
    }
    let retried = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("m2", "Again"),
            ev_completed("r2"),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    turn(&test, "first").await?;
    turn(&test, "second").await?;

    for mock in failed {
        assert_eq!(reasoning_ids(&mock.single_request()), ["rs_past"]);
    }
    assert_eq!(reasoning_ids(&retried.single_request()), kept);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_after_omission_is_sent() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_reasoning_item("rs_past", &[], &["thought"]),
            ev_assistant_message("m1", "Done"),
            ev_completed("r1"),
        ]),
    )
    .await;
    for index in 0..2 {
        let failure = sse_failed(
            &format!("f{index}"),
            "server_error",
            "An error occurred while processing your request.",
        );
        mount_sse_once(&server, failure).await;
    }
    mount_sse_once(
        &server,
        sse(vec![
            ev_reasoning_item("rs_fresh", &[], &["thought"]),
            ev_assistant_message("m2", "Again"),
            ev_completed("r2"),
        ]),
    )
    .await;
    let next = mount_sse_once(
        &server,
        sse(vec![ev_assistant_message("m3", "Next"), ev_completed("r3")]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    turn(&test, "first").await?;
    turn(&test, "second").await?;
    turn(&test, "third").await?;

    assert_eq!(reasoning_ids(&next.single_request()), ["rs_fresh"]);
    Ok(())
}
