use anyhow::Result;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;

fn reasoning_ids(request: &ResponsesRequest) -> Vec<String> {
    request
        .inputs_of_type("reasoning")
        .into_iter()
        .map(|item| item["id"].as_str().expect("reasoning item ID").to_owned())
        .collect()
}

fn failed(code: &str) -> String {
    sse_failed(
        "failed",
        code,
        "An error occurred while processing your request.",
    )
}

#[test_case::test_case(&["server_error"], &["rs_past"]; "one failure")]
#[test_case::test_case(&["server_error", "server_error"], &[]; "two failures")]
#[test_case::test_case(&["server_error", "other_error", "server_error"], &["rs_past"]; "intervening error")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_server_errors_omit_past_reasoning(codes: &[&str], kept: &[&str]) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mut responses = vec![sse(vec![
        ev_reasoning_item("rs_past", &[], &["past reasoning"]),
        ev_assistant_message("msg_first", "Done"),
        ev_completed("first"),
    ])];
    responses.extend(codes.iter().map(|code| failed(code)));
    responses.push(sse(vec![
        ev_assistant_message("msg_second", "Again"),
        ev_completed("second"),
    ]));
    let mock = mount_sse_sequence(&server, responses).await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.submit_text_turn("first").await?;
    test.submit_text_turn("second").await?;

    let requests = mock.requests();
    for request in &requests[1..=codes.len()] {
        assert_eq!(reasoning_ids(request), ["rs_past"]);
    }
    let retry = &requests[codes.len() + 1];
    assert_eq!(reasoning_ids(retry), kept);
    assert!(
        retry
            .inputs_of_type("message")
            .iter()
            .any(|item| item["content"][0]["text"] == "Done")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_after_omission_is_sent() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_reasoning_item("rs_past", &[], &["past reasoning"]),
                ev_assistant_message("msg_first", "Done"),
                ev_completed("first"),
            ]),
            failed("server_error"),
            failed("server_error"),
            sse(vec![
                ev_reasoning_item("rs_fresh", &[], &["fresh reasoning"]),
                ev_assistant_message("msg_second", "Again"),
                ev_completed("second"),
            ]),
            sse(vec![
                ev_assistant_message("msg_third", "Next"),
                ev_completed("third"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.submit_text_turn("first").await?;
    test.submit_text_turn("second").await?;
    test.submit_text_turn("third").await?;

    let requests = mock.requests();
    assert!(reasoning_ids(&requests[3]).is_empty());
    assert_eq!(reasoning_ids(&requests[4]), ["rs_fresh"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_error_after_output_does_not_count_toward_omission() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_reasoning_item("rs_past", &[], &["past reasoning"]),
                ev_assistant_message("msg_first", "Done"),
                ev_completed("first"),
            ]),
            failed("server_error"),
            sse(vec![
                ev_reasoning_item("rs_partial", &[], &["partial reasoning"]),
                json!({
                    "type": "response.failed",
                    "response": {"id": "partial", "output": [], "error": {"code": "server_error"}}
                }),
            ]),
            sse(vec![
                ev_assistant_message("msg_second", "Again"),
                ev_completed("second"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.submit_text_turn("first").await?;
    test.submit_text_turn("second").await?;

    assert_eq!(reasoning_ids(&mock.requests()[3]), ["rs_past", "rs_partial"]);
    Ok(())
}
