//! `get_desktop_state` — full-display vision screenshot (macOS).
//!
//! Vision-only desktop capture: grabs the ENTIRE main display at native
//! pixel size (no downscale) so screen-absolute pixel picks land exactly,
//! then reports the true screen size + backing scale. No AX walk, no
//! pid/window_id. This is the capture surface for actions with a primary-display
//! desktop target and screen-absolute coordinates.
//!
//! Mirrors `get_window_state.rs`'s vision ToolResult shape: an `image_png`
//! content part (or a written-out file path), a text summary line, and a
//! `structuredContent` object.

use async_trait::async_trait;
use cua_driver_contract::GetDesktopStateInput;
use cua_driver_core::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef},
    tool_args::parse_typed_input,
};
use serde_json::Value;
use std::sync::Arc;

use super::{get_screen_size::main_screen_size, ToolState};

pub struct GetDesktopStateTool {
    state: Arc<ToolState>,
}

impl GetDesktopStateTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "get_desktop_state".into(),
        description: "Capture the full display (the primary one unless `display_id` names \
            another from list_displays). The long edge is capped at \
            `max_image_dimension` (the configured default unless given); pixels of the \
            returned image map back to the screen through its capture. Use its PNG as the coordinate source for actions \
            whose target is {kind:\"desktop\",display_id:\"primary\"}. Returns the true \
            screen size and backing scale factor. Vision-only: no AX tree walk."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session": { "type": "string", "description": "For multi-call work, prefer a short public session label and repeat it on every call that accepts it. Omit it to use the authenticated transport's implicit lifecycle session." },
                "screenshot_out_file": { "type": "string", "description": "Write PNG here instead of base64." },
                "max_image_dimension": cua_driver_core::tool_schema::desktop_max_image_dimension_schema(),
                "display_id": { "type": "string", "description": "\"primary\" (default) or an id from list_displays. A non-primary capture is view-only: it has no capture_id, and desktop pixel actions still address the primary display." }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: false,
        open_world: false,
    })
}

