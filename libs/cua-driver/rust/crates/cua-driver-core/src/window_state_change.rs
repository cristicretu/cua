//! Pure decisions for `set_window_state`: minimize, maximize, restore and
//! full screen of one exact window.
//!
//! The platform reads the window's facts, asks [`plan`] what to write, writes
//! it, polls the same facts back, and asks [`verdict`] whether the requested
//! state was reached. Keeping the decisions here makes every refusal and
//! readback outcome testable without a window on screen.
//!
//! Background operation is the core property: minimize, restore and maximize
//! are accessibility attribute writes that leave the frontmost app and the
//! current Space alone. Entering or leaving full screen moves the window to
//! (or from) its own Space and switches the user's display to it, so it is
//! refused unless the caller asked for `delivery_mode:"foreground"`.
//!
//! Maximize follows agent-desktop (crates/macos/src/system/window/
//! window_ops.rs `maximize_to_main_display`, Apache-2.0): the window frame is
//! set to the visible work area of its display rather than pressing the zoom
//! button, because zoom is a toggle whose result depends on the app and can't
//! be read back as a state.

/// What the caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowStateAction {
    Minimize,
    Maximize,
    Restore,
    Fullscreen,
    ExitFullscreen,
}

impl WindowStateAction {
    pub const NAMES: &'static [&'static str] = &[
        "minimize",
        "maximize",
        "restore",
        "fullscreen",
        "exit_fullscreen",
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "minimize" => Self::Minimize,
            "maximize" => Self::Maximize,
            "restore" => Self::Restore,
            "fullscreen" => Self::Fullscreen,
            "exit_fullscreen" => Self::ExitFullscreen,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Restore => "restore",
            Self::Fullscreen => "fullscreen",
            Self::ExitFullscreen => "exit_fullscreen",
        }
    }
}

/// Screen rectangle in global top-left points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn is_valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f64::is_finite)
            && self.width > 0.0
            && self.height > 0.0
    }

    pub fn approximately_eq(self, other: Self, tolerance: f64) -> bool {
        (self.x - other.x).abs() <= tolerance
            && (self.y - other.y).abs() <= tolerance
            && (self.width - other.width).abs() <= tolerance
            && (self.height - other.height).abs() <= tolerance
    }

    pub fn intersection_area(self, other: Self) -> f64 {
        let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        width.max(0.0) * height.max(0.0)
    }
}

/// Frame readback tolerance, matching `set_window_frame`.
pub const FRAME_TOLERANCE: f64 = 2.0;

/// Facts read from the window. `None` means the attribute could not be read.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WindowStateFacts {
    pub minimized: Option<bool>,
    pub fullscreen: Option<bool>,
    pub frame: Option<Rect>,
    pub minimized_settable: bool,
    pub fullscreen_settable: bool,
    pub position_settable: bool,
    pub size_settable: bool,
}

/// What the platform must write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowWrite {
    SetMinimized(bool),
    SetFullscreen(bool),
    SetFrame(Rect),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WindowStatePlan {
    /// The window is already in the requested state; nothing is written.
    AlreadyInState,
    Write(WindowWrite),
    /// Nothing is written. `code` is a stable refusal code.
    Refuse {
        code: &'static str,
        message: String,
    },
}

fn refuse(code: &'static str, message: impl Into<String>) -> WindowStatePlan {
    WindowStatePlan::Refuse {
        code,
        message: message.into(),
    }
}

