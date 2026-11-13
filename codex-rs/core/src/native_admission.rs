use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::function_tool::FunctionCallError;
use crate::function_tool::OrCancelToolExt;
use crate::hook_runtime::PreToolUseHookResult;
use crate::hook_runtime::run_pre_tool_use_hooks_with_native;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::flat_tool_name;
use crate::tools::hook_names::HookToolName;
use codex_protocol::ThreadId;
use serde_json::Value;
use serde_json::json;

/// A live invocation owns its admission context. Children and other transports
/// cannot acquire it from environment variables or tool arguments.
pub(crate) struct NativeToolCall {
    invocation: ToolInvocation,
    active: AtomicBool,
}

pub(crate) struct NativeToolCallGuard {
    call: Arc<NativeToolCall>,
}

impl std::ops::Deref for NativeToolCallGuard {
    type Target = NativeToolCall;

    fn deref(&self) -> &Self::Target {
        &self.call
    }
}

impl Drop for NativeToolCallGuard {
    fn drop(&mut self) {
        self.call.active.store(false, Ordering::Release);
    }
}

impl NativeToolCall {
    pub(crate) fn supports(invocation: &ToolInvocation) -> bool {
        external_task_name(invocation).is_some()
    }

    pub(crate) fn register(
        invocation: &ToolInvocation,
    ) -> Result<NativeToolCallGuard, FunctionCallError> {
        let mut calls = invocation
            .session
            .native_tool_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.retain(|_, call| {
            call.upgrade()
                .is_some_and(|call| call.active.load(Ordering::Acquire))
        });
        if calls
            .get(&invocation.call_id)
            .and_then(Weak::upgrade)
            .is_some()
        {
            return Err(FunctionCallError::RespondToModel(
                "native tool call is already active".into(),
            ));
        }
        let call = Arc::new(Self {
            invocation: invocation.clone(),
            active: AtomicBool::new(true),
        });
        calls.insert(invocation.call_id.clone(), Arc::downgrade(&call));
        Ok(NativeToolCallGuard { call })
    }

    pub(crate) async fn admit_external(&self, operation: Value) -> Result<(), FunctionCallError> {
        self.ensure_active()?;
        let tool = external_task_name(&self.invocation).ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "native task dispatch belongs to a different tool".into(),
            )
        })?;
        if operation.get("action").and_then(Value::as_str) != Some(tool) {
            return Err(FunctionCallError::RespondToModel(
                "native task dispatch does not match its invocation".into(),
            ));
        }
        let ToolPayload::Function { arguments } = &self.invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "native task dispatch requires function arguments".into(),
            ));
        };
        let input: Value = serde_json::from_str(arguments)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        let expected = match input.get("threadId").and_then(Value::as_str) {
            Some(target) => ThreadId::from_string(target)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
            None => self.invocation.session.thread_id,
        };
        let actual = operation
            .get("thread_id")
            .and_then(Value::as_str)
            .and_then(|target| ThreadId::from_string(target).ok());
        if actual != Some(expected) {
            return Err(FunctionCallError::RespondToModel(
                "resolved native task target differs from its invocation".into(),
            ));
        }
        admit(&self.invocation, operation).await?;
        self.ensure_active()
    }

    fn ensure_active(&self) -> Result<(), FunctionCallError> {
        if self.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(FunctionCallError::RespondToModel(
                "native tool invocation is no longer active".into(),
            ))
        }
    }
}

fn external_task_name(invocation: &ToolInvocation) -> Option<&str> {
    let name = &invocation.tool_name;
    let tool = if matches!(name.namespace.as_deref(), Some("codex_tui" | "mcp__codex_tui")) {
        name.name.as_str()
    } else {
        name.name.strip_prefix("mcp__codex_tui__")?
    };
    matches!(tool, "create_thread" | "send_message_to_thread" | "fork_thread").then_some(tool)
}

pub(crate) async fn admit(
    invocation: &ToolInvocation,
    operation: Value,
) -> Result<(), FunctionCallError> {
    if invocation.cancellation_token.is_cancelled() {
        return Err(FunctionCallError::RespondToModel(
            "native dispatch was cancelled".into(),
        ));
    }
    let source = match &invocation.source {
        ToolCallSource::Direct | ToolCallSource::DirectPlaintextMessage => {
            json!({"kind": "direct"})
        }
        ToolCallSource::CodeMode {
            cell_id,
            runtime_tool_call_id,
        } => json!({
            "kind": "code_mode", "cell_id": cell_id, "runtime_tool_call_id": runtime_tool_call_id,
        }),
    };
    let input = match &invocation.payload {
        ToolPayload::Function { arguments } => {
            serde_json::from_str(arguments).map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "invalid native dispatch arguments: {error}"
                ))
            })?
        }
        _ => Value::Null,
    };
    let native = json!({
        "thread_id": invocation.session.thread_id,
        "turn_id": invocation.turn.sub_id,
        "call_id": invocation.call_id,
        "tool_name": flat_tool_name(&invocation.tool_name),
        "tool": invocation.tool_name,
        "source": source,
        "operation": operation,
    });
    let outcome = run_pre_tool_use_hooks_with_native(
        &invocation.session,
        &invocation.step_context,
        invocation.call_id.clone(),
        &HookToolName::new("NativeDispatch"),
        &input,
        None,
        &invocation.environments.hook_ids(),
        Some(native),
    )
    .or_cancel_tool(&invocation.cancellation_token)
    .await?;
    if invocation.cancellation_token.is_cancelled() {
        return Err(FunctionCallError::RespondToModel(
            "native dispatch was cancelled".into(),
        ));
    }
    match outcome {
        PreToolUseHookResult::Blocked(reason) => Err(FunctionCallError::RespondToModel(reason)),
        PreToolUseHookResult::Continue {
            updated_input: Some(_),
        } => Err(FunctionCallError::RespondToModel(
            "resolved native dispatch cannot be rewritten".into(),
        )),
        PreToolUseHookResult::Continue {
            updated_input: None,
        } => Ok(()),
    }
}

pub(crate) async fn admit_agent(
    invocation: &ToolInvocation,
    action: &str,
    target: &str,
    resolved: ThreadId,
) -> Result<(), FunctionCallError> {
    let metadata = invocation
        .session
        .services
        .local_agent_runtime
        .control(invocation.session.session_id())
        .get_agent_metadata(resolved);
    admit(
        invocation,
        json!({
            "kind": "agent", "action": action, "thread_id": resolved,
            "agent_path": metadata.as_ref().and_then(|metadata| metadata.agent_path.as_ref()),
        }),
    )
    .await?;
    let current = crate::agent::agent_resolver::resolve_agent_target(
        &invocation.session,
        &invocation.turn,
        target,
    )
    .await?;
    if current != resolved {
        return Err(FunctionCallError::RespondToModel(
            "native agent target changed during admission".into(),
        ));
    }
    Ok(())
}
