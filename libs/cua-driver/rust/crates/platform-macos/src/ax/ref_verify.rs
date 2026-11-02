//! Live re-read of a tokenized element for strict token identity.
//!
//! The decision lives in `cua_driver_core::ref_identity`; this module only
//! gathers the facts. One `AXUIElementCopyMultipleAttributeValues` round trip
//! reads role, subrole, identifier, title, description, frame and enabled
//! state (plus `AXValue` for label roles), so the identity check costs about
//! one attribute read. Only an element no stable text names also climbs its
//! ancestry for the child-index path: two reads per level, plus two to
//! prove the climb ended at the walk root.

use super::bindings::*;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFEqual, CFGetTypeID, CFRelease, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use cua_driver_core::ref_identity::{Field, LiveElementFacts, LiveRead, RefIdentity};

/// `kAXValueAXErrorType`: the per-attribute error slot of a multi-attribute
/// read.
const AX_VALUE_AX_ERROR_TYPE: AXValueType = 5;

/// Longest ancestry the path check climbs. Walks stop at depth 25, but
/// collapsed layout containers add raw levels; anything deeper is reported
/// as unknown rather than climbed.
const MAX_PATH_DEPTH: usize = 64;

/// Same per-request bound as the tree walk: an unresponsive app cannot hold
/// the dispatch for longer than this per read.
const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 2.0;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCopyMultipleAttributeValues(
        element: AXUIElementRef,
        attributes: CFArrayRef,
        options: u32,
        values: *mut CFArrayRef,
    ) -> AXError;
}

const ROLE: usize = 0;
const SUBROLE: usize = 1;
const IDENTIFIER: usize = 2;
const TITLE: usize = 3;
const DESCRIPTION: usize = 4;
const POSITION: usize = 5;
const SIZE: usize = 6;
const ENABLED: usize = 7;
const VALUE: usize = 8;
const BASE_ATTRIBUTES: [&str; 8] = [
    "AXRole",
    "AXSubrole",
    "AXIdentifier",
    "AXTitle",
    "AXDescription",
    "AXPosition",
    "AXSize",
    "AXEnabled",
];

/// Read what [`cua_driver_core::ref_identity::check_identity`] needs about
/// the live element. Blocking (cross-process AX reads); call it off the
/// async runtime.
///
/// # Safety
///
/// `element` must be a retained `AXUIElementRef` that stays retained for the
/// duration of the call.
pub unsafe fn read_live_identity(element: AXUIElementRef, recorded: &RefIdentity) -> LiveRead {
    if element.is_null() {
        return LiveRead::Invalid;
    }
    let _ = AXUIElementSetMessagingTimeout(element, AX_MESSAGING_TIMEOUT_SECONDS);
    let mut names: Vec<CFString> = BASE_ATTRIBUTES
        .iter()
        .map(|name| CFString::new(name))
        .collect();
    if recorded.reads_value() {
        names.push(CFString::new("AXValue"));
    }
    let names = CFArray::from_CFTypes(&names);
    let mut values: CFArrayRef = std::ptr::null();
    let error = AXUIElementCopyMultipleAttributeValues(
        element,
        names.as_concrete_TypeRef(),
        0,
        &mut values,
    );
    if error == kAXErrorInvalidUIElement {
        return LiveRead::Invalid;
    }
    if error != kAXErrorSuccess || values.is_null() {
        if !values.is_null() {
            CFRelease(values as CFTypeRef);
        }
        return LiveRead::Facts(Box::default());
    }
    let values = CFArray::<CFTypeRef>::wrap_under_create_rule(values);
    let slot = |index: usize| -> Slot {
        match values.get(index as isize) {
            Some(value) => classify(*value),
            None => Slot::Unknown,
        }
    };
    let slots: Vec<Slot> = (0..names.len() as usize).map(slot).collect();
    // A dead element can still answer the call itself and fail every slot.
    if slots.iter().any(|slot| matches!(slot, Slot::Invalid)) {
        return LiveRead::Invalid;
    }
    let text = |index: usize| slots.get(index).map_or(Field::Unknown, Slot::text);
    let frame = match (
        slots.get(POSITION).map_or(Field::Unknown, Slot::point),
        slots.get(SIZE).map_or(Field::Unknown, Slot::size),
    ) {
        (Field::Known((x, y)), Field::Known((w, h))) => Field::Known([x, y, w, h]),
        (Field::Unknown, _) | (_, Field::Unknown) => Field::Unknown,
        _ => Field::Absent,
    };
    let facts = LiveElementFacts {
        role: text(ROLE),
        subrole: text(SUBROLE),
        identifier: text(IDENTIFIER),
        title: text(TITLE),
        description: text(DESCRIPTION),
        value: if recorded.reads_value() {
            text(VALUE)
        } else {
            Field::Unknown
        },
        frame,
        enabled: slots.get(ENABLED).map_or(Field::Unknown, Slot::boolean),
        path: if recorded.needs_live_path() {
            live_path(element, recorded.path.len())
        } else {
            Field::Unknown
        },
    };
    LiveRead::Facts(Box::new(facts))
}

