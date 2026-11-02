//! Pure decisions behind the semantic state tools: `set_state` (checked,
//! unchecked, toggled, expanded, collapsed, focused), `clear_value` and
//! `scroll_to`.
//!
//! The platform adapters gather a handful of accessibility facts about one
//! token-addressed element, ask [`plan_check`] / [`plan_disclosure`] /
//! [`plan_focus`] / [`plan_clear`] which mechanisms may run, and after each
//! mechanism ask [`after_attempt`] whether the state was reached, whether the
//! next mechanism may run, or whether the call must stop. Keeping the decision
//! here makes the fail-closed rules testable without a GUI:
//!
//! * a state that already holds is reported verified and nothing is sent;
//! * an unreadable state is never flipped blindly (a press is a toggle, so
//!   pressing a checkbox of unknown state can land in either state);
//! * a mechanism whose outcome is uncertain (the app returned a generic
//!   failure, or timed out) ends the call: a second mechanism could undo or
//!   double what the first one did;
//! * a press that ran but left the opposite state stops instead of pressing
//!   again.
//!
//! The check/uncheck chain (idempotent value write, then press only when the
//! write is absent or ignored; a radio button cannot be unchecked) and the
//! disclosure chain (expand/collapse action, then the expanded attribute, and
//! a press only from a known opposite state) follow agent-desktop's
//! `macos/src/actions/toggle_state.rs` and
//! `macos/src/actions/chain/chain_disclosure_steps.rs` (Vercel Labs,
//! Apache-2.0); [`parse_checked_value`] is its `state_reader` table.

/// The state a `set_state` call asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestedState {
    Checked,
    Unchecked,
    /// Flip a checkbox or switch from its current, readable state.
    Toggled,
    Expanded,
    Collapsed,
    /// Keyboard focus on the element.
    Focused,
}

impl RequestedState {
    /// The actionability requirements this state change is gated on.
    pub const fn action_kind(self) -> crate::actionability::ActionKind {
        use crate::actionability::ActionKind;
        match self {
            Self::Checked | Self::Unchecked | Self::Toggled => ActionKind::Toggle,
            Self::Expanded | Self::Collapsed => ActionKind::Expand,
            Self::Focused => ActionKind::Focus,
        }
    }
}

/// Spellings `set_state` accepts, in schema order.
pub const STATE_NAMES: &[&str] = &[
    "checked",
    "unchecked",
    "toggled",
    "expanded",
    "collapsed",
    "focused",
];

impl RequestedState {
    /// Parse a `state` argument. Accepts the verb forms models write too
    /// (`check`, `toggle`, `expand`, `focus`, ...).
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "checked" | "check" | "on" | "selected" | "select" => Self::Checked,
            "unchecked" | "uncheck" | "off" | "deselected" | "deselect" => Self::Unchecked,
            "toggled" | "toggle" => Self::Toggled,
            "expanded" | "expand" | "open" => Self::Expanded,
            "collapsed" | "collapse" | "close" | "closed" => Self::Collapsed,
            "focused" | "focus" => Self::Focused,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Checked => "checked",
            Self::Unchecked => "unchecked",
            Self::Toggled => "toggled",
            Self::Expanded => "expanded",
            Self::Collapsed => "collapsed",
            Self::Focused => "focused",
        }
    }
}

/// agent-desktop's checked-value table: AppKit reports 0/1 (2 = mixed),
/// web and Electron content sometimes a word. Mixed and anything else is
/// `None`, which the planners treat as unreadable.
pub fn parse_checked_value(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "checked" => Some(true),
        "0" | "false" | "no" | "off" | "unchecked" => Some(false),
        _ => None,
    }
}

/// Roles whose value is a checked state: checkboxes (switches are a checkbox
/// subrole on macOS), radio buttons, and the toggle role some toolkits
/// publish.
pub fn is_toggleable_role(role: &str) -> bool {
    matches!(
        role,
        "AXCheckBox" | "AXRadioButton" | "AXSwitch" | "AXToggle"
    )
}

/// Whether a role is a radio button: one that can be selected but never
/// unchecked directly (selecting a sibling does that).
pub fn is_radio_role(role: &str) -> bool {
    role == "AXRadioButton"
}

/// The expanded state, from the first attribute that reports one:
/// `AXExpanded` (combo boxes, web disclosure), `AXDisclosing` (outline rows),
/// or a disclosure triangle's checked value.
pub fn disclosure_state(
    expanded: Option<bool>,
    disclosing: Option<bool>,
    role: Option<&str>,
    value: Option<&str>,
) -> Option<bool> {
    expanded.or(disclosing).or_else(|| {
        (role == Some("AXDisclosureTriangle"))
            .then(|| value.and_then(parse_checked_value))
            .flatten()
    })
}

