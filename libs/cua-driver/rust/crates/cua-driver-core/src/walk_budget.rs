//! Wall-clock and node budget for the accessibility walk behind
//! `get_window_state` (`timeout_ms` / `max_elements`).
//!
//! Every backend walks its own accessibility API (AT-SPI, AX, UIA/MSAA), but
//! the contract is shared: the walk stops at whichever budget runs out first,
//! the tool returns the PARTIAL tree it has, and the response says so with the
//! same structured fields and the same note on every platform.

use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Why a walk stopped before it had visited every node it discovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkStop {
    /// The `timeout_ms` wall-clock budget ran out.
    Timeout,
    /// The `max_elements` node budget ran out.
    NodeBudget,
    /// The application could not be reached through the platform
    /// accessibility API before `timeout_ms` ran out (still launching, or not
    /// yet registered), so nothing was walked.
    AppLookupTimeout,
}

impl WalkStop {
    pub fn as_str(self) -> &'static str {
        match self {
            WalkStop::Timeout => "timeout",
            WalkStop::NodeBudget => "node_budget",
            WalkStop::AppLookupTimeout => "app_lookup_timeout",
        }
    }
}

/// Budget for one walk. Call [`WalkBudget::admit`] before visiting each node;
/// a refused node is counted as discovered but not visited.
///
/// The clock starts at the first admitted node, so `timeout_ms` bounds the
/// walk itself rather than the backend's setup (resolving the application and
/// the target window, which on a freshly launched app can take most of a
/// second on a slow host). Each backend bounds that setup separately with its
/// own backstop past `timeout_ms`.
#[derive(Debug)]
pub struct WalkBudget {
    /// Set by the first [`WalkBudget::admit`].
    started: Option<Instant>,
    timeout: Duration,
    timeout_ms: u64,
    /// False for [`WalkBudget::nodes_only`], whose clock never fires.
    time_bounded: bool,
    max_elements: usize,
    visited: usize,
    pending: usize,
    stop: Option<WalkStop>,
}

impl WalkBudget {
    /// A walk bounded by `timeout_ms` of wall-clock time and `max_elements`
    /// nodes.
    pub fn new(timeout_ms: u64, max_elements: usize) -> Self {
        Self {
            started: None,
            timeout: Duration::from_millis(timeout_ms),
            timeout_ms,
            time_bounded: true,
            max_elements,
            visited: 0,
            pending: 0,
            stop: None,
        }
    }

    /// A walk bounded only by `max_elements` (internal callers with no
    /// caller-supplied time budget).
    pub fn nodes_only(max_elements: usize) -> Self {
        // A year: never fires, without overflowing Instant.
        Self {
            time_bounded: false,
            ..Self::new(365 * 24 * 60 * 60 * 1000, max_elements)
        }
    }

    /// The caller's wall-clock budget, or `None` for a node-only budget.
    /// Backends use it to bound setup that happens before the first node,
    /// such as waiting for a launching application to become reachable.
    pub fn time_limit(&self) -> Option<Duration> {
        self.time_bounded.then_some(self.timeout)
    }

    /// Admit one node for visiting. `false` means the walk must not visit it:
    /// a budget has run out, and the node is counted as pending.
    pub fn admit(&mut self) -> bool {
        let started = *self.started.get_or_insert_with(Instant::now);
        if self.stop.is_none() {
            if self.visited >= self.max_elements {
                self.stop = Some(WalkStop::NodeBudget);
            } else if started.elapsed() >= self.timeout {
                self.stop = Some(WalkStop::Timeout);
            }
        }
        if self.stop.is_some() {
            self.pending += 1;
            return false;
        }
        self.visited += 1;
        true
    }

    /// Whether the wall-clock budget has run out (for phases that cannot be
    /// interrupted per node). Never true before the walk has started.
    pub fn expired(&self) -> bool {
        self.started
            .is_some_and(|started| started.elapsed() >= self.timeout)
    }

    /// Record that the walk ran out of time outside [`WalkBudget::admit`]
    /// (a bulk fetch that returned after the deadline).
    pub fn stop_for_timeout(&mut self) {
        self.stop.get_or_insert(WalkStop::Timeout);
    }