/// Decide the single write that moves the window into `action`'s state.
/// `work_area` is the visible frame of the display the window is on; it is
/// only consulted for `Maximize`.
pub fn plan(
    action: WindowStateAction,
    facts: &WindowStateFacts,
    foreground: bool,
    work_area: Option<Rect>,
) -> WindowStatePlan {
    use WindowStateAction::*;
    if matches!(action, Fullscreen | ExitFullscreen) && !foreground {
        return refuse(
            "foreground_required",
            "Entering or leaving full screen moves the window to its own Space and switches the \
             user's display to it, so it is never done in the background. Retry with \
             delivery_mode:\"foreground\" only if the user allows it, or use maximize.",
        );
    }
    let Some(minimized) = facts.minimized else {
        return refuse(
            "window_state_unknown",
            "The window's AXMinimized state could not be read, so the change could not be verified.",
        );
    };
    match action {
        Minimize => {
            if minimized {
                return WindowStatePlan::AlreadyInState;
            }
            if facts.fullscreen == Some(true) {
                return refuse(
                    "window_fullscreen",
                    "A full-screen window cannot be minimized. Use exit_fullscreen first.",
                );
            }
            if !facts.minimized_settable {
                return refuse("unsupported", "The window does not allow minimizing.");
            }
            WindowStatePlan::Write(WindowWrite::SetMinimized(true))
        }
        Restore => {
            if !minimized {
                if facts.fullscreen == Some(true) {
                    return refuse(
                        "window_fullscreen",
                        "The window is not minimized; it is full screen. Use exit_fullscreen.",
                    );
                }
                return WindowStatePlan::AlreadyInState;
            }
            if !facts.minimized_settable {
                return refuse("unsupported", "The window does not allow restoring.");
            }
            WindowStatePlan::Write(WindowWrite::SetMinimized(false))
        }
        Maximize => {
            if minimized {
                return refuse(
                    "window_minimized",
                    "The window is minimized. Use restore first.",
                );
            }
            if facts.fullscreen == Some(true) {
                return refuse(
                    "window_fullscreen",
                    "The window is full screen. Use exit_fullscreen first.",
                );
            }
            let Some(work_area) = work_area.filter(|area| area.is_valid()) else {
                return refuse(
                    "display_unknown",
                    "The visible work area of the window's display could not be read.",
                );
            };
            if facts
                .frame
                .is_some_and(|frame| frame.approximately_eq(work_area, FRAME_TOLERANCE))
            {
                return WindowStatePlan::AlreadyInState;
            }
            if !facts.position_settable || !facts.size_settable {
                return refuse(
                    "unsupported",
                    "The window does not expose a settable position and size.",
                );
            }
            WindowStatePlan::Write(WindowWrite::SetFrame(work_area))
        }
        Fullscreen | ExitFullscreen => {
            let want = action == Fullscreen;
            if minimized {
                return refuse(
                    "window_minimized",
                    "The window is minimized. Use restore first.",
                );
            }
            match facts.fullscreen {
                Some(current) if current == want => WindowStatePlan::AlreadyInState,
                Some(_) if facts.fullscreen_settable => {
                    WindowStatePlan::Write(WindowWrite::SetFullscreen(want))
                }
                Some(_) => refuse(
                    "unsupported",
                    "The window does not support full screen through accessibility.",
                ),
                None => refuse(
                    "unsupported",
                    "The window does not report AXFullScreen, so full screen cannot be set or verified.",
                ),
            }
        }
    }
}

/// Readback outcome after a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowStateVerdict {
    /// The readback shows the requested state.
    Verified,
    /// The readback shows the window did not reach the requested state.
    NotReached,
    /// The attribute could not be read back.
    Unverifiable,
}

impl WindowStateVerdict {
    pub fn name(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::NotReached => "not_reached",
            Self::Unverifiable => "unverifiable",
        }
    }
}

/// Whether `after` shows the state `write` asked for.
pub fn verdict(write: WindowWrite, after: &WindowStateFacts) -> WindowStateVerdict {
    let reached = match write {
        WindowWrite::SetMinimized(want) => after.minimized.map(|value| value == want),
        WindowWrite::SetFullscreen(want) => after.fullscreen.map(|value| value == want),
        WindowWrite::SetFrame(target) => after
            .frame
            .map(|frame| frame.approximately_eq(target, FRAME_TOLERANCE)),
    };
    match reached {
        Some(true) => WindowStateVerdict::Verified,
        Some(false) => WindowStateVerdict::NotReached,
        None => WindowStateVerdict::Unverifiable,
    }
}

