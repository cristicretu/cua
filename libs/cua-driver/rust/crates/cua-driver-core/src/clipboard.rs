//! Cross-platform clipboard tools and platform backend seam.

use std::sync::Arc;

use async_trait::async_trait;
use cua_driver_contract::{
    ClipboardReadInput, ClipboardReadOutput, ClipboardWriteInput, ClipboardWriteOutput,
};
use serde_json::Value;

use crate::{
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
    tool_args::parse_typed_input,
};

pub trait ClipboardBackend: Send + Sync {
    fn available_formats(&self) -> Result<Vec<String>, String>;
    fn read_text(&self) -> Result<Option<String>, String>;
    fn write_text(&self, text: String) -> Result<(), String>;
    fn write_image(&self, absolute_path: &str) -> Result<(), String>;
    fn write_file_url(&self, absolute_path: &str) -> Result<(), String>;
    /// Whether [`ClipboardBackend::clear`] is implemented. `clipboard_clear`
    /// is registered only for backends that say so, so a platform without it
    /// does not advertise a tool that always fails.
    fn supports_clear(&self) -> bool {
        false
    }
    /// Remove every item from the system clipboard.
    fn clear(&self) -> Result<(), String> {
        Err("clearing the clipboard is not implemented on this platform".to_owned())
    }
}

pub fn register_clipboard_tools(registry: &mut ToolRegistry, backend: Arc<dyn ClipboardBackend>) {
    registry.register(Box::new(ClipboardReadTool {
        backend: backend.clone(),
    }));
    if backend.supports_clear() {
        registry.register(Box::new(ClipboardClearTool {
            backend: backend.clone(),
        }));
    }
    registry.register(Box::new(ClipboardWriteTool { backend }));
}

fn contract_def(name: &str) -> ToolDef {
    let contract = cua_driver_contract::tool_contract(name)
        .unwrap_or_else(|| panic!("missing {name} clipboard contract"));
    ToolDef::from_contract(&contract)
}

fn normalize_types(mut types: Vec<String>) -> Vec<String> {
    types.sort();
    types.dedup();
    types
}

fn unavailable(operation: &str, error: impl Into<String>) -> ToolResult {
    let message = error.into();
    ToolResult::error(format!("Clipboard {operation} is unavailable: {message}")).with_structured(
        serde_json::json!({
            "supported": false,
            "capability": format!("clipboard.{operation}"),
            "status": "unavailable",
            "error_code": "clipboard_unavailable",
            "privacy_sensitive": true,
            "content_redacted_from_telemetry": true,
        }),
    )
}

pub struct ClipboardReadTool {
    backend: Arc<dyn ClipboardBackend>,
}

