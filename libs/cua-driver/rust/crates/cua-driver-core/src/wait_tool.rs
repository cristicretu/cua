//! `wait`: block, bounded, until a UI condition holds.
//!
//! The predicates are run_steps' own `wait_for`/`expect` checks
//! ([`Check`], evaluated by [`Locator`]) plus three the batch had no use
//! for: a window appearing or closing by app and title (no `window_id`
//! needed), an open menu / sheet / popover / alert (`surface`, read through
//! the platform's `list_surfaces`), and a fixed pause (`ms`). The predicate
//! vocabulary follows agent-desktop's `wait` (crates/core/src/commands/
//! wait_mode.rs, Apache-2.0); the evaluation is the batch's.
//!
//! Every read goes through the registry like any direct read, so each poll
//! passes the same session, policy and approval checks. The tool itself is
//! read-only: it takes no desktop input lane and no mutation lease, and polls
//! no faster than [`locate::POLL_INTERVAL`]. Its polls publish no snapshot,
//! so the caller's element tokens stay current while it waits and after it
//! times out; only an element wait that holds makes one ordinary read, to
//! return the matched element's `element_token` (that read replaces the
//! window's snapshot, as any read does). In run_steps the wait reads like
//! the batch's other lookups, whose tokens the later steps act on.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::{
    batch_tools::locate::{self, Check, Locator, Miss, Window, WindowSpec},
    protocol::ToolResult,
    recording_tools::ReplayRegistrySlot,
    tool::{Tool, ToolDef},
};

/// Public name of the tool, and of the run_steps step that runs one.
pub const WAIT_TOOL: &str = "wait";
/// Wait used when the caller names none.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Longest wait one call may hold. A larger `timeout_ms` is cut to this, so
/// a wait stays inside client call timeouts.
pub const MAX_TIMEOUT_MS: u64 = 60_000;
/// Surface kinds a wait (and `get_window_state` `surface`) can name.
/// `menu` covers both a menu-bar menu and a context menu.
pub const SURFACE_KINDS: &[&str] = &["menu", "sheet", "popover", "alert"];
/// Platform tool that lists an app's open surfaces.
pub const LIST_SURFACES_TOOL: &str = "list_surfaces";

/// Keys only `wait` adds to a check.
const WAIT_ONLY_KEYS: &[&str] = &["surface", "ms"];
const ELEMENT_KEYS: &[&str] = &["role", "name", "label", "text", "nth"];
const WINDOW_KEYS: &[&str] = &["pid", "window_id", "app", "window"];
/// Check fields that need an element to read.
const FIELD_KEYS: &[&str] = &[
    "value",
    "value_contains",
    "enabled",
    "selected",
    "count",
    "nth",
];

/// What a wait waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Predicate {
    /// An element (role/name/text) and its state: run_steps' check.
    Element(Check),
    /// A window of the app (title substring) is listed, or with `gone` is not.
    Window { window: WindowSpec, gone: bool },
    /// An open surface of the app; with `gone`, none of that kind.
    Surface {
        window: WindowSpec,
        kind: String,
        gone: bool,
    },
    /// A fixed pause.
    Sleep,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Wait {
    pub predicate: Predicate,
    pub timeout: Duration,
    /// The caller asked for more than [`MAX_TIMEOUT_MS`].
    pub capped: bool,
}

impl Wait {
    /// Parse a wait from the tool's arguments or a run_steps `wait` step.
    /// Runtime-private (`_`) keys and `session` are the caller's business.
    pub fn parse(raw: &Value) -> Result<Self, String> {
        let object = raw.as_object().ok_or(
            "must be an object such as {\"role\":\"button\",\"name\":\"Save\"} or {\"app\":\"Mail\",\"window\":\"New Message\"}",
        )?;
        let mut object: Map<String, Value> = object
            .iter()
            .filter(|(key, _)| !key.starts_with('_') && key.as_str() != "session")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        object.retain(|_, value| !value.is_null());
        if let Some(key) = object.keys().find(|key| {
            !locate::CHECK_KEYS.contains(&key.as_str()) && !WAIT_ONLY_KEYS.contains(&key.as_str())
        }) {
            return Err(format!(
                "unknown field `{key}`; a wait takes {}, {}",
                locate::CHECK_KEYS.join(", "),
                WAIT_ONLY_KEYS.join(", ")
            ));
        }
        let requested_ms = match object.remove("timeout_ms") {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .ok_or("`timeout_ms` must be a non-negative integer")?,
            ),
        };
        let has = |keys: &[&str]| keys.iter().any(|key| object.contains_key(*key));
        let gone = match object.get("gone") {
            None => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => return Err("`gone` must be true or false".to_owned()),
        };