/// One way of moving an element toward the requested state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mechanism {
    /// Write the checked value (`AXValue` 1/0).
    WriteValue,
    /// The element's own expand or collapse action (`AXExpand`/`AXCollapse`).
    DisclosureAction,
    /// Write `AXExpanded`.
    WriteExpanded,
    /// Write `AXDisclosing` (outline rows).
    WriteDisclosing,
    /// Press the element (`AXPress`). A toggle: used only from a known
    /// opposite state.
    Press,
    /// Write `AXFocused = true`.
    WriteFocused,
    /// Ask the element's containers to scroll it into view
    /// (`AXScrollToVisible`).
    ScrollToVisible,
}

impl Mechanism {
    /// Whether running this mechanism twice is harmless. An ignored
    /// idempotent write may be followed by the next mechanism; a press that
    /// ran may not.
    pub const fn idempotent(self) -> bool {
        !matches!(self, Self::Press | Self::DisclosureAction)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WriteValue => "AXValue",
            Self::DisclosureAction => "disclosure_action",
            Self::WriteExpanded => "AXExpanded",
            Self::WriteDisclosing => "AXDisclosing",
            Self::Press => "AXPress",
            Self::WriteFocused => "AXFocused",
            Self::ScrollToVisible => "AXScrollToVisible",
        }
    }
}

/// A refusal before anything was sent. Always safe to retry after the cause
/// is fixed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub reason: String,
}

fn refuse(code: &'static str, reason: impl Into<String>) -> Refusal {
    Refusal {
        code,
        reason: reason.into(),
    }
}

/// What to do before any mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The element already holds the state: report it verified, send nothing.
    AlreadySatisfied,
    /// Run these mechanisms in order, consulting [`after_attempt`] after each.
    Run {
        desired: bool,
        mechanisms: Vec<Mechanism>,
    },
    Refuse(Refusal),
}

/// Facts about a checkbox-like element, read just before dispatch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToggleFacts {
    pub role: String,
    /// The parsed checked value; `None` when unreadable or mixed.
    pub checked: Option<bool>,
    pub value_settable: bool,
    pub press_advertised: bool,
    pub enabled: Option<bool>,
}

/// Plan `checked`, `unchecked` or `toggled`.
pub fn plan_check(requested: RequestedState, facts: &ToggleFacts) -> Plan {
    if !is_toggleable_role(&facts.role) {
        return Plan::Refuse(refuse(
            "state_not_supported",
            format!(
                "{} applies to checkboxes, switches and radio buttons; this element is {}. \
                 Use click for other elements.",
                requested.as_str(),
                display_role(&facts.role)
            ),
        ));
    }
    if facts.enabled == Some(false) {
        return Plan::Refuse(disabled());
    }
    let Some(current) = facts.checked else {
        return Plan::Refuse(refuse(
            "state_unreadable",
            "the element's checked state is unreadable or mixed, so a press could land in \
             either state; nothing was sent. Read it with get_window_state, or click it and \
             check the result.",
        ));
    };
    let desired = match requested {
        RequestedState::Checked => true,
        RequestedState::Unchecked => false,
        RequestedState::Toggled => !current,
        _ => unreachable!("plan_check takes only checked/unchecked/toggled"),
    };
    if requested != RequestedState::Toggled && current == desired {
        return Plan::AlreadySatisfied;
    }
    if is_radio_role(&facts.role) && !desired {
        return Plan::Refuse(refuse(
            "radio_cannot_uncheck",
            "a radio button cannot be unchecked directly; select another radio button in the \
             same group instead. Nothing was sent.",
        ));
    }
    let mut mechanisms = Vec::new();
    if facts.value_settable {
        mechanisms.push(Mechanism::WriteValue);
    }
    if facts.press_advertised {
        mechanisms.push(Mechanism::Press);
    }
    if mechanisms.is_empty() {
        return Plan::Refuse(no_route());
    }
    Plan::Run {
        desired,
        mechanisms,
    }
}

/// Facts about a disclosure-capable element, read just before dispatch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisclosureFacts {
    /// From [`disclosure_state`].
    pub expanded: Option<bool>,
    /// The element advertises the action for the requested direction
    /// (`AXExpand` to expand, `AXCollapse` to collapse).
    pub direction_action_advertised: bool,
    pub expanded_settable: bool,
    pub disclosing_settable: bool,
    pub press_advertised: bool,
    pub enabled: Option<bool>,
}

