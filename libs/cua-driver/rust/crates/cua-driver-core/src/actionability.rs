//! Actionability: whether a resolved, identity-confirmed element can take an
//! action right now, and how long to wait for it to become able to.
//!
//! Identity (`crate::ref_identity`) answers "is this still the element the
//! caller saw?". This module answers the next question: is it in a state
//! where acting on it means something? A button that is disabled while a
//! form validates, a sheet still animating in at zero size, or a row sliding
//! under the pointer will swallow an action or put it somewhere else, and the
//! caller sees "sent" for something that did nothing. So each action kind
//! names the checks it needs ([`ActionKind::requirements`]), the platform
//! gathers live facts ([`ActionabilityFacts`]), and [`evaluate`] decides:
//! act, wait (a transient check failed), or refuse (a terminal one did).
//! [`Waiter`] polls the transient checks against a deadline and turns the
//! last failure into a structured `actionability_timeout`.
//!
//! The requirements table, the check vocabulary and the deadline budgets
//! (5000 ms, plus 30 ms per character for typing) are ported from
//! agent-desktop (Apache-2.0, `crates/core/src/actionability/requirements.rs`,
//! `gates.rs`, `crates/core/src/wait/deadline.rs`,
//! `crates/core/src/commands/type_text.rs`). Two deliberate differences:
//!
//! - Unknown evidence never blocks. agent-desktop reports some unreadable
//!   facts as failing checks; here a read that failed (or an app that does
//!   not report an attribute) passes, as agent-desktop does for occlusion.
//!   Only positive evidence (`AXEnabled` false, a zero-size frame, two
//!   different live frames) holds an action back. The driver acted on such
//!   elements before this gate existed; an unanswerable check must not turn
//!   them into timeouts.
//! - Window intersection and stability are pointer-only. A semantic AX
//!   action (`AXPress`, an `AXValue` write) reaches the element wherever it
//!   is drawn, including a row scrolled out of its list; only a routed
//!   pointer event needs the element's centre inside its window and still.
//!
//! This module performs no I/O; the platform shell gathers the facts.

use crate::protocol::ToolResult;
use crate::ref_identity::Field;
use std::time::{Duration, Instant};

/// Refusal code when a transient check still failed at the deadline.
/// Nothing was sent, so retrying is safe.
pub const ACTIONABILITY_TIMEOUT: &str = "actionability_timeout";
/// Terminal refusal: the element cannot take text or a value at all.
pub const ELEMENT_NOT_EDITABLE: &str = "element_not_editable";
/// Terminal refusal: the element does not advertise the AX action the tool
/// would perform, and the tool has no other route.
pub const ACTION_NOT_SUPPORTED: &str = "action_not_supported";

/// Default wait for a target to become actionable.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Extra wait per character of `type_text`, so a long payload's budget is not
/// sized for a single click.
pub const TYPE_TEXT_PER_CHAR_MS: u64 = 30;
/// Longest wait a caller may ask for; longer requests are clamped.
pub const MAX_TIMEOUT_MS: u64 = 60_000;

/// Points of slack when comparing two frames: AX frames are fractional and
/// some apps round them differently between reads.
const FRAME_TOLERANCE: f64 = 1.0;

/// First re-sample interval, then the cap the backoff grows to. One display
/// frame first: a frame that differs from the snapshot usually belongs to a
/// window that moved and is now still, so the second sample settles it at
/// once; a disabled control waiting on validation is polled at most ten
/// times a second.
const FIRST_POLL: Duration = Duration::from_millis(16);
const MAX_POLL: Duration = Duration::from_millis(100);

/// How old the last observation may be when the mutation lease is taken
/// without queuing before it is read again under the lease. A call that
/// queued behind another mutation of the same app always re-checks (that
/// mutation may have changed the element); one that took a free lease right
/// after observing has nothing new to catch, and skipping the re-read keeps
/// an already-actionable target's cost to the one identity read it paid
/// anyway (an unnamed Finder cell's ancestry climb is ~7 ms).
const RECHECK_AFTER: Duration = Duration::from_millis(50);

/// What the tool is about to do to the element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Click,
    DoubleClick,
    RightClick,
    /// Reveal-only actions (`AXShowAlternateUI`, pointer hover).
    Hover,
    SetValue,
    TypeText,
    Scroll,
    /// check / uncheck / toggle.
    Toggle,
    /// expand / collapse.
    Expand,
    Clear,
    Focus,
    ScrollTo,
}

/// Which checks an action needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Requirements {
    pub visible: bool,
    /// Applied only to pointer delivery (see [`Delivery`]).
    pub stable: bool,
    pub enabled: bool,
    pub editable: bool,
}