        if let Some(ms) = object.get("ms") {
            let ms = ms
                .as_u64()
                .ok_or("`ms` must be a non-negative integer (milliseconds)")?;
            if object.len() > 1 || requested_ms.is_some() {
                return Err(
                    "`ms` waits a fixed time and takes no other field; drop it to wait for a condition"
                        .to_owned(),
                );
            }
            return Ok(Self {
                predicate: Predicate::Sleep,
                timeout: Duration::from_millis(ms.min(MAX_TIMEOUT_MS)),
                capped: ms > MAX_TIMEOUT_MS,
            });
        }

        let (timeout_ms, capped) = match requested_ms {
            None => (DEFAULT_TIMEOUT_MS, false),
            Some(ms) => (ms.min(MAX_TIMEOUT_MS), ms > MAX_TIMEOUT_MS),
        };
        let predicate = if let Some(kind) = object.get("surface") {
            let kind = kind
                .as_str()
                .map(|kind| kind.trim().to_ascii_lowercase())
                .filter(|kind| SURFACE_KINDS.contains(&kind.as_str()))
                .ok_or_else(|| format!("`surface` must be one of {}", SURFACE_KINDS.join(", ")))?;
            if has(ELEMENT_KEYS) || has(FIELD_KEYS) {
                return Err(
                    "`surface` waits for a menu/sheet/popover/alert of the app; it takes app/pid/window/window_id and gone, not an element"
                        .to_owned(),
                );
            }
            let window = window_spec(&object)?;
            if window.pid.is_none() && window.app.is_none() {
                return Err("`surface` needs `app` or `pid` to say whose surface".to_owned());
            }
            Predicate::Surface { window, kind, gone }
        } else if has(&["role", "name", "label", "text"]) {
            let mut check = Check::parse(&Value::Object(object), DEFAULT_TIMEOUT_MS)?;
            check.timeout = Duration::from_millis(timeout_ms);
            Predicate::Element(check)
        } else if has(WINDOW_KEYS) {
            if has(FIELD_KEYS) {
                return Err(
                    "value/enabled/selected/count/nth need `role`, `name` or `text` to say which element"
                        .to_owned(),
                );
            }
            let window = window_spec(&object)?;
            if window.pid.is_none() && window.app.is_none() && window.title.is_none() {
                return Err("a window wait needs `app`, `pid` or `window` (title)".to_owned());
            }
            Predicate::Window { window, gone }
        } else {
            return Err(
                "say what to wait for: `role`/`name`/`text` (an element), `app`/`window` (a window appearing, or closing with gone:true), `surface` (menu, sheet, popover, alert), or `ms` (a fixed pause)"
                    .to_owned(),
            );
        };
        Ok(Self {
            predicate,
            timeout: Duration::from_millis(timeout_ms),
            capped,
        })
    }

    pub fn kind(&self) -> &'static str {
        match self.predicate {
            Predicate::Element(_) => "element",
            Predicate::Window { .. } => "window",
            Predicate::Surface { .. } => "surface",
            Predicate::Sleep => "sleep",
        }
    }

    /// The window the wait names, empty when it names none.
    pub fn window(&self) -> WindowSpec {
        match &self.predicate {
            Predicate::Element(check) => check.window.clone(),
            Predicate::Window { window, .. } | Predicate::Surface { window, .. } => window.clone(),
            Predicate::Sleep => WindowSpec::default(),
        }
    }

    /// Whether the wait reads a window it does not name, so needs one from
    /// the context (a run_steps step) or fails.
    pub fn needs_window(&self) -> bool {
        matches!(&self.predicate, Predicate::Element(check) if check.window.is_empty())
    }

    pub fn describe(&self) -> String {
        match &self.predicate {
            Predicate::Element(check) => check.describe(),
            Predicate::Window { window, gone } => format!(
                "{} {}",
                window.describe(),
                if *gone { "is closed" } else { "is open" }
            ),
            Predicate::Surface { window, kind, gone } => format!(
                "{kind} of {} {}",
                window.describe(),
                if *gone { "is closed" } else { "is open" }
            ),
            Predicate::Sleep => format!("{} ms pause", self.timeout.as_millis()),
        }
    }
}