/// One slot of a multi-attribute read. Holds an unretained value owned by
/// the result array.
enum Slot {
    Value(CFTypeRef),
    /// The attribute is unsupported or has no value.
    Absent,
    /// `kAXErrorInvalidUIElement`.
    Invalid,
    Unknown,
}

unsafe fn classify(value: CFTypeRef) -> Slot {
    if value.is_null() {
        return Slot::Absent;
    }
    if CFGetTypeID(value) == AXValueGetTypeID()
        && AXValueGetType(value as AXValueRef) == AX_VALUE_AX_ERROR_TYPE
    {
        let mut error: AXError = 0;
        if !AXValueGetValue(
            value as AXValueRef,
            AX_VALUE_AX_ERROR_TYPE,
            &mut error as *mut AXError as *mut std::ffi::c_void,
        ) {
            return Slot::Unknown;
        }
        return if error == kAXErrorInvalidUIElement {
            Slot::Invalid
        } else if error == kAXErrorAttributeUnsupported || error == kAXErrorNoValue {
            Slot::Absent
        } else {
            Slot::Unknown
        };
    }
    Slot::Value(value)
}

impl Slot {
    fn text(&self) -> Field<String> {
        match self {
            Slot::Value(value) => unsafe {
                if CFGetTypeID(*value) == CFString::type_id() {
                    let text = CFString::wrap_under_get_rule(*value as _).to_string();
                    Field::text(Some(&text))
                } else {
                    // A non-string where text was expected: unreadable as
                    // identity, so it refutes nothing.
                    Field::Unknown
                }
            },
            Slot::Absent => Field::Absent,
            Slot::Invalid | Slot::Unknown => Field::Unknown,
        }
    }

    fn boolean(&self) -> Field<bool> {
        match self {
            Slot::Value(value) => unsafe {
                let type_id = CFGetTypeID(*value);
                if type_id == CFBoolean::type_id() {
                    Field::Known(CFBoolean::wrap_under_get_rule(*value as _).into())
                } else if type_id == CFNumber::type_id() {
                    CFNumber::wrap_under_get_rule(*value as _)
                        .to_f64()
                        .map_or(Field::Unknown, |n| Field::Known(n != 0.0))
                } else {
                    Field::Unknown
                }
            },
            Slot::Absent => Field::Absent,
            Slot::Invalid | Slot::Unknown => Field::Unknown,
        }
    }

    fn point(&self) -> Field<(f64, f64)> {
        self.ax_pair(kAXValueCGPointType)
    }

    fn size(&self) -> Field<(f64, f64)> {
        self.ax_pair(kAXValueCGSizeType)
    }