impl ActionKind {
    /// agent-desktop's requirements table, by action.
    pub fn requirements(self) -> Requirements {
        let (visible, stable, enabled, editable) = match self {
            Self::Click | Self::DoubleClick | Self::RightClick => (true, true, true, false),
            Self::Hover => (true, true, false, false),
            Self::SetValue | Self::TypeText | Self::Clear => (true, false, true, true),
            Self::Toggle | Self::Expand | Self::Scroll => (true, true, true, false),
            Self::ScrollTo => (false, true, false, false),
            Self::Focus => (false, false, false, false),
        };
        Requirements {
            visible,
            stable,
            enabled,
            editable,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::DoubleClick => "double_click",
            Self::RightClick => "right_click",
            Self::Hover => "hover",
            Self::SetValue => "set_value",
            Self::TypeText => "type_text",
            Self::Scroll => "scroll",
            Self::Toggle => "toggle",
            Self::Expand => "expand",
            Self::Clear => "clear",
            Self::Focus => "focus",
            Self::ScrollTo => "scroll_to",
        }
    }
}

/// How the action reaches the element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// An AX action or attribute write on the element itself.
    Semantic,
    /// A routed pointer event at the element's centre.
    Pointer,
}

/// One gated action: its kind, its route, and what else the gate must know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionRequest {
    pub kind: ActionKind,
    pub delivery: Delivery,
    /// `delivery_mode:"foreground"`. The app is activated only inside the
    /// actuator, after this gate, and activation can enable menu items that
    /// are disabled while the app is in the background; so a foreground
    /// menu item's `AXEnabled` is left to the actuator's own post-activation
    /// check instead of being waited on here.
    pub foreground: bool,
    /// The AX action the tool performs with no other route. When set and the
    /// element positively does not advertise it, the action is refused as
    /// [`ACTION_NOT_SUPPORTED`]. Tools with fallbacks (click's container
    /// selection, double_click's pointer route) leave it unset.
    pub required_ax_action: Option<&'static str>,
}

impl ActionRequest {
    pub fn semantic(kind: ActionKind) -> Self {
        Self {
            kind,
            delivery: Delivery::Semantic,
            foreground: false,
            required_ax_action: None,
        }
    }

    pub fn pointer(kind: ActionKind) -> Self {
        Self {
            delivery: Delivery::Pointer,
            ..Self::semantic(kind)
        }
    }

    pub fn foreground(mut self, foreground: bool) -> Self {
        self.foreground = foreground;
        self
    }

    pub fn requiring(mut self, ax_action: &'static str) -> Self {
        self.required_ax_action = Some(ax_action);
        self
    }

    /// Whether the gatherer must read the window's frame (pointer delivery
    /// of an action that needs visibility).
    pub fn reads_window_frame(&self) -> bool {
        self.delivery == Delivery::Pointer && self.kind.requirements().visible
    }

    /// Whether the gatherer must read settability. Only for an editing
    /// action on a role that cannot hold text ([`is_non_text_role`]): a text
    /// field, a web group or a terminal grid never pays for the reads, and
    /// is never refused as not editable.
    pub fn reads_editability(&self, role: &Field<String>) -> bool {
        self.kind.requirements().editable
            && role
                .known()
                .is_some_and(|role| is_non_text_role(self.kind, role))
    }

    /// Whether the gatherer must read the advertised AX actions.
    pub fn reads_actions(&self) -> bool {
        self.required_ax_action.is_some()
    }
}

/// Roles that display or trigger rather than hold text, per editing action.
/// An editing action on one of these whose `AXValue` and `AXSelectedText`
/// are both positively not settable has nowhere to go: `set_value` would
/// fail its write and `type_text` would send keys to whatever else holds
/// focus. Denylists, so an unfamiliar role (a contenteditable web `AXGroup`,
/// a terminal's `AXTextArea`) is never refused.
///
/// `type_text` focuses the element and falls back to key events, which reach
/// more than `AXValue` does: a static text or image inside a contenteditable
/// region hands focus to its editor, and a list row takes type-to-select. So
/// its list holds only controls keys can never edit.
pub fn is_non_text_role(kind: ActionKind, role: &str) -> bool {
    let control = matches!(
        role,
        "AXButton"
            | "AXMenuItem"
            | "AXMenuBarItem"
            | "AXMenuButton"
            | "AXCheckBox"
            | "AXRadioButton"
            | "AXDisclosureTriangle"
            | "AXToolbar"
    );
    match kind {
        ActionKind::TypeText => control,
        _ => {
            control
                || matches!(
                    role,
                    "AXStaticText" | "AXHeading" | "AXImage" | "AXLink" | "AXRow"
                )
        }
    }
}