fn window_spec(object: &Map<String, Value>) -> Result<WindowSpec, String> {
    let string = |key: &str| -> Result<Option<String>, String> {
        match object.get(key) {
            None => Ok(None),
            Some(Value::String(text)) if !text.trim().is_empty() => {
                Ok(Some(text.trim().to_owned()))
            }
            Some(_) => Err(format!("`{key}` must be a non-empty string")),
        }
    };
    let integer = |key: &str| -> Result<Option<i64>, String> {
        match object.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_i64()
                .filter(|n| *n >= 0)
                .map(Some)
                .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
        }
    };
    Ok(WindowSpec {
        pid: integer("pid")?,
        window_id: integer("window_id")?.map(|id| id as u64),
        app: string("app")?,
        title: string("window")?,
    })
}

/// A wait that held.
#[derive(Debug)]
pub(crate) struct Waited {
    pub waited: Duration,
    pub observed: Option<String>,
    pub window: Option<Window>,
    pub element: Option<Value>,
    pub surface: Option<Value>,
}

/// A wait that did not hold.
#[derive(Debug)]
pub(crate) struct Failed {
    /// `timeout` when the condition never held; otherwise why the wait
    /// stopped early (a refused read, an ambiguous name, ...).
    pub code: &'static str,
    /// The last miss's code, for a timeout.
    pub last_code: Option<&'static str>,
    pub message: String,
    pub waited: Duration,
    pub window: Option<Window>,
}

impl Failed {
    fn from_miss(miss: Miss, started: Instant) -> Self {
        let timed_out = miss.worth_waiting();
        Self {
            code: if timed_out { "timeout" } else { miss.code },
            last_code: timed_out.then_some(miss.code),
            message: miss.message,
            waited: started.elapsed(),
            window: miss.window,
        }
    }
}

/// Run one wait. `fallback` is the window an element wait reads when it
/// names none.
pub(crate) async fn run(
    locator: &Locator<'_>,
    wait: &Wait,
    fallback: &WindowSpec,
) -> Result<Waited, Failed> {
    let started = Instant::now();
    let deadline = (started + wait.timeout).min(locator.deadline());
    match &wait.predicate {
        Predicate::Sleep => {
            tokio::time::sleep_until(deadline.into()).await;
            Ok(Waited {
                waited: started.elapsed(),
                observed: None,
                window: None,
                element: None,
                surface: None,
            })
        }
        Predicate::Element(check) => {
            let checked = locator
                .check(check, fallback)
                .await
                .map_err(|miss| Failed::from_miss(miss, started))?;
            Ok(Waited {
                waited: checked.waited,
                observed: checked.observed,
                window: checked.window,
                element: checked.element,
                surface: None,
            })
        }
        Predicate::Window { window, gone } => {
            poll(started, deadline, || window_once(locator, window, *gone)).await
        }
        Predicate::Surface { window, kind, gone } => {
            poll(started, deadline, || {
                surface_once(locator, window, kind, *gone)
            })
            .await
        }
    }
}

/// After an element wait held on observation reads (which carry no tokens),
/// read the window once for real so the matched element comes back with an
/// `element_token`. If the condition no longer holds on that read, the
/// observed match is returned without a token rather than failing a wait
/// that did hold.
async fn with_fresh_token(locator: &Locator<'_>, wait: &Wait, mut waited: Waited) -> Waited {
    let Predicate::Element(check) = &wait.predicate else {
        return waited;
    };
    if check.gone || waited.element.is_none() {
        return waited;
    }
    let spec = if check.window.is_empty() {
        WindowSpec::default()
    } else {
        check.window.clone()
    };
    if let Ok(fresh) = locator.check_once(check, &spec, Instant::now()).await {
        if fresh.element.is_some() {
            waited.element = fresh.element;
            waited.window = fresh.window.or(waited.window);
        }
    }
    waited
}

