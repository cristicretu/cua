use super::*;

fn checkbox(checked: Option<bool>) -> ToggleFacts {
    ToggleFacts {
        role: "AXCheckBox".into(),
        checked,
        value_settable: false,
        press_advertised: true,
        enabled: Some(true),
    }
}

fn run(desired: bool, mechanisms: &[Mechanism]) -> Plan {
    Plan::Run {
        desired,
        mechanisms: mechanisms.to_vec(),
    }
}

fn refusal_code(plan: Plan) -> &'static str {
    match plan {
        Plan::Refuse(refusal) => refusal.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn state_names_parse_including_verb_forms() {
    for name in STATE_NAMES {
        assert_eq!(RequestedState::parse(name).unwrap().as_str(), *name);
    }
    assert_eq!(
        RequestedState::parse("Check"),
        Some(RequestedState::Checked)
    );
    assert_eq!(
        RequestedState::parse("toggle"),
        Some(RequestedState::Toggled)
    );
    assert_eq!(
        RequestedState::parse("expand"),
        Some(RequestedState::Expanded)
    );
    assert_eq!(
        RequestedState::parse("focus"),
        Some(RequestedState::Focused)
    );
    assert_eq!(RequestedState::parse("visible"), None);
}

#[test]
fn checked_values_follow_the_agent_desktop_table() {
    for value in ["1", "true", "TRUE", "YES", "on", "checked"] {
        assert_eq!(parse_checked_value(value), Some(true), "{value}");
    }
    for value in ["0", "false", "NO", "off", "unchecked"] {
        assert_eq!(parse_checked_value(value), Some(false), "{value}");
    }
    for value in ["2", "mixed", "indeterminate", ""] {
        assert_eq!(parse_checked_value(value), None, "{value}");
    }
}

#[test]
fn check_is_idempotent_and_sends_nothing_when_already_held() {
    assert_eq!(
        plan_check(RequestedState::Checked, &checkbox(Some(true))),
        Plan::AlreadySatisfied
    );
    assert_eq!(
        plan_check(RequestedState::Unchecked, &checkbox(Some(false))),
        Plan::AlreadySatisfied
    );
    assert_eq!(
        plan_check(RequestedState::Checked, &checkbox(Some(false))),
        run(true, &[Mechanism::Press])
    );
}

#[test]
fn toggle_always_acts_toward_the_opposite_state() {
    assert_eq!(
        plan_check(RequestedState::Toggled, &checkbox(Some(true))),
        run(false, &[Mechanism::Press])
    );
    assert_eq!(
        plan_check(RequestedState::Toggled, &checkbox(Some(false))),
        run(true, &[Mechanism::Press])
    );
}

#[test]
fn a_settable_value_is_written_before_any_press() {
    let facts = ToggleFacts {
        value_settable: true,
        ..checkbox(Some(false))
    };
    assert_eq!(
        plan_check(RequestedState::Checked, &facts),
        run(true, &[Mechanism::WriteValue, Mechanism::Press])
    );
}

#[test]
fn unreadable_or_mixed_state_is_never_pressed_blindly() {
    for requested in [
        RequestedState::Checked,
        RequestedState::Unchecked,
        RequestedState::Toggled,
    ] {
        assert_eq!(
            refusal_code(plan_check(requested, &checkbox(None))),
            "state_unreadable"
        );
    }
}

#[test]
fn check_refuses_wrong_roles_disabled_elements_and_radio_uncheck() {
    let button = ToggleFacts {
        role: "AXButton".into(),
        ..checkbox(Some(false))
    };
    assert_eq!(
        refusal_code(plan_check(RequestedState::Checked, &button)),
        "state_not_supported"
    );
    let disabled = ToggleFacts {
        enabled: Some(false),
        ..checkbox(Some(false))
    };
    assert_eq!(
        refusal_code(plan_check(RequestedState::Checked, &disabled)),
        "element_disabled"
    );
    let radio = |checked| ToggleFacts {
        role: "AXRadioButton".into(),
        ..checkbox(Some(checked))
    };
    assert_eq!(
        refusal_code(plan_check(RequestedState::Unchecked, &radio(true))),
        "radio_cannot_uncheck"
    );
    assert_eq!(
        refusal_code(plan_check(RequestedState::Toggled, &radio(true))),
        "radio_cannot_uncheck"
    );
    assert_eq!(
        plan_check(RequestedState::Unchecked, &radio(false)),
        Plan::AlreadySatisfied,
        "an unselected radio is already unchecked"
    );
    assert_eq!(
        plan_check(RequestedState::Checked, &radio(false)),
        run(true, &[Mechanism::Press])
    );
    let no_route = ToggleFacts {
        press_advertised: false,
        ..checkbox(Some(false))
    };
    assert_eq!(
        refusal_code(plan_check(RequestedState::Checked, &no_route)),
        "no_state_route"
    );
}

#[test]
fn disclosure_never_presses_from_an_unknown_state() {
    let facts = |expanded| DisclosureFacts {
        expanded,
        direction_action_advertised: false,
        expanded_settable: false,
        disclosing_settable: false,
        press_advertised: true,
        enabled: Some(true),
    };
    assert_eq!(
        plan_disclosure(RequestedState::Expanded, &facts(Some(true))),
        Plan::AlreadySatisfied
    );
    assert_eq!(
        plan_disclosure(RequestedState::Expanded, &facts(Some(false))),
        run(true, &[Mechanism::Press])
    );
    assert_eq!(
        refusal_code(plan_disclosure(RequestedState::Collapsed, &facts(None))),
        "state_not_supported"
    );
}

#[test]
fn disclosure_prefers_the_action_then_attributes_then_press() {
    let facts = DisclosureFacts {
        expanded: Some(true),
        direction_action_advertised: true,
        expanded_settable: true,
        disclosing_settable: true,
        press_advertised: true,
        enabled: None,
    };
    assert_eq!(
        plan_disclosure(RequestedState::Collapsed, &facts),
        run(
            false,
            &[
                Mechanism::DisclosureAction,
                Mechanism::WriteExpanded,
                Mechanism::WriteDisclosing,
                Mechanism::Press,
            ]
        )
    );
    let unknown = DisclosureFacts {
        expanded: None,
        ..facts
    };
    assert_eq!(
        plan_disclosure(RequestedState::Expanded, &unknown),
        run(
            true,
            &[
                Mechanism::DisclosureAction,
                Mechanism::WriteExpanded,
                Mechanism::WriteDisclosing,
            ]
        ),
        "idempotent routes may run on an unreadable state; a press may not"
    );
}

#[test]
fn disclosure_state_reads_expanded_then_disclosing_then_a_triangle_value() {
    assert_eq!(
        disclosure_state(Some(false), Some(true), None, None),
        Some(false)
    );
    assert_eq!(disclosure_state(None, Some(true), None, None), Some(true));
    assert_eq!(
        disclosure_state(None, None, Some("AXDisclosureTriangle"), Some("1")),
        Some(true)
    );
    assert_eq!(
        disclosure_state(None, None, Some("AXCheckBox"), Some("1")),
        None,
        "a checkbox value is not an expanded state"
    );
}

#[test]
fn focus_plan() {
    let facts = |focused, settable| FocusFacts {
        focused,
        focused_settable: settable,
        enabled: None,
    };
    assert_eq!(plan_focus(&facts(true, false)), Plan::AlreadySatisfied);
    assert_eq!(
        plan_focus(&facts(false, true)),
        run(true, &[Mechanism::WriteFocused])
    );
    assert_eq!(
        refusal_code(plan_focus(&facts(false, false))),
        "focus_not_settable"
    );
}

#[test]
fn the_read_back_decides_even_after_an_error() {
    for result in [
        AttemptResult::Ok,
        AttemptResult::Unsupported,
        AttemptResult::Uncertain,
    ] {
        assert_eq!(
            after_attempt(Mechanism::Press, result, true, Some(true)),
            Next::Verified
        );
    }
}

#[test]
fn an_uncertain_mechanism_ends_the_call() {
    assert_eq!(
        after_attempt(
            Mechanism::WriteValue,
            AttemptResult::Uncertain,
            true,
            Some(false)
        ),
        Next::Stop(Failure::Uncertain)
    );
    assert!(Failure::Uncertain.may_have_delivered());
}

#[test]
fn an_ignored_write_falls_through_but_a_press_that_ran_does_not() {
    assert_eq!(
        after_attempt(Mechanism::WriteValue, AttemptResult::Ok, true, Some(false)),
        Next::TryNext {
            ignored_write: true
        }
    );
    assert_eq!(
        after_attempt(Mechanism::Press, AttemptResult::Ok, true, Some(false)),
        Next::Stop(Failure::NotReached)
    );
    assert_eq!(
        after_attempt(
            Mechanism::DisclosureAction,
            AttemptResult::Ok,
            true,
            Some(false)
        ),
        Next::Stop(Failure::NotReached)
    );
    assert_eq!(
        after_attempt(Mechanism::Press, AttemptResult::Ok, true, None),
        Next::Unverified
    );
}

#[test]
fn unsupported_and_stale_mechanisms_deliver_nothing() {
    assert_eq!(
        after_attempt(
            Mechanism::DisclosureAction,
            AttemptResult::Unsupported,
            true,
            Some(false)
        ),
        Next::TryNext {
            ignored_write: false
        }
    );
    assert_eq!(
        after_attempt(Mechanism::Press, AttemptResult::Stale, true, None),
        Next::Stop(Failure::Stale)
    );
    assert!(!Failure::Stale.may_have_delivered());
    assert_eq!(Failure::Stale.code(), "stale_element_token");
    assert_eq!(exhausted(true), Failure::WriteIgnored);
    assert_eq!(exhausted(false), Failure::NoRoute);
}

#[test]
fn clear_plan_and_secure_fields() {
    let field = |role: &str, secure, empty, settable| ClearFacts {
        role: role.into(),
        secure,
        value_settable: settable,
        empty,
        enabled: None,
    };
    assert_eq!(
        plan_clear(&field("AXTextField", false, Some(true), true)),
        Plan::AlreadySatisfied
    );
    assert_eq!(
        plan_clear(&field("AXTextField", true, Some(true), true)),
        run(true, &[Mechanism::WriteValue]),
        "a secure field reads empty whatever it holds, so it is always written"
    );
    assert_eq!(
        refusal_code(plan_clear(&field("AXTextField", false, Some(false), false))),
        "value_not_settable"
    );
    assert_eq!(
        refusal_code(plan_clear(&field("AXSlider", false, Some(false), true))),
        "state_not_supported"
    );
}

#[test]
fn clear_readback_trusts_only_the_character_count_for_secure_fields() {
    assert_eq!(clear_readback(true, Some(true), None), None);
    assert_eq!(clear_readback(true, Some(true), Some(0)), Some(true));
    assert_eq!(clear_readback(true, Some(true), Some(4)), Some(false));
    assert_eq!(clear_readback(false, Some(true), Some(9)), Some(true));
    assert_eq!(clear_readback(false, Some(false), None), Some(false));
    assert_eq!(clear_readback(false, None, Some(0)), Some(true));
    assert_eq!(clear_readback(false, None, None), None);
}

#[test]
fn viewport_is_the_intersection_of_clips() {
    assert_eq!(
        viewport(&[[0.0, 0.0, 100.0, 100.0], [50.0, 20.0, 100.0, 50.0]]),
        Some([50.0, 20.0, 50.0, 50.0])
    );
    assert_eq!(
        viewport(&[[0.0, 0.0, 10.0, 10.0], [20.0, 20.0, 5.0, 5.0]]),
        None
    );
    assert_eq!(viewport(&[]), None);
}

#[test]
fn visibility_needs_half_the_element_and_rejects_clamped_slivers() {
    let view = Some([0.0, 0.0, 400.0, 300.0]);
    assert_eq!(
        visibility(Some([10.0, 10.0, 100.0, 20.0]), view),
        Visibility::InView
    );
    assert_eq!(
        visibility(Some([10.0, 290.0, 100.0, 20.0]), view),
        Visibility::InView,
        "exactly half shows"
    );
    assert_eq!(
        visibility(Some([10.0, 295.0, 100.0, 20.0]), view),
        Visibility::OutOfView
    );
    assert_eq!(
        visibility(Some([10.0, 299.0, 100.0, 1.0]), view),
        Visibility::OutOfView,
        "Chromium's 1 pt clamped sliver"
    );
    assert_eq!(
        visibility(Some([10.0, 500.0, 100.0, 20.0]), view),
        Visibility::OutOfView
    );
    assert_eq!(
        visibility(Some([0.0, -100.0, 400.0, 1000.0]), view),
        Visibility::InView,
        "an element taller than the viewport that fills it"
    );
    assert_eq!(visibility(None, view), Visibility::Unknown);
    assert_eq!(
        visibility(Some([10.0, 10.0, 100.0, 20.0]), None),
        Visibility::Unknown
    );
}
