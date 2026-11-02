//! macOS `list_displays` — every active display with its id, bounds and scale.
//!
//! `get_screen_size` reports only the main display. Agents working on a
//! multi-monitor desktop need the other displays' ids (for
//! `get_desktop_state({display_id})`) and their global bounds (to place or
//! maximize windows). Read-only and permission-free: CoreGraphics display
//! queries need no TCC grant and touch no window.
//!
//! Ordering and display selection follow agent-desktop's display inventory
//! (crates/macos/src/system/display.rs, Apache-2.0): primary first, then by id.

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
    window_state_change::Rect,
};
use serde_json::Value;

pub struct ListDisplaysTool;

/// One active display. Bounds are global top-left points, the coordinate
/// space `list_windows` and `set_window_frame` use.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DisplayInfo {
    pub id: u32,
    pub bounds: Rect,
    pub pixel_width: u64,
    pub pixel_height: u64,
    pub scale_factor: f64,
    pub primary: bool,
    /// The area not covered by the menu bar and Dock, from AppKit. `None`
    /// when AppKit's screen list disagrees with CoreGraphics (a stale cache
    /// in a daemon) or does not list the display.
    pub visible_bounds: Option<Rect>,
}

impl DisplayInfo {
    pub fn to_json(&self) -> Value {
        let rect = |rect: Rect| {
            serde_json::json!({
                "x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height,
            })
        };
        serde_json::json!({
            "id": self.id.to_string(),
            "primary": self.primary,
            "bounds": rect(self.bounds),
            "visible_bounds": self.visible_bounds.map(rect),
            "pixel_width": self.pixel_width,
            "pixel_height": self.pixel_height,
            "scale_factor": self.scale_factor,
        })
    }
}

/// Primary first, then ascending id, so the order is stable across calls.
pub(crate) fn order_displays(displays: &mut [DisplayInfo]) {
    displays.sort_by_key(|display| (!display.primary, display.id));
}

/// Resolve a caller's `display_id` ("primary" or a decimal id from
/// `list_displays`) against the inventory.
pub(crate) fn select_display<'a>(
    displays: &'a [DisplayInfo],
    requested: &str,
) -> Result<&'a DisplayInfo, String> {
    let requested = requested.trim();
    if requested.eq_ignore_ascii_case("primary") {
        return displays
            .iter()
            .find(|display| display.primary)
            .ok_or_else(|| "no primary display is active".to_owned());
    }
    let id: u32 = requested
        .parse()
        .map_err(|_| format!("display_id {requested:?} is neither \"primary\" nor a display id from list_displays"))?;
    displays
        .iter()
        .find(|display| display.id == id)
        .ok_or_else(|| format!("display {id} is not active; call list_displays for current ids"))
}

/// AppKit's visible frame for `id`, converted to top-left points, but only
/// when AppKit's full frame for that screen agrees with CoreGraphics. NSScreen
/// updates its list from a run-loop notification, so a daemon can hold a
/// stale copy after a display change; a disagreeing frame is treated as
/// unknown rather than trusted.
pub(crate) fn trusted_visible_bounds(
    cg_bounds: Rect,
    appkit_frame: Rect,
    appkit_visible: Rect,
) -> Option<Rect> {
    (appkit_frame.approximately_eq(cg_bounds, 1.0)
        && appkit_visible.is_valid()
        && appkit_visible.intersection_area(cg_bounds) > 0.0)
        .then_some(appkit_visible)
}

/// `(display id, frame, visible frame)` for every NSScreen, in top-left
/// points relative to the main display.
fn appkit_screens(main_height: f64) -> Vec<(u32, Rect, Rect)> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSRect, NSString};

    let flip = |rect: NSRect| Rect {
        x: rect.origin.x,
        y: main_height - (rect.origin.y + rect.size.height),
        width: rect.size.width,
        height: rect.size.height,
    };
    let mut out = Vec::new();
    objc2::rc::autoreleasepool(|_| unsafe {
        // SAFETY: NSScreen's class methods and properties are read-only
        // queries; agent-desktop reads the same values off the main thread
        // (crates/macos/src/system/screen_bridge.m). Every pointer is checked
        // before use and nothing is retained past the pool.
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            return;
        }
        let count: usize = msg_send![screens, count];
        let key = NSString::from_str("NSScreenNumber");
        for index in 0..count {
            let screen: *mut AnyObject = msg_send![screens, objectAtIndex: index];
            if screen.is_null() {
                continue;
            }
            let description: *mut AnyObject = msg_send![screen, deviceDescription];
            if description.is_null() {
                continue;
            }
            let number: *mut AnyObject = msg_send![description, objectForKey: &*key];
            if number.is_null() {
                continue;
            }
            let id: u32 = msg_send![number, unsignedIntValue];
            let frame: NSRect = msg_send![screen, frame];
            let visible: NSRect = msg_send![screen, visibleFrame];
            out.push((id, flip(frame), flip(visible)));
        }
    });
    out
}