/// Poll `once` every [`locate::POLL_INTERVAL`] until it holds, fails for a
/// reason waiting cannot fix, or `deadline` passes.
async fn poll<F, Fut>(started: Instant, deadline: Instant, mut once: F) -> Result<Waited, Failed>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Waited, Miss>>,
{
    loop {
        match once().await {
            Ok(mut waited) => {
                waited.waited = started.elapsed();
                return Ok(waited);
            }
            Err(miss) if !miss.worth_waiting() => return Err(Failed::from_miss(miss, started)),
            Err(miss) => {
                if Instant::now() + locate::POLL_INTERVAL > deadline {
                    let mut failed = Failed::from_miss(miss, started);
                    failed.message = format!(
                        "{} (checked for {} ms)",
                        failed.message,
                        started.elapsed().as_millis()
                    );
                    return Err(failed);
                }
                tokio::time::sleep(locate::POLL_INTERVAL).await;
            }
        }
    }
}

async fn window_once(locator: &Locator<'_>, spec: &WindowSpec, gone: bool) -> Result<Waited, Miss> {
    match locator.listed_windows(spec).await {
        Ok(windows) if gone => Err(Miss::new(
            "still_present",
            format!(
                "{} is still open: {}",
                spec.describe(),
                windows[0].describe()
            ),
        )),
        Ok(mut windows) => {
            let window = windows.remove(0);
            Ok(Waited {
                waited: Duration::ZERO,
                observed: Some(window.describe()),
                window: Some(window),
                element: None,
                surface: None,
            })
        }
        Err(miss) if gone && matches!(miss.code, "window_not_found" | "app_not_found") => {
            Ok(Waited {
                waited: Duration::ZERO,
                observed: Some(format!("{} is closed", spec.describe())),
                window: None,
                element: None,
                surface: None,
            })
        }
        Err(miss) => Err(miss),
    }
}

async fn surface_once(
    locator: &Locator<'_>,
    spec: &WindowSpec,
    kind: &str,
    gone: bool,
) -> Result<Waited, Miss> {
    // The app's pid, and with a title the window that must own a sheet or
    // popover. A pid alone needs no lookup.
    let (pid, window_id, window) = match (spec.pid, &spec.app, &spec.title) {
        (Some(pid), None, None) => (pid, spec.window_id, None),
        _ => match locator.listed_window(spec).await {
            Ok(window) => {
                let window_id =
                    (spec.title.is_some() || spec.window_id.is_some()).then_some(window.window_id);
                (window.pid, window_id, Some(window))
            }
            Err(miss) if gone && matches!(miss.code, "window_not_found" | "app_not_found") => {
                return Ok(Waited {
                    waited: Duration::ZERO,
                    observed: Some(format!("{} has no window", spec.describe())),
                    window: None,
                    element: None,
                    surface: None,
                })
            }
            Err(miss) => return Err(miss),
        },
    };
    let mut args = json!({ "pid": pid });
    if let Some(window_id) = window_id {
        args["window_id"] = json!(window_id);
    }
    let listed = locator
        .call(LIST_SURFACES_TOOL, args)
        .await
        .map_err(|error| Miss::new("lookup_failed", error))?;
    match (surface_present(&listed, kind, window_id), gone) {
        (Some(surface), false) => Ok(Waited {
            waited: Duration::ZERO,
            observed: Some(describe_surface(&surface)),
            window,
            element: None,
            surface: Some(surface),
        }),
        (None, true) => Ok(Waited {
            waited: Duration::ZERO,
            observed: Some(format!("no {kind} open")),
            window,
            element: None,
            surface: None,
        }),
        (Some(surface), true) => Err(Miss::new(
            "still_present",
            format!("{} is still open", describe_surface(&surface)),
        )),
        (None, false) => Err(Miss::new(
            "not_found",
            format!(
                "no {kind} open for {} (open: {})",
                spec.describe(),
                open_kinds(&listed)
            ),
        )),
    }
}