/// Menu items publish no usable geometry while their menu is closed, yet
/// take `AXPress` (agent-desktop's closed-menu exception).
fn is_menu_role(role: &str) -> bool {
    matches!(role, "AXMenuItem" | "AXMenuBarItem" | "AXMenu")
}

/// Live facts about the element, gathered right before the decision.
/// `role`, `frame` and `enabled` come from the identity re-read; the rest is
/// read only when the request needs it (otherwise `Unknown`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActionabilityFacts {
    pub role: Field<String>,
    /// Screen frame `[x, y, w, h]`.
    pub frame: Field<[f64; 4]>,
    pub enabled: Field<bool>,
    /// The target window's screen frame ([`ActionRequest::reads_window_frame`]).
    pub window_frame: Field<[f64; 4]>,
    /// `AXUIElementIsAttributeSettable(AXValue)`
    /// ([`ActionRequest::reads_editability`]). An unsupported attribute
    /// reads as `Known(false)`: it is positively not settable.
    pub value_settable: Field<bool>,
    /// `AXUIElementIsAttributeSettable(AXSelectedText)`.
    pub selected_text_settable: Field<bool>,
    /// Advertised AX actions ([`ActionRequest::reads_actions`]).
    pub actions: Field<Vec<String>>,
}

/// One check of the battery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Visible,
    Enabled,
    Stable,
    Editable,
    SupportedAction,
}

impl Check {
    pub fn name(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Enabled => "enabled",
            Self::Stable => "stable",
            Self::Editable => "editable",
            Self::SupportedAction => "supported_action",
        }
    }
}

/// The frame a stability check compares the live one against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameReference {
    pub frame: [f64; 4],
    /// `false` for the snapshot-time frame, `true` for an earlier live
    /// sample. A live frame that differs from the snapshot proves only that
    /// the element moved at some point (the window was dragged, the list
    /// scrolled); one that differs from the previous live sample proves it
    /// is moving now.
    pub live: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ready,
    /// Every check passed except stability, which has no live comparison
    /// yet: sample once more if time allows, otherwise act.
    Resample,
    /// A transient check failed on positive evidence.
    Wait {
        check: Check,
        reason: &'static str,
    },
    /// A terminal check failed: waiting cannot help.
    Refuse {
        check: Check,
        code: &'static str,
        reason: &'static str,
    },
}

/// Decide one observation. Terminal checks run first so an action that can
/// never succeed fails fast instead of waiting out a transient one.
pub fn evaluate(
    request: &ActionRequest,
    facts: &ActionabilityFacts,
    reference: Option<FrameReference>,
) -> Verdict {
    let needs = request.kind.requirements();
    let role = facts.role.known().map(String::as_str);
    if let (Some(required), Field::Known(actions)) = (request.required_ax_action, &facts.actions) {
        if !actions.iter().any(|action| action == required) {
            return Verdict::Refuse {
                check: Check::SupportedAction,
                code: ACTION_NOT_SUPPORTED,
                reason: "the element does not advertise the accessibility action this tool \
                         performs",
            };
        }
    }
    if needs.editable
        && role.is_some_and(|role| is_non_text_role(request.kind, role))
        && facts.value_settable == Field::Known(false)
        && facts.selected_text_settable == Field::Known(false)
    {
        return Verdict::Refuse {
            check: Check::Editable,
            code: ELEMENT_NOT_EDITABLE,
            reason: "the element's role holds no text and its value is not settable",
        };
    }
    if needs.visible {
        if let Field::Known([_, _, w, h]) = facts.frame {
            if (w <= 0.0 || h <= 0.0) && !role.is_some_and(is_menu_role) {
                return Verdict::Wait {
                    check: Check::Visible,
                    reason: "the element has a zero-size frame (hidden, collapsed or still \
                             appearing)",
                };
            }
        }
        if request.delivery == Delivery::Pointer {
            if let (Field::Known(frame), Field::Known(window)) = (&facts.frame, &facts.window_frame)
            {
                if !center_inside(*frame, *window) {
                    return Verdict::Wait {
                        check: Check::Visible,
                        reason: "the element's centre is outside its window, where a pointer \
                                 event cannot reach it",
                    };
                }
            }
        }
    }
    if needs.enabled && facts.enabled == Field::Known(false) {
        let deferred = request.foreground && role.is_some_and(is_menu_role);
        if !deferred {
            return Verdict::Wait {
                check: Check::Enabled,
                reason: "the element reports AXEnabled=false",
            };
        }
    }
    if needs.stable && request.delivery == Delivery::Pointer {
        if let (Some(reference), Field::Known(frame)) = (reference, &facts.frame) {
            if !frames_agree(reference.frame, *frame) {
                return if reference.live {
                    Verdict::Wait {
                        check: Check::Stable,
                        reason: "the element's frame is still changing between samples",
                    }
                } else {
                    Verdict::Resample
                };
            }
        }
    }
    Verdict::Ready
}