/// Every active display, ordered by [`order_displays`].
pub(crate) fn active_displays() -> Result<Vec<DisplayInfo>, String> {
    use core_graphics::display::CGDisplay;

    let ids = CGDisplay::active_displays()
        .map_err(|error| format!("CGGetActiveDisplayList failed with CGError {error}"))?;
    let main_id = CGDisplay::main().id;
    let main_height = CGDisplay::main().bounds().size.height;
    let screens = appkit_screens(main_height);
    let mut displays: Vec<DisplayInfo> = ids
        .into_iter()
        .map(|id| {
            let display = CGDisplay::new(id);
            let bounds = display.bounds();
            let bounds = Rect {
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
            };
            let (pixel_width, pixel_height) = display
                .display_mode()
                .map(|mode| (mode.pixel_width(), mode.pixel_height()))
                .unwrap_or((display.pixels_wide(), display.pixels_high()));
            let visible_bounds = screens
                .iter()
                .find(|(screen_id, _, _)| *screen_id == id)
                .and_then(|(_, frame, visible)| trusted_visible_bounds(bounds, *frame, *visible));
            DisplayInfo {
                id,
                bounds,
                pixel_width,
                pixel_height,
                scale_factor: super::get_screen_size::get_backing_scale(id),
                primary: id == main_id,
                visible_bounds,
            }
        })
        .collect();
    if displays.is_empty() {
        return Err("CoreGraphics reports no active displays".to_owned());
    }
    order_displays(&mut displays);
    Ok(displays)
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "list_displays".into(),
        description: "List active displays, primary first: id, primary, global bounds and \
            visible_bounds (minus menu bar and Dock) in points, pixel size and scale_factor. \
            Pass an id to get_desktop_state({display_id}) to capture that display. Requires \
            no TCC permissions."
            .into(),
        input_schema: serde_json::json!({"type":"object","properties":{
            "session": cua_driver_core::tool_schema::session_schema()
        },"additionalProperties":false}),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ListDisplaysTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Some(unknown) = args
            .as_object()
            .and_then(|map| map.keys().find(|key| !key.starts_with('_') && *key != "session"))
        {
            return ToolResult::error(format!("list_displays: unknown argument `{unknown}`"));
        }
        let displays = match tokio::task::spawn_blocking(active_displays).await {
            Ok(Ok(displays)) => displays,
            Ok(Err(error)) => return ToolResult::error(format!("list_displays: {error}")),
            Err(error) => return ToolResult::error(format!("list_displays: worker failed: {error}")),
        };
        let mut text = format!("{} display(s):", displays.len());
        for display in &displays {
            text.push_str(&format!(
                "\n- {}{}: {}x{} pts at ({}, {}) @ {}x",
                display.id,
                if display.primary { " (primary)" } else { "" },
                display.bounds.width,
                display.bounds.height,
                display.bounds.x,
                display.bounds.y,
                display.scale_factor,
            ));
        }
        ToolResult::text(text).with_structured(serde_json::json!({
            "displays": displays.iter().map(DisplayInfo::to_json).collect::<Vec<_>>(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, primary: bool, x: f64) -> DisplayInfo {
        DisplayInfo {
            id,
            bounds: Rect {
                x,
                y: 0.0,
                width: 1512.0,
                height: 982.0,
            },
            pixel_width: 3024,
            pixel_height: 1964,
            scale_factor: 2.0,
            primary,
            visible_bounds: None,
        }
    }

    #[test]
    fn primary_comes_first_then_ascending_id() {
        let mut displays = vec![
            display(9, false, 3000.0),
            display(4, false, 1512.0),
            display(7, true, 0.0),
        ];
        order_displays(&mut displays);
        let ids: Vec<u32> = displays.iter().map(|display| display.id).collect();
        assert_eq!(ids, [7, 4, 9]);
    }

    #[test]
    fn display_ids_resolve_exactly() {
        let displays = vec![display(7, true, 0.0), display(4, false, 1512.0)];
        assert_eq!(select_display(&displays, "primary").unwrap().id, 7);
        assert_eq!(select_display(&displays, "PRIMARY").unwrap().id, 7);
        assert_eq!(select_display(&displays, "4").unwrap().id, 4);
        assert!(select_display(&displays, "5").is_err());
        assert!(select_display(&displays, "secondary").is_err());
        assert!(select_display(&displays, "-4").is_err());
        assert!(select_display(&[display(4, false, 0.0)], "primary").is_err());
    }

    #[test]
    fn appkit_visible_frames_are_trusted_only_when_appkit_agrees_with_coregraphics() {
        let cg = Rect {
            x: 0.0,
            y: 0.0,
            width: 1512.0,
            height: 982.0,
        };
        let visible = Rect {
            x: 0.0,
            y: 33.0,
            width: 1512.0,
            height: 880.0,
        };
        assert_eq!(trusted_visible_bounds(cg, cg, visible), Some(visible));
        let stale = Rect {
            width: 1440.0,
            height: 900.0,
            ..cg
        };
        assert_eq!(trusted_visible_bounds(cg, stale, visible), None);
        let empty = Rect {
            height: 0.0,
            ..visible
        };
        assert_eq!(trusted_visible_bounds(cg, cg, empty), None);
    }

    #[test]
    fn json_shape_reports_string_ids_and_both_rects() {
        let mut info = display(7, true, 0.0);
        info.visible_bounds = Some(Rect {
            x: 0.0,
            y: 33.0,
            width: 1512.0,
            height: 880.0,
        });
        let json = info.to_json();
        assert_eq!(json["id"], "7");
        assert_eq!(json["primary"], true);
        assert_eq!(json["bounds"]["width"], 1512.0);
        assert_eq!(json["visible_bounds"]["y"], 33.0);
        assert_eq!(json["scale_factor"], 2.0);
    }

    #[test]
    fn schema_is_read_only_and_closed() {
        let def = def();
        assert!(def.read_only);
        assert!(!def.destructive);
        assert_eq!(def.input_schema["additionalProperties"], false);
    }
}
