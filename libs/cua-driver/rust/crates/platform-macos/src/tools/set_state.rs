//! set_state — put one token-addressed element into a named state: checked,
//! unchecked, toggled, expanded, collapsed or focused.
//!
//! Every state is reached through Accessibility on the element itself
//! (`AXValue`, `AXExpand`/`AXCollapse`, `AXExpanded`, `AXDisclosing`,
//! `AXPress`, `AXFocused`), so the call is background-safe: no pointer moves,
//! no window is raised, and the app is not activated. The state is read
//! before anything is sent (a state that already holds sends nothing) and
//! read back after each mechanism; the pure rules for both live in
//! [`cua_driver_core::semantic_state`].
//!
//! The helpers at the bottom (target resolution under the per-pid mutation
//! lease, cursor feedback, AX error classification and result shaping) are
//! shared with `clear_value` and `scroll_to`.

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    semantic_state::{
        self, after_attempt, AttemptResult, DisclosureFacts, Failure, FocusFacts, Mechanism, Next,
        Plan, Refusal, RequestedState, ToggleFacts,
    },
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ax::bindings::{
    copy_action_names, copy_binary_attr, copy_bool_attr, copy_string_attr, copy_stringish_attr,
    is_attribute_settable, perform_action, set_bool_attr, set_number_attr, AXError, AXUIElementRef,
};

use super::ToolState;

pub struct SetStateTool {
    state: Arc<ToolState>,
}