#[async_trait]
impl Tool for GetDesktopStateTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, mut args: Value) -> ToolResult {
        // `display_id` is a macOS runtime extension, not part of the portable
        // typed contract (whose SDK records are generated), so it is taken
        // out before the typed parse.
        let display_id = match args.as_object_mut().and_then(|map| map.remove("display_id")) {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id),
            Some(Value::Number(id)) => Some(id.to_string()),
            Some(_) => {
                return ToolResult::error(
                    "get_desktop_state: `display_id` must be \"primary\" or an id from list_displays",
                )
            }
        };
        let capture_args = args.clone();
        let input = match parse_typed_input::<GetDesktopStateInput>("get_desktop_state", args) {
            Ok(input) => input,
            Err(result) => return result,
        };
        // Call arg > session override > global config, as get_window_state.
        let max_image_dimension = input.max_image_dimension.or_else(|| {
            use cua_driver_core::tool_args::ArgsExt;
            let session_id = capture_args.opt_str("_session_id");
            let cfg = self.state.config.read().unwrap();
            Some(
                self.state
                    .session_config
                    .effective_max_image_dimension(session_id.as_deref(), &cfg),
            )
        });
        let max_image_dimension = max_image_dimension.filter(|cap| *cap > 0);
        let screenshot_out_file = input.screenshot_out_file.map(|s| {
            // Expand ~ prefix (mirrors get_window_state).
            if let Some(relative) = s.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/{relative}")
            } else {
                s
            }
        });

        if let Some(requested) = display_id {
            let displays = match tokio::task::spawn_blocking(
                super::list_displays::active_displays,
            )
            .await
            {
                Ok(Ok(displays)) => displays,
                Ok(Err(error)) => return ToolResult::error(format!("get_desktop_state: {error}")),
                Err(error) => {
                    return ToolResult::error(format!("get_desktop_state: worker failed: {error}"))
                }
            };
            let display = match super::list_displays::select_display(&displays, &requested) {
                Ok(display) => display.clone(),
                Err(message) => {
                    let message = format!("get_desktop_state: {message}");
                    return ToolResult::error(message.clone()).with_structured(serde_json::json!({
                        "status": "refused",
                        "refusal": { "code": "display_not_found", "message": message },
                    }));
                }
            };
            if !display.primary {
                return capture_secondary_display(
                    display,
                    max_image_dimension,
                    screenshot_out_file,
                )
                .await;
            }
        }

        // True screen geometry (points + backing scale). Safe off the main thread.
        let (screen_width, screen_height, scale_factor) = match main_screen_size() {
            Some(t) => t,
            None => return ToolResult::error("No main display detected."),
        };

        // Capture the FULL display at native size — no resize. Run the
        // blocking display capture off the async runtime.
        let out_file = screenshot_out_file.clone();
        let res = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(
                Vec<u8>,
                Option<String>,
                u32,
                u32,
                (u32, u32),
                cursor_overlay::capture_exclusion::AgentOverlayCapture,
            )> {
                // The agent reads this image, so the Driver's own cursor and
                // session pill must not sit over the controls it is reading.
                let (png, overlay_capture) =
                    crate::capture::screenshot_display_bytes_excluding_overlay()?;
                let full = crate::capture::png_dimensions(&png)?;
                // Capped image: later desktop-scope pixels from it are
                // mapped back at dispatch and by its capture_id.
                let png = match max_image_dimension {
                    Some(cap) if full.0.max(full.1) > cap => {
                        crate::capture::resize_png_if_needed(&png, cap)?
                    }
                    _ => png,
                };
                let (w, h) = crate::capture::png_dimensions(&png)?;
                if let Some(ref path) = out_file {
                    std::fs::write(path, &png)?;
                    Ok((png, Some(path.clone()), w, h, full, overlay_capture))
                } else {
                    Ok((png, None, w, h, full, overlay_capture))
                }
            },
        )
        .await;

        let (png, file_path, screenshot_width, screenshot_height, full_size, overlay_capture) =
            match res {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return ToolResult::error(format!("Desktop screenshot failed: {e}")),
                Err(e) => return ToolResult::error(format!("Desktop screenshot task error: {e}")),
            };

        let native_width = match u32::try_from(screen_width) {
            Ok(width) => width,
            Err(_) => return ToolResult::error("Desktop screen width is out of range."),
        };
        let native_height = match u32::try_from(screen_height) {
            Ok(height) => height,
            Err(_) => return ToolResult::error("Desktop screen height is out of range."),
        };
        let capture_id = match self.state.capture_bindings.publish_desktop(
            &capture_args,
            png.clone(),
            (screenshot_width, screenshot_height),
            (native_width, native_height),
        ) {
            Ok(capture_id) => capture_id,
            Err(error) => return error,
        };

        let mut content: Vec<Content> = Vec::new();
        if file_path.is_none() {
            use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
            content.push(Content::image_png(BASE64.encode(&png)));
        }
        let mut summary = format!(
            "desktop screenshot {screenshot_width}x{screenshot_height} px \
             (screen {screen_width}x{screen_height} pts @ {scale_factor}x)"
        );
        cursor_overlay::capture_exclusion::append_summary_note(&mut summary, &overlay_capture);
        content.push(Content::text(summary));

        let mut structured = serde_json::json!({
            "platform": "macos",
            "display": "primary",
            "screenshot_width": screenshot_width,
            "screenshot_height": screenshot_height,
            "screen_width": screen_width,
            "screen_height": screen_height,
            "scale_factor": scale_factor,
            "screenshot_mime_type": "image/png",
            "capture_id": capture_id,
            "agent_overlay_capture": overlay_capture,
        });
        if full_size != (screenshot_width, screenshot_height) {
            structured["screenshot_original_width"] = serde_json::json!(full_size.0);
            structured["screenshot_original_height"] = serde_json::json!(full_size.1);
        }
        if let Some(ref fp) = file_path {
            structured["screenshot_file_path"] = serde_json::json!(fp);
        }

        ToolResult {
            content,
            is_error: None,
            structured_content: Some(structured),
            action_record: None,
        }
    }
}

