//! An app's open transient surfaces: menu-bar and context menus, sheets,
//! popovers, alerts, and the focused element.
//!
//! Backs `list_surfaces`, the `surface` predicate of `wait`, and
//! `get_window_state`'s `surface` argument, which roots the walk at one of
//! them so a model reads the open menu or sheet without the window around it
//! (or a menu bar it collapsed). Where to look, and how a role/subrole pair
//! classifies, follows agent-desktop's `tree/surfaces.rs` and
//! `tree/surface_inventory.rs` (Apache-2.0); the classification and the
//! choice of root are pure functions below, the AX reads stay thin.
//!
//! Everything here only reads attributes. Nothing activates the app, raises
//! a window or moves focus, so it is safe on a background app.

use core_foundation::base::{CFEqual, CFRelease, CFTypeRef};
use serde_json::{json, Value};

use super::bindings::*;

/// Most AX elements one inventory reads. An app with hundreds of windows or a
/// huge menu bar stops here and says the inventory is incomplete instead of
/// stalling the caller.
const MAX_SURFACE_NODES: usize = 512;
/// Per-element AX messaging timeout: a hung app costs at most this per read.
const MESSAGING_TIMEOUT_SECONDS: f32 = 1.0;

/// `surface` values `get_window_state` accepts. `menu` is the open menu-bar
/// menu or context menu; `focused` the app's focused element in the window.
pub const SURFACE_ARGS: &[&str] = &["menu", "sheet", "popover", "alert", "focused"];

/// One open surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surface {
    /// `menu`, `context_menu`, `sheet`, `popover`, `alert` or `focused`.
    pub kind: &'static str,
    pub role: String,
    pub subrole: Option<String>,
    /// AXTitle, else AXDescription.
    pub title: Option<String>,
    /// The CGWindowID that owns it; `None` for app-level menus.
    pub window_id: Option<u32>,
    /// Item count of a menu.
    pub item_count: Option<usize>,
}

impl Surface {
    pub fn to_json(&self) -> Value {
        let mut out = json!({ "kind": self.kind, "role": self.role });
        if let Some(subrole) = &self.subrole {
            out["subrole"] = json!(subrole);
        }
        if let Some(title) = &self.title {
            out["title"] = json!(title);
        }
        if let Some(window_id) = self.window_id {
            out["window_id"] = json!(window_id);
        }
        if let Some(count) = self.item_count {
            out["item_count"] = json!(count);
        }
        out
    }
}

/// The window-owned surface a role/subrole pair is, if any. A subrole wins
/// over the role: AppKit reports a sheet as `AXSheet` either way, a SwiftUI
/// alert as an `AXWindow` with subrole `AXDialog`.
pub fn window_surface_kind(role: &str, subrole: Option<&str>) -> Option<&'static str> {
    match subrole {
        Some("AXSheet") => Some("sheet"),
        Some("AXPopover") => Some("popover"),
        Some("AXDialog" | "AXSystemDialog" | "AXAlert") => Some("alert"),
        _ => match role {
            "AXSheet" => Some("sheet"),
            "AXPopover" => Some("popover"),
            _ => None,
        },
    }
}

/// Whether a top-level window classified as `kind` is a surface. A dialog
/// subrole alone does not make a window an alert: System Settings' main
/// window is an `AXWindow`/`AXDialog` and would be listed as an alert over
/// itself. A top-level dialog counts unless it reports `AXModal == false`;
/// an unreadable `AXModal` keeps it, so a real alert is never missed.
pub fn top_level_surface(kind: &str, modal: Option<bool>) -> bool {
    kind != "alert" || modal != Some(false)
}

/// Which listed surface a `surface:<wanted>` read of `window_id` roots at:
/// an app-level menu (menu-bar first, then context), or a sheet, popover,
/// alert or focused element that belongs to that window.
pub fn pick(surfaces: &[Surface], wanted: &str, window_id: u32) -> Option<usize> {
    if wanted == "menu" {
        return surfaces
            .iter()
            .position(|s| s.kind == "menu")
            .or_else(|| surfaces.iter().position(|s| s.kind == "context_menu"));
    }
    surfaces
        .iter()
        .position(|s| s.kind == wanted && s.window_id == Some(window_id))
}

/// Surfaces found by one inventory, with the retained element of each.
pub(crate) struct Inventory {
    pub surfaces: Vec<Surface>,
    /// False when the node budget ran out before every source was read.
    pub complete: bool,
    elements: Vec<AXUIElementRef>,
}

impl Inventory {
    /// Take the element of `surfaces[index]` out, retained, for the caller to
    /// release.
    pub fn take_element(&mut self, index: usize) -> Option<AXUIElementRef> {
        let element = *self.elements.get(index)?;
        if element.is_null() {
            return None;
        }
        self.elements[index] = std::ptr::null_mut();
        Some(element)
    }
}

impl Drop for Inventory {
    fn drop(&mut self) {
        for element in self.elements.drain(..) {
            if !element.is_null() {
                unsafe { CFRelease(element as CFTypeRef) };
            }
        }
    }
}

