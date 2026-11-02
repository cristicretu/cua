// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Delivery disposition: whether a call's input reached the app, and whether
//! repeating it is safe.
//!
//! `effect` says what an action did once it ran. It cannot say whether a
//! failed call sent anything, and a plain error said nothing at all, so an
//! agent had to guess whether a retry could double-fire a press or retype
//! text. Every mutating tool's structured result now carries
//! `disposition: {delivery, retry}` (vocabulary from agent-desktop's
//! `docs/json-output.md`, Apache-2.0):
//!
//! - a refusal, invalid arguments, or any error from a read-only tool is
//!   `not_delivered` / `safe`;
//! - a refusal whose code can follow delivery ([`MAY_HAVE_DELIVERED`]), or an
//!   AX write the app answered with a failure, is `delivery_uncertain` /
//!   `unsafe`;
//! - a successful action follows its `effect`: `confirmed` is
//!   `delivered_verified`, `unverifiable` and `suspected_noop` are
//!   `delivered_unverified`;
//! - an error with no code and no evidence is `unknown` / `unknown`.
//!
//! [`stamp`] runs once at the dispatch seam beside
//! [`crate::recovery_hint::annotate`], so every platform, every tool and every
//! nested `run_steps`/`run_script` call is covered, including refusal codes
//! added later. A producer that already set `disposition` (an AX error it
//! classified, `run_steps` aggregating its steps) keeps its own value.
//! Read-only successes are left as they are: they send nothing and their
//! payloads stay lean.

use serde_json::{Map, Value};

use crate::protocol::ToolResult;

pub use cua_driver_contract::{DeliveryDisposition, Disposition, RetryDisposition};

/// Refusal codes a tool reports after input may already have landed. Every
/// other structured refusal is reported before anything is sent.
pub const MAY_HAVE_DELIVERED: &[&str] = &[
    // Part of the input landed, or the post failed midway.
    "delivery_failed",
    "foreground_partial_unknown",
    "type_text_incomplete",
    "browser_input_incomplete",
    // The tool ran; the driver could not publish or confirm its result.
    "typed_output_mismatch",
    "action_outcome_mismatch",
    "tool_output_invalid",
    "runtime_output_key_collision",
    // A write was sent and the readback disagreed.
    "write_not_applied",
    "save_unconfirmed",
    "rename_unconfirmed",
    // An Apple Event script failed or timed out after it was sent.
    "automation_failed",
    "automation_timeout",
    // The app answered an AX write with kAXErrorFailure, kAXErrorCannotComplete
    // or an unclassified error (agent-desktop `ax_mutation.rs`).
    "ax_action_failed",
    "app_unresponsive",
    // AppKit returns "unsupported" from presses it performed (#3835).
    "ax_action_unsupported",
    // set_state: a press ran and the old state reads back (a second press
    // would toggle again), or a mechanism failed in a way that may have acted.
    "state_not_reached",
    "delivery_uncertain",
    // set_window_state wrote the attribute and the window did not follow.
    "window_state_not_reached",
    // close_app asked the app to quit; it may still be saving or prompting.
    "app_still_running",
];

/// The disposition a structured result already declares, when it is valid.
pub fn declared(structured: &Map<String, Value>) -> Option<Disposition> {
    serde_json::from_value(structured.get("disposition")?.clone()).ok()
}

/// The disposition of one tool result, or `None` when it gets none: a
/// read-only success, which sends nothing and is not an action.
pub fn derive(
    read_only: bool,
    is_error: bool,
    structured: Option<&Map<String, Value>>,
) -> Option<Disposition> {
    if let Some(declared) = structured.and_then(declared) {
        return Some(declared);
    }
    let field = |key: &str| {
        structured
            .and_then(|structured| structured.get(key))
            .and_then(Value::as_str)
    };
    let effect = field("effect");
    let refused = is_error || effect == Some("refused") || field("status") == Some("refused");
    if read_only {
        // A read never reaches the app, whatever it answered.
        return refused.then(Disposition::not_delivered);
    }
    if refused {
        if field("execution_state") == Some("unknown") {
            return Some(Disposition::of(DeliveryDisposition::Unknown));
        }
        return Some(match structured.and_then(crate::recovery_hint::code_of) {
            Some(code) if MAY_HAVE_DELIVERED.contains(&code) => Disposition::uncertain(),
            Some(_) => Disposition::not_delivered(),
            // A bare error carries no evidence either way.
            None => Disposition::of(DeliveryDisposition::Unknown),
        });
    }
    Some(Disposition::of(match effect {
        Some("confirmed") => DeliveryDisposition::DeliveredVerified,
        Some("partial") => DeliveryDisposition::DeliveryUncertain,
        // The tool ran and nothing proved what it did.
        _ => DeliveryDisposition::DeliveredUnverified,
    }))
}