/// View-only capture of a non-primary display.
///
/// Desktop pixel actions accept only `display_id:"primary"`
/// (`action_target.rs`), so this image publishes no capture binding: a
/// capture_id from it would let an action map its pixels onto the primary
/// display. The structured result carries the display's global bounds so the
/// caller can relate the pixels to `list_windows` coordinates.
async fn capture_secondary_display(
    display: super::list_displays::DisplayInfo,
    max_image_dimension: Option<u32>,
    screenshot_out_file: Option<String>,
) -> ToolResult {
    let display_id = display.id;
    let out_file = screenshot_out_file.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let (png, overlay_capture) =
            crate::capture::screenshot_display_by_id_excluding_overlay(display_id)?;
        let full = crate::capture::png_dimensions(&png)?;
        let png = match max_image_dimension {
            Some(cap) if full.0.max(full.1) > cap => crate::capture::resize_png_if_needed(&png, cap)?,
            _ => png,
        };
        let (w, h) = crate::capture::png_dimensions(&png)?;
        if let Some(ref path) = out_file {
            std::fs::write(path, &png)?;
        }
        Ok((png, w, h, full, overlay_capture))
    })
    .await;
    let (png, screenshot_width, screenshot_height, full_size, overlay_capture) = match res {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            return ToolResult::error(format!(
                "Desktop screenshot of display {display_id} failed: {error}"
            ))
        }
        Err(error) => return ToolResult::error(format!("Desktop screenshot task error: {error}")),
    };

    let mut content: Vec<Content> = Vec::new();
    if screenshot_out_file.is_none() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        content.push(Content::image_png(BASE64.encode(&png)));
    }
    let bounds = display.bounds;
    let mut summary = format!(
        "display {display_id} screenshot {screenshot_width}x{screenshot_height} px \
         (screen {}x{} pts at ({}, {}) @ {}x); view-only, no capture_id",
        bounds.width, bounds.height, bounds.x, bounds.y, display.scale_factor
    );
    cursor_overlay::capture_exclusion::append_summary_note(&mut summary, &overlay_capture);
    content.push(Content::text(summary));

    let mut structured = serde_json::json!({
        "platform": "macos",
        "display": display_id.to_string(),
        "screenshot_width": screenshot_width,
        "screenshot_height": screenshot_height,
        "screen_width": bounds.width as u64,
        "screen_height": bounds.height as u64,
        "scale_factor": display.scale_factor,
        "screenshot_mime_type": "image/png",
        "agent_overlay_capture": overlay_capture,
        "display_bounds": {
            "x": bounds.x, "y": bounds.y, "width": bounds.width, "height": bounds.height,
        },
        "view_only": true,
    });
    if full_size != (screenshot_width, screenshot_height) {
        structured["screenshot_original_width"] = serde_json::json!(full_size.0);
        structured["screenshot_original_height"] = serde_json::json!(full_size.1);
    }
    if let Some(ref path) = screenshot_out_file {
        structured["screenshot_file_path"] = serde_json::json!(path);
    }
    ToolResult {
        content,
        is_error: None,
        structured_content: Some(structured),
        action_record: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_no_pid_or_window_id_and_is_read_only() {
        let d = def();
        assert!(d.read_only, "get_desktop_state must be read_only");
        assert!(!d.destructive);
        assert!(!d.idempotent);
        assert!(!d.open_world);

        let props = d.input_schema["properties"].as_object().unwrap();
        assert!(!props.contains_key("pid"), "must not accept pid");
        assert!(
            !props.contains_key("window_id"),
            "must not accept window_id"
        );
        assert!(
            !props.contains_key("capture_mode"),
            "must not accept capture_mode"
        );
        assert!(props.contains_key("session"));
        assert!(props.contains_key("screenshot_out_file"));
        assert_eq!(props["display_id"]["type"], "string");
        assert_eq!(
            d.input_schema["additionalProperties"],
            serde_json::json!(false)
        );
    }

    #[tokio::test]
    async fn malformed_display_id_is_rejected_before_capture() {
        let tool = GetDesktopStateTool::new(Arc::new(ToolState::default()));
        let result = tool
            .invoke(serde_json::json!({"display_id": {"id": 1}}))
            .await;
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn description_mentions_full_and_screen_or_display() {
        let desc = def().description.to_lowercase();
        assert!(desc.contains("full"), "description must mention 'full'");
        assert!(
            desc.contains("screen") || desc.contains("display"),
            "description must mention 'screen' or 'display'"
        );
    }
}