/// Plan `expanded` or `collapsed`. Idempotent mechanisms may run when the
/// state is unreadable; a press only from the known opposite state.
pub fn plan_disclosure(requested: RequestedState, facts: &DisclosureFacts) -> Plan {
    let desired = match requested {
        RequestedState::Expanded => true,
        RequestedState::Collapsed => false,
        _ => unreachable!("plan_disclosure takes only expanded/collapsed"),
    };
    if facts.expanded == Some(desired) {
        return Plan::AlreadySatisfied;
    }
    if facts.enabled == Some(false) {
        return Plan::Refuse(disabled());
    }
    let mut mechanisms = Vec::new();
    if facts.direction_action_advertised {
        mechanisms.push(Mechanism::DisclosureAction);
    }
    if facts.expanded_settable {
        mechanisms.push(Mechanism::WriteExpanded);
    }
    if facts.disclosing_settable {
        mechanisms.push(Mechanism::WriteDisclosing);
    }
    if facts.press_advertised && facts.expanded == Some(!desired) {
        mechanisms.push(Mechanism::Press);
    }
    if mechanisms.is_empty() {
        return Plan::Refuse(if facts.expanded.is_none() {
            refuse(
                "state_not_supported",
                "the element reports no expanded state and no expand/collapse route; nothing \
                 was sent. Use click on a disclosure triangle or row instead.",
            )
        } else {
            no_route()
        });
    }
    Plan::Run {
        desired,
        mechanisms,
    }
}

/// Facts for `focused`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FocusFacts {
    /// The element holds keyboard focus now (the app's focused element, or
    /// its own `AXFocused`).
    pub focused: bool,
    pub focused_settable: bool,
    pub enabled: Option<bool>,
}

pub fn plan_focus(facts: &FocusFacts) -> Plan {
    if facts.focused {
        return Plan::AlreadySatisfied;
    }
    if facts.enabled == Some(false) {
        return Plan::Refuse(disabled());
    }
    if !facts.focused_settable {
        return Plan::Refuse(refuse(
            "focus_not_settable",
            "the element does not accept keyboard focus through accessibility (AXFocused is \
             not settable); nothing was sent. Focus a field inside it, or click it.",
        ));
    }
    Plan::Run {
        desired: true,
        mechanisms: vec![Mechanism::WriteFocused],
    }
}

fn disabled() -> Refusal {
    refuse(
        "element_disabled",
        "the element reports AXEnabled=false; nothing was sent.",
    )
}

fn no_route() -> Refusal {
    refuse(
        "no_state_route",
        "the element has no settable state attribute and advertises no press; nothing was \
         sent. Use click instead.",
    )
}

fn display_role(role: &str) -> &str {
    if role.is_empty() {
        "of unknown role"
    } else {
        role
    }
}

/// How one mechanism's native call returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptResult {
    /// The call returned success.
    Ok,
    /// The element does not support this mechanism; nothing was delivered.
    Unsupported,
    /// The element no longer exists.
    Stale,
    /// The call failed in a way that may or may not have acted (generic
    /// failure, timeout).
    Uncertain,
}

/// Why a call stopped without reaching the requested state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The element went away before anything was delivered. Safe to retry
    /// after a fresh read.
    Stale,
    /// Every mechanism was unsupported. Nothing was delivered.
    NoRoute,
    /// An idempotent write returned success but the state did not move. The
    /// app ignored it; retrying the same call will not help.
    WriteIgnored,
    /// A press or action ran and the opposite state is still read back. Do
    /// not retry blindly: a second press would toggle again.
    NotReached,
    /// A mechanism failed in a way that may have acted. Read the state before
    /// any retry.
    Uncertain,
}

impl Failure {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Stale => "stale_element_token",
            Self::NoRoute => "no_state_route",
            Self::WriteIgnored => "write_not_applied",
            Self::NotReached => "state_not_reached",
            Self::Uncertain => "delivery_uncertain",
        }
    }

    /// Whether anything may have reached the app.
    pub const fn may_have_delivered(self) -> bool {
        matches!(self, Self::NotReached | Self::Uncertain)
    }

    /// The `effect` an error result reports.
    pub const fn effect(self) -> &'static str {
        match self {
            Self::Stale | Self::NoRoute => "refused",
            Self::WriteIgnored | Self::NotReached => "not_applied",
            Self::Uncertain => "unverifiable",
        }
    }
}

/// What to do after one mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// The state was read back as requested.
    Verified,
    /// The mechanism ran but the state cannot be read back.
    Unverified,
    /// The mechanism did not apply; the next one may run.
    TryNext {
        ignored_write: bool,
    },
    Stop(Failure),
}