impl SetStateTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "set_state".into(),
        description: "Put an element (by `element_token`) into a state, in the background, and \
            read it back. `checked`/`unchecked`: a checkbox, switch or radio button (a radio \
            cannot be unchecked; select another). `toggled`: flip a checkbox. `expanded`/\
            `collapsed`: a disclosure triangle, outline row or combo box. `focused`: keyboard \
            focus on the element. A state that already holds sends nothing; every state but \
            `toggled` is safe to repeat. An unreadable state is refused rather than pressed \
            blindly."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["state"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                "pid": { "type": "integer", "description": "Target process ID. Optional with element_token (the token carries the pid)." },
                "window_id": {
                    "type": "integer",
                    "description": "Window ID. Omit with element_token."
                },
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "state": {
                    "type": "string",
                    "enum": semantic_state::STATE_NAMES,
                    "description": "The state the element should be in."
                }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

#[async_trait]
impl Tool for SetStateTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    /// Always a background AX mutation of one snapshot element.
    fn has_independent_input_lane(&self, args: &Value) -> bool {
        super::ax_semantic_lane(args)
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let requested = match args.require_str("state") {
            Ok(state) => match RequestedState::parse(&state) {
                Some(requested) => requested,
                None => {
                    return ToolResult::error(format!(
                        "set_state: unknown state \"{state}\"; use one of {}.",
                        semantic_state::STATE_NAMES.join(", ")
                    ))
                }
            },
            Err(error) => return error,
        };
        let target = match acquire_semantic_target(&self.state, &args, "set_state").await {
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
        let outcome = run_observed(&args, pid, "set_state", move || {
            let ptr = task_element.as_ptr();
            let web = super::type_text::target_in_web_area(
                pid,
                Some((ptr, Some(element_index))),
                Some(window_id),
            );
            unsafe { set_state_blocking(ptr as AXUIElementRef, pid, requested, web) }
        })
        .await;
        drop(target);
        let (outcome, changes) = match outcome {
            Ok(done) => done,
            Err(error) => return error,
        };
        semantic_result(
            "set_state",
            requested.as_str(),
            &describe_state(requested),
            pid,
            window_id,
            element_index,
            outcome,
            &changes.result_suffix(),
        )
    }
}

fn describe_state(requested: RequestedState) -> String {
    match requested {
        RequestedState::Toggled => "toggled".into(),
        other => other.as_str().into(),
    }
}

// ── Blocking implementation ──────────────────────────────────────────────────

/// How long a mechanism's effect may take to show in the read-back.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(600);
const SETTLE_POLL: Duration = Duration::from_millis(25);
/// A read-back that reached the state is read again this much later, so a
/// control that bounces back (an app that reverts a write) is not reported
/// verified.
const SETTLE_STABILITY: Duration = Duration::from_millis(100);

/// What a semantic mutation did, for [`semantic_result`].
#[derive(Debug)]
pub(super) enum SemanticOutcome {
    Refused(Refusal),
    AlreadySatisfied,
    Verified(Mechanism),
    Unverified(Mechanism),
    Failed(Failure, Option<Mechanism>),
}

/// # Safety
///
/// `element` must stay retained for the call.
unsafe fn set_state_blocking(
    element: AXUIElementRef,
    pid: i32,
    requested: RequestedState,
    web: bool,
) -> SemanticOutcome {
    let plan = match requested {
        RequestedState::Checked | RequestedState::Unchecked | RequestedState::Toggled => {
            semantic_state::plan_check(requested, &toggle_facts(element))
        }
        RequestedState::Expanded | RequestedState::Collapsed => {
            semantic_state::plan_disclosure(requested, &disclosure_facts(element, requested))
        }
        RequestedState::Focused => semantic_state::plan_focus(&focus_facts(element, pid)),
    };
    let read = |element: AXUIElementRef| read_state(element, pid, requested);
    execute_plan(
        element,
        trusted_for_surface(plan, web),
        read,
        |element, m, d| attempt(element, m, d, requested),
    )
}

/// Web content echoes accessibility attribute writes without the renderer
/// seeing them (the trap `set_value` documents), so only routes the renderer
/// performs (press, the disclosure action, focus) are used there.
fn trusted_for_surface(plan: Plan, web: bool) -> Plan {
    match plan {
        Plan::Run {
            desired,
            mechanisms,
        } if web => {
            let mechanisms: Vec<_> = mechanisms
                .into_iter()
                .filter(|m| {
                    matches!(
                        m,
                        Mechanism::Press | Mechanism::DisclosureAction | Mechanism::WriteFocused
                    )
                })
                .collect();
            if mechanisms.is_empty() {
                Plan::Refuse(Refusal {
                    code: "no_state_route",
                    reason: "in web content only a press, the element's own expand/collapse \
                             action, or focus is trusted, and the element offers none from \
                             its current state; nothing was sent. Use click instead."
                        .into(),
                })
            } else {
                Plan::Run {
                    desired,
                    mechanisms,
                }
            }
        }
        plan => plan,
    }
}

/// Run a plan's mechanisms in order, reading the state back after each.
///
/// # Safety
///
/// `element` must stay retained for the call.
pub(super) unsafe fn execute_plan(
    element: AXUIElementRef,
    plan: Plan,
    read: impl Fn(AXUIElementRef) -> Option<bool>,
    attempt: impl Fn(AXUIElementRef, Mechanism, bool) -> AttemptResult,
) -> SemanticOutcome {
    let (desired, mechanisms) = match plan {
        Plan::AlreadySatisfied => return SemanticOutcome::AlreadySatisfied,
        Plan::Refuse(refusal) => return SemanticOutcome::Refused(refusal),
        Plan::Run {
            desired,
            mechanisms,
        } => (desired, mechanisms),
    };
    let mut ignored_write = false;
    for mechanism in mechanisms {
        let result = attempt(element, mechanism, desired);
        let observed = match result {
            // Nothing reached the element: one read is enough.
            AttemptResult::Stale => None,
            AttemptResult::Unsupported if mechanism.idempotent() => read(element),
            _ => settle(|| read(element), desired),
        };
        match after_attempt(mechanism, result, desired, observed) {
            Next::Verified => return SemanticOutcome::Verified(mechanism),
            Next::Unverified => return SemanticOutcome::Unverified(mechanism),
            Next::TryNext {
                ignored_write: ignored,
            } => ignored_write |= ignored,
            Next::Stop(failure) => return SemanticOutcome::Failed(failure, Some(mechanism)),
        }
    }
    SemanticOutcome::Failed(semantic_state::exhausted(ignored_write), None)
}

/// Poll `read` until it shows `desired` stably or [`SETTLE_TIMEOUT`] passes;
/// returns the last value read.
pub(super) fn settle(read: impl Fn() -> Option<bool>, desired: bool) -> Option<bool> {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let now = read();
        if now == Some(desired) {
            std::thread::sleep(SETTLE_STABILITY);
            let again = read();
            if again == Some(desired) || Instant::now() >= deadline {
                return again;
            }
        } else if Instant::now() >= deadline {
            return now;
        } else {
            std::thread::sleep(SETTLE_POLL);
        }
    }
}

unsafe fn role_of(element: AXUIElementRef) -> String {
    copy_string_attr(element, "AXRole").unwrap_or_default()
}

