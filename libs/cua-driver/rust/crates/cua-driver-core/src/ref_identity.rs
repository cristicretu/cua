//! Strict identity for element tokens.
//!
//! An `element_token` resolves to the `AXUIElement` the snapshot retained, and
//! that retained object stays the locator. It is not proof on its own: AppKit
//! recycles views (an `NSTableView` cell scrolled out of view is reused for
//! another row, keeping the same AX object), so a token can resolve, act, and
//! hit the wrong row. Snapshot time therefore records identity evidence beside
//! the retained object ([`RefIdentity`]), and every dispatch re-reads the live
//! element ([`LiveElementFacts`]) and asks [`check_identity`] whether it is
//! still the element the caller saw. A dead element or positive evidence of a
//! different one refuses as `stale_element_token`; nothing is re-resolved to a
//! different element behind the caller's back.
//!
//! The tiers follow agent-desktop's ref identity (Apache-2.0,
//! `crates/core/src/refs/ref_identity.rs`): an identifier and role-conditional
//! stable text (titles, descriptions, label values; never the contents of a
//! text field or the state of a control) identify an element; a child-index
//! path and geometry are consulted only when it has no stable text. One
//! deliberate difference: agent-desktop lets a matching identifier decide
//! alone, but an AppKit cell's `AXIdentifier` is its column's, shared by every
//! row, so here an identifier match never outvotes changed text.
//!
//! This module performs no I/O; the platform shell gathers the facts.

use crate::protocol::ToolResult;

/// Refusal code for a token whose element is dead or is no longer the
/// element the snapshot recorded. Shared with the snapshot store's own
/// stale-snapshot refusal so recovery hints and batch re-find treat both the
/// same way.
pub const STALE_ELEMENT_TOKEN: &str = "stale_element_token";

/// Points of slack when geometry corroborates an element: AX frames are
/// fractional and some apps round them differently between reads.
const FRAME_TOLERANCE: f64 = 1.0;

/// Identity evidence recorded for one tokenized element at snapshot time.
///
/// Text fields hold trimmed, non-blank text only. Build it with
/// [`RefIdentity::new`], which applies the role rules (which values are
/// labels, when a title is only an echo of a value).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RefIdentity {
    pub role: String,
    pub subrole: Option<String>,
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    /// `AXValue`, kept only for roles whose value is their label
    /// ([`value_is_label`]). A text field's contents or a checkbox's state is
    /// never identity: it changes because the caller acts on it.
    pub value: Option<String>,
    /// Screen frame `[x, y, w, h]` at snapshot time.
    pub frame: Option<[f64; 4]>,
    /// Raw `AXChildren` indices from the walk root (the window) down to the
    /// element.
    pub path: Vec<u32>,
}

impl RefIdentity {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        role: &str,
        subrole: Option<&str>,
        identifier: Option<&str>,
        title: Option<&str>,
        description: Option<&str>,
        value: Option<&str>,
        frame: Option<[f64; 4]>,
        path: Vec<u32>,
    ) -> Self {
        let value_text = non_blank(value);
        // Some controls mirror their value into AXTitle; that title is the
        // value, so it is as mutable as the value.
        let title = non_blank(title)
            .filter(|title| value_is_label(role) || value_text.as_deref() != Some(title.as_str()));
        Self {
            role: role.to_owned(),
            subrole: non_blank(subrole),
            identifier: non_blank(identifier),
            title,
            description: non_blank(description),
            value: value_text.filter(|_| value_is_label(role)),
            frame,
            path,
        }
    }

    /// Whether any stable text names this element.
    pub fn has_stable_text(&self) -> bool {
        self.title.is_some() || self.description.is_some() || self.value.is_some()
    }

    /// Whether the check needs the live child-index path: only an element no
    /// stable text names. Reading it climbs the ancestry, so elements that
    /// text identifies never pay for it.
    pub fn needs_live_path(&self) -> bool {
        !self.has_stable_text() && !self.path.is_empty()
    }

    /// Whether the gatherer must read `AXValue`: only label roles carry it.
    /// Never set for text fields, whose value can be a whole document.
    pub fn reads_value(&self) -> bool {
        value_is_label(&self.role)
    }
}