/// The first listed surface of `kind` (`menu` also takes a context menu),
/// owned by `window_id` when one is named. App-level surfaces (menus) carry
/// no window and match any window.
pub(crate) fn surface_present(listed: &Value, kind: &str, window_id: Option<u64>) -> Option<Value> {
    listed
        .get("surfaces")
        .and_then(Value::as_array)?
        .iter()
        .find(|surface| {
            let surface_kind = surface.get("kind").and_then(Value::as_str).unwrap_or("");
            let kind_matches =
                surface_kind == kind || (kind == "menu" && surface_kind == "context_menu");
            let owner = surface.get("window_id").and_then(Value::as_u64);
            kind_matches && (window_id.is_none() || owner.is_none() || owner == window_id)
        })
        .cloned()
}

fn describe_surface(surface: &Value) -> String {
    let kind = surface
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("surface");
    let mut out = kind.replace('_', " ");
    if let Some(title) = surface
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
    {
        out.push_str(&format!(" \"{title}\""));
    }
    if let Some(window_id) = surface.get("window_id").and_then(Value::as_u64) {
        out.push_str(&format!(" in window {window_id}"));
    }
    out
}

fn open_kinds(listed: &Value) -> String {
    let kinds: Vec<&str> = listed
        .get("surfaces")
        .and_then(Value::as_array)
        .map(|surfaces| {
            surfaces
                .iter()
                .filter_map(|surface| surface.get("kind").and_then(Value::as_str))
                .filter(|kind| *kind != "focused")
                .collect()
        })
        .unwrap_or_default();
    if kinds.is_empty() {
        "none".to_owned()
    } else {
        kinds.join(", ")
    }
}

/// The structured report of a wait, shared by the tool and run_steps.
pub(crate) fn report(wait: &Wait, outcome: &Result<Waited, Failed>) -> Value {
    let mut report = json!({
        "predicate": wait.kind(),
        "condition": wait.describe(),
        "timeout_ms": wait.timeout.as_millis() as u64,
    });
    if wait.capped {
        report["timeout_capped"] = json!(true);
    }
    match outcome {
        Ok(waited) => {
            report["ok"] = json!(true);
            report["waited_ms"] = json!(waited.waited.as_millis() as u64);
            if let Some(observed) = &waited.observed {
                report["observed"] = json!(observed);
            }
            if let Some(window) = &waited.window {
                report["window"] = window.to_json();
            }
            if let Some(element) = &waited.element {
                report["element"] = element.clone();
            }
            if let Some(surface) = &waited.surface {
                report["surface"] = surface.clone();
            }
        }
        Err(failed) => {
            report["ok"] = json!(false);
            report["code"] = json!(failed.code);
            report["waited_ms"] = json!(failed.waited.as_millis() as u64);
            report["detail"] = json!(failed.message);
            if let Some(code) = failed.last_code {
                report["last_code"] = json!(code);
            }
            if let Some(window) = &failed.window {
                report["window"] = window.to_json();
            }
        }
    }
    report
}

pub struct WaitTool {
    registry: ReplayRegistrySlot,
}

impl WaitTool {
    pub fn new(registry: ReplayRegistrySlot) -> Self {
        Self { registry }
    }
}

static DEF: OnceLock<ToolDef> = OnceLock::new();

