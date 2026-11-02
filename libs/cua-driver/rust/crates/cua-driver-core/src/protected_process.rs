//! Session-critical processes that `close_app` and `kill_app` never end.
//!
//! Quitting `loginwindow` logs the user out, killing `WindowServer` ends the
//! graphical session, and `Dock`, `Finder`, `SystemUIServer` and
//! `ControlCenter` own the menu bar, Spaces and the desktop itself. None of
//! them is a reasonable target for an agent, so the refusal happens before any
//! termination request is sent, in every permission mode.
//!
//! Ported from agent-desktop's `PROTECTED_PROCESSES`
//! (crates/macos/src/system/app/app_ops.rs:13-20, Apache-2.0) and widened to
//! the other session UI agents macOS restarts on its own. Matching differs on
//! purpose: agent-desktop matches any dotted component of a bundle id, which
//! also protects `com.example.dock`. Here every comparison is exact (ASCII
//! case-insensitive, the way APFS compares names), so lookalikes such as
//! `Docker`, `FinderSync` or `com.apple.finder.SyncExtension` stay closable,
//! while an impostor that copies an exact name is refused (fail closed).

/// Identity facts gathered for one pid before termination. Gathering is
/// platform work; the decision below is pure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessFacts {
    pub pid: i64,
    /// Display name (`NSRunningApplication.localizedName` on macOS).
    pub name: Option<String>,
    pub bundle_id: Option<String>,
    /// Absolute executable path (`proc_pidpath` on macOS).
    pub executable_path: Option<String>,
    /// The Driver itself, or the process that launched it (the host app).
    /// Ending either would end the session that is asking.
    pub is_driver_or_host: bool,
}

/// Why a pid must not be terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessProtection {
    /// Not on the denylist.
    Allowed,
    /// On the denylist; `matched` names the fact that matched.
    Protected { matched: String },
    /// No name, bundle id or executable could be read, so the denylist cannot
    /// be ruled out. Refused rather than guessed.
    IdentityUnknown,
}

/// Process and executable names, compared exactly against the display name and
/// the executable's file name.
const PROTECTED_NAMES: &[&str] = &[
    "kernel_task",
    "launchd",
    "loginwindow",
    "WindowServer",
    "Dock",
    "Finder",
    "SystemUIServer",
    "ControlCenter",
    "NotificationCenter",
    "WindowManager",
];

/// Bundle identifiers, compared exactly.
const PROTECTED_BUNDLE_IDS: &[&str] = &[
    "com.apple.loginwindow",
    "com.apple.dock",
    "com.apple.finder",
    "com.apple.systemuiserver",
    "com.apple.controlcenter",
    "com.apple.notificationcenterui",
    "com.apple.WindowManager",
];

