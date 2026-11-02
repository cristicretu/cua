//! Live facts for the actionability gate.
//!
//! The decision lives in `cua_driver_core::actionability`; this module only
//! gathers. Role, frame and `AXEnabled` come from the identity re-read
//! (`super::ref_verify`, one multi-attribute round trip), so an action whose
//! target needs nothing else costs no further AX call. The rest is read only
//! when the request needs it: settability only for an editing action on a
//! role that cannot hold text, advertised actions only for a tool with no
//! fallback route, the window frame only for pointer delivery. None of these
//! are attribute values, so they cannot ride on the batched read.

use super::bindings::*;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use cua_driver_core::actionability::{ActionRequest, ActionabilityFacts};
use cua_driver_core::ref_identity::{Field, LiveElementFacts};

/// Complete the identity read's facts with what `request` needs.
/// `window_frame` is a frame an earlier observation of the same action
/// already read (the WindowServer query enumerates every window, ~5 ms, so
/// one action pays it once). Blocking (cross-process AX reads and a
/// WindowServer query); call it off the async runtime.
///
/// # Safety
///
/// `element` must be a retained `AXUIElementRef` that stays retained for the
/// duration of the call.
pub unsafe fn gather(
    element: AXUIElementRef,
    live: &LiveElementFacts,
    request: &ActionRequest,
    window_id: Option<u32>,
    window_frame: Option<Field<[f64; 4]>>,
) -> ActionabilityFacts {
    let mut facts = ActionabilityFacts {
        role: live.role.clone(),
        frame: live.frame.clone(),
        enabled: live.enabled.clone(),
        ..ActionabilityFacts::default()
    };
    if request.reads_editability(&live.role) {
        facts.value_settable = settable(element, "AXValue");
        facts.selected_text_settable = settable(element, "AXSelectedText");
    }
    if request.reads_actions() {
        facts.actions = action_names(element);
    }
    if request.reads_window_frame() {
        facts.window_frame = window_frame.unwrap_or_else(|| {
            match window_id.and_then(crate::windows::window_bounds_by_id) {
                Some(bounds) => Field::Known([bounds.x, bounds.y, bounds.width, bounds.height]),
                None => Field::Unknown,
            }
        });
    }
    facts
}

/// Tri-state settability. An attribute the element does not support is
/// positively not settable; any other error proves nothing.
unsafe fn settable(element: AXUIElementRef, attribute: &str) -> Field<bool> {
    let name = CFString::new(attribute);
    let mut settable = 0_u8;
    let error = AXUIElementIsAttributeSettable(element, name.as_concrete_TypeRef(), &mut settable);
    if error == kAXErrorSuccess {
        Field::Known(settable != 0)
    } else if error == kAXErrorAttributeUnsupported || error == kAXErrorNoValue {
        Field::Known(false)
    } else {
        Field::Unknown
    }
}

/// Tri-state action list: a failed read is unknown, never "no actions"
/// (`copy_action_names` folds the two together).
unsafe fn action_names(element: AXUIElementRef) -> Field<Vec<String>> {
    let mut names: CFArrayRef = std::ptr::null_mut();
    if AXUIElementCopyActionNames(element, &mut names) != kAXErrorSuccess || names.is_null() {
        return Field::Unknown;
    }
    let names = CFArray::<CFString>::wrap_under_create_rule(names);
    Field::Known(names.iter().map(|name| name.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::actionability::{evaluate, ActionKind, Verdict};
    use cua_driver_core::ref_identity::LiveRead;

    /// What the gate adds per action against a live system process (Dock,
    /// Finder; read-only: nothing is pressed, focused or opened): the
    /// under-lease identity re-read, and the extra reads for an editing
    /// action on a non-text role and for pointer delivery. Needs
    /// Accessibility trust, so it is opt-in:
    /// `cargo test -p platform-macos actionability_facts -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads a live app's accessibility tree; needs Accessibility trust"]
    fn measure_gate_cost() {
        for process in ["Dock", "Finder"] {
            let Some(pid) = std::process::Command::new("/usr/bin/pgrep")
                .args(["-x", process])
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .and_then(|text| text.lines().next()?.trim().parse::<i32>().ok())
            else {
                eprintln!("{process} is not running; nothing measured");
                continue;
            };
            let walk = crate::ax::walk_tree(pid, None, None);
            let nodes: Vec<_> = walk
                .nodes
                .iter()
                .filter(|node| node.element_index.is_some())
                .collect();
            assert!(!nodes.is_empty(), "no {process} elements");
            let window_id = crate::windows::all_windows()
                .into_iter()
                .find(|window| window.pid == pid)
                .map(|window| window.window_id);
            let requests = [
                (
                    "click (semantic)",
                    ActionRequest::semantic(ActionKind::Click),
                ),
                ("set_value", ActionRequest::semantic(ActionKind::SetValue)),
                (
                    "double_click (pointer)",
                    ActionRequest::pointer(ActionKind::DoubleClick),
                ),
            ];
            for (label, request) in requests {
                let mut ready = 0;
                let started = std::time::Instant::now();
                for node in &nodes {
                    let element = node.element_ptr as AXUIElementRef;
                    let identity = crate::ax::snapshot::identity_of(node);
                    let LiveRead::Facts(live) =
                        (unsafe { crate::ax::ref_verify::read_live_identity(element, &identity) })
                    else {
                        continue;
                    };
                    let facts = unsafe { gather(element, &live, &request, window_id, None) };
                    ready += usize::from(
                        evaluate(
                            &request,
                            &facts,
                            identity.frame.map(|frame| {
                                cua_driver_core::actionability::FrameReference {
                                    frame,
                                    live: false,
                                }
                            }),
                        ) == Verdict::Ready,
                    );
                }
                eprintln!(
                    "{process} {label}: {:?} per observation (identity re-read + facts), \
                     {ready}/{} ready",
                    started.elapsed() / nodes.len() as u32,
                    nodes.len()
                );
            }
            drop(crate::ax::snapshot::AxSnapshot::from_nodes(&walk.nodes));
        }
    }
}