/// The wait budget for one action. `requested` is the caller's `timeout_ms`;
/// `None` takes the default (plus [`TYPE_TEXT_PER_CHAR_MS`] per character of
/// typed text). Clamped to [`MAX_TIMEOUT_MS`]. `0` means one check, no wait.
pub fn timeout_for(kind: ActionKind, requested: Option<u64>, typed_chars: usize) -> Duration {
    let ms = requested.unwrap_or_else(|| {
        let per_char = if kind == ActionKind::TypeText {
            (typed_chars as u64).saturating_mul(TYPE_TEXT_PER_CHAR_MS)
        } else {
            0
        };
        DEFAULT_TIMEOUT_MS.saturating_add(per_char)
    });
    Duration::from_millis(ms.min(MAX_TIMEOUT_MS))
}

/// Parse the optional `timeout_ms` argument into [`timeout_for`]'s budget.
/// A present value that is not a non-negative integer is refused as
/// `invalid_arguments` before anything is read or sent.
pub fn timeout_from_args(
    args: &serde_json::Value,
    kind: ActionKind,
    typed_chars: usize,
) -> Result<Duration, ToolResult> {
    let requested = match args.get("timeout_ms") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(ms) => Some(ms),
            None => {
                return Err(ToolResult::error(format!(
                    "{}: timeout_ms must be a non-negative integer number of milliseconds.",
                    kind.name()
                ))
                .with_structured(serde_json::json!({ "code": "invalid_arguments" })))
            }
        },
    };
    Ok(timeout_for(kind, requested, typed_chars))
}

/// What the platform loop does next.
#[derive(Debug)]
pub enum Step {
    Act,
    /// Sleep this long (holding no mutation lease), then observe again.
    Sleep(Duration),
    Refuse(Box<ToolResult>),
}

/// The polling state of one gated action: deadline, backoff, and the last
/// live frame for the stability comparison. The platform loop feeds it one
/// [`ActionabilityFacts`] per observation; the same waiter re-checks after
/// the mutation lease is acquired, so the whole action shares one deadline.
#[derive(Clone, Debug)]
pub struct Waiter {
    request: ActionRequest,
    started: Instant,
    deadline: Instant,
    timeout: Duration,
    reference: Option<FrameReference>,
    polls: u32,
    token: Option<String>,
    observed_at: Option<Instant>,
}

impl Waiter {
    /// `snapshot_frame` is the frame the snapshot recorded: when the live
    /// frame matches it the element has not moved since the caller looked,
    /// and stability needs no second sample.
    pub fn new(
        request: ActionRequest,
        timeout: Duration,
        snapshot_frame: Option<[f64; 4]>,
        token: Option<&str>,
        now: Instant,
    ) -> Self {
        Self {
            request,
            started: now,
            deadline: now + timeout,
            timeout,
            reference: snapshot_frame.map(|frame| FrameReference { frame, live: false }),
            polls: 0,
            token: token.map(str::to_owned),
            observed_at: None,
        }
    }

    /// Whether the element must be observed again now that the mutation
    /// lease is held: always after `queued` behind another mutation of the
    /// app, otherwise only when the last observation is older than
    /// [`RECHECK_AFTER`] (or there was none).
    pub fn needs_recheck(&self, queued: bool, now: Instant) -> bool {
        queued
            || self
                .observed_at
                .is_none_or(|at| now.saturating_duration_since(at) > RECHECK_AFTER)
    }

    pub fn request(&self) -> &ActionRequest {
        &self.request
    }

    /// Decide one observation taken at `now`.
    pub fn observe(&mut self, facts: &ActionabilityFacts, now: Instant) -> Step {
        let verdict = evaluate(&self.request, facts, self.reference);
        self.observed_at = Some(now);
        if let Field::Known(frame) = facts.frame {
            self.reference = Some(FrameReference { frame, live: true });
        }
        let remaining = self.deadline.saturating_duration_since(now);
        match verdict {
            Verdict::Ready => Step::Act,
            // Stability is unproven, not refuted: with no time to sample
            // again, act (unknown evidence never blocks).
            Verdict::Resample if remaining.is_zero() => Step::Act,
            Verdict::Refuse {
                check,
                code,
                reason,
            } => Step::Refuse(Box::new(self.refusal(code, check, reason, now))),
            Verdict::Wait { check, reason } if remaining.is_zero() => Step::Refuse(Box::new(
                self.refusal(ACTIONABILITY_TIMEOUT, check, reason, now),
            )),
            Verdict::Resample | Verdict::Wait { .. } => {
                let interval = FIRST_POLL
                    .saturating_mul(1 << self.polls.min(3))
                    .min(MAX_POLL);
                self.polls = self.polls.saturating_add(1);
                Step::Sleep(interval.min(remaining))
            }
        }
    }

