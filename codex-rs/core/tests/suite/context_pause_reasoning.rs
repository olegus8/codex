use anyhow::Result;
use codex_core::compact::AutoCompactionDisabled;
use codex_features::Feature;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(118_000, false; "below threshold")]
#[test_case(127_000, true; "above threshold")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_policy_counts_past_reasoning_once(used: i64, crosses_threshold: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const WINDOW: i64 = 180_000;
    const REMAINING_CALL: &str = "remaining-context";

    let server = start_mock_server().await;
    let reasoning = "x".repeat(160_000);
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_reasoning_item("reasoning", &[], &[reasoning.as_str()]),
                ev_assistant_message("m1", "Done"),
                ev_completed_with_tokens("r1", used),
            ]),
            sse(vec![
                ev_function_call(REMAINING_CALL, "get_context_remaining", "{}"),
                ev_completed_with_tokens("r2", used),
            ]),
            sse(vec![
                ev_assistant_message("m3", "Done again"),
                ev_completed("r3"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_model_info_override("gpt-5.5", |model| {
            model.context_window = Some(200_000);
            model.effective_context_window_percent = 90;
            model.auto_compact_token_limit = Some(WINDOW);
        })
        .with_config(|config| {
            config.features.enable(Feature::TokenBudget).unwrap();
        })
        .build(&server)
        .await?;
    test.codex
        .thread_extension_data()
        .insert(AutoCompactionDisabled);

    test.submit_turn("Do the work").await?;
    test.submit_turn("Check remaining context").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    let (output, _) = requests[2]
        .function_call_output_content_and_success(REMAINING_CALL)
        .expect("context remaining tool output");
    let output = output.expect("context remaining text");
    let remaining: i64 = output
        .strip_prefix("You have ")
        .and_then(|text| text.strip_suffix(" tokens left in this context window."))
        .expect("reported remaining context")
        .parse()?;
    let measured = WINDOW - remaining;
    assert!((used..used + 1_000).contains(&measured), "{measured}");
    assert_eq!(measured * 100 >= WINDOW * 70, crosses_threshold);
    Ok(())
}
