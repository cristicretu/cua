//! `wait` against a fake app whose windows, rows and menus change as it is
//! read, plus the parser and the pure surface match.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::*;
use crate::tool::{Tool, ToolDef, ToolRegistry};

#[derive(Default)]
struct App {
    /// Reads of the main window before the "Saved" text shows.
    saved_after_reads: u32,
    /// list_windows calls before the "Export" dialog is listed.
    dialog_after_lists: u32,
    /// list_windows calls after which the main window closes.
    main_closes_after_lists: Option<u32>,
    /// list_surfaces calls before a context menu is open.
    menu_after_lists: u32,
    rows: u32,
    snapshot: u32,
    calls: Vec<(String, Value, Instant)>,
}

struct Fake {
    def: ToolDef,
    app: Arc<Mutex<App>>,
}

#[async_trait]
impl Tool for Fake {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let mut app = self.app.lock().unwrap();
        let name = self.def.name.clone();
        app.calls.push((name.clone(), args.clone(), Instant::now()));
        match name.as_str() {
            "list_windows" => {
                let lists = app
                    .calls
                    .iter()
                    .filter(|(n, ..)| n == "list_windows")
                    .count() as u32;
                let mut windows = Vec::new();
                if app
                    .main_closes_after_lists
                    .is_none_or(|after| lists <= after)
                {
                    windows.push(json!({
                        "pid": 42, "window_id": 7, "app_name": "Demo", "title": "Main",
                        "z_index": 1, "is_on_screen": true,
                        "bounds": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 600.0}
                    }));
                }
                if lists > app.dialog_after_lists {
                    windows.push(json!({
                        "pid": 42, "window_id": 8, "app_name": "Demo", "title": "Export",
                        "z_index": 2, "is_on_screen": true,
                        "bounds": {"x": 100.0, "y": 100.0, "width": 300.0, "height": 200.0}
                    }));
                }
                ToolResult::text("windows").with_structured(json!({ "windows": windows }))
            }
            "list_apps" => ToolResult::text("apps").with_structured(json!({
                "apps": [{"pid": 42, "name": "Demo", "bundle_id": "com.example.demo"}]
            })),
            "list_surfaces" => {
                let lists = app
                    .calls
                    .iter()
                    .filter(|(n, ..)| n == "list_surfaces")
                    .count() as u32;
                let mut surfaces = vec![json!({"kind": "focused", "role": "AXTextField"})];
                if lists > app.menu_after_lists {
                    surfaces
                        .push(json!({"kind": "context_menu", "role": "AXMenu", "item_count": 3}));
                }
                ToolResult::text("surfaces")
                    .with_structured(json!({ "pid": 42, "surfaces": surfaces }))
            }
            "get_window_state" => {
                app.snapshot += 1;
                let snapshot = app.snapshot;
                let window_id = args["window_id"].as_u64().unwrap_or(7);
                let mut markdown = String::from("- [0] AXWindow \"Demo\"\n");
                let mut elements = vec![];
                if window_id == 7 {
                    app.rows += 1;
                    markdown.push_str("  - [1] AXButton \"Save\"\n");
                    elements.push(json!({
                        "element_index": 1, "element_token": format!("s{snapshot:08x}:1"),
                        "role": "AXButton", "label": "Save", "depth": 1,
                        "frame": {"x": 0, "y": 0, "w": 10, "h": 10}
                    }));
                    if app.saved_after_reads == 0 {
                        markdown.push_str("  - AXStaticText \"Saved\"\n");
                        markdown.push_str("  - AXStaticText \"Saved to disk\"\n");
                    } else {
                        app.saved_after_reads -= 1;
                    }
                }
                ToolResult::text(markdown.clone()).with_structured(json!({
                    "pid": 42, "window_id": window_id,
                    "snapshot_id": format!("s{snapshot:08x}"),
                    "elements": elements, "tree_markdown": markdown,
                }))
            }
            other => ToolResult::text(format!("{other} done")),
        }
    }
}

struct Harness {
    registry: Arc<ToolRegistry>,
    app: Arc<Mutex<App>>,
}