    fn refusal(&self, code: &str, check: Check, reason: &str, now: Instant) -> ToolResult {
        let action = self.request.kind.name();
        let waited_ms = now.saturating_duration_since(self.started).as_millis() as u64;
        let timeout_ms = self.timeout.as_millis() as u64;
        let hint = match check {
            Check::Enabled => {
                " Wait for the app to enable it (a form still validating, a step not yet \
                 done). A menu item the app enables only while it is active needs \
                 delivery_mode:\"foreground\"."
            }
            Check::Visible => {
                " Bring it on screen (scroll_to, open its container) or read the window again \
                 for a visible element."
            }
            Check::Stable => " It is still animating; retry once it settles.",
            Check::Editable => {
                " set_value and type_text need a text field or a control with a settable \
                 value; press buttons and checkboxes with click."
            }
            Check::SupportedAction => " Read the element's actions and pick another tool.",
        };
        let message = if code == ACTIONABILITY_TIMEOUT {
            format!(
                "{action} did not run: after {waited_ms} ms the element was still not \
                 actionable ({}: {reason}). Nothing was sent; retrying is safe.{hint}",
                check.name()
            )
        } else {
            format!(
                "{action} refused: {reason} ({}). Nothing was sent; waiting will not change \
                 this.{hint}",
                check.name()
            )
        };
        let mut structured = serde_json::json!({
            "status": "refused",
            "code": code,
            "effect": "refused",
            "check": check.name(),
            "action": action,
            "refusal": { "code": code, "message": message },
        });
        if code == ACTIONABILITY_TIMEOUT {
            structured["timeout_ms"] = timeout_ms.into();
            structured["waited_ms"] = waited_ms.into();
        }
        if let Some(token) = &self.token {
            structured["element_token"] = token.as_str().into();
        }
        ToolResult::error(message).with_structured(structured)
    }
}

fn center_inside(frame: [f64; 4], window: [f64; 4]) -> bool {
    let (cx, cy) = (frame[0] + frame[2] / 2.0, frame[1] + frame[3] / 2.0);
    cx >= window[0] && cy >= window[1] && cx <= window[0] + window[2] && cy <= window[1] + window[3]
}