    /// Record that the application never became reachable within the budget,
    /// so the walk visited nothing.
    pub fn stop_for_app_lookup_timeout(&mut self) {
        self.stop.get_or_insert(WalkStop::AppLookupTimeout);
    }

    pub fn outcome(&self) -> WalkOutcome {
        WalkOutcome {
            timeout_ms: self.timeout_ms,
            stop: self.stop,
            nodes_visited: self.visited,
            nodes_pending: self.pending,
            elapsed_ms: self
                .started
                .map_or(0, |started| started.elapsed().as_millis() as u64),
        }
    }
}

/// What a finished walk reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkOutcome {
    pub timeout_ms: u64,
    pub stop: Option<WalkStop>,
    pub nodes_visited: usize,
    pub nodes_pending: usize,
    pub elapsed_ms: u64,
}

impl WalkOutcome {
    /// A walk that produced nothing because the whole budget ran out before
    /// the backend returned (an uninterruptible provider call).
    pub fn timed_out(timeout_ms: u64, elapsed: Duration) -> Self {
        Self {
            timeout_ms,
            stop: Some(WalkStop::Timeout),
            nodes_visited: 0,
            nodes_pending: 0,
            elapsed_ms: elapsed.as_millis() as u64,
        }
    }

    pub fn truncated(&self) -> bool {
        self.stop.is_some()
    }

    pub fn reason(&self) -> Option<&'static str> {
        self.stop.map(WalkStop::as_str)
    }

    /// Write the shared walk fields into a `get_window_state` payload.
    pub fn apply(&self, structured: &mut Value) {
        structured["truncated"] = json!(self.truncated());
        if let Some(reason) = self.reason() {
            structured["truncation_reason"] = json!(reason);
        }
        structured["nodes_visited"] = json!(self.nodes_visited);
        structured["nodes_pending"] = json!(self.nodes_pending);
        structured["walk_elapsed_ms"] = json!(self.elapsed_ms);
        structured["timeout_ms"] = json!(self.timeout_ms);
    }

    /// The PARTIAL TREE note for the tree text, when the walk was cut short.
    pub fn note(&self) -> Option<String> {
        self.truncated().then(|| {
            truncation_note(
                self.reason(),
                self.timeout_ms,
                self.nodes_visited,
                self.nodes_pending,
            )
        })
    }
}

/// The note that heads a partial tree, naming why the walk stopped.
pub fn truncation_note(
    reason: Option<&str>,
    timeout_ms: u64,
    visited: usize,
    pending: usize,
) -> String {
    let why = match reason {
        Some("timeout") => format!("the {timeout_ms} ms timeout_ms budget ran out"),
        Some("node_budget") => "the max_elements node budget ran out".to_owned(),
        Some("app_unresponsive") => {
            "the application stopped answering its accessibility API".to_owned()
        }
        Some("app_lookup_timeout") => format!(
            "the application was not reachable through its accessibility API (AT-SPI, AX) \
             within {timeout_ms} ms; it may still be launching"
        ),
        Some("huge_container") => "a container with more children than can be enumerated \
            (e.g. a spreadsheet's cell grid) was not expanded"
            .to_owned(),
        Some(other) => other.to_owned(),
        None => "the walk stopped early".to_owned(),
    };
    let remedy = if reason == Some("node_budget") {
        "retry with a larger max_elements"
    } else {
        "retry with a larger timeout_ms (e.g. 5000)"
    };
    format!(
        "⚠️ PARTIAL TREE: {why} after {visited} node(s) ({pending} discovered but not visited). \
         Every element listed is real; elements after the cut are missing. If the element you \
         need is absent, {remedy} or narrow with query / max_depth."
    )
}

/// Depth clamp of a skeleton read (`skeleton:true`): rows at rendered depth 0
/// to 3, deep enough to name a window's regions (toolbar, sidebar, list,
/// web area) and shallow enough to stay small on dense apps. The value and
/// the progressive skeleton-then-`root` workflow follow agent-desktop's
/// `snapshot --skeleton` (Apache-2.0, crates/core/src/commands/snapshot.rs).
pub const SKELETON_MAX_DEPTH: usize = 3;