fn exact(candidate: &str, list: &[&str]) -> Option<String> {
    list.iter()
        .find(|protected| candidate.eq_ignore_ascii_case(protected))
        .map(|protected| (*protected).to_owned())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Decide whether `facts` names a process the Driver must never terminate.
pub fn classify(facts: &ProcessFacts) -> ProcessProtection {
    if facts.pid <= 1 {
        return ProcessProtection::Protected {
            matched: format!("pid {}", facts.pid),
        };
    }
    if facts.is_driver_or_host {
        return ProcessProtection::Protected {
            matched: "the Cua Driver or its host process".to_owned(),
        };
    }
    fn present(value: &Option<String>) -> Option<&str> {
        value.as_deref().filter(|value| !value.is_empty())
    }
    let name = present(&facts.name);
    let bundle_id = present(&facts.bundle_id);
    let executable = present(&facts.executable_path);
    if let Some(matched) = bundle_id.and_then(|id| exact(id, PROTECTED_BUNDLE_IDS)) {
        return ProcessProtection::Protected { matched };
    }
    if let Some(matched) = name.and_then(|name| exact(name, PROTECTED_NAMES)) {
        return ProcessProtection::Protected { matched };
    }
    if let Some(matched) = executable.and_then(|path| exact(file_name(path), PROTECTED_NAMES)) {
        return ProcessProtection::Protected { matched };
    }
    if name.is_none() && bundle_id.is_none() && executable.is_none() {
        return ProcessProtection::IdentityUnknown;
    }
    ProcessProtection::Allowed
}

/// The structured refusal for a protected or unidentifiable pid, or `None`
/// when termination may proceed. Nothing was sent, so a retry is safe only
/// against a different target.
pub fn refusal(tool: &str, facts: &ProcessFacts) -> Option<crate::protocol::ToolResult> {
    let (code, message) = match classify(facts) {
        ProcessProtection::Allowed => return None,
        ProcessProtection::Protected { matched } => (
            "protected_process",
            format!(
                "{tool}: pid {} is a protected system process ({matched}) and is never closed. \
                 Target a regular application.",
                facts.pid
            ),
        ),
        ProcessProtection::IdentityUnknown => (
            "process_identity_unknown",
            format!(
                "{tool}: could not read the name, bundle id or executable of pid {}, so it cannot \
                 be ruled out as a protected system process. Nothing was sent.",
                facts.pid
            ),
        ),
    };
    Some(
        crate::protocol::ToolResult::error(message.clone()).with_structured(serde_json::json!({
            "status": "refused",
            "refusal": { "code": code, "message": message },
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> ProcessFacts {
        ProcessFacts {
            pid: 500,
            name: Some(name.to_owned()),
            ..ProcessFacts::default()
        }
    }

    fn bundle(id: &str) -> ProcessFacts {
        ProcessFacts {
            pid: 500,
            name: Some("Some App".to_owned()),
            bundle_id: Some(id.to_owned()),
            ..ProcessFacts::default()
        }
    }

    fn executable(path: &str) -> ProcessFacts {
        ProcessFacts {
            pid: 500,
            executable_path: Some(path.to_owned()),
            ..ProcessFacts::default()
        }
    }

    fn protected(facts: &ProcessFacts) -> bool {
        matches!(classify(facts), ProcessProtection::Protected { .. })
    }

    #[test]
    fn session_critical_names_bundles_and_executables_are_protected() {
        for name in ["Finder", "Dock", "WindowServer", "loginwindow", "SystemUIServer"] {
            assert!(protected(&named(name)), "{name}");
        }
        assert!(protected(&named("finder")), "APFS-style case folding");
        assert!(protected(&bundle("com.apple.dock")));
        assert!(protected(&bundle("com.apple.Finder")));
        assert!(protected(&bundle("com.apple.controlcenter")));
        assert!(protected(&executable(
            "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer"
        )));
        assert!(protected(&executable("/sbin/launchd")));
    }

    #[test]
    fn lookalike_names_stay_closable() {
        // Substrings, prefixes and suffixes of protected names.
        for name in [
            "Docker",
            "Docker Desktop",
            "FinderSync",
            "PathFinder",
            "launchdarkly-agent",
            "Dock Helper",
            "Finder ",
            " Finder",
            // Cyrillic "і" and "е": a different name, not the real Finder.
            "Fіnder",
            "Dоck",
        ] {
            assert_eq!(classify(&named(name)), ProcessProtection::Allowed, "{name:?}");
        }
        for id in [
            "com.docker.docker",
            "com.apple.finder.SyncExtension",
            "com.example.dock",
            "com.apple.dockhelper",
        ] {
            assert_eq!(classify(&bundle(id)), ProcessProtection::Allowed, "{id}");
        }
        assert_eq!(
            classify(&executable("/Applications/Docker.app/Contents/MacOS/Docker")),
            ProcessProtection::Allowed
        );
        // A directory that happens to be named like a protected process does
        // not protect the binary inside it.
        assert_eq!(
            classify(&executable("/Users/a/Finder/tool")),
            ProcessProtection::Allowed
        );
    }

    #[test]
    fn an_impostor_with_an_exact_protected_name_fails_closed() {
        assert!(protected(&ProcessFacts {
            pid: 900,
            name: Some("Finder".into()),
            bundle_id: Some("com.evil.finder".into()),
            executable_path: Some("/tmp/Finder.app/Contents/MacOS/Finder".into()),
            is_driver_or_host: false,
        }));
    }

    #[test]
    fn ordinary_apps_are_allowed() {
        assert_eq!(
            classify(&ProcessFacts {
                pid: 4242,
                name: Some("TextEdit".into()),
                bundle_id: Some("com.apple.TextEdit".into()),
                executable_path: Some(
                    "/System/Applications/TextEdit.app/Contents/MacOS/TextEdit".into()
                ),
                is_driver_or_host: false,
            }),
            ProcessProtection::Allowed
        );
    }

    #[test]
    fn launchd_pids_the_driver_and_its_host_are_protected() {
        assert!(protected(&named("TextEdit").with_pid(1)));
        assert!(protected(&named("TextEdit").with_pid(0)));
        assert!(protected(&ProcessFacts {
            is_driver_or_host: true,
            ..named("Diri")
        }));
    }

    #[test]
    fn an_unidentifiable_process_is_refused_not_guessed() {
        let facts = ProcessFacts {
            pid: 777,
            name: Some(String::new()),
            ..ProcessFacts::default()
        };
        assert_eq!(classify(&facts), ProcessProtection::IdentityUnknown);
        let result = refusal("close_app", &facts).expect("refused");
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["status"], "refused");
        assert_eq!(structured["refusal"]["code"], "process_identity_unknown");
    }

    #[test]
    fn refusal_is_structured_and_absent_for_allowed_targets() {
        assert!(refusal("kill_app", &named("TextEdit")).is_none());
        let result = refusal("kill_app", &named("Dock")).expect("refused");
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["refusal"]["code"], "protected_process");
    }

    impl ProcessFacts {
        fn with_pid(mut self, pid: i64) -> Self {
            self.pid = pid;
            self
        }
    }
}