/// The surfaces `pid` has open. With `window_id`, window-owned surfaces are
/// those of that window only; menus are app-level and always listed.
pub fn list_surfaces(pid: i32, window_id: Option<u32>) -> (Vec<Surface>, bool) {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return (Vec::new(), true);
        }
        let mut inventory = inventory(app, window_id);
        CFRelease(app as CFTypeRef);
        (std::mem::take(&mut inventory.surfaces), inventory.complete)
    }
}

/// Read the inventory of the application element `app`.
///
/// # Safety
/// `app` must be a live application `AXUIElementRef`.
pub(crate) unsafe fn inventory(app: AXUIElementRef, window_filter: Option<u32>) -> Inventory {
    let mut reader = Reader {
        budget: MAX_SURFACE_NODES,
        inventory: Inventory {
            surfaces: Vec::new(),
            complete: true,
            elements: Vec::new(),
        },
    };
    AXUIElementSetMessagingTimeout(app, MESSAGING_TIMEOUT_SECONDS);

    // Menus: the selected menu-bar item's menu, and menus the app hosts
    // directly (a context menu is an AXMenu child of the application).
    for child in reader.children(app) {
        match reader.role(child).as_deref() {
            Some("AXMenuBar") => {
                for item in reader.children(child) {
                    if reader.role(item).as_deref() == Some("AXMenuBarItem")
                        && copy_bool_attr(item, "AXSelected") == Some(true)
                    {
                        for menu in reader.children(item) {
                            if reader.role(menu).as_deref() == Some("AXMenu") {
                                reader.push_menu(menu, "menu");
                            } else {
                                CFRelease(menu as CFTypeRef);
                            }
                        }
                    }
                    CFRelease(item as CFTypeRef);
                }
                CFRelease(child as CFTypeRef);
            }
            Some("AXMenu") => reader.push_menu(child, "context_menu"),
            _ => CFRelease(child as CFTypeRef),
        }
    }

    // The focused element, and a context menu it hosts.
    if let Some(focused) = reader.element_attr(app, "AXFocusedUIElement") {
        for child in reader.children(focused) {
            if reader.role(child).as_deref() == Some("AXMenu") {
                reader.push_menu(child, "context_menu");
            } else {
                CFRelease(child as CFTypeRef);
            }
        }
        let window_id = reader.element_attr(focused, "AXWindow").and_then(|window| {
            let id = ax_get_window_id(window);
            CFRelease(window as CFTypeRef);
            id
        });
        if window_filter.is_none() || window_id == window_filter {
            let role = reader.role(focused).unwrap_or_default();
            let subrole = copy_string_attr(focused, "AXSubrole");
            let title = title_of(focused);
            reader.push(
                Surface {
                    kind: "focused",
                    role,
                    subrole,
                    title,
                    window_id,
                    item_count: None,
                },
                focused,
            );
        } else {
            CFRelease(focused as CFTypeRef);
        }
    }

    // Sheets, popovers and alerts: the window itself or a direct child.
    for window in copy_ax_windows(app) {
        let window_id = ax_get_window_id(window);
        if window_filter.is_some() && window_id != window_filter {
            CFRelease(window as CFTypeRef);
            continue;
        }
        // Read the children before the window is handed over: push_if_surface
        // takes ownership, keeping it or releasing it, so the window must not
        // be touched or released after that call.
        let children = reader.children(window);
        reader.push_if_surface(window, window_id, true);
        for child in children {
            reader.push_if_surface(child, window_id, false);
        }
    }
    reader.inventory
}

struct Reader {
    budget: usize,
    inventory: Inventory,
}

impl Reader {
    /// Spend one node of the budget; false (and incomplete) once it is gone.
    fn admit(&mut self) -> bool {
        if self.budget == 0 {
            self.inventory.complete = false;
            return false;
        }
        self.budget -= 1;
        true
    }

    unsafe fn role(&mut self, element: AXUIElementRef) -> Option<String> {
        AXUIElementSetMessagingTimeout(element, MESSAGING_TIMEOUT_SECONDS);
        copy_string_attr(element, "AXRole")
    }

    /// Retained children, as many as the budget admits.
    unsafe fn children(&mut self, element: AXUIElementRef) -> Vec<AXUIElementRef> {
        let mut kept = Vec::new();
        for child in copy_children(element) {
            if self.admit() {
                kept.push(child);
            } else {
                CFRelease(child as CFTypeRef);
            }
        }
        kept
    }

    unsafe fn element_attr(
        &mut self,
        element: AXUIElementRef,
        attribute: &str,
    ) -> Option<AXUIElementRef> {
        if !self.admit() {
            return None;
        }
        copy_element_attr(element, attribute)
    }

    /// Record `element` (retained; ownership moves here) unless an equal
    /// element is already listed: a context menu can be reached both from
    /// the application and from the focused element.
    unsafe fn push(&mut self, surface: Surface, element: AXUIElementRef) {
        let seen = self
            .inventory
            .elements
            .iter()
            .any(|&e| !e.is_null() && CFEqual(e as CFTypeRef, element as CFTypeRef) != 0);
        if seen {
            CFRelease(element as CFTypeRef);
            return;
        }
        self.inventory.surfaces.push(surface);
        self.inventory.elements.push(element);
    }