#[async_trait]
impl Tool for WaitTool {
    fn def(&self) -> &ToolDef {
        DEF.get_or_init(|| ToolDef {
            name: WAIT_TOOL.into(),
            description: "Wait, bounded, until a UI condition holds, instead of sleeping and \
                re-reading. Give ONE of: an element (`role`/`name`/`text`, optionally \
                `gone:true`, `value`, `value_contains`, `enabled`, `selected`, or `count` \
                for exactly N matches) in a window named by pid+window_id or `app`/`window`; \
                a window (`app` and/or `window` title substring, no window_id needed) that \
                opens, or closes with `gone:true`; a `surface` (menu, sheet, popover, \
                alert) of `app`/`pid` that opens, or closes with `gone:true`; or `ms` for a \
                fixed pause. Polls every 250 ms up to `timeout_ms` (default 5000, at most \
                60000; larger values are cut). Read-only: it never acts or takes focus. \
                Polling leaves your element_tokens current; a timeout costs none. Returns \
                what held, or `code:\"timeout\"` with the last thing seen. An element wait \
                that holds re-reads the window once and returns the match's fresh \
                element_token: other tokens of that window then need a new read. Also usable as a run_steps step: \
                {\"wait\": {...}}."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                    "pid": { "type": "integer", "description": "Target process." },
                    "window_id": { "type": "integer", "description": "Exact window, with pid." },
                    "app": { "type": "string", "description": "App name or bundle id, instead of pid." },
                    "window": { "type": "string", "description": "Window title substring. Alone (no role/name/text), the wait is for that window to open (or close with gone:true)." },
                    "role": { "type": "string", "description": "Element role (button, textfield, menuitem, ...)." },
                    "name": { "type": "string", "description": "Element label; exact match wins over word-start substring." },
                    "label": { "type": "string", "description": "Alias of name." },
                    "text": { "type": "string", "description": "Text shown in any row, static text included." },
                    "nth": { "type": "integer", "minimum": 0, "description": "Which match the value/enabled/selected check reads." },
                    "gone": { "type": "boolean", "description": "Wait for the element, window or surface to disappear." },
                    "value": { "type": "string", "description": "Element value equals this." },
                    "value_contains": { "type": "string", "description": "Element value contains this." },
                    "enabled": { "type": "boolean", "description": "Enabled (true) or disabled (false)." },
                    "selected": { "type": "boolean", "description": "Selected or checked." },
                    "count": { "type": "integer", "minimum": 1, "description": "Exactly this many matches." },
                    "surface": { "type": "string", "enum": SURFACE_KINDS, "description": "Wait for an open menu (menu bar or context), sheet, popover or alert of the app." },
                    "ms": { "type": "integer", "minimum": 0, "description": "Fixed pause in milliseconds, at most 60000; takes no other field." },
                    "timeout_ms": { "type": "integer", "minimum": 0, "description": "Longest wait. Default 5000, at most 60000." }
                },
                "additionalProperties": false
            }),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let registry = match self.registry.lock().unwrap().upgrade() {
            Some(registry) => registry,
            None => {
                return ToolResult::error("wait is not available: registry not initialised yet.")
            }
        };
        let wait = match Wait::parse(&args) {
            Ok(wait) => wait,
            Err(detail) => {
                return ToolResult::error(format!("wait: {detail}")).with_structured(json!({
                    "ok": false,
                    "code": "invalid_arguments",
                    "tool": WAIT_TOOL,
                    "detail": detail,
                }))
            }
        };
        if wait.needs_window() {
            let detail =
                "an element wait needs a window: give pid and window_id, or `app` (and `window`)";
            return ToolResult::error(format!("wait: {detail}")).with_structured(json!({
                "ok": false,
                "code": "invalid_arguments",
                "tool": WAIT_TOOL,
                "detail": detail,
            }));
        }
        let session = args
            .get("_public_session_label")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // The locator's own stop leaves room for the last poll's read.
        let deadline = Instant::now() + wait.timeout + locate::POLL_INTERVAL;
        let observer = Locator::new(&registry, session.clone(), deadline).observing();
        let outcome = match run(&observer, &wait, &WindowSpec::default()).await {
            Ok(waited) => {
                Ok(
                    with_fresh_token(&Locator::new(&registry, session, deadline), &wait, waited)
                        .await,
                )
            }
            failed => failed,
        };
        let structured = report(&wait, &outcome);
        match outcome {
            Ok(waited) => {
                let mut text = format!(
                    "wait: {} held after {} ms",
                    wait.describe(),
                    waited.waited.as_millis()
                );
                if let Some(observed) = &waited.observed {
                    text.push_str(&format!(": {observed}"));
                }
                if let Some(token) = waited
                    .element
                    .as_ref()
                    .and_then(|element| element.get("element_token"))
                    .and_then(Value::as_str)
                {
                    text.push_str(&format!(" (element_token {token})"));
                }
                ToolResult::text(text).with_structured(structured)
            }
            Err(failed) => {
                let text = if failed.code == "timeout" {
                    format!(
                        "wait timed out after {} ms: {} did not hold: {}",
                        failed.waited.as_millis(),
                        wait.describe(),
                        failed.message
                    )
                } else {
                    format!("wait stopped: {}: {}", wait.describe(), failed.message)
                };
                ToolResult::error(text).with_structured(structured)
            }
        }
    }
}

#[cfg(test)]
mod tests;