impl Harness {
    fn new(app: App) -> Self {
        let app = Arc::new(Mutex::new(app));
        let mut registry = ToolRegistry::new();
        for name in [
            "get_window_state",
            "list_windows",
            "list_apps",
            "list_surfaces",
            "click",
        ] {
            registry.register(Box::new(Fake {
                def: ToolDef {
                    name: name.into(),
                    description: "fake".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "pid": {"type": "integer"},
                            "window_id": {"type": "integer"},
                            "element_token": {"type": "string"}
                        }
                    }),
                    read_only: name != "click",
                    destructive: false,
                    idempotent: false,
                    open_world: false,
                },
                app: app.clone(),
            }));
        }
        registry.register_session_tools();
        let registry = Arc::new(registry);
        registry.init_self_weak();
        Self { registry, app }
    }

    async fn wait(&self, args: Value) -> ToolResult {
        self.registry.invoke(WAIT_TOOL, args).await
    }

    fn calls(&self, tool: &str) -> Vec<Instant> {
        self.app
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(name, ..)| name == tool)
            .map(|(.., at)| *at)
            .collect()
    }
}

fn structured(result: &ToolResult) -> Value {
    result.structured_content.clone().unwrap_or(Value::Null)
}

#[test]
fn parse_picks_one_mode_and_caps_the_timeout() {
    let wait = Wait::parse(&json!({"app": "Demo", "window": "Export"})).unwrap();
    assert_eq!(wait.kind(), "window");
    assert_eq!(wait.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
    assert!(!wait.needs_window());

    let wait =
        Wait::parse(&json!({"role": "button", "name": "Save", "timeout_ms": 120_000})).unwrap();
    assert_eq!(wait.kind(), "element");
    assert!(wait.capped);
    assert_eq!(wait.timeout, Duration::from_millis(MAX_TIMEOUT_MS));
    // An element check longer than a batch check's 10 s is fine here.
    let Predicate::Element(check) = Wait::parse(&json!({"text": "Done", "timeout_ms": 30_000}))
        .unwrap()
        .predicate
    else {
        panic!("element");
    };
    assert_eq!(check.timeout, Duration::from_secs(30));
    assert!(Wait::parse(&json!({"text": "Done"}))
        .unwrap()
        .needs_window());

    let wait = Wait::parse(&json!({"surface": "Menu", "app": "Demo", "gone": true})).unwrap();
    assert_eq!(
        wait.predicate,
        Predicate::Surface {
            window: WindowSpec {
                app: Some("Demo".into()),
                ..Default::default()
            },
            kind: "menu".into(),
            gone: true
        }
    );
    let wait = Wait::parse(&json!({"ms": 90_000, "session": "s"})).unwrap();
    assert_eq!(wait.predicate, Predicate::Sleep);
    assert_eq!(wait.timeout, Duration::from_millis(MAX_TIMEOUT_MS));
}

#[test]
fn parse_refuses_unclear_waits() {
    for (args, needle) in [
        (json!({}), "say what to wait for"),
        (json!({"ms": 10, "app": "Demo"}), "fixed time"),
        (json!({"surface": "menu"}), "needs `app` or `pid`"),
        (json!({"surface": "toast", "app": "Demo"}), "must be one of"),
        (
            json!({"surface": "sheet", "app": "Demo", "role": "button"}),
            "not an element",
        ),
        (json!({"app": "Demo", "value": "x"}), "need `role`"),
        (
            json!({"text": "Saved", "count": 2, "gone": true}),
            "`count` only counts",
        ),
        (json!({"text": "Saved", "count": 0}), "at least 1"),
        (
            json!({"role": "button", "colour": "red"}),
            "unknown field `colour`",
        ),
        (json!({"role": "button", "timeout_ms": -1}), "timeout_ms"),
    ] {
        let error = Wait::parse(&args).unwrap_err();
        assert!(error.contains(needle), "{args}: {error}");
    }
}

#[test]
fn surface_match_takes_context_menus_for_menu_and_respects_the_window() {
    let listed = json!({"surfaces": [
        {"kind": "focused"},
        {"kind": "sheet", "window_id": 8, "title": "Save"},
        {"kind": "context_menu"}
    ]});
    assert_eq!(
        surface_present(&listed, "menu", Some(7)).unwrap()["kind"],
        "context_menu"
    );
    assert!(surface_present(&listed, "sheet", Some(7)).is_none());
    assert_eq!(
        surface_present(&listed, "sheet", Some(8)).unwrap()["title"],
        "Save"
    );
    assert!(surface_present(&listed, "sheet", None).is_some());
    assert!(surface_present(&listed, "alert", None).is_none());
    assert!(surface_present(&json!({}), "menu", None).is_none());
}

#[tokio::test]
async fn an_element_wait_returns_the_matched_element_once_it_shows() {
    let harness = Harness::new(App {
        saved_after_reads: 2,
        ..Default::default()
    });
    let result = harness
        .wait(json!({"pid": 42, "window_id": 7, "text": "Saved", "count": 2}))
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let report = structured(&result);
    assert_eq!(report["ok"], true);
    assert_eq!(report["predicate"], "element");
    assert_eq!(harness.calls("get_window_state").len(), 3);

    let found = harness
        .wait(json!({"app": "Demo", "role": "button", "name": "Save"}))
        .await;
    let report = structured(&found);
    assert_eq!(report["element"]["label"], "Save", "{report}");
    assert!(report["element"]["element_token"]
        .as_str()
        .unwrap()
        .ends_with(":1"));
    assert_eq!(report["window"]["window_id"], 7);
}

#[tokio::test]
async fn a_window_wait_needs_no_window_id_and_sees_it_close() {
    let harness = Harness::new(App {
        dialog_after_lists: 2,
        main_closes_after_lists: Some(4),
        ..Default::default()
    });
    let opened = harness
        .wait(json!({"app": "Demo", "window": "export"}))
        .await;
    let report = structured(&opened);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["window"]["window_id"], 8);
    assert_eq!(report["window"]["title"], "Export");

    let closed = harness
        .wait(json!({"pid": 42, "window_id": 7, "gone": true}))
        .await;
    assert_eq!(structured(&closed)["ok"], true, "{:?}", structured(&closed));
}