/// The walk depth a read uses: a skeleton read clamps the caller's
/// `max_depth` to [`SKELETON_MAX_DEPTH`]; an explicit smaller value wins.
pub fn effective_max_depth(max_depth: usize, skeleton: bool) -> usize {
    if skeleton {
        max_depth.min(SKELETON_MAX_DEPTH)
    } else {
        max_depth
    }
}

/// Whether a row at `depth` with `child_count` children has those children
/// cut by the depth clamp. The children sit at `depth + 1`.
pub fn depth_cut(depth: usize, max_depth: usize, child_count: usize) -> bool {
    child_count > 0 && depth + 1 > max_depth
}

/// Whether a row gets an element index only so it can be a `root` drill
/// target: it is not addressable itself, but the depth clamp hid its
/// children, and drilling into it is how a caller reaches them. Mirrors
/// agent-desktop's skeleton anchors (crates/core/src/refs/ref_alloc.rs), which
/// are planned only where a depth clamp left a `children_count`.
pub fn is_drill_anchor(actionable: bool, depth_cut: bool) -> bool {
    !actionable && depth_cut
}

/// How much of one subtree a walk observed. Ordered: merging keeps the worst.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Observed {
    /// Every descendant was visited (or deliberately collapsed).
    #[default]
    Complete,
    /// The node was visited, but something below it was not.
    Partial,
    /// The node itself was not visited (node or time budget, or depth
    /// clamp), or its children could not be read. A rendered parent that
    /// receives this from a direct child is where the cut happened.
    Skipped,
}

impl Observed {
    pub fn merge(self, other: Observed) -> Observed {
        self.max(other)
    }
}

/// Per-row completeness, recorded on the walked node. Completeness is
/// reported on each row whose descendants were cut, so a reader can walk from
/// the root to the boundary (agent-desktop CONCEPTS.md, "Partial
/// Observation"): a depth clamp knows how many children it skipped, while a
/// budget stop cannot afford the count and marks the row without one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubtreeMark {
    /// Some descendant of this row was not observed.
    pub truncated: bool,
    /// The cut is at this row's own children (not further down), so this
    /// is the row to drill into or to re-read with a larger budget.
    pub cut_here: bool,
    /// Children hidden by the depth clamp. `None` for a budget stop.
    pub children_count: Option<usize>,
    /// The row is indexed only as a drill target (see [`is_drill_anchor`]).
    pub drill_anchor: bool,
}

impl SubtreeMark {
    /// Mark for a row from what its children reported. `children` is the
    /// merge of every direct child's [`Observed`] (`Skipped` also when the
    /// child list itself could not be read); `children_count` is the depth
    /// clamp's count when the clamp cut this row's children.
    pub fn from_children(children: Observed, children_count: Option<usize>) -> Self {
        Self {
            truncated: children_count.is_some() || children != Observed::Complete,
            cut_here: children_count.is_some() || children == Observed::Skipped,
            children_count,
            drill_anchor: false,
        }
    }

    /// The marker appended to the row's markdown, only where the cut is:
    /// `{+N children}` for a depth clamp, `{partial}` for a budget stop or an
    /// unreadable child list. Ancestors of a cut carry the structured
    /// `subtree_truncated` only, so a skeleton read is not noise on every row.
    pub fn row_suffix(&self) -> Option<String> {
        match (self.cut_here, self.children_count) {
            (_, Some(1)) => Some(" {+1 child}".to_owned()),
            (_, Some(n)) => Some(format!(" {{+{n} children}}")),
            (true, None) => Some(" {partial}".to_owned()),
            (false, None) => None,
        }
    }

    /// Write the mark into a structured `elements` entry.
    pub fn apply(&self, entry: &mut Value) {
        if self.truncated {
            entry["subtree_truncated"] = json!(true);
        }
        if let Some(count) = self.children_count {
            entry["children_count"] = json!(count);
        }
        if self.drill_anchor {
            entry["drill_only"] = json!(true);
        }
    }
}

