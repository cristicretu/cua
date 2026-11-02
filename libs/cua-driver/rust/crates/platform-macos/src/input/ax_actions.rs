//! AX action dispatch — the preferred click/interaction path for indexed elements.

use crate::ax::bindings::*;
use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};
use cua_driver_core::disposition::Disposition;
use cua_driver_core::protocol::ToolResult;

const MAX_SELECTION_ANCESTORS: usize = 8;

fn is_selectable_container_role(role: &str) -> bool {
    matches!(role, "AXRow" | "AXCell" | "AXListItem" | "AXImage")
}

/// Select the nearest list-like element at or above `element_ptr` and confirm
/// the write through `AXSelected` read-back.
///
/// Finder and other AppKit collection views commonly expose an item's label as
/// an actionable child (`AXTextField`) while the selectable object is its
/// parent `AXRow`. Neither object necessarily advertises `AXPress`, and Finder
/// can return `kAXErrorCannotComplete` for a press on the row. A pointer click
/// selects that row, so the AX equivalent is to set the row's `AXSelected`
/// attribute rather than treating the failed press as terminal.
///
/// Finder icon views expose each selectable file directly as an `AXImage` with
/// an `AXSelected` attribute, so that role is included alongside the standard
/// row-like containers. The fallback remains bounded and requires a successful
/// `AXSelected=true` read-back, so an arbitrary failed image/button press cannot
/// become a claimed success.
pub fn select_nearest_container(element_ptr: usize) -> Option<String> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..MAX_SELECTION_ANCESTORS {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if is_selectable_container_role(&role)
            && unsafe { copy_bool_attr(current, "AXSelected") }.is_some()
        {
            let err = unsafe { set_bool_attr_true(current, "AXSelected") };
            if err == kAXErrorSuccess
                && unsafe { copy_bool_attr(current, "AXSelected") } == Some(true)
            {
                if owns_current {
                    unsafe { CFRelease(current as CFTypeRef) };
                }
                return Some(role);
            }
        }

        let parent = unsafe { copy_element_attr(current, "AXParent") };
        if owns_current {
            unsafe { CFRelease(current as CFTypeRef) };
        }
        current = parent?;
        owns_current = true;
    }

    if owns_current {
        unsafe { CFRelease(current as CFTypeRef) };
    }
    None
}

/// Read the selection state of the nearest collection-like element without
/// mutating it. This is used to verify a coordinate fallback when AppKit
/// exposes `AXSelected` but refuses to set it directly (notably Finder icon
/// views).
pub fn nearest_container_selection_state(element_ptr: usize) -> Option<(String, bool)> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..MAX_SELECTION_ANCESTORS {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if is_selectable_container_role(&role) {
            if let Some(selected) = unsafe { copy_bool_attr(current, "AXSelected") } {
                if owns_current {
                    unsafe { CFRelease(current as CFTypeRef) };
                }
                return Some((role, selected));
            }
        }

        let parent = unsafe { copy_element_attr(current, "AXParent") };
        if owns_current {
            unsafe { CFRelease(current as CFTypeRef) };
        }
        current = parent?;
        owns_current = true;
    }

    if owns_current {
        unsafe { CFRelease(current as CFTypeRef) };
    }
    None
}

/// A retained selection context for proving the settled result of a modified
/// pointer click.
///
/// Reading only the target's `AXSelected` bit is insufficient for multi-select:
/// AppKit can expose a transient target transition while it is still resolving
/// the synthetic gesture, and a modifier-less outcome can replace the prior
/// selection with the target. Keep the target and every selected sibling alive
/// across delivery so callers can require both the intended target transition
/// and preservation of the pre-existing selection.
pub struct SelectionReadback {
    role: String,
    target: AXUIElementRef,
    peer_model_observed: bool,
    previously_selected_peers: Vec<AXUIElementRef>,
}

impl SelectionReadback {
    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn observe(&self) -> Option<(bool, bool)> {
        let target_selected = unsafe { copy_bool_attr(self.target, "AXSelected") }?;
        let peers_preserved = self.peer_model_observed
            && self
                .previously_selected_peers
                .iter()
                .all(|&peer| unsafe { copy_bool_attr(peer, "AXSelected") } == Some(true));
        Some((target_selected, peers_preserved))
    }
}