/// Decide what one mechanism's result and the settled read-back mean.
/// `observed` is the state read after the mechanism (polled briefly toward
/// `desired`).
pub fn after_attempt(
    mechanism: Mechanism,
    result: AttemptResult,
    desired: bool,
    observed: Option<bool>,
) -> Next {
    // Some apps act and still return an error (Finder's view switcher returns
    // -25205 from a press it performed, #3835): the read-back decides.
    if observed == Some(desired) {
        return Next::Verified;
    }
    match result {
        AttemptResult::Stale => Next::Stop(Failure::Stale),
        AttemptResult::Uncertain => Next::Stop(Failure::Uncertain),
        AttemptResult::Unsupported => Next::TryNext {
            ignored_write: false,
        },
        AttemptResult::Ok => match observed {
            None => Next::Unverified,
            Some(_) if mechanism.idempotent() => Next::TryNext {
                ignored_write: true,
            },
            Some(_) => Next::Stop(Failure::NotReached),
        },
    }
}

/// The failure when every planned mechanism was tried without success.
pub fn exhausted(any_ignored_write: bool) -> Failure {
    if any_ignored_write {
        Failure::WriteIgnored
    } else {
        Failure::NoRoute
    }
}

/// Text roles `clear_value` empties. Search fields are an `AXTextField`
/// subrole; secure fields an `AXTextField` subrole too.
pub fn is_clearable_role(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox")
}

/// Facts for `clear_value`. Values are never carried, only emptiness, so
/// nothing a secure field holds can be echoed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClearFacts {
    pub role: String,
    pub secure: bool,
    pub value_settable: bool,
    /// `Some(true)` when AXValue reads as the empty string.
    pub empty: Option<bool>,
    pub enabled: Option<bool>,
}

pub fn plan_clear(facts: &ClearFacts) -> Plan {
    if !is_clearable_role(&facts.role) {
        return Plan::Refuse(refuse(
            "state_not_supported",
            format!(
                "clear_value applies to text fields, text areas and combo boxes; this element \
                 is {}. Nothing was sent.",
                display_role(&facts.role)
            ),
        ));
    }
    if facts.enabled == Some(false) {
        return Plan::Refuse(disabled());
    }
    // A secure field reads back empty or masked whatever it holds, so its
    // emptiness proves nothing and it is always written.
    if !facts.secure && facts.empty == Some(true) {
        return Plan::AlreadySatisfied;
    }
    if !facts.value_settable {
        return Plan::Refuse(refuse(
            "value_not_settable",
            "the field's AXValue is not settable, so it cannot be cleared through \
             accessibility; nothing was sent. Focus it (set_state focused), select all \
             (hotkey cmd+a) and press_key delete.",
        ));
    }
    Plan::Run {
        desired: true,
        mechanisms: vec![Mechanism::WriteValue],
    }
}

/// What a post-clear read proves. `empty` is the AXValue emptiness and
/// `char_count` the field's `AXNumberOfCharacters`; for a secure field only
/// the count is evidence.
pub fn clear_readback(secure: bool, empty: Option<bool>, char_count: Option<u64>) -> Option<bool> {
    if secure {
        return char_count.map(|count| count == 0);
    }
    match (empty, char_count) {
        (Some(empty), _) => Some(empty),
        (None, Some(count)) => Some(count == 0),
        (None, None) => None,
    }
}

/// `[x, y, width, height]` in screen points.
pub type Rect = [f64; 4];

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let left = a[0].max(b[0]);
    let top = a[1].max(b[1]);
    let right = (a[0] + a[2]).min(b[0] + b[2]);
    let bottom = (a[1] + a[3]).min(b[1] + b[3]);
    (right > left && bottom > top).then_some([left, top, right - left, bottom - top])
}

/// The visible region of nested clip rects (scroll areas, then the window):
/// their intersection, `None` when they do not overlap or none is known.
pub fn viewport(clips: &[Rect]) -> Option<Rect> {
    let (first, rest) = clips.split_first()?;
    rest.iter()
        .try_fold(*first, |visible, clip| intersect(visible, *clip))
}

/// Whether an element is in view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    InView,
    OutOfView,
    Unknown,
}

/// An element is in view when at least half of it, or half of the viewport
/// for an element larger than the viewport, shows. Chromium reports an
/// element scrolled out of view as a 1 pt sliver clamped to the viewport
/// edge (see `reveal_scroll`), so a sliver never counts.
pub fn visibility(element: Option<Rect>, viewport: Option<Rect>) -> Visibility {
    let (Some(element), Some(viewport)) = (element, viewport) else {
        return Visibility::Unknown;
    };
    if element[2] <= 1.5 || element[3] <= 1.5 {
        return Visibility::OutOfView;
    }
    let Some(shown) = intersect(element, viewport) else {
        return Visibility::OutOfView;
    };
    let area = |r: Rect| r[2] * r[3];
    let needed = area(element).min(area(viewport)) * 0.5;
    if shown[2] > 1.5 && shown[3] > 1.5 && area(shown) + 1e-6 >= needed {
        Visibility::InView
    } else {
        Visibility::OutOfView
    }
}

#[cfg(test)]
#[path = "semantic_state/tests.rs"]
mod tests;
