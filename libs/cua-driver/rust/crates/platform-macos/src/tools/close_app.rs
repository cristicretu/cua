//! macOS `close_app` — ask one application to quit, then prove it exited.
//!
//! The cooperative counterpart to `kill_app`. It sends the standard quit
//! request (`-[NSRunningApplication terminate]`, the same Apple Event the Dock's
//! Quit item sends), so the app can save state and run its own shutdown. It
//! never activates the app and never forces: an app that answers with a
//! "save changes?" prompt stays running, and the result says so instead of
//! claiming success. `kill_app` remains the explicit escalation.
//!
//! Exit is proven from the kernel process table, not AppKit: in a daemon
//! without a serviced run loop `NSRunningApplication.isTerminated` is a cache
//! that may never update (see `apps::list_running_apps`).
//!
//! Termination semantics follow agent-desktop's `close_app_impl`
//! (crates/macos/src/system/app/app_ops.rs, Apache-2.0): protected processes
//! are refused before any request, and an already-gone target is reported as
//! not delivered rather than as a successful close.

use async_trait::async_trait;
use cua_driver_core::{
    protected_process::ProcessFacts,
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::time::{Duration, Instant};

pub struct CloseAppTool;

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const EXIT_POLL: Duration = Duration::from_millis(50);

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "close_app".into(),
        description: "Ask an application to quit (the Dock's Quit), without activating it, and \
            wait up to timeout_ms for the process to exit. exited:false means it is still \
            running, usually behind a save prompt: inspect it with get_window_state, or \
            escalate to kill_app. Session-critical system processes (Finder, Dock, \
            WindowServer, loginwindow, ...) are always refused."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid"],
            "properties": {
                "pid": { "type": "integer", "description": "PID of the application to quit." },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_TIMEOUT_MS,
                    "description": "How long to wait for the process to exit. Default 5000."
                },
                "session": cua_driver_core::tool_schema::session_schema()
            },
            "additionalProperties": false,
        }),
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
    })
}

/// Whether the kernel still has `pid`. `EPERM` means it exists but belongs to
/// someone else, which is still alive.
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs only the existence and permission check.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Identity facts for the protected-process check, from the kernel (path,
/// bundle id) plus LaunchServices' display name.
pub(super) fn process_facts(pid: i32) -> ProcessFacts {
    let (executable_path, bundle_id) = crate::apps::process_identity_of_pid(pid);
    // SAFETY: plain Objective-C messages on a retained NSRunningApplication;
    // none requires the main thread.
    let name = unsafe {
        objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .and_then(|app| app.localizedName())
            .map(|name| name.to_string())
    };
    let own = std::process::id() as i64;
    // SAFETY: getppid has no preconditions.
    let host = i64::from(unsafe { libc::getppid() });
    ProcessFacts {
        pid: i64::from(pid),
        name,
        bundle_id,
        executable_path,
        is_driver_or_host: i64::from(pid) == own || (host > 1 && i64::from(pid) == host),
    }
}

/// How a quit request ended, decided from what was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseOutcome {
    /// The process was already gone; nothing was sent.
    NotRunning,
    /// AppKit refused to send the request (not an app, or already quitting).
    NotAccepted,
    /// The request was sent and the process exited within the timeout.
    Exited,
    /// The request was sent but the process was still running at the deadline.
    StillRunning,
}

fn result_for(pid: i32, outcome: CloseOutcome, elapsed: Duration, timeout_ms: u64) -> ToolResult {
    let elapsed_ms = elapsed.as_millis() as u64;
    match outcome {
        CloseOutcome::NotRunning => refused(
            "process_not_running",
            format!("close_app: pid {pid} is not running; no quit request was sent."),
        ),
        CloseOutcome::NotAccepted => refused(
            "quit_not_accepted",
            format!(
                "close_app: AppKit did not accept a quit request for pid {pid} (it may be a \
                 background process rather than an application, or already quitting). Nothing \
                 was sent; kill_app force-terminates."
            ),
        ),
        CloseOutcome::Exited => ToolResult::text(format!(
            "pid {pid} quit after {elapsed_ms} ms."
        ))
        .with_structured(serde_json::json!({
            "pid": pid,
            "requested": true,
            "exited": true,
            "elapsed_ms": elapsed_ms,
        })),
        CloseOutcome::StillRunning => {
            let message = format!(
                "close_app: the quit request was delivered to pid {pid}, but it was still running \
                 after {timeout_ms} ms (often a save prompt). Inspect it with get_window_state; \
                 kill_app force-terminates and loses unsaved work."
            );
            ToolResult::error(message.clone()).with_structured(serde_json::json!({
                "status": "not_confirmed",
                "code": "app_still_running",
                "message": message,
                "pid": pid,
                "requested": true,
                "exited": false,
                "elapsed_ms": elapsed_ms,
            }))
        }
    }
}

fn refused(code: &str, message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(serde_json::json!({
        "status": "refused",
        "refusal": { "code": code, "message": message },
    }))
}