impl Drop for SelectionReadback {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.target as CFTypeRef);
            for peer in self.previously_selected_peers.drain(..) {
                CFRelease(peer as CFTypeRef);
            }
        }
    }
}

/// Capture the nearest selectable container and its currently-selected peers.
/// Returns `None` when the platform does not expose a readable selection model;
/// callers must then leave the action unverifiable instead of inventing proof.
pub fn capture_nearest_container_selection(element_ptr: usize) -> Option<SelectionReadback> {
    let mut current = element_ptr as AXUIElementRef;
    let mut owns_current = false;

    for _ in 0..MAX_SELECTION_ANCESTORS {
        let role = unsafe { copy_string_attr(current, "AXRole") }.unwrap_or_default();
        if is_selectable_container_role(&role)
            && unsafe { copy_bool_attr(current, "AXSelected") }.is_some()
        {
            if !owns_current {
                unsafe { CFRetain(current as CFTypeRef) };
            }
            let target = current;
            let parent = unsafe { copy_element_attr(target, "AXParent") };
            let mut peers = Vec::new();
            let mut peer_model_observed = false;
            if let Some(parent) = parent {
                for child in unsafe { copy_children(parent) } {
                    let is_target =
                        unsafe { CFEqual(child as CFTypeRef, target as CFTypeRef) != 0 };
                    peer_model_observed |= is_target;
                    if !is_target && unsafe { copy_bool_attr(child, "AXSelected") } == Some(true) {
                        peers.push(child);
                    } else {
                        unsafe { CFRelease(child as CFTypeRef) };
                    }
                }
                unsafe { CFRelease(parent as CFTypeRef) };
            }
            return Some(SelectionReadback {
                role,
                target,
                peer_model_observed,
                previously_selected_peers: peers,
            });
        }

        let parent = unsafe { copy_element_attr(current, "AXParent") };
        if owns_current {
            unsafe { CFRelease(current as CFTypeRef) };
        }
        current = parent?;
        owns_current = true;
    }

    if owns_current {
        unsafe { CFRelease(current as CFTypeRef) };
    }
    None
}

// AX error codes `bindings` does not name (HIServices `AXError.h`).
const AX_ERROR_ILLEGAL_ARGUMENT: AXError = -25201;
const AX_ERROR_CANNOT_COMPLETE: AXError = -25204;
const AX_ERROR_NOT_IMPLEMENTED: AXError = -25208;

/// What an AX write's return code proves about delivery.
///
/// Ported from agent-desktop's `crates/macos/src/actions/ax_mutation.rs`
/// (Apache-2.0), with one change: agent-desktop lets an unsupported action
/// fall through as not delivered, but AppKit returns
/// `kAXErrorActionUnsupported`/`kAXErrorAttributeUnsupported` from presses it
/// performed (#3835), so here an unsupported answer settles nothing on its
/// own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxMutation {
    Delivered,
    /// `kAXErrorInvalidUIElement`: the element is gone; nothing was sent.
    Stale,
    /// The request never reached the app: Accessibility is off, or the
    /// argument was refused.
    Rejected,
    /// The action or attribute is not supported. Only an observed effect, or
    /// another mechanism that cannot double-fire, settles it.
    Unsupported,
    /// `kAXErrorFailure`, `kAXErrorCannotComplete` or an unclassified error:
    /// the app may have acted. Never try a second mechanism after this.
    Uncertain,
}

pub fn classify_ax_error(error: AXError) -> AxMutation {
    if error == kAXErrorSuccess {
        AxMutation::Delivered
    } else if error == kAXErrorInvalidUIElement {
        AxMutation::Stale
    } else if error == kAXErrorAPIDisabled || error == AX_ERROR_ILLEGAL_ARGUMENT {
        AxMutation::Rejected
    } else if [
        kAXErrorActionUnsupported,
        AX_ERROR_NOT_IMPLEMENTED,
        kAXErrorAttributeUnsupported,
        kAXErrorNoValue,
    ]
    .contains(&error)
    {
        AxMutation::Unsupported
    } else {
        AxMutation::Uncertain
    }
}

impl AxMutation {
    /// Whether another mechanism may follow this answer. Only an answer that
    /// proves the app did nothing, or said it cannot do this, allows one.
    pub fn allows_another_mechanism(self) -> bool {
        matches!(self, AxMutation::Unsupported)
    }
}