/// The display a window is on: the one its frame overlaps most, else the
/// primary display, else the first. Ported from agent-desktop's
/// `select_display` (crates/macos/src/system/display.rs, Apache-2.0).
pub fn display_for_frame<T>(
    displays: &[T],
    frame: Option<Rect>,
    bounds: impl Fn(&T) -> Rect,
    is_primary: impl Fn(&T) -> bool,
) -> Option<&T> {
    frame
        .and_then(|frame| {
            displays
                .iter()
                .map(|display| (display, frame.intersection_area(bounds(display))))
                .filter(|(_, area)| *area > 0.0)
                .max_by(|left, right| left.1.total_cmp(&right.1))
                .map(|(display, _)| display)
        })
        .or_else(|| displays.iter().find(|display| is_primary(display)))
        .or_else(|| displays.first())
}

#[cfg(test)]
mod tests {
    use super::*;
    use WindowStateAction::*;

    const AREA: Rect = Rect {
        x: 0.0,
        y: 25.0,
        width: 1512.0,
        height: 920.0,
    };

    fn normal() -> WindowStateFacts {
        WindowStateFacts {
            minimized: Some(false),
            fullscreen: Some(false),
            frame: Some(Rect {
                x: 100.0,
                y: 100.0,
                width: 800.0,
                height: 600.0,
            }),
            minimized_settable: true,
            fullscreen_settable: true,
            position_settable: true,
            size_settable: true,
        }
    }