/// Send the quit request and wait for exit. Blocking: run off the runtime.
fn quit_and_wait(pid: i32, timeout: Duration) -> (CloseOutcome, Duration) {
    let started = Instant::now();
    // SAFETY: plain Objective-C messages on a retained NSRunningApplication.
    // `terminate` posts the quit Apple Event; it does not activate the app.
    let accepted = unsafe {
        objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .is_some_and(|app| app.terminate())
    };
    if !accepted {
        let outcome = if process_alive(pid) {
            CloseOutcome::NotAccepted
        } else {
            CloseOutcome::NotRunning
        };
        return (outcome, started.elapsed());
    }
    let deadline = started + timeout;
    loop {
        if !process_alive(pid) {
            return (CloseOutcome::Exited, started.elapsed());
        }
        if Instant::now() >= deadline {
            return (CloseOutcome::StillRunning, started.elapsed());
        }
        std::thread::sleep(EXIT_POLL);
    }
}

#[async_trait]
impl Tool for CloseAppTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        super::kill_app::process_control_scope("close_app", adapter_id, args).await
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let pid = match args.get("pid").and_then(Value::as_i64) {
            Some(pid) if pid > 0 && pid <= i64::from(i32::MAX) => pid as i32,
            Some(_) => return ToolResult::error("close_app: `pid` must be a positive integer"),
            None => return ToolResult::error("close_app: missing required integer field `pid`"),
        };
        let timeout_ms = match args.get("timeout_ms") {
            None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
            Some(value) => match value.as_u64() {
                Some(ms) if ms <= MAX_TIMEOUT_MS => ms,
                _ => {
                    return ToolResult::error(format!(
                        "close_app: `timeout_ms` must be an integer from 0 to {MAX_TIMEOUT_MS}"
                    ))
                }
            },
        };
        if !process_alive(pid) {
            return result_for(pid, CloseOutcome::NotRunning, Duration::ZERO, timeout_ms);
        }
        if let Err(refusal) = super::kill_app::recheck_protected_fingerprint("close_app", &args).await
        {
            return refusal;
        }
        let facts = match tokio::task::spawn_blocking(move || process_facts(pid)).await {
            Ok(facts) => facts,
            Err(error) => {
                return ToolResult::error(format!("close_app: identity lookup failed: {error}"))
            }
        };
        if let Some(refusal) = cua_driver_core::protected_process::refusal("close_app", &facts) {
            return refusal;
        }
        let timeout = Duration::from_millis(timeout_ms);
        match tokio::task::spawn_blocking(move || quit_and_wait(pid, timeout)).await {
            Ok((outcome, elapsed)) => result_for(pid, outcome, elapsed, timeout_ms),
            Err(error) => ToolResult::error(format!(
                "close_app: the quit worker failed: {error}; the request may have been sent. \
                 Check list_apps before retrying."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structured(result: &ToolResult) -> &Value {
        result.structured_content.as_ref().expect("structured")
    }

    #[test]
    fn a_target_that_is_already_gone_is_refused_as_not_delivered() {
        let result = result_for(42, CloseOutcome::NotRunning, Duration::ZERO, 5_000);
        assert_eq!(result.is_error, Some(true));
        assert_eq!(structured(&result)["status"], "refused");
        assert_eq!(
            structured(&result)["refusal"]["code"],
            "process_not_running"
        );
    }

    #[test]
    fn exit_is_reported_only_when_observed() {
        let exited = result_for(42, CloseOutcome::Exited, Duration::from_millis(120), 5_000);
        assert_eq!(exited.is_error, None);
        assert_eq!(structured(&exited)["exited"], true);
        assert_eq!(structured(&exited)["elapsed_ms"], 120);

        let running = result_for(42, CloseOutcome::StillRunning, Duration::from_secs(5), 5_000);
        assert_eq!(running.is_error, Some(true));
        assert_eq!(structured(&running)["exited"], false);
        assert_eq!(structured(&running)["requested"], true);
        assert_eq!(structured(&running)["code"], "app_still_running");
        assert_ne!(structured(&running)["status"], "refused");
    }

    #[test]
    fn an_unaccepted_request_is_a_refusal() {
        let result = result_for(42, CloseOutcome::NotAccepted, Duration::ZERO, 5_000);
        assert_eq!(structured(&result)["refusal"]["code"], "quit_not_accepted");
    }

    #[test]
    fn the_driver_itself_is_protected() {
        let facts = process_facts(std::process::id() as i32);
        assert!(facts.is_driver_or_host);
        assert!(cua_driver_core::protected_process::refusal("close_app", &facts).is_some());
    }

    #[test]
    fn liveness_comes_from_the_kernel() {
        assert!(process_alive(std::process::id() as i32));
        // pid 1 (launchd) is alive and owned by root: EPERM still means alive.
        assert!(process_alive(1));
    }

    #[tokio::test]
    async fn invalid_arguments_are_rejected_before_any_lookup() {
        let tool = CloseAppTool;
        for args in [
            serde_json::json!({}),
            serde_json::json!({"pid": 0}),
            serde_json::json!({"pid": 42, "timeout_ms": 30_001}),
            serde_json::json!({"pid": 42, "timeout_ms": -1}),
        ] {
            let result = tool.invoke(args.clone()).await;
            assert_eq!(result.is_error, Some(true), "{args}");
        }
    }

    #[test]
    fn schema_is_destructive_and_pid_addressed() {
        let def = def();
        assert!(def.destructive);
        assert!(!def.read_only);
        assert_eq!(def.input_schema["required"], serde_json::json!(["pid"]));
        assert_eq!(def.input_schema["additionalProperties"], false);
    }
}