/// A refused or failed AX write, with its structured code and disposition.
///
/// It travels through `anyhow` like any other error; the tool turns it into
/// a structured result with [`ax_error_result`], so the dispatch seam keeps
/// its disposition instead of reading a bare message as `unknown`.
#[derive(Debug, Clone)]
pub struct AxMutationError {
    pub code: &'static str,
    pub ax_error: Option<AXError>,
    /// The AX action or attribute written, such as `AXPress`.
    pub operation: Option<String>,
    pub disposition: Disposition,
    message: String,
}

impl std::fmt::Display for AxMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AxMutationError {}

impl AxMutationError {
    /// The error for a non-success answer to `operation`, or `None` on
    /// success. `message` is what the caller would have said anyway.
    pub fn from_ax_error(operation: &str, error: AXError, message: String) -> Option<Self> {
        let (code, disposition) = match classify_ax_error(error) {
            AxMutation::Delivered => return None,
            AxMutation::Stale => ("stale_element_token", Disposition::not_delivered()),
            AxMutation::Rejected if error == kAXErrorAPIDisabled => {
                ("permission_required", Disposition::not_delivered())
            }
            AxMutation::Rejected => ("ax_argument_rejected", Disposition::not_delivered()),
            AxMutation::Unsupported => ("ax_action_unsupported", Disposition::uncertain()),
            AxMutation::Uncertain if error == AX_ERROR_CANNOT_COMPLETE => {
                ("app_unresponsive", Disposition::uncertain())
            }
            AxMutation::Uncertain => ("ax_action_failed", Disposition::uncertain()),
        };
        Some(Self {
            code,
            ax_error: Some(error),
            operation: Some(operation.to_owned()),
            disposition,
            message,
        })
    }

    /// A failure after the app may have acted, whatever the last code said:
    /// a chain that already sent part of its input.
    pub fn uncertain(operation: &str, error: AXError, message: String) -> Self {
        Self {
            code: "ax_action_failed",
            ax_error: Some(error),
            operation: Some(operation.to_owned()),
            disposition: Disposition::uncertain(),
            message,
        }
    }

    /// A refusal decided before anything was sent.
    pub fn refused(code: &'static str, message: String) -> Self {
        Self {
            code,
            ax_error: None,
            operation: None,
            disposition: Disposition::not_delivered(),
            message,
        }
    }

    fn structured(&self) -> serde_json::Value {
        let mut structured = serde_json::json!({
            "code": self.code,
            "disposition": self.disposition,
        });
        if let Some(ax_error) = self.ax_error {
            structured["ax_error"] = serde_json::json!(ax_error);
        }
        if let Some(operation) = &self.operation {
            structured["operation"] = serde_json::json!(operation);
        }
        structured
    }
}

/// The tool result for a failed element action: structured with its code and
/// disposition when the failure is an [`AxMutationError`], plain otherwise.
pub fn ax_error_result(prefix: &str, error: &anyhow::Error) -> ToolResult {
    let result = ToolResult::error(format!("{prefix}: {error}"));
    match error.downcast_ref::<AxMutationError>() {
        Some(mutation) => result.with_structured(mutation.structured()),
        None => result,
    }
}

fn ensure_ax_enabled(enabled: Option<bool>, action: &str) -> anyhow::Result<()> {
    if enabled == Some(false) {
        return Err(AxMutationError::refused(
            "element_disabled",
            format!(
                "refusing {action}: the target reports AXEnabled=false. \
                 Retry this action with delivery_mode:\"foreground\" or call bring_to_front first"
            ),
        )
        .into());
    }
    Ok(())
}

/// Refuse AX actions that macOS reports as disabled.
///
/// This must be checked immediately before dispatch rather than trusting the
/// cached snapshot value: foreground delivery can make a menu item live after
/// it was resolved, while backgrounding can disable it in the other direction.
pub fn ensure_ax_action_enabled(element_ptr: usize, action: &str) -> anyhow::Result<()> {
    let enabled = unsafe { copy_bool_attr(element_ptr as AXUIElementRef, "AXEnabled") };
    ensure_ax_enabled(enabled, action)
}

