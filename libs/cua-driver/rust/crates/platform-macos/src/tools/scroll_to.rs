//! scroll_to — scroll the containers of one token-addressed element until it
//! shows, through `AXScrollToVisible`, then read back that it is in view.
//!
//! "In view" is measured against the element's viewport: the intersection of
//! every enclosing `AXScrollArea` and the window frame
//! ([`cua_driver_core::semantic_state::visibility`]). An element already in
//! view sends nothing. Like agent-desktop's `actions/scroll_into_view.rs`
//! (Vercel Labs, Apache-2.0), a scroll that ran but left the element clipped
//! is an error rather than a success.

use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::{
    protocol::ToolResult,
    semantic_state::{self, AttemptResult, Mechanism, Refusal, Visibility},
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::ax::bindings::{
    copy_element_attr, copy_string_attr, element_screen_rect, perform_action, AXUIElementRef,
};

use super::set_state::{
    acquire_semantic_target, classify_ax_error, point_cursor_at, run_observed, semantic_result,
    settle, SemanticOutcome, SemanticTarget,
};
use super::ToolState;

pub struct ScrollToTool {
    state: Arc<ToolState>,
}

impl ScrollToTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "scroll_to".into(),
        description: "Scroll an element (by `element_token`) into view in the background \
            (AXScrollToVisible) and read back that at least half of it shows inside its scroll \
            areas and window. An element already in view sends nothing. To scroll by a \
            distance use `scroll`."
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
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "timeout_ms": cua_driver_core::tool_schema::actionability_timeout_schema()
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: true,
    })
}

#[async_trait]
impl Tool for ScrollToTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    /// Always a background AX mutation of one snapshot element.
    fn has_independent_input_lane(&self, args: &Value) -> bool {
        super::ax_semantic_lane(args)
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let target = match acquire_semantic_target(
            &self.state,
            &args,
            "scroll_to",
            cua_driver_core::actionability::ActionKind::ScrollTo,
        )
        .await
        {
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
        let task_element = element.clone();
        let outcome = run_observed(&args, pid, "scroll_to", move || unsafe {
            scroll_to_blocking(task_element.as_ptr() as AXUIElementRef, window_id)
        })
        .await;
        // The overlay points at where the element ended up.
        point_cursor_at(&self.state, &args, window_id, element).await;
        drop(target);
        let (outcome, changes) = match outcome {
            Ok(done) => done,
            Err(error) => return error,
        };
        semantic_result(
            "scroll_to",
            "in_view",
            "in view",
            pid,
            window_id,
            element_index,
            outcome,
            &changes.result_suffix(),
        )
    }
}

/// Ancestors searched for enclosing scroll areas.
const MAX_ASCENT: usize = 40;

/// The element's viewport: its enclosing scroll areas and the window frame.
///
/// # Safety
///
/// `element` must stay retained for the call.
unsafe fn viewport_of(element: AXUIElementRef, window_id: u32) -> Option<semantic_state::Rect> {
    let window = crate::windows::window_bounds_by_id(window_id)?;
    let mut clips = vec![[window.x, window.y, window.width, window.height]];
    let mut current = copy_element_attr(element, "AXParent");
    for _ in 0..MAX_ASCENT {
        let Some(node) = current else { break };
        let role = copy_string_attr(node, "AXRole");
        if role.as_deref() == Some("AXScrollArea") {
            if let Some(rect) = element_screen_rect(node) {
                clips.push(rect);
            }
        }
        let parent = if role.as_deref() == Some("AXWindow") {
            None
        } else {
            copy_element_attr(node, "AXParent")
        };
        CFRelease(node as CFTypeRef);
        current = parent;
    }
    if let Some(node) = current {
        CFRelease(node as CFTypeRef);
    }
    semantic_state::viewport(&clips)
}

unsafe fn in_view(element: AXUIElementRef, window_id: u32) -> Option<bool> {
    match semantic_state::visibility(
        element_screen_rect(element),
        viewport_of(element, window_id),
    ) {
        Visibility::InView => Some(true),
        Visibility::OutOfView => Some(false),
        Visibility::Unknown => None,
    }
}

/// # Safety
///
/// `element` must stay retained for the call.
unsafe fn scroll_to_blocking(element: AXUIElementRef, window_id: u32) -> SemanticOutcome {
    if super::set_state::element_is_gone(element) {
        return SemanticOutcome::Failed(semantic_state::Failure::Stale, None);
    }
    if in_view(element, window_id) == Some(true) {
        return SemanticOutcome::AlreadySatisfied;
    }
    let mechanism = Mechanism::ScrollToVisible;
    let result = classify_ax_error(mechanism, perform_action(element, "AXScrollToVisible"));
    let observed = match result {
        AttemptResult::Stale => None,
        _ => settle(|| in_view(element, window_id), true),
    };
    match (result, observed) {
        (_, Some(true)) => SemanticOutcome::Verified(mechanism),
        (AttemptResult::Stale, _) => {
            SemanticOutcome::Failed(semantic_state::Failure::Stale, Some(mechanism))
        }
        (AttemptResult::Unsupported, _) => SemanticOutcome::Refused(Refusal {
            code: "scroll_to_unsupported",
            reason: "the element does not support AXScrollToVisible; nothing was sent. Use \
                     scroll with its element_token instead."
                .into(),
        }),
        (AttemptResult::Uncertain, _) => {
            SemanticOutcome::Failed(semantic_state::Failure::Uncertain, Some(mechanism))
        }
        (AttemptResult::Ok, None) => SemanticOutcome::Unverified(mechanism),
        (AttemptResult::Ok, Some(false)) => {
            SemanticOutcome::Failed(semantic_state::Failure::NotReached, Some(mechanism))
        }
    }
}
