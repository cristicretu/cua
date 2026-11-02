//! `list_surfaces`: the transient surfaces an app has open (menus, sheets,
//! popovers, alerts) and its focused element. See [`crate::ax::surfaces`].

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::{json, Value};

pub struct ListSurfacesTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Backstop for an inventory whose AX reads stop answering: each read is
/// bounded by its own messaging timeout, so this only catches a stuck call.
const BACKSTOP: std::time::Duration = std::time::Duration::from_secs(10);

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "list_surfaces".into(),
        description: "List the transient surfaces an app has open, without walking a window: the open menu-bar menu (`menu`), a `context_menu`, and per window a `sheet`, `popover` or `alert` (dialog), plus the `focused` element. Each has kind, role, subrole, title, window_id (absent for app-level menus) and item_count for menus. Read one with get_window_state `surface`; wait for one with wait `surface`. Read-only; never activates the app. `complete:false` means the inventory stopped at its node budget.".into(),
        input_schema: json!({
            "type": "object",
            "required": ["pid"],
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it." },
                "pid": { "type": "integer", "description": "Target process ID." },
                "window_id": { "type": "integer", "description": "Only this window's sheets, popovers, alerts and focus; menus are always listed." }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ListSurfacesTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") {
            Ok(pid) => pid,
            Err(error) => return error,
        };
        let window_id = args.opt_i64("window_id").map(|id| id as u32);
        let read =
            tokio::task::spawn_blocking(move || crate::ax::surfaces::list_surfaces(pid, window_id));
        let (surfaces, complete) = match tokio::time::timeout(BACKSTOP, read).await {
            Ok(Ok(listed)) => listed,
            Ok(Err(error)) => return ToolResult::error(format!("list_surfaces failed: {error}")),
            Err(_) => {
                return ToolResult::error(format!(
                    "list_surfaces: pid {pid} did not answer accessibility reads within {} s",
                    BACKSTOP.as_secs()
                ))
            }
        };
        let mut lines = vec![format!("{} surface(s) open for pid {pid}", surfaces.len())];
        lines.extend(surfaces.iter().map(|surface| {
            let mut line = format!("- {} {}", surface.kind, surface.role);
            if let Some(title) = &surface.title {
                line.push_str(&format!(" \"{title}\""));
            }
            if let Some(window_id) = surface.window_id {
                line.push_str(&format!(" window {window_id}"));
            }
            if let Some(count) = surface.item_count {
                line.push_str(&format!(" ({count} items)"));
            }
            line
        }));
        if !complete {
            lines.push("(incomplete: stopped at the node budget)".to_owned());
        }
        ToolResult::text(lines.join("\n")).with_structured(json!({
            "pid": pid,
            "surfaces": surfaces
                .iter()
                .map(crate::ax::surfaces::Surface::to_json)
                .collect::<Vec<_>>(),
            "complete": complete,
        }))
    }
}