/// The disposition of a batch from its steps' dispositions, in order. Nothing
/// sent is `not_delivered`; any step that may have landed makes a retry of
/// the whole batch unsafe.
pub fn aggregate(steps: impl IntoIterator<Item = Disposition>) -> Disposition {
    use DeliveryDisposition::*;
    let mut delivered = Vec::new();
    for step in steps {
        if step.delivery != NotDelivered {
            delivered.push(step.delivery);
        }
    }
    let delivery = if delivered.is_empty() {
        NotDelivered
    } else if delivered.contains(&Unknown) {
        Unknown
    } else if delivered.contains(&DeliveryUncertain) {
        DeliveryUncertain
    } else if delivered
        .iter()
        .all(|delivery| *delivery == DeliveredVerified)
    {
        DeliveredVerified
    } else {
        DeliveredUnverified
    };
    Disposition::of(delivery)
}

/// The disposition [`stamp`] put on a result, if any.
pub fn of_result(result: &ToolResult) -> Option<Disposition> {
    match result.structured_content.as_ref() {
        Some(Value::Object(structured)) => declared(structured),
        _ => None,
    }
}

/// Add `disposition` to a tool result. A producer's own value stays. Called
/// once per tool call at the dispatch seam, after the recovery hint, so a
/// plain argument error the hint gave a code is recognised as one.
pub fn stamp(read_only: bool, result: &mut ToolResult) {
    let is_error = result.is_error == Some(true);
    let disposition = match result.structured_content.as_ref() {
        Some(Value::Object(structured)) => derive(read_only, is_error, Some(structured)),
        Some(_) => return,
        // A success with text only stays text only: some clients show the
        // model only `structuredContent` when it is present.
        None if !is_error => return,
        None => derive(read_only, is_error, None),
    };
    let Some(disposition) = disposition else {
        return;
    };
    let structured = result
        .structured_content
        .get_or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(structured) = structured {
        structured
            .entry("disposition")
            .or_insert_with(|| serde_json::to_value(disposition).expect("disposition serializes"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn of(read_only: bool, is_error: bool, structured: Value) -> Option<Disposition> {
        derive(read_only, is_error, structured.as_object())
    }

    fn delivery(read_only: bool, is_error: bool, structured: Value) -> DeliveryDisposition {
        of(read_only, is_error, structured)
            .expect("disposition")
            .delivery
    }

    #[test]
    fn retry_follows_delivery() {
        use DeliveryDisposition::*;
        assert_eq!(Disposition::of(NotDelivered).retry, RetryDisposition::Safe);
        assert_eq!(Disposition::of(Unknown).retry, RetryDisposition::Unknown);
        for delivery in [DeliveryUncertain, DeliveredUnverified, DeliveredVerified] {
            assert_eq!(Disposition::of(delivery).retry, RetryDisposition::Unsafe);
        }
        assert_eq!(
            serde_json::to_value(Disposition::uncertain()).unwrap(),
            json!({"delivery": "delivery_uncertain", "retry": "unsafe"})
        );
    }

    #[test]
    fn any_coded_refusal_is_not_delivered_and_safe() {
        // Including codes this table has never heard of, such as a stale
        // token refusal from the shared resolve path.
        for code in [
            "stale_element_token",
            "invalid_arguments",
            "background_unavailable",
            "a_code_added_later",
        ] {
            let disposition = of(false, true, json!({"code": code})).unwrap();
            assert_eq!(disposition, Disposition::not_delivered(), "{code}");
            assert!(disposition.is_retry_safe());
        }
        // The browser and window-target refusal shapes.
        assert_eq!(
            delivery(
                false,
                false,
                json!({"status": "refused", "refusal": {"code": "browser_ref_stale"}})
            ),
            DeliveryDisposition::NotDelivered
        );
        assert_eq!(
            delivery(
                false,
                true,
                json!({"effect": "refused", "error": {"code": "target_occluded"}})
            ),
            DeliveryDisposition::NotDelivered
        );
    }

    #[test]
    fn a_refusal_after_delivery_is_uncertain_and_unsafe() {
        for code in MAY_HAVE_DELIVERED {
            let disposition = of(false, true, json!({"code": code})).unwrap();
            assert_eq!(disposition, Disposition::uncertain(), "{code}");
        }
        assert_eq!(
            delivery(
                false,
                false,
                json!({"status": "refused", "refusal": {"code": "browser_input_incomplete"}})
            ),
            DeliveryDisposition::DeliveryUncertain
        );
    }

    #[test]
    fn an_error_without_evidence_is_unknown() {
        assert_eq!(
            of(false, true, json!({})).unwrap(),
            Disposition::of(DeliveryDisposition::Unknown)
        );
        assert_eq!(
            derive(false, true, None).unwrap().retry,
            RetryDisposition::Unknown
        );
        // An execution the driver lost track of stays unknown even with a code.
        assert_eq!(
            delivery(
                false,
                true,
                json!({"code": "a_code", "execution_state": "unknown"})
            ),
            DeliveryDisposition::Unknown
        );
    }

    #[test]
    fn a_successful_action_follows_its_effect() {
        use DeliveryDisposition::*;
        for (effect, expected) in [
            ("confirmed", DeliveredVerified),
            ("unverifiable", DeliveredUnverified),
            ("suspected_noop", DeliveredUnverified),
            ("partial", DeliveryUncertain),
        ] {
            assert_eq!(
                delivery(
                    false,
                    false,
                    json!({"effect": effect, "route": "accessibility"})
                ),
                expected,
                "{effect}"
            );
        }
        assert_eq!(
            delivery(false, false, json!({"pid": 42})),
            DeliveredUnverified
        );
    }

    #[test]
    fn reads_are_not_delivered_on_error_and_unstamped_on_success() {
        assert_eq!(
            of(true, true, json!({"code": "window_id_not_found"})),
            Some(Disposition::not_delivered())
        );
        // A read-only error is not_delivered even when its code is one that
        // follows delivery for a mutating tool.
        assert_eq!(
            of(true, true, json!({"code": "typed_output_mismatch"})),
            Some(Disposition::not_delivered())
        );
        assert_eq!(derive(true, true, None), Some(Disposition::not_delivered()));
        assert_eq!(of(true, false, json!({"elements": []})), None);
    }

    #[test]
    fn a_producer_disposition_wins() {
        let declared = json!({"code": "invalid_arguments",
            "disposition": {"delivery": "delivery_uncertain", "retry": "unsafe"}});
        assert_eq!(of(false, true, declared), Some(Disposition::uncertain()));
        // A malformed one is ignored rather than trusted.
        let malformed = json!({"code": "invalid_arguments", "disposition": "safe"});
        assert_eq!(
            of(false, true, malformed),
            Some(Disposition::not_delivered())
        );
    }

    #[test]
    fn a_batch_is_safe_to_retry_only_when_no_step_delivered() {
        use DeliveryDisposition::*;
        let d = Disposition::of;
        assert_eq!(aggregate([]), d(NotDelivered));
        assert_eq!(
            aggregate([d(NotDelivered), d(NotDelivered)]),
            d(NotDelivered)
        );
        assert_eq!(
            aggregate([d(DeliveredVerified), d(DeliveredVerified)]),
            d(DeliveredVerified)
        );
        // A batch that stopped at a refusal after a step landed.
        assert_eq!(
            aggregate([d(DeliveredVerified), d(NotDelivered)]),
            d(DeliveredVerified)
        );
        assert_eq!(
            aggregate([d(DeliveredVerified), d(DeliveredUnverified)]),
            d(DeliveredUnverified)
        );
        assert_eq!(
            aggregate([d(DeliveredVerified), d(DeliveryUncertain)]),
            d(DeliveryUncertain)
        );
        assert_eq!(aggregate([d(DeliveryUncertain), d(Unknown)]), d(Unknown));
    }

    #[test]
    fn stamp_keeps_text_only_successes_and_gives_bare_errors_a_payload() {
        let mut success = ToolResult::text("done");
        stamp(false, &mut success);
        assert!(success.structured_content.is_none());

        let mut error = ToolResult::error("boom");
        stamp(false, &mut error);
        assert_eq!(
            error.structured_content,
            Some(json!({"disposition": {"delivery": "unknown", "retry": "unknown"}}))
        );

        let mut refusal =
            ToolResult::error("stale").with_structured(json!({"code": "stale_element_token"}));
        stamp(false, &mut refusal);
        assert_eq!(of_result(&refusal), Some(Disposition::not_delivered()));

        let mut read = ToolResult::text("tree").with_structured(json!({"elements": []}));
        stamp(true, &mut read);
        assert_eq!(read.structured_content, Some(json!({"elements": []})));
    }
}