unsafe fn checked_state(element: AXUIElementRef) -> Option<bool> {
    copy_stringish_attr(element, "AXValue")
        .and_then(|value| semantic_state::parse_checked_value(&value.state_value))
}

unsafe fn expanded_state(element: AXUIElementRef) -> Option<bool> {
    let role = copy_string_attr(element, "AXRole");
    let value = copy_stringish_attr(element, "AXValue").map(|value| value.state_value);
    semantic_state::disclosure_state(
        copy_binary_attr(element, "AXExpanded"),
        copy_binary_attr(element, "AXDisclosing"),
        role.as_deref(),
        value.as_deref(),
    )
}

unsafe fn read_state(element: AXUIElementRef, pid: i32, requested: RequestedState) -> Option<bool> {
    match requested {
        RequestedState::Checked | RequestedState::Unchecked | RequestedState::Toggled => {
            checked_state(element)
        }
        RequestedState::Expanded | RequestedState::Collapsed => expanded_state(element),
        RequestedState::Focused => Some(crate::input::ax_actions::element_has_keyboard_focus(
            pid,
            element as usize,
        )),
    }
}

unsafe fn advertises(element: AXUIElementRef, action: &str) -> bool {
    copy_action_names(element).iter().any(|name| name == action)
}

unsafe fn toggle_facts(element: AXUIElementRef) -> ToggleFacts {
    ToggleFacts {
        role: role_of(element),
        checked: checked_state(element),
        value_settable: is_attribute_settable(element, "AXValue"),
        press_advertised: advertises(element, "AXPress"),
        enabled: copy_bool_attr(element, "AXEnabled"),
    }
}

unsafe fn disclosure_facts(element: AXUIElementRef, requested: RequestedState) -> DisclosureFacts {
    let actions = copy_action_names(element);
    let direction = disclosure_action(requested);
    DisclosureFacts {
        expanded: expanded_state(element),
        direction_action_advertised: actions.iter().any(|name| name == direction),
        expanded_settable: is_attribute_settable(element, "AXExpanded"),
        disclosing_settable: is_attribute_settable(element, "AXDisclosing"),
        press_advertised: actions.iter().any(|name| name == "AXPress"),
        enabled: copy_bool_attr(element, "AXEnabled"),
    }
}

unsafe fn focus_facts(element: AXUIElementRef, pid: i32) -> FocusFacts {
    FocusFacts {
        focused: crate::input::ax_actions::element_has_keyboard_focus(pid, element as usize),
        focused_settable: is_attribute_settable(element, "AXFocused"),
        enabled: copy_bool_attr(element, "AXEnabled"),
    }
}

fn disclosure_action(requested: RequestedState) -> &'static str {
    if requested == RequestedState::Collapsed {
        "AXCollapse"
    } else {
        "AXExpand"
    }
}

unsafe fn attempt(
    element: AXUIElementRef,
    mechanism: Mechanism,
    desired: bool,
    requested: RequestedState,
) -> AttemptResult {
    let err = match mechanism {
        Mechanism::WriteValue => {
            set_number_attr(element, "AXValue", if desired { 1.0 } else { 0.0 })
        }
        Mechanism::DisclosureAction => perform_action(element, disclosure_action(requested)),
        Mechanism::WriteExpanded => set_bool_attr(element, "AXExpanded", desired),
        Mechanism::WriteDisclosing => set_bool_attr(element, "AXDisclosing", desired),
        Mechanism::Press => perform_action(element, "AXPress"),
        Mechanism::WriteFocused => set_bool_attr(element, "AXFocused", true),
        Mechanism::ScrollToVisible => perform_action(element, "AXScrollToVisible"),
    };
    classify_ax_error(mechanism, err)
}

/// What an AX return code says about delivery.
///
/// `-25202` (invalid element) means the element is gone: nothing reached it.
/// Unsupported attribute/action, illegal argument and not-implemented mean the
/// element refused this mechanism. An advertised press is different: AppKit
/// apps return `-25205`/`-25206` from presses they performed (#3835), so any
/// press error other than a dead element is uncertain. Everything else (a
/// generic failure, a timeout) may have acted.
pub(super) fn classify_ax_error(mechanism: Mechanism, err: AXError) -> AttemptResult {
    use crate::ax::bindings::{
        kAXErrorActionUnsupported, kAXErrorAttributeUnsupported, kAXErrorInvalidUIElement,
        kAXErrorSuccess,
    };
    const ILLEGAL_ARGUMENT: AXError = -25201;
    const NOT_IMPLEMENTED: AXError = -25208;
    if err == kAXErrorSuccess {
        AttemptResult::Ok
    } else if err == kAXErrorInvalidUIElement {
        AttemptResult::Stale
    } else if mechanism != Mechanism::Press
        && [
            kAXErrorAttributeUnsupported,
            kAXErrorActionUnsupported,
            ILLEGAL_ARGUMENT,
            NOT_IMPLEMENTED,
        ]
        .contains(&err)
    {
        AttemptResult::Unsupported
    } else {
        AttemptResult::Uncertain
    }
}