    unsafe fn push_menu(&mut self, menu: AXUIElementRef, kind: &'static str) {
        let items = copy_children(menu);
        let item_count = items.len();
        for item in items {
            CFRelease(item as CFTypeRef);
        }
        let surface = Surface {
            kind,
            role: "AXMenu".to_owned(),
            subrole: None,
            title: title_of(menu),
            window_id: None,
            item_count: Some(item_count),
        };
        self.push(surface, menu);
    }

    /// Record `element` when it classifies as a window-owned surface;
    /// otherwise release it.
    unsafe fn push_if_surface(
        &mut self,
        element: AXUIElementRef,
        window_id: Option<u32>,
        top_level: bool,
    ) {
        let role = self.role(element).unwrap_or_default();
        let subrole = copy_string_attr(element, "AXSubrole");
        let kind = window_surface_kind(&role, subrole.as_deref()).filter(|kind| {
            !top_level || top_level_surface(kind, copy_bool_attr(element, "AXModal"))
        });
        match kind {
            Some(kind) => {
                let title = title_of(element);
                self.push(
                    Surface {
                        kind,
                        role,
                        subrole,
                        title,
                        window_id,
                        item_count: None,
                    },
                    element,
                );
            }
            None => CFRelease(element as CFTypeRef),
        }
    }
}

unsafe fn title_of(element: AXUIElementRef) -> Option<String> {
    copy_string_attr(element, "AXTitle")
        .filter(|title| !title.trim().is_empty())
        .or_else(|| copy_string_attr(element, "AXDescription"))
        .filter(|title| !title.trim().is_empty())
}

/// The retained root element a `surface:<wanted>` read of `window_id`
/// walks, with its description; `Err` lists what is open instead.
///
/// # Safety
/// `app` must be a live application `AXUIElementRef`. The caller releases
/// the returned element.
pub(crate) unsafe fn find_root(
    app: AXUIElementRef,
    window_id: u32,
    wanted: &str,
) -> Result<(AXUIElementRef, Surface), Vec<Surface>> {
    let mut inventory = inventory(app, Some(window_id));
    match pick(&inventory.surfaces, wanted, window_id) {
        Some(index) => match inventory.take_element(index) {
            Some(element) => Ok((element, inventory.surfaces[index].clone())),
            None => Err(std::mem::take(&mut inventory.surfaces)),
        },
        None => Err(std::mem::take(&mut inventory.surfaces)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(kind: &'static str, window_id: Option<u32>) -> Surface {
        Surface {
            kind,
            role: "AXMenu".into(),
            subrole: None,
            title: None,
            window_id,
            item_count: None,
        }
    }

    #[test]
    fn a_non_modal_top_level_dialog_window_is_not_an_alert() {
        // System Settings' main window: AXWindow / AXDialog, AXModal false.
        assert!(!top_level_surface("alert", Some(false)));
        assert!(top_level_surface("alert", Some(true)));
        // Unreadable modality keeps it: never miss a real alert.
        assert!(top_level_surface("alert", None));
        assert!(top_level_surface("sheet", Some(false)));
    }

    #[test]
    fn subroles_classify_window_owned_surfaces() {
        assert_eq!(window_surface_kind("AXSheet", None), Some("sheet"));
        assert_eq!(
            window_surface_kind("AXWindow", Some("AXDialog")),
            Some("alert")
        );
        assert_eq!(
            window_surface_kind("AXWindow", Some("AXSystemDialog")),
            Some("alert")
        );
        assert_eq!(
            window_surface_kind("AXGroup", Some("AXPopover")),
            Some("popover")
        );
        assert_eq!(window_surface_kind("AXPopover", None), Some("popover"));
        assert_eq!(
            window_surface_kind("AXWindow", Some("AXStandardWindow")),
            None
        );
        assert_eq!(window_surface_kind("AXButton", None), None);
    }

    #[test]
    fn a_menu_read_prefers_the_menu_bar_menu_and_others_need_their_window() {
        let open = [
            surface("focused", Some(7)),
            surface("context_menu", None),
            surface("sheet", Some(8)),
            surface("menu", None),
            surface("sheet", Some(7)),
        ];
        assert_eq!(pick(&open, "menu", 7), Some(3));
        assert_eq!(pick(&open[..3], "menu", 7), Some(1));
        assert_eq!(pick(&open, "sheet", 7), Some(4));
        assert_eq!(pick(&open, "sheet", 9), None);
        assert_eq!(pick(&open, "focused", 7), Some(0));
        assert_eq!(pick(&open, "focused", 8), None);
        assert_eq!(pick(&open, "alert", 7), None);
    }

    #[test]
    fn surface_json_omits_unknowns() {
        let menu = Surface {
            kind: "context_menu",
            role: "AXMenu".into(),
            subrole: None,
            title: None,
            window_id: None,
            item_count: Some(3),
        };
        assert_eq!(
            menu.to_json(),
            json!({"kind": "context_menu", "role": "AXMenu", "item_count": 3})
        );
    }
}