    fn code(plan: WindowStatePlan) -> &'static str {
        match plan {
            WindowStatePlan::Refuse { code, .. } => code,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn full_screen_is_refused_in_the_background_before_anything_is_read() {
        for action in [Fullscreen, ExitFullscreen] {
            assert_eq!(
                code(plan(action, &WindowStateFacts::default(), false, None)),
                "foreground_required"
            );
        }
        assert_eq!(
            plan(Fullscreen, &normal(), true, None),
            WindowStatePlan::Write(WindowWrite::SetFullscreen(true))
        );
    }

    #[test]
    fn background_actions_plan_one_attribute_write() {
        assert_eq!(
            plan(Minimize, &normal(), false, None),
            WindowStatePlan::Write(WindowWrite::SetMinimized(true))
        );
        let minimized = WindowStateFacts {
            minimized: Some(true),
            ..normal()
        };
        assert_eq!(
            plan(Restore, &minimized, false, None),
            WindowStatePlan::Write(WindowWrite::SetMinimized(false))
        );
        assert_eq!(
            plan(Maximize, &normal(), false, Some(AREA)),
            WindowStatePlan::Write(WindowWrite::SetFrame(AREA))
        );
    }

    #[test]
    fn a_window_already_in_the_state_is_not_written() {
        let minimized = WindowStateFacts {
            minimized: Some(true),
            ..normal()
        };
        assert_eq!(
            plan(Minimize, &minimized, false, None),
            WindowStatePlan::AlreadyInState
        );
        assert_eq!(
            plan(Restore, &normal(), false, None),
            WindowStatePlan::AlreadyInState
        );
        let maximized = WindowStateFacts {
            frame: Some(Rect {
                x: 1.0,
                y: 26.0,
                ..AREA
            }),
            ..normal()
        };
        assert_eq!(
            plan(Maximize, &maximized, false, Some(AREA)),
            WindowStatePlan::AlreadyInState
        );
        let fullscreen = WindowStateFacts {
            fullscreen: Some(true),
            ..normal()
        };
        assert_eq!(
            plan(Fullscreen, &fullscreen, true, None),
            WindowStatePlan::AlreadyInState
        );
    }

    #[test]
    fn contradictory_starting_states_are_refused() {
        let minimized = WindowStateFacts {
            minimized: Some(true),
            ..normal()
        };
        let fullscreen = WindowStateFacts {
            fullscreen: Some(true),
            ..normal()
        };
        assert_eq!(
            code(plan(Maximize, &minimized, false, Some(AREA))),
            "window_minimized"
        );
        assert_eq!(
            code(plan(Fullscreen, &minimized, true, None)),
            "window_minimized"
        );
        assert_eq!(
            code(plan(Minimize, &fullscreen, false, None)),
            "window_fullscreen"
        );
        assert_eq!(
            code(plan(Maximize, &fullscreen, false, Some(AREA))),
            "window_fullscreen"
        );
        assert_eq!(
            code(plan(Restore, &fullscreen, false, None)),
            "window_fullscreen"
        );
    }

    #[test]
    fn unreadable_or_unsettable_windows_fail_closed() {
        let unknown = WindowStateFacts {
            minimized: None,
            ..normal()
        };
        assert_eq!(
            code(plan(Minimize, &unknown, false, None)),
            "window_state_unknown"
        );
        let fixed = WindowStateFacts {
            minimized_settable: false,
            size_settable: false,
            fullscreen_settable: false,
            ..normal()
        };
        assert_eq!(code(plan(Minimize, &fixed, false, None)), "unsupported");
        assert_eq!(
            code(plan(Maximize, &fixed, false, Some(AREA))),
            "unsupported"
        );
        assert_eq!(code(plan(Fullscreen, &fixed, true, None)), "unsupported");
        let no_fullscreen_attr = WindowStateFacts {
            fullscreen: None,
            ..normal()
        };
        assert_eq!(
            code(plan(Fullscreen, &no_fullscreen_attr, true, None)),
            "unsupported"
        );
        assert_eq!(
            code(plan(Maximize, &normal(), false, None)),
            "display_unknown"
        );
        let bogus = Rect { width: 0.0, ..AREA };
        assert_eq!(
            code(plan(Maximize, &normal(), false, Some(bogus))),
            "display_unknown"
        );
    }

    #[test]
    fn readback_verdicts() {
        let minimized = WindowStateFacts {
            minimized: Some(true),
            ..normal()
        };
        assert_eq!(
            verdict(WindowWrite::SetMinimized(true), &minimized),
            WindowStateVerdict::Verified
        );
        assert_eq!(
            verdict(WindowWrite::SetMinimized(true), &normal()),
            WindowStateVerdict::NotReached
        );
        assert_eq!(
            verdict(
                WindowWrite::SetMinimized(true),
                &WindowStateFacts::default()
            ),
            WindowStateVerdict::Unverifiable
        );
        // An app that caps its window size settles short of the work area.
        let capped = WindowStateFacts {
            frame: Some(Rect {
                width: 1200.0,
                ..AREA
            }),
            ..normal()
        };
        assert_eq!(
            verdict(WindowWrite::SetFrame(AREA), &capped),
            WindowStateVerdict::NotReached
        );
        let filled = WindowStateFacts {
            frame: Some(Rect {
                height: 919.0,
                ..AREA
            }),
            ..normal()
        };
        assert_eq!(
            verdict(WindowWrite::SetFrame(AREA), &filled),
            WindowStateVerdict::Verified
        );
        assert_eq!(
            verdict(WindowWrite::SetFullscreen(false), &normal()),
            WindowStateVerdict::Verified
        );
    }

    #[test]
    fn a_window_maximizes_on_the_display_it_overlaps_most() {
        struct Display {
            bounds: Rect,
            primary: bool,
        }
        let displays = [
            Display {
                bounds: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1512.0,
                    height: 982.0,
                },
                primary: true,
            },
            Display {
                bounds: Rect {
                    x: 1512.0,
                    y: -200.0,
                    width: 2560.0,
                    height: 1440.0,
                },
                primary: false,
            },
        ];
        let pick = |frame| {
            display_for_frame(&displays, frame, |d| d.bounds, |d| d.primary).map(|d| d.primary)
        };
        let mostly_right = Rect {
            x: 1400.0,
            y: 100.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(pick(Some(mostly_right)), Some(false));
        let off_screen = Rect {
            x: -5000.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        assert_eq!(pick(Some(off_screen)), Some(true));
        assert_eq!(pick(None), Some(true));
        assert!(display_for_frame::<Display>(&[], None, |d| d.bounds, |d| d.primary).is_none());
    }

    #[test]
    fn action_names_round_trip() {
        for name in WindowStateAction::NAMES {
            assert_eq!(WindowStateAction::parse(name).unwrap().name(), *name);
        }
        assert!(WindowStateAction::parse("zoom").is_none());
    }
}