// ── Shared prelude and result shaping ────────────────────────────────────────

/// One element resolved from the call's `element_token`, with the per-pid
/// background mutation lease held. Keep it alive until the mutation and its
/// read-back are done.
pub(super) struct SemanticTarget {
    pub pid: i32,
    pub window_id: u32,
    pub element_index: usize,
    pub element: crate::ax::snapshot::RetainedElement,
    _lease: super::BackgroundMutationLease,
}

/// Resolve the token through the shared snapshot resolver (the same one every
/// token tool uses, so its staleness and identity checks apply here), then
/// take the per-pid mutation lease and re-prove the element still belongs to
/// the exact window, as `set_value` does.
pub(super) async fn acquire_semantic_target(
    state: &ToolState,
    args: &Value,
    tool: &str,
) -> Result<SemanticTarget, ToolResult> {
    let pid = super::target_pid(state, args)?;
    let (element_index, window_id, element) = match state.snapshots.resolve(pid, args)? {
        cua_driver_core::element_token::ResolvedElement::None => {
            return Err(ToolResult::error(format!(
                "{tool} requires element_token to address the target element."
            )))
        }
        cua_driver_core::element_token::ResolvedElement::Element {
            window_id,
            element_index,
            element,
        } => match u32::try_from(window_id) {
            Ok(window_id) => (element_index, window_id, element),
            Err(_) => return Err(ToolResult::error("window_id is out of range for macOS.")),
        },
    };
    let lease = super::gate_background_window_action(
        pid,
        window_id,
        Some(element.as_ptr()),
        cua_driver_core::background_input::BackgroundAction::AxSemantic,
    )
    .await?;
    Ok(SemanticTarget {
        pid,
        window_id,
        element_index,
        element,
        _lease: lease,
    })
}

/// Move the session's agent cursor overlay to the element, as `set_value`
/// does. Only the overlay moves; the user's pointer does not.
pub(super) async fn point_cursor_at(
    state: &ToolState,
    args: &Value,
    window_id: u32,
    element: &crate::ax::snapshot::RetainedElement,
) {
    let cursor_key = super::cursor_tools::resolve_cursor_key(args);
    let guard = element.clone();
    let Ok((Some((x, y)), rect)) = tokio::task::spawn_blocking(move || unsafe {
        let element = guard.as_ptr() as AXUIElementRef;
        (
            crate::ax::bindings::element_screen_center(element),
            crate::ax::bindings::element_screen_rect(element),
        )
    })
    .await
    else {
        return;
    };
    crate::cursor::overlay::send_command(
        cursor_key.clone(),
        cursor_overlay::OverlayCommand::PinAbove(window_id as u64),
    );
    crate::cursor::overlay::animate_cursor_to_target(cursor_key.clone(), x, y, rect).await;
    state.cursor_registry.update_position(&cursor_key, x, y);
}

/// Run a blocking AX mutation with reflex activations suppressed and the
/// post-action window-change report, as `set_value` does.
pub(super) async fn run_observed<R: Send + 'static>(
    args: &Value,
    pid: i32,
    origin: &'static str,
    work: impl FnOnce() -> R + Send + 'static,
) -> Result<(R, crate::window_change_detector::Changes), ToolResult> {
    let prior_front = crate::apps::frontmost_pid();
    let snapshot = super::element_action_snapshot(super::ax_semantic_lane(args), prior_front, pid);
    let result =
        crate::focus_guard::with_focus_suppressed(Some(pid), prior_front, origin, || async move {
            tokio::task::spawn_blocking(work).await
        })
        .await;
    let changes = super::finish_window_observation(snapshot).await;
    match result {
        Ok(result) => Ok((result, changes)),
        Err(error) => Err(ToolResult::error(format!("{origin} task error: {error}"))),
    }
}