static READ_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ClipboardReadTool {
    fn def(&self) -> &ToolDef {
        READ_DEF.get_or_init(|| contract_def("clipboard_read"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let input = match parse_typed_input::<ClipboardReadInput>("clipboard_read", args) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let backend = self.backend.clone();
        let result = tokio::task::spawn_blocking(move || {
            let types = normalize_types(backend.available_formats()?);
            let text = if input.include_text {
                backend.read_text()?
            } else {
                None
            };
            Ok::<_, String>((types, text))
        })
        .await;

        let (types, text) = match result {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return unavailable("read", error),
            Err(error) => return unavailable("read", format!("worker failed: {error}")),
        };
        let output = ClipboardReadOutput {
            supported: true,
            types,
            text,
            privacy_sensitive: true,
            content_redacted_from_telemetry: true,
        };
        ToolResult::text(format!(
            "Clipboard read succeeded ({} available type(s); text {}).",
            output.types.len(),
            if input.include_text {
                "requested"
            } else {
                "omitted"
            }
        ))
        .with_structured(serde_json::to_value(output).expect("clipboard output serializes"))
    }
}

pub struct ClipboardWriteTool {
    backend: Arc<dyn ClipboardBackend>,
}

static WRITE_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ClipboardWriteTool {
    fn def(&self) -> &ToolDef {
        WRITE_DEF.get_or_init(|| contract_def("clipboard_write"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let input = match parse_typed_input::<ClipboardWriteInput>("clipboard_write", args) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let supplied = usize::from(input.text.is_some())
            + usize::from(input.image_path.is_some())
            + usize::from(input.file_path.is_some());
        if supplied != 1 {
            return ToolResult::error(
                "clipboard_write requires exactly one of text, image_path, or file_path",
            );
        }

        let backend = self.backend.clone();
        let result = tokio::task::spawn_blocking(move || {
            let written_type = if let Some(text) = input.text {
                backend.write_text(text)?;
                "text"
            } else if let Some(path) = input.image_path {
                backend.write_image(&path)?;
                "image"
            } else if let Some(path) = input.file_path {
                backend.write_file_url(&path)?;
                "file_url"
            } else {
                unreachable!("exactly one clipboard value was checked above")
            };
            let types = normalize_types(backend.available_formats()?);
            Ok::<_, String>((written_type, types))
        })
        .await;

        let (written_type, types) = match result {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return unavailable("write", error),
            Err(error) => return unavailable("write", format!("worker failed: {error}")),
        };
        let output = ClipboardWriteOutput {
            supported: true,
            written_type: written_type.into(),
            types,
            privacy_sensitive: true,
            content_redacted_from_telemetry: true,
        };
        ToolResult::text(format!(
            "Clipboard write succeeded as {written_type}; inspect structuredContent.types before paste."
        ))
        .with_structured(serde_json::to_value(output).expect("clipboard output serializes"))
    }
}

/// `clipboard_clear` — empty the system clipboard and prove it is empty.
///
/// Runtime-only (not in the portable contract): agents that put a secret or a
/// large payload on the clipboard to paste it can remove it afterwards.
/// The readback is the format list, so no content is read.
pub struct ClipboardClearTool {
    backend: Arc<dyn ClipboardBackend>,
}

static CLEAR_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn clear_def() -> &'static ToolDef {
    CLEAR_DEF.get_or_init(|| ToolDef {
        name: "clipboard_clear".into(),
        description: "Empty the system clipboard, then verify no types remain. Reads no \
            clipboard content."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "session": crate::tool_schema::session_schema() },
            "additionalProperties": false,
        }),
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ClipboardClearTool {
    fn def(&self) -> &ToolDef {
        clear_def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Some(unknown) = args.as_object().and_then(|map| {
            map.keys()
                .find(|key| !key.starts_with('_') && *key != "session")
        }) {
            return ToolResult::error(format!("clipboard_clear: unknown argument `{unknown}`"));
        }
        let backend = self.backend.clone();
        let result = tokio::task::spawn_blocking(move || {
            backend.clear()?;
            Ok::<_, String>(normalize_types(backend.available_formats()?))
        })
        .await;
        let remaining = match result {
            Ok(Ok(types)) => types,
            Ok(Err(error)) => return unavailable("clear", error),
            Err(error) => return unavailable("clear", format!("worker failed: {error}")),
        };
        let cleared = remaining.is_empty();
        let structured = serde_json::json!({
            "supported": true,
            "cleared": cleared,
            "types": remaining,
            "privacy_sensitive": true,
            "content_redacted_from_telemetry": true,
        });
        if cleared {
            ToolResult::text("Clipboard cleared; no types remain.").with_structured(structured)
        } else {
            ToolResult::error(format!(
                "Clipboard clear ran, but {} type(s) remain (another process may have written \
                 to it).",
                remaining.len()
            ))
            .with_structured(structured)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeClipboard {
        value: Mutex<Option<(String, String)>>,
    }

    struct UnavailableClipboard;

    impl ClipboardBackend for UnavailableClipboard {
        fn available_formats(&self) -> Result<Vec<String>, String> {
            Err("no display clipboard provider".into())
        }

        fn read_text(&self) -> Result<Option<String>, String> {
            Err("no display clipboard provider".into())
        }

        fn write_text(&self, _text: String) -> Result<(), String> {
            Err("no display clipboard provider".into())
        }

        fn write_image(&self, _absolute_path: &str) -> Result<(), String> {
            Err("no display clipboard provider".into())
        }

        fn write_file_url(&self, _absolute_path: &str) -> Result<(), String> {
            Err("no display clipboard provider".into())
        }
    }

    impl ClipboardBackend for FakeClipboard {
        fn available_formats(&self) -> Result<Vec<String>, String> {
            Ok(self
                .value
                .lock()
                .unwrap()
                .as_ref()
                .map(|(kind, _)| vec![kind.clone(), kind.clone()])
                .unwrap_or_default())
        }

        fn read_text(&self) -> Result<Option<String>, String> {
            Ok(self
                .value
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(kind, _)| kind == "text/plain")
                .map(|(_, value)| value.clone()))
        }

        fn write_text(&self, text: String) -> Result<(), String> {
            *self.value.lock().unwrap() = Some(("text/plain".into(), text));
            Ok(())
        }

        fn write_image(&self, absolute_path: &str) -> Result<(), String> {
            *self.value.lock().unwrap() = Some(("image/png".into(), absolute_path.into()));
            Ok(())
        }

        fn write_file_url(&self, absolute_path: &str) -> Result<(), String> {
            *self.value.lock().unwrap() = Some(("text/uri-list".into(), absolute_path.into()));
            Ok(())
        }

        fn supports_clear(&self) -> bool {
            true
        }

        fn clear(&self) -> Result<(), String> {
            *self.value.lock().unwrap() = None;
            Ok(())
        }
    }

    /// A clipboard another process writes to again right after a clear.
    struct RefillingClipboard;

    impl ClipboardBackend for RefillingClipboard {
        fn available_formats(&self) -> Result<Vec<String>, String> {
            Ok(vec!["text/plain".into()])
        }
        fn read_text(&self) -> Result<Option<String>, String> {
            panic!("clipboard_clear must never read content")
        }
        fn write_text(&self, _text: String) -> Result<(), String> {
            unreachable!()
        }
        fn write_image(&self, _absolute_path: &str) -> Result<(), String> {
            unreachable!()
        }
        fn write_file_url(&self, _absolute_path: &str) -> Result<(), String> {
            unreachable!()
        }
        fn supports_clear(&self) -> bool {
            true
        }
        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn clear_empties_the_clipboard_and_verifies_by_types() {
        let backend: Arc<dyn ClipboardBackend> = Arc::new(FakeClipboard::default());
        backend.write_text("secret".into()).unwrap();
        let result = ClipboardClearTool {
            backend: backend.clone(),
        }
        .invoke(serde_json::json!({"session": "s"}))
        .await;
        assert_ne!(result.is_error, Some(true));
        let output = result.structured_content.unwrap();
        assert_eq!(output["cleared"], true);
        assert_eq!(output["types"], serde_json::json!([]));
        assert!(!output.to_string().contains("secret"));
        assert!(backend.available_formats().unwrap().is_empty());
    }

    #[tokio::test]
    async fn clear_reports_remaining_types_instead_of_claiming_success() {
        let result = ClipboardClearTool {
            backend: Arc::new(RefillingClipboard),
        }
        .invoke(serde_json::json!({}))
        .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content.unwrap()["cleared"], false);
    }

    #[tokio::test]
    async fn clear_is_unavailable_on_backends_without_it_and_rejects_unknown_args() {
        let result = ClipboardClearTool {
            backend: Arc::new(UnavailableClipboard),
        }
        .invoke(serde_json::json!({}))
        .await;
        assert_eq!(
            result.structured_content.unwrap()["error_code"],
            "clipboard_unavailable"
        );
        let result = ClipboardClearTool {
            backend: Arc::new(FakeClipboard::default()),
        }
        .invoke(serde_json::json!({"text": "x"}))
        .await;
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn clear_is_registered_only_for_backends_that_support_it() {
        let mut registry = ToolRegistry::new();
        register_clipboard_tools(&mut registry, Arc::new(UnavailableClipboard));
        assert!(registry.get_def("clipboard_clear").is_none());
        let mut registry = ToolRegistry::new();
        register_clipboard_tools(&mut registry, Arc::new(FakeClipboard::default()));
        assert!(registry.get_def("clipboard_clear").is_some());
    }

    #[tokio::test]
    async fn text_round_trip_returns_types_and_privacy_metadata() {
        let backend: Arc<dyn ClipboardBackend> = Arc::new(FakeClipboard::default());
        let write = ClipboardWriteTool {
            backend: backend.clone(),
        }
        .invoke(serde_json::json!({"text": "private value"}))
        .await;
        assert_ne!(write.is_error, Some(true));
        assert_eq!(
            write.structured_content.as_ref().unwrap()["types"],
            serde_json::json!(["text/plain"])
        );

        let read = ClipboardReadTool { backend }
            .invoke(serde_json::json!({"include_text": true}))
            .await;
        let output = read.structured_content.unwrap();
        assert_eq!(output["text"], "private value");
        assert_eq!(output["privacy_sensitive"], true);
        assert_eq!(output["content_redacted_from_telemetry"], true);
    }

    #[tokio::test]
    async fn write_requires_exactly_one_value() {
        for args in [
            serde_json::json!({}),
            serde_json::json!({"text": "x", "file_path": "/tmp/x"}),
        ] {
            let result = ClipboardWriteTool {
                backend: Arc::new(FakeClipboard::default()),
            }
            .invoke(args)
            .await;
            assert_eq!(result.is_error, Some(true));
        }
    }

    #[tokio::test]
    async fn unavailable_backend_returns_an_explicit_content_free_status() {
        let result = ClipboardReadTool {
            backend: Arc::new(UnavailableClipboard),
        }
        .invoke(serde_json::json!({}))
        .await;
        assert_eq!(result.is_error, Some(true));
        let output = result.structured_content.unwrap();
        assert_eq!(output["supported"], false);
        assert_eq!(output["status"], "unavailable");
        assert_eq!(output["error_code"], "clipboard_unavailable");
        assert_eq!(output["content_redacted_from_telemetry"], true);
        assert!(output
            .to_string()
            .find("no display clipboard provider")
            .is_none());
    }
}