    fn ax_pair(&self, kind: AXValueType) -> Field<(f64, f64)> {
        match self {
            Slot::Value(value) => unsafe {
                if CFGetTypeID(*value) != AXValueGetTypeID() {
                    return Field::Unknown;
                }
                let mut pair = [0.0_f64; 2];
                if AXValueGetValue(
                    *value as AXValueRef,
                    kind,
                    pair.as_mut_ptr() as *mut std::ffi::c_void,
                ) {
                    Field::Known((pair[0], pair[1]))
                } else {
                    Field::Unknown
                }
            },
            Slot::Absent => Field::Absent,
            Slot::Invalid | Slot::Unknown => Field::Unknown,
        }
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXValueGetTypeID() -> core_foundation::base::CFTypeID;
}

/// The element's raw `AXChildren` index path, `depth` levels up from it.
/// `Unknown` when a parent cannot be read or does not list its child (some
/// toolkits report an `AXParent` whose children skip the element): such a
/// chain proves nothing either way.
unsafe fn live_path(element: AXUIElementRef, depth: usize) -> Field<Vec<u32>> {
    if depth == 0 || depth > MAX_PATH_DEPTH {
        return Field::Unknown;
    }
    let mut path = Vec::with_capacity(depth);
    let mut current = element;
    let mut owned = false;
    let release = |element: AXUIElementRef, owned: bool| {
        if owned {
            CFRelease(element as CFTypeRef);
        }
    };
    for _ in 0..depth {
        let Some(parent) = copy_element_attr(current, "AXParent") else {
            release(current, owned);
            return Field::Unknown;
        };
        let _ = AXUIElementSetMessagingTimeout(parent, AX_MESSAGING_TIMEOUT_SECONDS);
        let children = copy_children(parent);
        let index = children
            .iter()
            .position(|&child| CFEqual(child as CFTypeRef, current as CFTypeRef) != 0);
        for child in children {
            CFRelease(child as CFTypeRef);
        }
        release(current, owned);
        current = parent;
        owned = true;
        match index {
            Some(index) => path.push(index as u32),
            None => {
                release(current, owned);
                return Field::Unknown;
            }
        }
    }
    // The climb must end on a top-level element (the walk root): a path read
    // through a toolkit's inconsistent parent chain must not be compared as
    // if it were the walked one.
    let top_level = copy_element_attr(current, "AXParent").is_some_and(|parent| {
        let role = copy_string_attr(parent, "AXRole");
        CFRelease(parent as CFTypeRef);
        role.as_deref() == Some("AXApplication")
    });
    release(current, owned);
    if !top_level {
        return Field::Unknown;
    }
    path.reverse();
    Field::Known(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-action cost of the re-read, against a live system process
    /// (Dock, always running; read-only: nothing is pressed, focused or
    /// opened). Needs Accessibility trust, so it is opt-in:
    /// `cargo test -p platform-macos ref_verify -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads a live app's accessibility tree; needs Accessibility trust"]
    fn measure_live_identity_read() {
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
            assert!(
                !nodes.is_empty(),
                "no {process} elements (Accessibility trust?)"
            );
            let mut matched = 0;
            let started = std::time::Instant::now();
            for node in &nodes {
                let identity = crate::ax::snapshot::identity_of(node);
                let read =
                    unsafe { read_live_identity(node.element_ptr as AXUIElementRef, &identity) };
                let verdict = cua_driver_core::ref_identity::check_identity(&identity, &read);
                assert!(verdict.is_ok(), "{verdict:?} on {}", node.role);
                matched += usize::from(matches!(
                    verdict,
                    Ok(cua_driver_core::ref_identity::IdentityVerdict::Match(_))
                ));
            }
            eprintln!(
                "{process}: {} checks ({matched} positive matches), {:?} per check",
                nodes.len(),
                started.elapsed() / nodes.len() as u32
            );
            let started = std::time::Instant::now();
            let mut known = 0;
            for node in &nodes {
                let path =
                    unsafe { live_path(node.element_ptr as AXUIElementRef, node.path.len()) };
                if let Field::Known(path) = path {
                    assert_eq!(path, node.path, "{}", node.role);
                    known += 1;
                }
            }
            eprintln!(
                "{process}: path climb {:?} per element ({known}/{} followed, depth up to {})",
                started.elapsed() / nodes.len() as u32,
                nodes.len(),
                nodes.iter().map(|node| node.path.len()).max().unwrap_or(0)
            );
            // The snapshot payload owns (and releases) the walk's retains.
            drop(crate::ax::snapshot::AxSnapshot::from_nodes(&walk.nodes));
        }
    }
}