/// Roles whose `AXValue` is the text they show rather than state the caller
/// changes. An allowlist, so an unfamiliar control's value never becomes
/// identity (and a large text area's value is never read for a check).
pub fn value_is_label(role: &str) -> bool {
    matches!(role, "AXStaticText" | "AXHeading")
}

/// Roles whose value is state (ported from agent-desktop's
/// `is_mutable_value_role`, in AX role names). An element of such a role with
/// no recorded text has no text to refute it with, so appearing text is not
/// evidence of another element.
pub fn is_mutable_value_role(role: &str) -> bool {
    matches!(
        role,
        "AXComboBox"
            | "AXCheckBox"
            | "AXHandle"
            | "AXIncrementor"
            | "AXStepper"
            | "AXList"
            | "AXRadioButton"
            | "AXScrollBar"
            | "AXSlider"
            | "AXSwitch"
            | "AXTextField"
            | "AXTextArea"
            | "AXSearchField"
            | "AXPopUpButton"
            | "AXDisclosureTriangle"
            | "AXValueIndicator"
            | "AXSplitter"
    )
}

/// One live read: known (with its text), absent (the element reports none),
/// or unknown (the read failed). Unknown never refutes and never confirms.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Field<T> {
    Known(T),
    Absent,
    #[default]
    Unknown,
}

impl<T> Field<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Field::Known(value) => Some(value),
            Field::Absent | Field::Unknown => None,
        }
    }
}

impl Field<String> {
    /// A string read: blank text counts as absent, matching how
    /// [`RefIdentity::new`] records it.
    pub fn text(value: Option<&str>) -> Self {
        match non_blank(value) {
            Some(text) => Field::Known(text),
            None => Field::Absent,
        }
    }
}

/// What a dispatch re-read of the live element found.
///
/// `frame` and `enabled` ride along on the same native read so later checks
/// (actionability) can use them without another round trip.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveElementFacts {
    pub role: Field<String>,
    pub subrole: Field<String>,
    pub identifier: Field<String>,
    pub title: Field<String>,
    pub description: Field<String>,
    /// Read only when [`RefIdentity::reads_value`]; otherwise `Unknown`.
    pub value: Field<String>,
    pub frame: Field<[f64; 4]>,
    pub enabled: Field<bool>,
    /// Read only when [`RefIdentity::needs_live_path`]; otherwise `Unknown`.
    /// `Unknown` also when the ancestry could not be followed (an `AXParent`
    /// whose `AXChildren` does not list the child).
    pub path: Field<Vec<u32>>,
}

/// A live read of a tokenized element.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveRead {
    /// The element is gone (`kAXErrorInvalidUIElement`, -25202).
    Invalid,
    Facts(Box<LiveElementFacts>),
}

