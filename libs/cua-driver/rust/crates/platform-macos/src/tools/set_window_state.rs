//! macOS `set_window_state` — minimize, maximize, restore or full-screen one
//! exact window, verified by reading the state back.
//!
//! Every change is an accessibility attribute write on the window itself
//! (`AXMinimized`, `AXPosition`/`AXSize`, `AXFullScreen`). None of them
//! activates the app or raises another window, so minimize, restore and
//! maximize run in the background. Full screen moves the window to its own
//! Space and switches the display there, so the decision module refuses it
//! unless the caller asked for `delivery_mode:"foreground"`.
//!
//! Decisions (what to write, which refusal, whether the readback reached the
//! requested state) live in `cua_driver_core::window_state_change`; this file
//! only gathers facts and performs the one write.

use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    window_state_change::{
        display_for_frame, plan, verdict, Rect, WindowStateAction, WindowStateFacts,
        WindowStatePlan, WindowStateVerdict, WindowWrite,
    },
};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct SetWindowStateTool;

/// How long to poll for the state to settle. The minimize genie and the
/// full-screen transition each take roughly half a second.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(2_500);
const SETTLE_POLL: Duration = Duration::from_millis(50);

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "set_window_state".into(),
        description: "Minimize, maximize (fill the display's visible area), restore (un-minimize) \
            or full-screen one exact window, then read the state back. Never activates the app. \
            minimize/maximize/restore work in the background; fullscreen and exit_fullscreen \
            switch the user's Space, so they require delivery_mode:\"foreground\"."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "window_id", "action"],
            "properties": {
                "pid": { "type": "integer", "description": "Owner pid of the window." },
                "window_id": { "type": "integer", "description": "Exact window id from list_windows." },
                "action": {
                    "type": "string",
                    "enum": WindowStateAction::NAMES,
                    "description": "Target state change."
                },
                "delivery_mode": {
                    "type": "string",
                    "enum": ["background", "foreground"],
                    "description": "Default background. foreground is required for fullscreen and exit_fullscreen."
                },
                "session": cua_driver_core::tool_schema::session_schema()
            },
            "additionalProperties": false,
        }),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[derive(Debug)]
struct Request {
    pid: i32,
    window_id: u32,
    action: WindowStateAction,
    foreground: bool,
}