fn frames_agree(a: [f64; 4], b: [f64; 4]) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(a, b)| (a - b).abs() <= FRAME_TOLERANCE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: [f64; 4] = [100.0, 100.0, 80.0, 20.0];
    const WINDOW: [f64; 4] = [0.0, 0.0, 800.0, 600.0];

    fn button() -> ActionabilityFacts {
        ActionabilityFacts {
            role: Field::Known("AXButton".to_owned()),
            frame: Field::Known(FRAME),
            enabled: Field::Known(true),
            window_frame: Field::Known(WINDOW),
            ..ActionabilityFacts::default()
        }
    }

    fn waiter(request: ActionRequest, timeout_ms: u64, now: Instant) -> Waiter {
        Waiter::new(
            request,
            Duration::from_millis(timeout_ms),
            Some(FRAME),
            Some("s00000001:4"),
            now,
        )
    }

    fn refusal(step: Step) -> serde_json::Value {
        match step {
            Step::Refuse(result) => {
                assert_eq!(result.is_error, Some(true));
                result.structured_content.expect("structured refusal")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_actionable_target_acts_on_the_first_observation() {
        let now = Instant::now();
        for request in [
            ActionRequest::semantic(ActionKind::Click),
            ActionRequest::pointer(ActionKind::DoubleClick),
            ActionRequest::semantic(ActionKind::SetValue),
            ActionRequest::semantic(ActionKind::Scroll),
        ] {
            let mut waiter = waiter(request, 5_000, now);
            assert!(matches!(waiter.observe(&button(), now), Step::Act));
        }
    }

    /// Unknown evidence never blocks: a read that failed for every fact is
    /// how the driver acted before this gate, and must still act.
    #[test]
    fn unknown_facts_never_block_any_action() {
        let facts = ActionabilityFacts::default();
        for kind in [
            ActionKind::Click,
            ActionKind::DoubleClick,
            ActionKind::RightClick,
            ActionKind::Hover,
            ActionKind::SetValue,
            ActionKind::TypeText,
            ActionKind::Scroll,
            ActionKind::Toggle,
            ActionKind::Expand,
            ActionKind::Clear,
            ActionKind::Focus,
            ActionKind::ScrollTo,
        ] {
            for request in [ActionRequest::semantic(kind), ActionRequest::pointer(kind)] {
                assert_eq!(
                    evaluate(&request.requiring("AXPress"), &facts, None),
                    Verdict::Ready
                );
            }
        }
    }

    #[test]
    fn a_disabled_control_is_waited_for_until_it_enables() {
        let start = Instant::now();
        let mut waiter = waiter(ActionRequest::semantic(ActionKind::Click), 5_000, start);
        let mut disabled = button();
        disabled.enabled = Field::Known(false);
        let Step::Sleep(first) = waiter.observe(&disabled, start) else {
            panic!("a disabled button must be waited for");
        };
        assert_eq!(first, FIRST_POLL);
        let later = start + Duration::from_millis(300);
        assert!(matches!(waiter.observe(&button(), later), Step::Act));
    }

    #[test]
    fn a_control_disabled_past_the_deadline_times_out_naming_the_check() {
        let start = Instant::now();
        let mut waiter = waiter(ActionRequest::semantic(ActionKind::Click), 5_000, start);
        let mut disabled = button();
        disabled.enabled = Field::Known(false);
        assert!(matches!(waiter.observe(&disabled, start), Step::Sleep(_)));
        let structured = refusal(waiter.observe(&disabled, start + Duration::from_millis(5_010)));
        assert_eq!(structured["code"], ACTIONABILITY_TIMEOUT);
        assert_eq!(structured["refusal"]["code"], ACTIONABILITY_TIMEOUT);
        assert_eq!(structured["status"], "refused");
        assert_eq!(structured["effect"], "refused");
        assert_eq!(structured["check"], "enabled");
        assert_eq!(structured["action"], "click");
        assert_eq!(structured["timeout_ms"], 5_000);
        assert_eq!(structured["waited_ms"], 5_010);
        assert_eq!(structured["element_token"], "s00000001:4");
        let message = structured["refusal"]["message"].as_str().unwrap();
        assert!(
            message.contains("Nothing was sent; retrying is safe"),
            "{message}"
        );
    }

    #[test]
    fn timeout_zero_checks_once() {
        let now = Instant::now();
        let mut disabled = button();
        disabled.enabled = Field::Known(false);
        let mut waiter_once = waiter(ActionRequest::semantic(ActionKind::Click), 0, now);
        assert_eq!(
            refusal(waiter_once.observe(&disabled, now))["code"],
            ACTIONABILITY_TIMEOUT
        );
        let mut waiter_once = waiter(ActionRequest::semantic(ActionKind::Click), 0, now);
        assert!(matches!(waiter_once.observe(&button(), now), Step::Act));
    }

    #[test]
    fn backoff_grows_to_its_cap_and_never_overshoots_the_deadline() {
        let start = Instant::now();
        let mut waiter = waiter(ActionRequest::semantic(ActionKind::Click), 5_000, start);
        let mut disabled = button();
        disabled.enabled = Field::Known(false);
        let sleeps: Vec<u64> = (0..6)
            .map(|_| match waiter.observe(&disabled, start) {
                Step::Sleep(duration) => duration.as_millis() as u64,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sleeps, [16, 32, 64, 100, 100, 100]);
        match waiter.observe(&disabled, start + Duration::from_millis(4_990)) {
            Step::Sleep(duration) => assert_eq!(duration, Duration::from_millis(10)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_lease_recheck_runs_after_queuing_or_a_stale_observation() {
        let start = Instant::now();
        let mut waiter = waiter(ActionRequest::semantic(ActionKind::Click), 5_000, start);
        assert!(waiter.needs_recheck(false, start), "never observed");
        assert!(matches!(waiter.observe(&button(), start), Step::Act));
        assert!(!waiter.needs_recheck(false, start + Duration::from_millis(5)));
        assert!(waiter.needs_recheck(true, start + Duration::from_millis(5)));
        assert!(waiter.needs_recheck(false, start + Duration::from_millis(80)));
    }

    #[test]
    fn a_zero_size_frame_is_not_visible_except_for_menu_items() {
        let mut hidden = button();
        hidden.frame = Field::Known([10.0, 10.0, 0.0, 0.0]);
        let request = ActionRequest::semantic(ActionKind::Click);
        assert!(matches!(
            evaluate(&request, &hidden, None),
            Verdict::Wait {
                check: Check::Visible,
                ..
            }
        ));
        hidden.role = Field::Known("AXMenuItem".to_owned());
        assert_eq!(evaluate(&request, &hidden, None), Verdict::Ready);
        // Focus and scroll_to need no visibility.
        hidden.role = Field::Known("AXButton".to_owned());
        assert_eq!(
            evaluate(&ActionRequest::semantic(ActionKind::Focus), &hidden, None),
            Verdict::Ready
        );
    }

    /// AXPress reaches a row scrolled out of its list; a pointer event at
    /// its centre would land outside the window.
    #[test]
    fn only_pointer_delivery_needs_the_centre_inside_the_window() {
        let mut outside = button();
        outside.frame = Field::Known([100.0, 900.0, 80.0, 20.0]);
        assert_eq!(
            evaluate(&ActionRequest::semantic(ActionKind::Click), &outside, None),
            Verdict::Ready
        );
        assert!(matches!(
            evaluate(
                &ActionRequest::pointer(ActionKind::DoubleClick),
                &outside,
                None
            ),
            Verdict::Wait {
                check: Check::Visible,
                ..
            }
        ));
        outside.window_frame = Field::Unknown;
        assert_eq!(
            evaluate(
                &ActionRequest::pointer(ActionKind::DoubleClick),
                &outside,
                None
            ),
            Verdict::Ready
        );
    }

    #[test]
    fn stability_reuses_the_snapshot_frame_and_samples_again_only_when_it_moved() {
        let start = Instant::now();
        let request = ActionRequest::pointer(ActionKind::DoubleClick);
        // Unmoved since the snapshot: no second sample.
        let mut unmoved = waiter(request, 5_000, start);
        assert!(matches!(unmoved.observe(&button(), start), Step::Act));

        // The window moved after the snapshot and is now still: one re-sample.
        let mut moved = button();
        moved.frame = Field::Known([300.0, 100.0, 80.0, 20.0]);
        let mut settled = waiter(request, 5_000, start);
        assert!(matches!(
            settled.observe(&moved, start),
            Step::Sleep(d) if d == FIRST_POLL
        ));
        assert!(matches!(
            settled.observe(&moved, start + FIRST_POLL),
            Step::Act
        ));

        // Still sliding: two live samples differ, so it waits, then times out.
        let mut sliding = waiter(request, 50, start);
        assert!(matches!(sliding.observe(&moved, start), Step::Sleep(_)));
        let mut further = moved.clone();
        further.frame = Field::Known([320.0, 100.0, 80.0, 20.0]);
        assert!(matches!(
            evaluate(
                &request,
                &further,
                Some(FrameReference {
                    frame: [300.0, 100.0, 80.0, 20.0],
                    live: true
                })
            ),
            Verdict::Wait {
                check: Check::Stable,
                ..
            }
        ));
        assert!(matches!(
            sliding.observe(&further, start + Duration::from_millis(16)),
            Step::Sleep(_)
        ));
        let mut furthest = moved.clone();
        furthest.frame = Field::Known([340.0, 100.0, 80.0, 20.0]);
        let structured = refusal(sliding.observe(&furthest, start + Duration::from_millis(60)));
        assert_eq!(structured["check"], "stable");

        // A single check cannot sample twice: an unproven frame acts.
        let mut once = waiter(request, 0, start);
        assert!(matches!(once.observe(&moved, start), Step::Act));
    }

    #[test]
    fn semantic_delivery_never_waits_on_motion() {
        let mut moved = button();
        moved.frame = Field::Known([300.0, 100.0, 80.0, 20.0]);
        let reference = Some(FrameReference {
            frame: FRAME,
            live: true,
        });
        assert_eq!(
            evaluate(
                &ActionRequest::semantic(ActionKind::Click),
                &moved,
                reference
            ),
            Verdict::Ready
        );
    }

    #[test]
    fn a_non_text_control_with_no_settable_text_is_refused_at_once() {
        let start = Instant::now();
        let mut facts = button();
        facts.value_settable = Field::Known(false);
        facts.selected_text_settable = Field::Known(false);
        let request = ActionRequest::semantic(ActionKind::SetValue);
        assert!(request.reads_editability(&facts.role));
        let mut waiter = waiter(request, 30_000, start);
        let structured = refusal(waiter.observe(&facts, start));
        assert_eq!(structured["code"], ELEMENT_NOT_EDITABLE);
        assert_eq!(structured["check"], "editable");
        assert!(structured.get("timeout_ms").is_none());

        // Terminal beats transient: even a disabled one fails fast.
        facts.enabled = Field::Known(false);
        assert!(matches!(
            evaluate(&request, &facts, None),
            Verdict::Refuse {
                code: ELEMENT_NOT_EDITABLE,
                ..
            }
        ));

        // Unknown settability, or a settable value, does not refuse.
        facts.enabled = Field::Known(true);
        facts.selected_text_settable = Field::Unknown;
        assert_eq!(evaluate(&request, &facts, None), Verdict::Ready);
        facts.selected_text_settable = Field::Known(false);
        facts.value_settable = Field::Known(true);
        assert_eq!(evaluate(&request, &facts, None), Verdict::Ready);
    }

    #[test]
    fn text_bearing_roles_are_never_read_for_editability() {
        for role in [
            "AXTextField",
            "AXTextArea",
            "AXGroup",
            "AXWebArea",
            "AXComboBox",
        ] {
            let role = Field::Known(role.to_owned());
            for kind in [
                ActionKind::SetValue,
                ActionKind::TypeText,
                ActionKind::Clear,
            ] {
                assert!(!ActionRequest::semantic(kind).reads_editability(&role));
            }
        }
        // Keys typed at a static text reach a contenteditable editor, and a
        // row takes type-to-select: type_text never refuses those.
        for role in ["AXStaticText", "AXRow", "AXImage", "AXLink"] {
            let role = Field::Known(role.to_owned());
            assert!(!ActionRequest::semantic(ActionKind::TypeText).reads_editability(&role));
            assert!(ActionRequest::semantic(ActionKind::SetValue).reads_editability(&role));
        }
        assert!(!ActionRequest::semantic(ActionKind::Click)
            .reads_editability(&Field::Known("AXButton".to_owned())));
    }

    /// Activation can enable a menu item the background app greys out; the
    /// foreground actuator checks AXEnabled again after activating.
    #[test]
    fn a_foreground_menu_item_defers_its_enabled_check_to_the_actuator() {
        let mut item = button();
        item.role = Field::Known("AXMenuItem".to_owned());
        item.enabled = Field::Known(false);
        let request = ActionRequest::semantic(ActionKind::Click);
        assert!(matches!(
            evaluate(&request, &item, None),
            Verdict::Wait {
                check: Check::Enabled,
                ..
            }
        ));
        assert_eq!(
            evaluate(&request.foreground(true), &item, None),
            Verdict::Ready
        );
        item.role = Field::Known("AXButton".to_owned());
        assert!(matches!(
            evaluate(&request.foreground(true), &item, None),
            Verdict::Wait { .. }
        ));
    }

    #[test]
    fn a_missing_required_action_is_terminal_only_on_positive_evidence() {
        let request = ActionRequest::semantic(ActionKind::Expand).requiring("AXPress");
        let mut facts = button();
        assert!(request.reads_actions());
        facts.actions = Field::Known(vec!["AXShowMenu".to_owned()]);
        assert!(matches!(
            evaluate(&request, &facts, None),
            Verdict::Refuse {
                check: Check::SupportedAction,
                code: ACTION_NOT_SUPPORTED,
                ..
            }
        ));
        facts.actions = Field::Known(vec!["AXPress".to_owned()]);
        assert_eq!(evaluate(&request, &facts, None), Verdict::Ready);
        facts.actions = Field::Unknown;
        assert_eq!(evaluate(&request, &facts, None), Verdict::Ready);
    }

    #[test]
    fn budgets_follow_agent_desktop_and_are_capped() {
        assert_eq!(
            timeout_for(ActionKind::Click, None, 0),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            timeout_for(ActionKind::TypeText, None, 100),
            Duration::from_millis(8_000)
        );
        assert_eq!(
            timeout_for(ActionKind::SetValue, None, 100),
            Duration::from_millis(5_000)
        );
        assert_eq!(timeout_for(ActionKind::Click, Some(0), 0), Duration::ZERO);
        assert_eq!(
            timeout_for(ActionKind::Click, Some(u64::MAX), 0),
            Duration::from_millis(MAX_TIMEOUT_MS)
        );
        assert_eq!(
            timeout_for(ActionKind::TypeText, None, 1_000_000),
            Duration::from_millis(MAX_TIMEOUT_MS)
        );
    }

    #[test]
    fn timeout_ms_must_be_a_non_negative_integer() {
        let args = serde_json::json!({ "timeout_ms": 250 });
        assert_eq!(
            timeout_from_args(&args, ActionKind::Click, 0).unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(
            timeout_from_args(&serde_json::json!({}), ActionKind::TypeText, 10).unwrap(),
            Duration::from_millis(5_300)
        );
        for bad in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("5"),
        ] {
            let error = timeout_from_args(
                &serde_json::json!({ "timeout_ms": bad }),
                ActionKind::Click,
                0,
            )
            .unwrap_err();
            assert_eq!(
                error.structured_content.unwrap()["code"],
                "invalid_arguments"
            );
        }
    }
}