/// What confirmed an element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityEvidence {
    StableText,
    Identifier,
    Path,
    Frame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityVerdict {
    Match(IdentityEvidence),
    /// The named field proves the live element is a different one.
    NoMatch(&'static str),
    /// Nothing refutes the element and nothing confirms it beyond the
    /// retained object itself.
    Unknown,
}

/// Why a token is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleReason {
    /// The element no longer exists.
    ElementInvalid,
    /// The live element is a different one; the field says which evidence.
    IdentityMismatch(&'static str),
}

/// Compare recorded identity with live facts. Tiers, strongest first:
/// role and subrole must agree; a recorded identifier must still be there;
/// every recorded stable text must still read the same. Only an element no
/// text names consults the child-index path, and then the frame (which
/// confirms but never refutes: scrolling and window moves shift it).
pub fn match_identity(recorded: &RefIdentity, live: &LiveElementFacts) -> IdentityVerdict {
    if !recorded.role.is_empty() && recorded.role != "AXUnknown" {
        if let Field::Known(role) = &live.role {
            if *role != recorded.role {
                return IdentityVerdict::NoMatch("role");
            }
        }
    }
    let mut evidence = None;
    if let Some(expected) = &recorded.subrole {
        if refuted(expected, &live.subrole) {
            return IdentityVerdict::NoMatch("subrole");
        }
    }
    if let Some(expected) = &recorded.identifier {
        if refuted(expected, &live.identifier) {
            return IdentityVerdict::NoMatch("identifier");
        }
        if live.identifier.known() == Some(expected) {
            evidence = Some(IdentityEvidence::Identifier);
        }
    }
    let texts = [
        ("title", &recorded.title, &live.title),
        ("description", &recorded.description, &live.description),
        ("value", &recorded.value, &live.value),
    ];
    for (field, expected, actual) in texts {
        let Some(expected) = expected else { continue };
        if refuted(expected, actual) {
            return IdentityVerdict::NoMatch(field);
        }
        if actual.known() == Some(expected) {
            evidence = Some(IdentityEvidence::StableText);
        }
    }
    if recorded.has_stable_text() {
        return evidence.map_or(IdentityVerdict::Unknown, IdentityVerdict::Match);
    }
    // No text named the element. If text has appeared on a role whose text
    // is not state, the object now shows something else (a recycled blank
    // row filled with another row's content).
    if !is_mutable_value_role(&recorded.role) {
        for (field, _, actual) in texts {
            if actual.known().is_some() {
                return IdentityVerdict::NoMatch(field);
            }
        }
    }
    if !recorded.path.is_empty() {
        if let Field::Known(path) = &live.path {
            if *path != recorded.path {
                return IdentityVerdict::NoMatch("path");
            }
            evidence = evidence.or(Some(IdentityEvidence::Path));
        }
    }
    if evidence.is_none() {
        if let (Some(expected), Field::Known(actual)) = (recorded.frame, &live.frame) {
            if frames_agree(expected, *actual) {
                evidence = Some(IdentityEvidence::Frame);
            }
        }
    }
    evidence.map_or(IdentityVerdict::Unknown, IdentityVerdict::Match)
}

/// The post-resolution identity gate. `Ok` carries the verdict (`Match` or
/// `Unknown`) and means the retained element may be acted on; `Unknown`
/// proceeds because the retained object is the very element the snapshot
/// walked and nothing contradicts it (agent-desktop treats a retained object
/// as identity outright). `Err` means refuse with [`stale_element_refusal`].
pub fn check_identity(
    recorded: &RefIdentity,
    live: &LiveRead,
) -> Result<IdentityVerdict, StaleReason> {
    match live {
        LiveRead::Invalid => Err(StaleReason::ElementInvalid),
        LiveRead::Facts(facts) => match match_identity(recorded, facts) {
            IdentityVerdict::NoMatch(field) => Err(StaleReason::IdentityMismatch(field)),
            verdict => Ok(verdict),
        },
    }
}

/// The structured `stale_element_token` refusal for a dead or replaced
/// element. Nothing was sent, so retrying after a fresh `get_window_state` is
/// safe. The message never quotes element text: it may be a secret.
pub fn stale_element_refusal(reason: StaleReason, token: Option<&str>) -> ToolResult {
    let (detail, why, field) = match reason {
        StaleReason::ElementInvalid => (
            "the element it names no longer exists (AX error -25202)".to_owned(),
            "element_invalid",
            None,
        ),
        StaleReason::IdentityMismatch(field) => (
            format!(
                "the live element is no longer the one the snapshot recorded (its {field} changed; \
                 the app may have reused the view for other content)"
            ),
            "identity_mismatch",
            Some(field),
        ),
    };
    let message = format!(
        "element_token is stale: {detail}. Nothing was sent; call get_window_state again and \
         act with a fresh token."
    );
    let mut structured = serde_json::json!({
        "status": "refused",
        "code": STALE_ELEMENT_TOKEN,
        "effect": "refused",
        "reason": why,
        "refusal": { "code": STALE_ELEMENT_TOKEN, "message": message },
    });
    if let Some(field) = field {
        structured["mismatch"] = field.into();
    }
    if let Some(token) = token {
        structured["element_token"] = token.into();
    }
    ToolResult::error(message).with_structured(structured)
}

fn refuted(expected: &str, actual: &Field<String>) -> bool {
    match actual {
        Field::Known(actual) => actual != expected,
        Field::Absent => true,
        Field::Unknown => false,
    }
}

fn frames_agree(a: [f64; 4], b: [f64; 4]) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(a, b)| (a - b).abs() <= FRAME_TOLERANCE)
}