#[tokio::test]
async fn a_surface_wait_reads_list_surfaces() {
    let harness = Harness::new(App {
        menu_after_lists: 1,
        ..Default::default()
    });
    let result = harness.wait(json!({"pid": 42, "surface": "menu"})).await;
    let report = structured(&result);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["surface"]["kind"], "context_menu");
    assert_eq!(harness.calls("list_surfaces").len(), 2);
    // A pid alone needs no window lookup.
    assert!(harness.calls("list_windows").is_empty());
}

#[tokio::test]
async fn a_wait_that_never_holds_times_out_bounded_without_fast_polling() {
    let harness = Harness::new(App::default());
    let started = Instant::now();
    let result = harness
        .wait(json!({"app": "Demo", "window": "Never", "timeout_ms": 700}))
        .await;
    let elapsed = started.elapsed();
    assert_eq!(result.is_error, Some(true));
    let report = structured(&result);
    assert_eq!(report["code"], "timeout", "{report}");
    assert_eq!(report["last_code"], "window_not_found");
    assert!(elapsed < Duration::from_millis(1_500), "{elapsed:?}");
    let polls = harness.calls("list_windows");
    assert!(
        polls.len() >= 2 && polls.len() <= 4,
        "{} polls",
        polls.len()
    );
    for pair in polls.windows(2) {
        assert!(pair[1] - pair[0] >= Duration::from_millis(100));
    }
}

#[tokio::test]
async fn a_wait_that_cannot_be_answered_stops_at_once() {
    // A Linux or Windows registry has no list_surfaces: the wait fails
    // closed instead of polling a refusal for the whole timeout.
    let harness = Harness::new(App::default());
    let mut registry = ToolRegistry::new();
    registry.register_session_tools();
    let registry = Arc::new(registry);
    registry.init_self_weak();
    let started = Instant::now();
    let result = registry
        .invoke(
            WAIT_TOOL,
            json!({"pid": 42, "surface": "sheet", "timeout_ms": 5_000}),
        )
        .await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(structured(&result)["code"], "lookup_failed");
    drop(harness);

    let invalid = registry.invoke(WAIT_TOOL, json!({"text": "Saved"})).await;
    assert_eq!(structured(&invalid)["code"], "invalid_arguments");
}

#[tokio::test]
async fn a_wait_is_a_run_steps_step_that_inherits_the_batch_window() {
    let harness = Harness::new(App {
        saved_after_reads: 1,
        dialog_after_lists: 1,
        ..Default::default()
    });
    let result = harness
        .registry
        .invoke(
            crate::batch_tools::RUN_STEPS_TOOL,
            json!({
                "pid": 42, "window_id": 7,
                "steps": [
                    {"click": {"element_token": "s00000001:1"}},
                    {"wait": {"text": "Saved"}},
                    {"tool": "wait", "args": {"app": "Demo", "window": "Export"}},
                    {"wait": {"ms": 10}}
                ]
            }),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let report = structured(&result);
    assert_eq!(report["executed"], 4, "{report}");
    assert_eq!(report["steps"][1]["tool"], "wait");
    assert_eq!(report["steps"][1]["predicate"], "element");
    assert_eq!(report["steps"][2]["window"]["window_id"], 8);

    let refused = harness
        .registry
        .invoke(
            crate::batch_tools::RUN_STEPS_TOOL,
            json!({"steps": [{"wait": {"surface": "menu"}}]}),
        )
        .await;
    assert_eq!(structured(&refused)["code"], "invalid_batch");
}