/// The route a mechanism publishes: a value write or an AX action.
fn transport(mechanism: Mechanism) -> cua_driver_core::action_record::ActionTransport {
    use cua_driver_core::action_record::ActionTransport;
    match mechanism {
        Mechanism::WriteValue => ActionTransport::MacosAxValue,
        _ => ActionTransport::MacosAxAction,
    }
}

fn record(
    effect: cua_driver_core::action_record::ActionEffect,
    mechanism: Option<Mechanism>,
    evidence: Option<String>,
) -> cua_driver_core::action_record::ActionExecutionRecord {
    use cua_driver_core::action_record::{
        ActionEvidence, ActionExecutionRecord, ActualDelivery, EvidenceKind, RequestedDelivery,
    };
    let mut builder = ActionExecutionRecord::builder(
        effect,
        mechanism.map_or(
            cua_driver_core::action_record::ActionTransport::MacosAxAction,
            transport,
        ),
        RequestedDelivery::Background,
    );
    // Nothing is delivered when the state already held.
    if mechanism.is_some() {
        builder = builder.actual_delivery(ActualDelivery::Background);
    }
    if let Some(detail) = evidence {
        builder = builder.evidence(ActionEvidence {
            kind: EvidenceKind::AccessibilityReadback,
            detail,
        });
    }
    builder.build().expect("semantic state record is valid")
}

/// Shape a [`SemanticOutcome`] into the tool result. `label` names the state
/// in prose ("checked", "cleared", "in view"); values are never echoed.
#[allow(clippy::too_many_arguments)]
pub(super) fn semantic_result(
    tool: &str,
    state: &str,
    label: &str,
    pid: i32,
    window_id: u32,
    element_index: usize,
    outcome: SemanticOutcome,
    changes_suffix: &str,
) -> ToolResult {
    use cua_driver_core::action_record::ActionEffect;
    match outcome {
        SemanticOutcome::AlreadySatisfied => ToolResult::text(format!(
            "✅ [{element_index}] is already {label}; nothing was sent.{changes_suffix}"
        ))
        .with_structured(serde_json::json!({
            "path": "ax", "verified": true, "effect": "confirmed", "state": state, "changed": false,
        }))
        .with_action_record(record(
            ActionEffect::Confirmed,
            None,
            Some(format!("read back {label} before acting")),
        )),
        SemanticOutcome::Verified(mechanism) => ToolResult::text(format!(
            "✅ [{element_index}] is now {label} (via {}; read back).{changes_suffix}",
            mechanism.as_str()
        ))
        .with_structured(serde_json::json!({
            "path": "ax", "verified": true, "effect": "confirmed", "state": state,
            "changed": true, "mechanism": mechanism.as_str(),
        }))
        .with_action_record(record(
            ActionEffect::Confirmed,
            Some(mechanism),
            Some(format!("read back {label} after {}", mechanism.as_str())),
        )),
        SemanticOutcome::Unverified(mechanism) => ToolResult::text(format!(
            "📨 Sent {} to [{element_index}] (unverified): the element's state cannot be read \
             back, so {label} is not confirmed. Check it in a fresh get_window_state.\
             {changes_suffix}",
            mechanism.as_str()
        ))
        .with_structured(serde_json::json!({
            "path": "ax", "verified": false, "effect": "unverifiable", "state": state,
            "mechanism": mechanism.as_str(),
        }))
        .with_action_record(record(ActionEffect::Unverifiable, Some(mechanism), None)),
        SemanticOutcome::Refused(refusal) => ToolResult::error(format!(
            "{tool} refused ({}): {}",
            refusal.code, refusal.reason
        ))
        .with_structured(serde_json::json!({
            "code": refusal.code, "effect": "refused", "path": "ax", "state": state,
            "pid": pid, "window_id": window_id, "reason": refusal.reason,
        })),
        SemanticOutcome::Failed(failure, mechanism) => {
            let reason = failure_reason(failure, label);
            ToolResult::error(format!(
                "{tool} did not reach {label} ({}): {reason}{changes_suffix}",
                failure.code()
            ))
            .with_structured(serde_json::json!({
                "code": failure.code(), "effect": failure.effect(), "path": "ax", "state": state,
                "pid": pid, "window_id": window_id, "reason": reason,
                "may_have_delivered": failure.may_have_delivered(),
                "mechanism": mechanism.map(Mechanism::as_str),
            }))
        }
    }
}

