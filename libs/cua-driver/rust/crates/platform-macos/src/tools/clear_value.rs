//! clear_value — empty one token-addressed text field in the background by
//! writing an empty `AXValue`, then read the emptiness back.
//!
//! A secure field reads back empty or masked whatever it holds, so for it only
//! `AXNumberOfCharacters` counts as evidence. No value, before or after, is
//! ever put in the result: the only fact carried is whether the field is
//! empty. Web content echoes AXValue writes without the renderer seeing them
//! (see `set_value`), so a clear there is reported unverified.

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    semantic_state::{self, AttemptResult, ClearFacts, Mechanism, Plan},
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::ax::bindings::{
    copy_bool_attr, copy_number_attr, copy_string_attr, is_attribute_settable, set_string_attr,
    AXUIElementRef,
};

use super::set_state::{
    acquire_semantic_target, execute_plan, point_cursor_at, run_observed, semantic_result,
    SemanticOutcome, SemanticTarget,
};
use super::ToolState;

pub struct ClearValueTool {
    state: Arc<ToolState>,
}

impl ClearValueTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "clear_value".into(),
        description: "Empty a text field, text area or combo box (by `element_token`) in the \
            background and read back that it is empty. Works on password fields; no value is \
            ever echoed. An already empty field sends nothing. A field whose value cannot be \
            written is refused; web inputs are reported unverified (WebKit/Chromium echo the \
            write)."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                "pid": { "type": "integer", "description": "Target process ID. Optional with element_token (the token carries the pid)." },
                "window_id": {
                    "type": "integer",
                    "description": "Window ID. Omit with element_token."
                },
                "element_token": cua_driver_core::tool_schema::element_token_schema()
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: true,
    })
}

#[async_trait]
impl Tool for ClearValueTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    /// Always a background AX mutation of one snapshot element.
    fn has_independent_input_lane(&self, args: &Value) -> bool {
        super::ax_semantic_lane(args)
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let target = match acquire_semantic_target(&self.state, &args, "clear_value").await {
            Ok(target) => target,
            Err(refusal) => return refusal,
        };
        let SemanticTarget {
            pid,
            window_id,
            element_index,
            ref element,
            ..
        } = target;
        point_cursor_at(&self.state, &args, window_id, element).await;
        let task_element = element.clone();
        let outcome = run_observed(&args, pid, "clear_value", move || {
            let ptr = task_element.as_ptr();
            let web = super::type_text::target_in_web_area(
                pid,
                Some((ptr, Some(element_index))),
                Some(window_id),
            );
            unsafe { clear_blocking(ptr as AXUIElementRef, pid, web) }
        })
        .await;
        drop(target);
        let ((outcome, web, document), changes) = match outcome {
            Ok(done) => done,
            Err(error) => return error,
        };
        let mut suffix = document.note().to_owned();
        if web {
            suffix.push_str(
                " AXValue read-back is not trusted for web content; verify through the \
                 renderer (screenshot or browser page tools), or select the text and delete \
                 it with the keyboard.",
            );
        }
        suffix.push_str(&changes.result_suffix());
        semantic_result(
            "clear_value",
            "cleared",
            "empty",
            pid,
            window_id,
            element_index,
            outcome,
            &suffix,
        )
    }
}

/// `(outcome, web, document_edit)`.
///
/// # Safety
///
/// `element` must stay retained for the call.
unsafe fn clear_blocking(
    element: AXUIElementRef,
    pid: i32,
    web: bool,
) -> (SemanticOutcome, bool, crate::document_edit::DocumentEdited) {
    let secure = copy_string_attr(element, "AXSubrole").as_deref() == Some("AXSecureTextField");
    let facts = ClearFacts {
        role: copy_string_attr(element, "AXRole").unwrap_or_default(),
        secure,
        value_settable: is_attribute_settable(element, "AXValue"),
        empty: copy_string_attr(element, "AXValue").map(|value| value.is_empty()),
        enabled: copy_bool_attr(element, "AXEnabled"),
    };
    let plan = semantic_state::plan_clear(&facts);
    let read = |element: AXUIElementRef| {
        semantic_state::clear_readback(
            secure,
            copy_string_attr(element, "AXValue").map(|value| value.is_empty()),
            copy_number_attr(element, "AXNumberOfCharacters")
                .filter(|count| *count >= 0.0)
                .map(|count| count as u64),
        )
    };
    let write = |element: AXUIElementRef, mechanism: Mechanism, _desired: bool| -> AttemptResult {
        super::set_state::classify_ax_error(mechanism, set_string_attr(element, "AXValue", ""))
    };
    let ran = matches!(plan, Plan::Run { .. });
    let mut outcome = execute_plan(element, plan, read, write);
    if web {
        if let SemanticOutcome::Verified(mechanism) = outcome {
            outcome = SemanticOutcome::Unverified(mechanism);
        }
    }
    // An AX write bypasses NSTextView's edit model, so a document is not
    // marked edited and would never save the change (DS-07).
    let document = if ran && matches!(outcome, SemanticOutcome::Verified(_)) && !secure {
        crate::document_edit::ensure_marked_after_ax_write(pid, element, true)
    } else {
        crate::document_edit::DocumentEdited::NotADocument
    };
    (outcome, web, document)
}