/// Perform an AX action on a cached element.
pub fn perform_ax_action(element_ptr: usize, action: &str) -> anyhow::Result<()> {
    let ax_action = map_action(action)?;
    ensure_ax_action_enabled(element_ptr, ax_action)?;
    let err = unsafe { perform_action(element_ptr as AXUIElementRef, ax_action) };

    match AxMutationError::from_ax_error(
        ax_action,
        err,
        format!("AXUIElementPerformAction({action}) failed with error {err}"),
    ) {
        None => Ok(()),
        Some(error) => Err(error.into()),
    }
}

/// The AX action named by a tool's `action` argument. Accepts the short
/// names (`press`, `show_menu`, ...) and the tree's spelling (`showalternateui`,
/// `AXShowAlternateUI`). An unknown name is an error: falling back to AXPress
/// would press a control the caller meant only to reveal.
pub fn map_action(action: &str) -> anyhow::Result<&'static str> {
    let key: String = action
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_lowercase();
    let key = key.strip_prefix("ax").unwrap_or(&key);
    Ok(match key {
        "press" | "click" => "AXPress",
        "showmenu" | "rightclick" => "AXShowMenu",
        "pick" => "AXPick",
        "confirm" => "AXConfirm",
        "cancel" => "AXCancel",
        "open" => "AXOpen",
        "showalternateui" => "AXShowAlternateUI",
        "showdefaultui" => "AXShowDefaultUI",
        _ => anyhow::bail!(
            "unknown action \"{action}\": use press, show_menu, pick, confirm, cancel, open, \
             show_alternate_ui or show_default_ui"
        ),
    })
}

/// Set AXFocused=true on an element (for pre-focusing before key press).
pub fn focus_element(element_ptr: usize) -> anyhow::Result<()> {
    let err = unsafe { set_bool_attr_true(element_ptr as AXUIElementRef, "AXFocused") };
    if err == kAXErrorSuccess {
        Ok(())
    } else {
        // Focus errors are often benign (element doesn't support focus).
        tracing::warn!("AXSetAttribute(AXFocused) returned {err}");
        Ok(())
    }
}

/// Report whether `element_ptr` is the application's currently focused element.
///
/// This is a read-only confirmation for the foreground typing rung: an
/// `AXFocused` write can be accepted by the element and then immediately
/// clobbered when AppKit installs the window's remembered first responder, so
/// "the write returned success" is not evidence that focus stuck. Identity is
/// compared with `CFEqual` because the app hands back a fresh `AXUIElementRef`
/// for the same underlying element.
///
/// A `false` return is deliberately conservative: an app whose
/// `AXFocusedUIElement` is unreadable reports not-focused, which at worst costs
/// one extra re-apply.
pub fn is_element_focused(pid: i32, element_ptr: usize) -> bool {
    unsafe {
        let Some(focused) = crate::ax::bindings::focused_element_of_pid(pid) else {
            return false;
        };
        let same = CFEqual(focused as CFTypeRef, element_ptr as CFTypeRef) != 0;
        CFRelease(focused as CFTypeRef);
        same
    }
}