fn parse_request(args: &Value) -> Result<Request, String> {
    let pid = args
        .get("pid")
        .and_then(Value::as_i64)
        .filter(|pid| *pid > 0)
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("`pid` must be a positive integer")?;
    let window_id = args
        .get("window_id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or("`window_id` must be a positive integer")?;
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .and_then(WindowStateAction::parse)
        .ok_or_else(|| format!("`action` must be one of {:?}", WindowStateAction::NAMES))?;
    let foreground = match args.get("delivery_mode").and_then(Value::as_str) {
        None | Some("background") => false,
        Some("foreground") => true,
        Some(other) => {
            return Err(format!(
                "`delivery_mode` must be background or foreground, not {other:?}"
            ))
        }
    };
    Ok(Request {
        pid,
        window_id,
        action,
        foreground,
    })
}

fn facts_json(facts: &WindowStateFacts) -> Value {
    serde_json::json!({
        "minimized": facts.minimized,
        "fullscreen": facts.fullscreen,
        "frame": facts.frame.map(rect_json),
    })
}

fn rect_json(rect: Rect) -> Value {
    serde_json::json!({"x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height})
}

/// What happened, for the result payload.
#[derive(Debug)]
struct Outcome {
    request_action: WindowStateAction,
    before: WindowStateFacts,
    after: WindowStateFacts,
    write: Option<WindowWrite>,
    write_errors: Vec<String>,
    verdict: Option<WindowStateVerdict>,
    /// Whether WindowServer made the target frontmost during the change.
    /// `None` when the front process could not be read.
    activated: Option<bool>,
}

fn refused(code: &str, message: String) -> ToolResult {
    ToolResult::error(format!("set_window_state: {message}")).with_structured(serde_json::json!({
        "status": "refused",
        "refusal": { "code": code, "message": message },
    }))
}

fn result_for(request: &Request, outcome: &Outcome) -> ToolResult {
    let action = outcome.request_action.name();
    let mut structured = serde_json::json!({
        "pid": request.pid,
        "window_id": request.window_id,
        "action": action,
        "changed": outcome.write.is_some(),
        "verdict": outcome.verdict.map_or("already_in_state", WindowStateVerdict::name),
        "before": facts_json(&outcome.before),
        "after": facts_json(&outcome.after),
        "app_activated": outcome.activated,
    });
    if let Some(WindowWrite::SetFrame(target)) = outcome.write {
        structured["target_frame"] = rect_json(target);
    }
    if !outcome.write_errors.is_empty() {
        structured["write_errors"] = serde_json::json!(outcome.write_errors);
    }
    match outcome.verdict {
        None => ToolResult::text(format!(
            "Window {} was already in the {action} state; nothing was changed.",
            request.window_id
        ))
        .with_structured(structured),
        Some(WindowStateVerdict::Verified) => ToolResult::text(format!(
            "{action} applied and verified on window {}.",
            request.window_id
        ))
        .with_structured(structured),
        Some(WindowStateVerdict::Unverifiable) => ToolResult::text(format!(
            "{action} was written to window {}, but its state could not be read back.",
            request.window_id
        ))
        .with_structured(structured),
        Some(WindowStateVerdict::NotReached) => {
            let message = format!(
                "{action} was written to window {} but the window did not reach that state \
                 (see after{}).",
                request.window_id,
                if outcome.write_errors.is_empty() {
                    ""
                } else {
                    " and write_errors"
                }
            );
            structured["status"] = "not_confirmed".into();
            structured["code"] = "window_state_not_reached".into();
            structured["message"] = message.clone().into();
            ToolResult::error(format!("set_window_state: {message}")).with_structured(structured)
        }
    }
}

/// The `AXUIElement` of one exact window plus its application element; both
/// released on drop.
struct AxWindow {
    app: crate::ax::bindings::AXUIElementRef,
    windows: Vec<crate::ax::bindings::AXUIElementRef>,
    target: crate::ax::bindings::AXUIElementRef,
}

impl Drop for AxWindow {
    fn drop(&mut self) {
        // SAFETY: every element was returned retained by the copy calls in
        // `resolve_window` and is released exactly once here.
        unsafe {
            for window in &self.windows {
                CFRelease(*window as CFTypeRef);
            }
            CFRelease(self.app as CFTypeRef);
        }
    }
}

fn resolve_window(pid: i32, window_id: u32) -> Result<AxWindow, (String, String)> {
    use crate::{
        ax::bindings::{
            ax_get_window_id, copy_ax_windows_including, AXUIElementCreateApplication,
            AXUIElementSetMessagingTimeout,
        },
        windows::WindowOwner,
    };
    match crate::windows::resolve_window_owner(pid, window_id) {
        WindowOwner::SamePid => {}
        WindowOwner::ForeignPid {
            owner_pid,
            owner_app_name,
        } => {
            return Err((
                "window_owner_mismatch".into(),
                format!(
                    "window_id {window_id} belongs to pid {owner_pid} ({owner_app_name}), not pid {pid}"
                ),
            ))
        }
        WindowOwner::Unknown => {
            return Err((
                "window_not_found".into(),
                format!("window_id {window_id} is closed, stale, or unknown to WindowServer"),
            ))
        }
    }
    // SAFETY: the application element is created retained; the window list
    // holds retained elements. `AxWindow::drop` releases them all.
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err((
                "window_not_found".into(),
                format!("could not create an accessibility element for pid {pid}"),
            ));
        }
        AXUIElementSetMessagingTimeout(app, 2.0);
        let windows = copy_ax_windows_including(app, pid, window_id);
        let target = windows
            .iter()
            .copied()
            .find(|window| ax_get_window_id(*window) == Some(window_id));
        let resolved = AxWindow {
            app,
            windows,
            target: target.unwrap_or(std::ptr::null_mut()),
        };
        if resolved.target.is_null() {
            return Err((
                "window_not_found".into(),
                format!("window_id {window_id} has no matching AXWindow in pid {pid}"),
            ));
        }
        AXUIElementSetMessagingTimeout(resolved.target, 2.0);
        Ok(resolved)
    }
}

fn read_facts(window: &AxWindow, window_id: u32) -> WindowStateFacts {
    use crate::ax::bindings::{copy_bool_attr, element_screen_rect, is_attribute_settable};
    // WindowServer is the frame source `list_windows` and `set_window_frame`
    // confirm against; AX is the fallback for a window it cannot report.
    let frame = crate::windows::window_bounds_by_id(window_id)
        .map(|bounds| Rect {
            x: bounds.x,
            y: bounds.y,
            width: bounds.width,
            height: bounds.height,
        })
        // SAFETY: `window.target` is a live retained element.
        .or_else(|| {
            unsafe { element_screen_rect(window.target) }.map(|rect| Rect {
                x: rect[0],
                y: rect[1],
                width: rect[2],
                height: rect[3],
            })
        })
        .filter(|rect| rect.is_valid());
    // SAFETY: `window.target` is a live retained element for every read.
    unsafe {
        WindowStateFacts {
            minimized: copy_bool_attr(window.target, "AXMinimized"),
            fullscreen: copy_bool_attr(window.target, "AXFullScreen"),
            frame,
            minimized_settable: is_attribute_settable(window.target, "AXMinimized"),
            fullscreen_settable: is_attribute_settable(window.target, "AXFullScreen"),
            position_settable: is_attribute_settable(window.target, "AXPosition"),
            size_settable: is_attribute_settable(window.target, "AXSize"),
        }
    }
}

/// The visible work area of the display the window is on.
fn work_area_for(frame: Option<Rect>) -> Option<Rect> {
    let displays = super::list_displays::active_displays().ok()?;
    display_for_frame(
        &displays,
        frame,
        |display| display.bounds,
        |display| display.primary,
    )
    .and_then(|display| display.visible_bounds)
}

/// Perform one write. Returns the AX errors it reported (empty on success).
fn apply(window: &AxWindow, write: WindowWrite, only: Option<&WindowStateFacts>) -> Vec<String> {
    use crate::ax::bindings::{kAXErrorSuccess, set_bool_attr, set_point_attr, set_size_attr};
    let mut errors = Vec::new();
    let mut check = |attribute: &str, error| {
        if error != kAXErrorSuccess {
            errors.push(format!("{attribute} was rejected with AXError {error}"));
        }
    };
    // SAFETY: `window.target` is a live retained element.
    unsafe {
        match write {
            WindowWrite::SetMinimized(value) => check(
                "AXMinimized",
                set_bool_attr(window.target, "AXMinimized", value),
            ),
            WindowWrite::SetFullscreen(value) => check(
                "AXFullScreen",
                set_bool_attr(window.target, "AXFullScreen", value),
            ),
            WindowWrite::SetFrame(target) => {
                // Position before size: on Tahoe, AXPosition written right
                // after AXSize can roll the size back (see set_window_frame).
                // A corrective pass rewrites only the component that missed.
                let current = only.and_then(|facts| facts.frame);
                let position_ok = current.is_some_and(|frame| {
                    (frame.x - target.x).abs() <= 2.0 && (frame.y - target.y).abs() <= 2.0
                });
                let size_ok = current.is_some_and(|frame| {
                    (frame.width - target.width).abs() <= 2.0
                        && (frame.height - target.height).abs() <= 2.0
                });
                if !position_ok {
                    check(
                        "AXPosition",
                        set_point_attr(window.target, "AXPosition", target.x, target.y),
                    );
                }
                if !size_ok {
                    check(
                        "AXSize",
                        set_size_attr(window.target, "AXSize", target.width, target.height),
                    );
                }
            }
        }
    }
    errors
}

fn change_state(request: &Request) -> Result<Outcome, ToolResult> {
    // Same-process AX writes run inline on this worker thread and trip
    // AppKit's main-thread assertion (#4323, see set_window_frame).
    if request.pid == std::process::id() as i32 {
        return Err(refused(
            "own_process_window",
            "refusing to change a window in the driver's own process".into(),
        ));
    }
    let window = resolve_window(request.pid, request.window_id)
        .map_err(|(code, message)| refused(&code, message))?;
    let before = read_facts(&window, request.window_id);
    let work_area = if request.action == WindowStateAction::Maximize {
        work_area_for(before.frame)
    } else {
        None
    };
    let write = match plan(request.action, &before, request.foreground, work_area) {
        WindowStatePlan::AlreadyInState => {
            return Ok(Outcome {
                request_action: request.action,
                before,
                after: before,
                write: None,
                write_errors: Vec::new(),
                verdict: None,
                activated: None,
            })
        }
        WindowStatePlan::Refuse { code, message } => return Err(refused(code, message)),
        WindowStatePlan::Write(write) => write,
    };
    let front_before =
        crate::input::skylight::front_process_matches(request.pid, request.window_id);
    let mut write_errors = apply(&window, write, Some(&before));
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    let mut after;
    let mut corrected = false;
    loop {
        std::thread::sleep(SETTLE_POLL);
        after = read_facts(&window, request.window_id);
        if verdict(write, &after) == WindowStateVerdict::Verified || Instant::now() >= deadline {
            break;
        }
        // One corrective frame pass for apps that settle one component and
        // roll the other back.
        if !corrected && matches!(write, WindowWrite::SetFrame(_)) {
            corrected = true;
            write_errors.extend(apply(&window, write, Some(&after)));
        }
    }
    let front_after = crate::input::skylight::front_process_matches(request.pid, request.window_id);
    let activated = match (front_before, front_after) {
        (Some(before), Some(after)) => Some(!before && after),
        _ => None,
    };
    Ok(Outcome {
        request_action: request.action,
        before,
        after,
        write: Some(write),
        write_errors,
        verdict: Some(verdict(write, &after)),
        activated,
    })
}

#[async_trait]
impl Tool for SetWindowStateTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let request = match parse_request(&args) {
            Ok(request) => request,
            Err(message) => return ToolResult::error(format!("set_window_state: {message}")),
        };
        match tokio::task::spawn_blocking(move || {
            change_state(&request).map(|outcome| result_for(&request, &outcome))
        })
        .await
        {
            Ok(Ok(result)) | Ok(Err(result)) => result,
            Err(error) => ToolResult::error(format!(
                "set_window_state: the worker failed: {error}; the window may have changed. \
                 Check list_windows before retrying."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(action: WindowStateAction) -> Request {
        Request {
            pid: 42,
            window_id: 7,
            action,
            foreground: false,
        }
    }

    fn facts(minimized: bool) -> WindowStateFacts {
        WindowStateFacts {
            minimized: Some(minimized),
            fullscreen: Some(false),
            ..WindowStateFacts::default()
        }
    }

    #[test]
    fn arguments_parse_strictly() {
        let ok = parse_request(&serde_json::json!({
            "pid": 42, "window_id": 7, "action": "fullscreen", "delivery_mode": "foreground"
        }))
        .unwrap();
        assert!(ok.foreground);
        assert_eq!(ok.action, WindowStateAction::Fullscreen);
        assert!(
            !parse_request(&serde_json::json!({"pid": 42, "window_id": 7, "action": "minimize"}))
                .unwrap()
                .foreground
        );
        for bad in [
            serde_json::json!({"pid": 42, "window_id": 7, "action": "zoom"}),
            serde_json::json!({"pid": 0, "window_id": 7, "action": "minimize"}),
            serde_json::json!({"pid": 42, "action": "minimize"}),
            serde_json::json!({"pid": 42, "window_id": 7, "action": "minimize", "delivery_mode": "Foreground"}),
        ] {
            assert!(parse_request(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn background_fullscreen_is_refused_without_touching_the_window() {
        // The decision runs before any write; the pid/window here do not
        // exist, so reaching AX would have failed with window_not_found.
        let decision = plan(WindowStateAction::Fullscreen, &facts(false), false, None);
        assert!(matches!(
            decision,
            WindowStatePlan::Refuse {
                code: "foreground_required",
                ..
            }
        ));
    }

    #[test]
    fn own_process_windows_are_refused_before_any_ax_access() {
        let mut own = request(WindowStateAction::Minimize);
        own.pid = std::process::id() as i32;
        let refusal = change_state(&own).expect_err("own process is refused");
        assert_eq!(
            refusal.structured_content.unwrap()["refusal"]["code"],
            "own_process_window"
        );
    }

    #[test]
    fn results_distinguish_already_verified_unverifiable_and_not_reached() {
        let request = request(WindowStateAction::Minimize);
        let outcome = |write, verdict| Outcome {
            request_action: WindowStateAction::Minimize,
            before: facts(false),
            after: facts(verdict == Some(WindowStateVerdict::Verified)),
            write,
            write_errors: Vec::new(),
            verdict,
            activated: Some(false),
        };
        let already = result_for(&request, &outcome(None, None));
        assert_eq!(already.is_error, None);
        let structured = already.structured_content.unwrap();
        assert_eq!(structured["verdict"], "already_in_state");
        assert_eq!(structured["changed"], false);

        let write = Some(WindowWrite::SetMinimized(true));
        let verified = result_for(
            &request,
            &outcome(write, Some(WindowStateVerdict::Verified)),
        );
        assert_eq!(verified.is_error, None);
        assert_eq!(
            verified.structured_content.unwrap()["after"]["minimized"],
            true
        );

        let unverifiable = result_for(
            &request,
            &outcome(write, Some(WindowStateVerdict::Unverifiable)),
        );
        assert_eq!(unverifiable.is_error, None);
        assert_eq!(
            unverifiable.structured_content.unwrap()["verdict"],
            "unverifiable"
        );

        let missed = result_for(
            &request,
            &outcome(write, Some(WindowStateVerdict::NotReached)),
        );
        assert_eq!(missed.is_error, Some(true));
        let structured = missed.structured_content.unwrap();
        assert_eq!(structured["code"], "window_state_not_reached");
        assert_eq!(structured["app_activated"], false);
    }

    #[test]
    fn schema_requires_an_exact_window_and_closed_actions() {
        let def = def();
        assert_eq!(
            def.input_schema["required"],
            serde_json::json!(["pid", "window_id", "action"])
        );
        assert_eq!(
            def.input_schema["properties"]["action"]["enum"],
            serde_json::json!(WindowStateAction::NAMES)
        );
        assert!(!def.read_only);
    }
}