fn non_blank(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(text: &str) -> Field<String> {
        Field::Known(text.to_owned())
    }

    fn button(title: &str) -> RefIdentity {
        RefIdentity::new(
            "AXButton",
            None,
            None,
            Some(title),
            None,
            None,
            Some([10.0, 10.0, 80.0, 20.0]),
            vec![0, 3],
        )
    }

    fn live_button(title: &str) -> LiveElementFacts {
        LiveElementFacts {
            role: known("AXButton"),
            subrole: Field::Absent,
            identifier: Field::Absent,
            title: known(title),
            description: Field::Absent,
            frame: Field::Known([200.0, 300.0, 80.0, 20.0]),
            enabled: Field::Known(true),
            ..LiveElementFacts::default()
        }
    }

    #[test]
    fn same_title_matches_even_after_the_window_moved() {
        assert_eq!(
            match_identity(&button("Save"), &live_button("Save")),
            IdentityVerdict::Match(IdentityEvidence::StableText)
        );
    }

    #[test]
    fn changed_title_or_role_is_another_element() {
        assert_eq!(
            match_identity(&button("Save"), &live_button("Delete")),
            IdentityVerdict::NoMatch("title")
        );
        let mut live = live_button("Save");
        live.role = known("AXCheckBox");
        assert_eq!(
            match_identity(&button("Save"), &live),
            IdentityVerdict::NoMatch("role")
        );
        let mut gone = live_button("Save");
        gone.title = Field::Absent;
        assert_eq!(
            match_identity(&button("Save"), &gone),
            IdentityVerdict::NoMatch("title")
        );
    }

    #[test]
    fn unreadable_facts_never_refute() {
        let live = LiveElementFacts::default();
        assert_eq!(
            match_identity(&button("Save"), &live),
            IdentityVerdict::Unknown
        );
        assert_eq!(
            check_identity(&button("Save"), &LiveRead::Facts(Box::new(live))),
            Ok(IdentityVerdict::Unknown)
        );
    }

    #[test]
    fn a_dead_element_is_stale() {
        assert_eq!(
            check_identity(&button("Save"), &LiveRead::Invalid),
            Err(StaleReason::ElementInvalid)
        );
    }

    /// NSTableView reuse: the token's cell view scrolled out and now renders
    /// another row. Same object, same role, same column identifier; only its
    /// text and its row position give it away.
    #[test]
    fn a_recycled_table_cell_is_refused() {
        let recorded = RefIdentity::new(
            "AXStaticText",
            None,
            Some("NameColumn"),
            None,
            None,
            Some("invoice-march.pdf"),
            Some([40.0, 120.0, 200.0, 18.0]),
            vec![0, 1, 7, 0],
        );
        let reused = LiveElementFacts {
            role: known("AXStaticText"),
            identifier: known("NameColumn"),
            title: Field::Absent,
            description: Field::Absent,
            value: known("tax-return.pdf"),
            ..LiveElementFacts::default()
        };
        assert_eq!(
            check_identity(&recorded, &LiveRead::Facts(Box::new(reused))),
            Err(StaleReason::IdentityMismatch("value"))
        );
        let unchanged = LiveElementFacts {
            role: known("AXStaticText"),
            identifier: known("NameColumn"),
            value: known("invoice-march.pdf"),
            ..LiveElementFacts::default()
        };
        assert_eq!(
            match_identity(&recorded, &unchanged),
            IdentityVerdict::Match(IdentityEvidence::StableText)
        );
    }

    /// An unlabeled editable cell has no stable text (its value is state), so
    /// the path decides: the reused view now sits in another row.
    #[test]
    fn a_recycled_unlabeled_cell_is_caught_by_its_path() {
        let recorded = RefIdentity::new(
            "AXTextField",
            None,
            Some("NameColumn"),
            None,
            None,
            Some("draft"),
            Some([40.0, 120.0, 200.0, 18.0]),
            vec![0, 1, 7, 0],
        );
        assert!(
            recorded.value.is_none(),
            "a text field's contents are not identity"
        );
        assert!(recorded.needs_live_path());
        let mut live = LiveElementFacts {
            role: known("AXTextField"),
            identifier: known("NameColumn"),
            value: known("something else typed"),
            frame: Field::Known([40.0, 120.0, 200.0, 18.0]),
            path: Field::Known(vec![0, 1, 12, 0]),
            ..LiveElementFacts::default()
        };
        assert_eq!(
            match_identity(&recorded, &live),
            IdentityVerdict::NoMatch("path")
        );
        live.path = Field::Known(vec![0, 1, 7, 0]);
        assert_eq!(
            match_identity(&recorded, &live),
            IdentityVerdict::Match(IdentityEvidence::Identifier)
        );
    }

    #[test]
    fn text_field_contents_never_refute() {
        let recorded = RefIdentity::new(
            "AXTextField",
            None,
            None,
            None,
            Some("Search"),
            Some("old query"),
            None,
            vec![2],
        );
        let live = LiveElementFacts {
            role: known("AXTextField"),
            description: known("Search"),
            value: known("new query"),
            ..LiveElementFacts::default()
        };
        assert_eq!(
            match_identity(&recorded, &live),
            IdentityVerdict::Match(IdentityEvidence::StableText)
        );
        assert!(!recorded.reads_value());
    }

    #[test]
    fn a_title_echoing_a_controls_value_is_not_identity() {
        let recorded = RefIdentity::new(
            "AXComboBox",
            None,
            None,
            Some("12 pt"),
            None,
            Some("12 pt"),
            None,
            vec![],
        );
        assert!(!recorded.has_stable_text());
    }

    #[test]
    fn text_appearing_on_a_blank_label_is_another_element() {
        let recorded = RefIdentity::new("AXRow", None, None, None, None, None, None, vec![0, 4]);
        let live = LiveElementFacts {
            role: known("AXRow"),
            description: known("Row 9"),
            ..LiveElementFacts::default()
        };
        assert_eq!(
            match_identity(&recorded, &live),
            IdentityVerdict::NoMatch("description")
        );
    }

    #[test]
    fn geometry_confirms_but_never_refutes() {
        let recorded = RefIdentity::new(
            "AXImage",
            None,
            None,
            None,
            None,
            None,
            Some([10.0, 10.0, 16.0, 16.0]),
            vec![],
        );
        let mut live = LiveElementFacts {
            role: known("AXImage"),
            title: Field::Absent,
            description: Field::Absent,
            frame: Field::Known([10.4, 9.8, 16.0, 16.0]),
            ..LiveElementFacts::default()
        };
        assert_eq!(
            match_identity(&recorded, &live),
            IdentityVerdict::Match(IdentityEvidence::Frame)
        );
        live.frame = Field::Known([300.0, 10.0, 16.0, 16.0]);
        assert_eq!(match_identity(&recorded, &live), IdentityVerdict::Unknown);
    }

    #[test]
    fn identifier_and_subrole_changes_are_another_element() {
        let mut recorded = button("OK");
        recorded.identifier = Some("ok-button".into());
        recorded.subrole = Some("AXCloseButton".into());
        let mut live = live_button("OK");
        live.identifier = known("cancel-button");
        live.subrole = known("AXCloseButton");
        assert_eq!(
            match_identity(&recorded, &live),
            IdentityVerdict::NoMatch("identifier")
        );
        live.identifier = known("ok-button");
        live.subrole = known("AXZoomButton");
        assert_eq!(
            match_identity(&recorded, &live),
            IdentityVerdict::NoMatch("subrole")
        );
    }

    #[test]
    fn refusal_is_structured_and_never_quotes_text() {
        let result =
            stale_element_refusal(StaleReason::IdentityMismatch("value"), Some("s00000001:4"));
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["code"], STALE_ELEMENT_TOKEN);
        assert_eq!(structured["refusal"]["code"], STALE_ELEMENT_TOKEN);
        assert_eq!(structured["reason"], "identity_mismatch");
        assert_eq!(structured["mismatch"], "value");
        assert_eq!(structured["element_token"], "s00000001:4");
        let dead = stale_element_refusal(StaleReason::ElementInvalid, None);
        assert_eq!(
            dead.structured_content.unwrap()["reason"],
            "element_invalid"
        );
    }

    /// The pure decision must be a rounding error beside the ~0.6 s action:
    /// the per-action cost is the native read, not this.
    #[test]
    fn matching_is_cheap() {
        let recorded = button("Save");
        let live = LiveRead::Facts(Box::new(live_button("Save")));
        let started = std::time::Instant::now();
        let rounds = 100_000;
        for _ in 0..rounds {
            std::hint::black_box(check_identity(
                std::hint::black_box(&recorded),
                std::hint::black_box(&live),
            ))
            .unwrap();
        }
        let per_check = started.elapsed() / rounds;
        assert!(
            per_check < std::time::Duration::from_micros(50),
            "identity check took {per_check:?}"
        );
    }
}