/// Report whether `element_ptr` holds keyboard focus, by either readback.
///
/// The application's `AXFocusedUIElement` is the stronger signal, but for
/// web content (the Tauri fixture) in a background window it does not name
/// the DOM control even after the control took focus, so a targeted
/// background `press_key` was refused although the key would have landed.
/// Accept the control's own `AXFocused = true` too. A write that AppKit
/// clobbered reads back `false` either way.
pub fn element_has_keyboard_focus(pid: i32, element_ptr: usize) -> bool {
    is_element_focused(pid, element_ptr)
        || unsafe { copy_bool_attr(element_ptr as AXUIElementRef, "AXFocused") } == Some(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn action_names_accept_the_tree_spelling_and_refuse_unknown_ones() {
        use super::map_action;
        assert_eq!(map_action("press").unwrap(), "AXPress");
        assert_eq!(
            map_action("show_alternate_ui").unwrap(),
            "AXShowAlternateUI"
        );
        assert_eq!(map_action("showalternateui").unwrap(), "AXShowAlternateUI");
        assert_eq!(map_action("AXShowDefaultUI").unwrap(), "AXShowDefaultUI");
        assert_eq!(map_action("right_click").unwrap(), "AXShowMenu");
        // An unknown name must not fall back to pressing the control.
        assert!(map_action("hover").is_err());
    }

    use super::*;

    #[test]
    fn disabled_elements_are_refused_before_dispatch() {
        let error = ensure_ax_enabled(Some(false), "AXPick").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("AXEnabled=false"));
        assert!(message.contains("delivery_mode:\"foreground\""));
        assert!(message.contains("bring_to_front"));
    }

    #[test]
    fn a_disabled_refusal_says_nothing_was_sent() {
        let error = ensure_ax_enabled(Some(false), "AXPress").unwrap_err();
        let mutation = error.downcast_ref::<AxMutationError>().expect("typed");
        assert_eq!(mutation.code, "element_disabled");
        assert!(mutation.disposition.is_retry_safe());
        // The batch foreground fallback still recognises the message.
        let result = ax_error_result("AX action failed", &error);
        assert_eq!(
            result.structured_content.unwrap()["code"],
            "element_disabled"
        );
    }

    #[test]
    fn ax_errors_classify_like_agent_desktop_except_unsupported() {
        use cua_driver_core::disposition::{DeliveryDisposition, RetryDisposition};
        assert_eq!(classify_ax_error(kAXErrorSuccess), AxMutation::Delivered);
        assert!(
            AxMutationError::from_ax_error("AXPress", kAXErrorSuccess, String::new()).is_none()
        );
        let cases = [
            (
                kAXErrorInvalidUIElement,
                "stale_element_token",
                RetryDisposition::Safe,
            ),
            (
                kAXErrorAPIDisabled,
                "permission_required",
                RetryDisposition::Safe,
            ),
            (
                AX_ERROR_ILLEGAL_ARGUMENT,
                "ax_argument_rejected",
                RetryDisposition::Safe,
            ),
            (
                kAXErrorFailure,
                "ax_action_failed",
                RetryDisposition::Unsafe,
            ),
            (
                AX_ERROR_CANNOT_COMPLETE,
                "app_unresponsive",
                RetryDisposition::Unsafe,
            ),
            (-25299, "ax_action_failed", RetryDisposition::Unsafe),
            // AppKit answers presses it performed with these (#3835).
            (
                kAXErrorActionUnsupported,
                "ax_action_unsupported",
                RetryDisposition::Unsafe,
            ),
            (
                kAXErrorAttributeUnsupported,
                "ax_action_unsupported",
                RetryDisposition::Unsafe,
            ),
        ];
        for (error, code, retry) in cases {
            let mutation =
                AxMutationError::from_ax_error("AXPress", error, format!("returned {error}"))
                    .expect("an error");
            assert_eq!(mutation.code, code, "{error}");
            assert_eq!(mutation.disposition.retry, retry, "{error}");
            assert_ne!(mutation.disposition.delivery, DeliveryDisposition::Unknown);
            let result = ax_error_result("AX action failed", &mutation.clone().into());
            let structured = result.structured_content.expect("structured");
            assert_eq!(structured["code"], code);
            assert_eq!(structured["ax_error"], error);
            assert_eq!(structured["operation"], "AXPress");
        }
    }

    #[test]
    fn only_an_unsupported_answer_allows_another_mechanism() {
        assert!(classify_ax_error(kAXErrorActionUnsupported).allows_another_mechanism());
        for error in [
            kAXErrorFailure,
            AX_ERROR_CANNOT_COMPLETE,
            kAXErrorInvalidUIElement,
            kAXErrorAPIDisabled,
            -25299,
        ] {
            assert!(
                !classify_ax_error(error).allows_another_mechanism(),
                "{error}"
            );
        }
    }

    #[test]
    fn a_plain_error_stays_plain() {
        let result = ax_error_result("AX action failed", &anyhow::anyhow!("boom"));
        assert!(result.structured_content.is_none());
    }

    #[test]
    fn enabled_or_unreported_state_is_allowed() {
        assert!(ensure_ax_enabled(Some(true), "AXPress").is_ok());
        assert!(ensure_ax_enabled(None, "AXPress").is_ok());
    }

    #[test]
    fn selection_fallback_is_limited_to_collection_item_roles() {
        for role in ["AXRow", "AXCell", "AXListItem", "AXImage"] {
            assert!(is_selectable_container_role(role), "{role}");
        }
        for role in ["AXButton", "AXTextField", "AXWindow", "AXOutline"] {
            assert!(!is_selectable_container_role(role), "{role}");
        }
    }
}
