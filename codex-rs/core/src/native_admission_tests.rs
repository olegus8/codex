use std::sync::Arc;

use crate::native_admission::NativeToolCall;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_hooks::HookMcpCall;
use codex_hooks::HookMcpExecutor;
use codex_hooks::HookMcpOutput;
use codex_hooks::Hooks;
use codex_hooks::HooksConfig;
use codex_protocol::ThreadId;
use codex_tools::ToolName;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

struct NoMcp;

impl HookMcpExecutor for NoMcp {
    fn execute(&self, _: HookMcpCall) -> BoxFuture<'_, anyhow::Result<HookMcpOutput>> {
        Box::pin(async { anyhow::bail!("unexpected MCP hook") })
    }
}

pub(crate) fn install(session: &Session, directory: &TempDir, command: Option<&str>) {
    let config_path = AbsolutePathBuf::try_from(directory.path().join("config.toml")).unwrap();
    let mut config = format!(
        "[hooks]\nrequired_admission = [{{ matcher = \"NativeDispatch\", hook = {} }}]\n",
        serde_json::to_string(&format!("{}:pre_tool_use:0:0", config_path.display())).unwrap()
    );
    if let Some(command) = command {
        config.push_str(&format!(
            "[[hooks.PreToolUse]]\nmatcher = \"NativeDispatch\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = {}\n",
            serde_json::to_string(command).unwrap(),
        ));
    }
    let stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: config_path,
                profile: None,
            },
            config.parse().unwrap(),
        )],
        Default::default(),
        Default::default(),
    )
    .unwrap();
    let (hooks, _) = Hooks::new(
        HooksConfig {
            feature_enabled: true,
            bypass_hook_trust: true,
            config_layer_stack: Some(stack),
            ..Default::default()
        },
        session.thread_id,
        Arc::new(NoMcp),
    )
    .unwrap();
    session.services.hooks.store(Arc::new(hooks));
}

pub(crate) fn logging_hook(directory: &TempDir, decision: &str) -> String {
    std::fs::write(directory.path().join("decision"), decision).unwrap();
    let script = directory.path().join("admission.py");
    std::fs::write(&script, r#"import json
from pathlib import Path
import sys
import time
root = Path(__file__).parent
value = json.load(sys.stdin)
with (root / 'calls.jsonl').open('a') as output:
    output.write(json.dumps(value) + '\n')
decision = (root / 'decision').read_text()
if decision == 'wait':
    (root / 'ready').touch()
    deadline = time.monotonic() + 10
    while not (root / 'continue').exists():
        if time.monotonic() > deadline:
            raise RuntimeError('fixture release timed out')
        time.sleep(0.01)
    decision = 'allow'
print(json.dumps({'hookSpecificOutput': {'hookEventName': 'PreToolUse', 'permissionDecision': decision, 'permissionDecisionReason': 'fixture decision'}}))
"#).unwrap();
    format!("python3 {}", script.display())
}

pub(crate) fn calls(directory: &TempDir) -> Vec<Value> {
    std::fs::read_to_string(directory.path().join("calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn invocation(call_id: &str, target: ThreadId) -> ToolInvocation {
    let (session, turn) = make_session_and_context().await;
    let turn = Arc::new(turn);
    ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.into(),
        tool_name: ToolName::namespaced("codex_tui", "send_message_to_thread"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: json!({
                "threadId": target, "prompt": "fixture",
                "native_dispatch": {"action": "allow"},
                "thread_id": "forged", "call_id": "forged",
            })
            .to_string(),
        },
        native_call: None,
        environments: Default::default(),
    }
}

#[tokio::test]
async fn native_admission_parallel_calls_keep_their_thread_and_nested_identity() {
    let directory = tempfile::tempdir().unwrap();
    let target = ThreadId::new();
    let first = invocation("same-call-id", target).await;
    let mut second = invocation("same-call-id", target).await;
    second.source = ToolCallSource::CodeMode {
        cell_id: "cell-1".into(),
        runtime_tool_call_id: "nested-1".into(),
    };
    let command = format!(
        "CODEX_THREAD_ID=forged CODEX_TOOL_CALL_ID=forged {}",
        logging_hook(&directory, "allow")
    );
    install(&first.session, &directory, Some(&command));
    install(&second.session, &directory, Some(&command));
    let first_call = NativeToolCall::register(&first).unwrap();
    let second_call = NativeToolCall::register(&second).unwrap();
    let operation = json!({"action": "send_message_to_thread", "thread_id": target});
    let (a, b) = tokio::join!(
        first_call.admit_external(operation.clone()),
        second_call.admit_external(operation)
    );
    a.unwrap();
    b.unwrap();
    let observed = calls(&directory);
    assert_eq!(observed.len(), 2);
    for call in observed {
        assert_eq!(call["tool_use_id"], "same-call-id");
        assert_eq!(
            call["native_dispatch"]["operation"]["thread_id"],
            target.to_string()
        );
        if call["native_dispatch"]["thread_id"] == first.session.thread_id.to_string() {
            assert_eq!(call["native_dispatch"]["source"]["kind"], "direct");
        } else {
            assert_eq!(
                call["native_dispatch"]["thread_id"],
                second.session.thread_id.to_string()
            );
            assert_eq!(
                call["native_dispatch"]["source"]["runtime_tool_call_id"],
                "nested-1"
            );
        }
    }
    assert!(NativeToolCall::register(&first).is_err());
    drop(first_call);
    assert!(NativeToolCall::register(&first).is_ok());
}

#[tokio::test]
async fn native_admission_rejects_forged_recipient_and_expired_calls() {
    let directory = tempfile::tempdir().unwrap();
    let target = ThreadId::new();
    let invocation = invocation("call-1", target).await;
    let command = logging_hook(&directory, "deny");
    install(&invocation.session, &directory, Some(&command));
    let call = NativeToolCall::register(&invocation).unwrap();
    assert!(
        call.admit_external(
            json!({"action": "send_message_to_thread", "thread_id": ThreadId::new()})
        )
        .await
        .is_err()
    );
    assert!(calls(&directory).is_empty());
    assert!(
        call.admit_external(json!({"action": "send_message_to_thread", "thread_id": target}))
            .await
            .is_err()
    );
    assert_eq!(calls(&directory).len(), 1);
    invocation.cancellation_token.cancel();
    assert!(
        call.admit_external(json!({"action": "send_message_to_thread", "thread_id": target}))
            .await
            .is_err()
    );
    assert_eq!(calls(&directory).len(), 1);
    let retained = invocation
        .session
        .native_tool_calls
        .lock()
        .unwrap()
        .get("call-1")
        .and_then(std::sync::Weak::upgrade)
        .unwrap();
    drop(call);
    let expired = retained
        .admit_external(json!({"action": "send_message_to_thread", "thread_id": target}))
        .await
        .unwrap_err();
    assert!(expired.to_string().contains("no longer active"));
    assert_eq!(calls(&directory).len(), 1);
}