fn failure_reason(failure: Failure, label: &str) -> String {
    match failure {
        Failure::Stale => "the element no longer exists; nothing was sent. Take a fresh \
                           get_window_state and retry with its token."
            .into(),
        Failure::NoRoute => "the element refused every accessibility route; nothing was sent. \
                             Use click instead."
            .into(),
        Failure::WriteIgnored => format!(
            "the app accepted the accessibility write but the element is still not {label}; \
             nothing changed. Use click or the keyboard instead."
        ),
        Failure::NotReached => format!(
            "the action ran but the element still reads as not {label}. Read the element \
             before repeating it: a second press toggles again."
        ),
        Failure::Uncertain => "the app returned an error that may or may not mean it acted. Read \
                               the element's state before any retry."
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ax_errors_split_into_stale_unsupported_and_uncertain() {
        assert_eq!(
            classify_ax_error(Mechanism::WriteValue, 0),
            AttemptResult::Ok
        );
        assert_eq!(
            classify_ax_error(Mechanism::WriteExpanded, -25202),
            AttemptResult::Stale
        );
        for unsupported in [-25201, -25205, -25206, -25208] {
            assert_eq!(
                classify_ax_error(Mechanism::DisclosureAction, unsupported),
                AttemptResult::Unsupported,
                "{unsupported}"
            );
        }
        for uncertain in [-25200, -25204] {
            assert_eq!(
                classify_ax_error(Mechanism::WriteValue, uncertain),
                AttemptResult::Uncertain,
                "{uncertain}"
            );
        }
    }

    /// #3835: an advertised press that returns -25205 may have acted, so it
    /// never falls through to another mechanism.
    #[test]
    fn a_press_error_is_uncertain_unless_the_element_is_gone() {
        for err in [-25200, -25201, -25205, -25206] {
            assert_eq!(
                classify_ax_error(Mechanism::Press, err),
                AttemptResult::Uncertain,
                "{err}"
            );
        }
        assert_eq!(
            classify_ax_error(Mechanism::Press, -25202),
            AttemptResult::Stale
        );
    }

    #[test]
    fn web_content_keeps_only_renderer_routes() {
        let plan = Plan::Run {
            desired: true,
            mechanisms: vec![Mechanism::WriteValue, Mechanism::Press],
        };
        assert_eq!(
            trusted_for_surface(plan.clone(), true),
            Plan::Run {
                desired: true,
                mechanisms: vec![Mechanism::Press],
            }
        );
        assert_eq!(trusted_for_surface(plan.clone(), false), plan);
        let writes_only = Plan::Run {
            desired: true,
            mechanisms: vec![Mechanism::WriteExpanded],
        };
        assert!(matches!(
            trusted_for_surface(writes_only, true),
            Plan::Refuse(Refusal {
                code: "no_state_route",
                ..
            })
        ));
    }

    /// The executor over a scripted element: an ignored write falls through
    /// to the press, a press that leaves the opposite state stops.
    #[test]
    fn executor_follows_the_core_decisions() {
        use std::cell::{Cell, RefCell};
        let state = Cell::new(Some(false));
        let calls = RefCell::new(Vec::new());
        let plan = Plan::Run {
            desired: true,
            mechanisms: vec![Mechanism::WriteValue, Mechanism::Press],
        };
        let outcome = unsafe {
            execute_plan(
                std::ptr::null_mut(),
                plan.clone(),
                |_| state.get(),
                |_, mechanism, desired| {
                    calls.borrow_mut().push(mechanism);
                    if mechanism == Mechanism::Press {
                        state.set(Some(desired));
                    }
                    AttemptResult::Ok
                },
            )
        };
        assert!(matches!(
            outcome,
            SemanticOutcome::Verified(Mechanism::Press)
        ));
        assert_eq!(*calls.borrow(), [Mechanism::WriteValue, Mechanism::Press]);

        state.set(Some(false));
        calls.borrow_mut().clear();
        let outcome = unsafe {
            execute_plan(
                std::ptr::null_mut(),
                plan,
                |_| state.get(),
                |_, mechanism, _| {
                    calls.borrow_mut().push(mechanism);
                    if mechanism == Mechanism::WriteValue {
                        AttemptResult::Uncertain
                    } else {
                        AttemptResult::Ok
                    }
                },
            )
        };
        assert!(matches!(
            outcome,
            SemanticOutcome::Failed(Failure::Uncertain, Some(Mechanism::WriteValue))
        ));
        assert_eq!(
            *calls.borrow(),
            [Mechanism::WriteValue],
            "nothing runs after an uncertain write"
        );
    }
}