/// The facts behind `elements_complete`, the claim that a read observed
/// every element in its scope, so that absence from it is evidence of
/// absence (verify_state uses it to prove an element is gone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletenessFacts {
    /// The walk ran out of nodes or time.
    pub walk_truncated: bool,
    /// Merge of every top-level subtree's [`Observed`]: depth clamps and
    /// unreadable child lists make it incomplete.
    pub subtrees: Observed,
    /// The requested window resolved to an AX surface.
    pub scope_matched: bool,
    /// Closed menu-bar menus were listed by title only.
    pub menus_collapsed: bool,
    /// The read was degraded (empty or unresolved tree).
    pub degraded: bool,
}

/// Whether `elements_complete` may be claimed. Every unknown fails closed:
/// a single unread child list keeps negative existence unknown.
pub fn elements_complete(facts: CompletenessFacts) -> bool {
    !facts.walk_truncated
        && facts.subtrees == Observed::Complete
        && facts.scope_matched
        && !facts.menus_collapsed
        && !facts.degraded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_budget_stops_and_counts_pending() {
        let mut budget = WalkBudget::nodes_only(2);
        assert!(budget.admit());
        assert!(budget.admit());
        assert!(!budget.admit());
        assert!(!budget.admit());
        let outcome = budget.outcome();
        assert_eq!(outcome.reason(), Some("node_budget"));
        assert_eq!((outcome.nodes_visited, outcome.nodes_pending), (2, 2));
    }

    #[test]
    fn only_a_caller_budget_bounds_setup_waits() {
        assert_eq!(
            WalkBudget::new(1500, 10).time_limit(),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(WalkBudget::nodes_only(10).time_limit(), None);
    }

    #[test]
    fn an_unreachable_application_reports_app_lookup_timeout() {
        let mut budget = WalkBudget::new(1000, 10);
        budget.stop_for_app_lookup_timeout();
        let outcome = budget.outcome();
        assert!(outcome.truncated());
        assert_eq!(outcome.reason(), Some("app_lookup_timeout"));
        assert_eq!((outcome.nodes_visited, outcome.nodes_pending), (0, 0));
        let mut structured = json!({});
        outcome.apply(&mut structured);
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["truncation_reason"], "app_lookup_timeout");
        let note = outcome.note().unwrap();
        assert!(note.contains("not reachable through its accessibility API"));
        assert!(note.contains("1000 ms"));
    }

    #[test]
    fn expired_deadline_stops_with_timeout() {
        let mut budget = WalkBudget::new(0, 100);
        // Setup before the first node does not count against the budget.
        assert!(!budget.expired());
        assert!(!budget.admit());
        assert!(budget.expired());
        let outcome = budget.outcome();
        assert_eq!(outcome.reason(), Some("timeout"));
        assert_eq!((outcome.nodes_visited, outcome.nodes_pending), (0, 1));
    }

    #[test]
    fn the_clock_starts_at_the_first_node() {
        let mut budget = WalkBudget::new(50, 100);
        std::thread::sleep(Duration::from_millis(80));
        assert!(budget.admit(), "setup time must not exhaust the budget");
    }

    #[test]
    fn a_complete_walk_is_not_truncated() {
        let mut budget = WalkBudget::new(60_000, 10);
        for _ in 0..10 {
            assert!(budget.admit());
        }
        let outcome = budget.outcome();
        assert!(!outcome.truncated());
        assert_eq!(outcome.note(), None);
        let mut structured = json!({});
        outcome.apply(&mut structured);
        assert_eq!(structured["truncated"], false);
        assert!(structured.get("truncation_reason").is_none());
        assert_eq!(structured["nodes_visited"], 10);
        assert_eq!(structured["timeout_ms"], 60_000);
    }

    #[test]
    fn a_timed_out_walk_reports_the_budget() {
        let outcome = WalkOutcome::timed_out(1000, Duration::from_millis(1200));
        let mut structured = json!({});
        outcome.apply(&mut structured);
        assert_eq!(structured["truncated"], true);
        assert_eq!(structured["truncation_reason"], "timeout");
        let note = outcome.note().unwrap();
        assert!(note.starts_with("⚠️ PARTIAL TREE: the 1000 ms timeout_ms budget ran out"));
    }

    #[test]
    fn truncation_note_names_the_budget_and_the_remedy() {
        let note = truncation_note(Some("timeout"), 1000, 240, 88);
        assert!(note.contains("PARTIAL TREE"));
        assert!(note.contains("1000 ms"));
        assert!(note.contains("240 node(s)"));
        assert!(note.contains("88 discovered"));
        assert!(note.contains("timeout_ms"));
        assert!(note.contains("query"));
        let note = truncation_note(Some("node_budget"), 1000, 5000, 3);
        assert!(note.contains("max_elements"));
        let note = truncation_note(Some("app_unresponsive"), 1000, 3, 0);
        assert!(note.contains("stopped answering"));
        let note = truncation_note(Some("app_lookup_timeout"), 250, 0, 0);
        assert!(note.contains("250 ms"));
        assert!(note.contains("still be launching"));
        let note = truncation_note(Some("huge_container"), 1000, 1918, 0);
        assert!(note.contains("not expanded"));
    }

    #[test]
    fn skeleton_clamps_depth_and_an_explicit_smaller_depth_wins() {
        assert_eq!(effective_max_depth(25, true), SKELETON_MAX_DEPTH);
        assert_eq!(effective_max_depth(2, true), 2);
        assert_eq!(effective_max_depth(25, false), 25);
    }

    #[test]
    fn depth_cut_needs_children_past_the_clamp() {
        assert!(depth_cut(3, 3, 4));
        assert!(!depth_cut(2, 3, 4), "children at depth 3 are still walked");
        assert!(!depth_cut(3, 3, 0), "a leaf has nothing to cut");
        assert!(is_drill_anchor(false, true));
        assert!(
            !is_drill_anchor(true, true),
            "an addressable row has its own index"
        );
        assert!(!is_drill_anchor(false, false));
    }

    #[test]
    fn observed_merges_to_the_worst() {
        use Observed::*;
        assert_eq!(Complete.merge(Partial), Partial);
        assert_eq!(Partial.merge(Skipped), Skipped);
        assert_eq!(Complete.merge(Complete), Complete);
    }

    #[test]
    fn marks_put_the_marker_only_where_the_cut_is() {
        let clamp = SubtreeMark::from_children(Observed::Complete, Some(12));
        assert!(clamp.truncated && clamp.cut_here);
        assert_eq!(clamp.row_suffix().as_deref(), Some(" {+12 children}"));
        assert_eq!(
            SubtreeMark::from_children(Observed::Complete, Some(1))
                .row_suffix()
                .as_deref(),
            Some(" {+1 child}")
        );

        let budget = SubtreeMark::from_children(Observed::Skipped, None);
        assert!(budget.truncated && budget.cut_here);
        assert_eq!(budget.row_suffix().as_deref(), Some(" {partial}"));

        let ancestor = SubtreeMark::from_children(Observed::Partial, None);
        assert!(ancestor.truncated && !ancestor.cut_here);
        assert_eq!(
            ancestor.row_suffix(),
            None,
            "ancestors stay unmarked in markdown"
        );

        let whole = SubtreeMark::from_children(Observed::Complete, None);
        assert_eq!(whole, SubtreeMark::default());
        assert_eq!(whole.row_suffix(), None);

        let mut entry = json!({});
        SubtreeMark {
            drill_anchor: true,
            ..clamp
        }
        .apply(&mut entry);
        assert_eq!(
            entry,
            json!({"subtree_truncated": true, "children_count": 12, "drill_only": true})
        );
        let mut entry = json!({});
        whole.apply(&mut entry);
        assert_eq!(entry, json!({}));
    }

    #[test]
    fn elements_complete_fails_closed_on_every_gap() {
        let complete = CompletenessFacts {
            walk_truncated: false,
            subtrees: Observed::Complete,
            scope_matched: true,
            menus_collapsed: false,
            degraded: false,
        };
        assert!(elements_complete(complete));
        for facts in [
            CompletenessFacts {
                walk_truncated: true,
                ..complete
            },
            CompletenessFacts {
                subtrees: Observed::Partial,
                ..complete
            },
            CompletenessFacts {
                subtrees: Observed::Skipped,
                ..complete
            },
            CompletenessFacts {
                scope_matched: false,
                ..complete
            },
            CompletenessFacts {
                menus_collapsed: true,
                ..complete
            },
            CompletenessFacts {
                degraded: true,
                ..complete
            },
        ] {
            assert!(!elements_complete(facts), "{facts:?}");
        }
    }
}
