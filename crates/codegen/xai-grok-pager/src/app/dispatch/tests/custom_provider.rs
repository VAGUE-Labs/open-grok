//! The custom-provider wizard: step machine, discovery, payload shape, Esc.
//!
//! These drive the real router, so they cover the wiring as well as the state
//! machine: `Action -> state + Effect`, and `TaskResult -> state`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

use crate::views::custom_provider_modal::{
    CustomProviderModalState, DiscoverResponse, DiscoveredModel, WizardStep, upsert_many_params,
};

const ADDRESS: &str = "https://api.example.com/v1";

fn press(code: KeyCode) -> Action {
    Action::CustomProviderWizardKey(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(ch: char) -> Action {
    Action::CustomProviderWizardKey(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
}

fn open_wizard(app: &mut AppView) {
    let effects = dispatch(Action::OpenCustomProviderWizard, app);
    assert!(effects.is_empty(), "opening is pure UI, got {effects:?}");
    assert!(app.custom_provider_modal.is_some(), "wizard must be open");
}

fn state(app: &AppView) -> &CustomProviderModalState {
    app.custom_provider_modal
        .as_ref()
        .expect("wizard should be open")
}

fn state_mut(app: &mut AppView) -> &mut CustomProviderModalState {
    app.custom_provider_modal
        .as_mut()
        .expect("wizard should be open")
}

fn type_text(app: &mut AppView, text: &str) {
    for ch in text.chars() {
        let _ = dispatch(press(KeyCode::Char(ch)), app);
    }
}

fn model(key: &str) -> DiscoveredModel {
    DiscoveredModel {
        key: key.to_owned(),
        id: format!("acme/{key}"),
        name: key.to_owned(),
        context_window: 128_000,
    }
}

/// The credential header the shell answers for a wire format: Messages uses
/// `x-api-key`, both OpenAI formats use a bearer token.
fn scheme_for(format: &str) -> &'static str {
    if format == "messages" {
        "x_api_key"
    } else {
        "bearer"
    }
}

/// The formats any discovery effect asked for.
fn asked_formats(effects: Vec<Effect>) -> Vec<String> {
    effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::DiscoverCustomProvider { format, .. } => Some(format),
            _ => None,
        })
        .collect()
}

/// Feed back the discovery effect the wizard just emitted, echoing the format
/// the wizard currently has selected the way the shell would.
fn answer_discovery(app: &mut AppView, models: Vec<DiscoveredModel>) {
    let generation = state(app).generation;
    let format = state(app).format().canonical.to_owned();
    let auth_scheme = scheme_for(&format).to_owned();
    dispatch_task_result(
        TaskResult::CustomProviderDiscovered {
            generation,
            error: None,
            response: Some(DiscoverResponse {
                base_url: ADDRESS.to_owned(),
                format,
                auth_scheme,
                models,
                notes: vec!["/v1 was added to the address".to_owned()],
            }),
        },
        app,
    );
}

/// Walk the happy path up to (and including) the checklist.
fn reach_checklist(app: &mut AppView, models: Vec<DiscoveredModel>) {
    open_wizard(app);
    type_text(app, ADDRESS);
    let _ = dispatch(press(KeyCode::Enter), app);
    let _ = dispatch(press(KeyCode::Enter), app);
    let _ = dispatch(press(KeyCode::Enter), app);
    answer_discovery(app, models);
}

#[test]
fn address_step_asks_the_shell_before_anything_is_written() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    type_text(&mut app, ADDRESS);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    assert_eq!(
        effects.len(),
        1,
        "Enter on the address asks once, got {effects:?}"
    );
    assert!(
        matches!(
            &effects[0],
            Effect::DiscoverCustomProvider { server_address, format, api_key, .. }
                if server_address == ADDRESS
                    && format == "chat_completions"
                    && api_key.is_none()
        ),
        "expected a bare discovery for the typed address, got {:?}",
        effects[0]
    );
    assert_eq!(state(&app).step, WizardStep::ApiKey);
}

#[test]
fn an_empty_address_is_refused_without_a_request() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    assert!(effects.is_empty(), "no address, no RPC, got {effects:?}");
    assert_eq!(state(&app).step, WizardStep::Address);
    assert!(state(&app).error.is_some(), "the user is told why");
}

#[test]
fn credentials_embedded_in_the_url_are_rejected_at_the_door() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    type_text(&mut app, "https://user:hunter2@api.example.com/v1");
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    assert!(
        effects.is_empty(),
        "a URL-embedded key must never be forwarded, got {effects:?}"
    );
    assert_eq!(state(&app).step, WizardStep::Address);
}

#[test]
fn an_ignored_repeat_enter_does_not_double_up_the_request() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    type_text(&mut app, ADDRESS);
    assert_eq!(dispatch(press(KeyCode::Enter), &mut app).len(), 1);
    // The key step is reached with the same (address, format, no key) tuple,
    // so the request already in flight is reused rather than re-sent.
    assert!(
        dispatch(press(KeyCode::Enter), &mut app).is_empty(),
        "the identical request is already on its way"
    );
    assert_eq!(state(&app).step, WizardStep::WireFormat);
}

#[test]
fn discovery_failure_keeps_the_user_on_the_address_step() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    type_text(&mut app, ADDRESS);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    let Effect::DiscoverCustomProvider { generation, .. } = effects[0] else {
        panic!("expected a discovery effect");
    };
    dispatch_task_result(
        TaskResult::CustomProviderDiscovered {
            generation,
            error: Some("connection refused".to_owned()),
            response: None,
        },
        &mut app,
    );
    assert_eq!(state(&app).step, WizardStep::Address);
    assert_eq!(state(&app).error.as_deref(), Some("connection refused"));
    assert!(app.custom_provider_modal.is_some(), "the wizard stays open");
}

#[test]
fn a_result_for_superseded_input_is_dropped() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    type_text(&mut app, ADDRESS);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    let Effect::DiscoverCustomProvider { generation, .. } = effects[0] else {
        panic!("expected a discovery effect");
    };
    // The user edits the address, which invalidates anything already running.
    type_text(&mut app, "/x");
    dispatch_task_result(
        TaskResult::CustomProviderDiscovered {
            generation,
            error: None,
            response: Some(DiscoverResponse {
                base_url: ADDRESS.to_owned(),
                format: "chat_completions".to_owned(),
                auth_scheme: "bearer".to_owned(),
                models: vec![model("stale")],
                notes: Vec::new(),
            }),
        },
        &mut app,
    );
    assert!(
        state(&app).models.is_empty(),
        "a stale answer must not populate the checklist"
    );
}

#[test]
fn the_filter_narrows_the_checklist_without_losing_selections() {
    let mut app = test_app_with_agent();
    reach_checklist(
        &mut app,
        vec![model("alpha"), model("beta"), model("gamma")],
    );
    assert_eq!(state(&app).step, WizardStep::Models);
    assert_eq!(state(&app).filtered_rows().len(), 3);
    assert_eq!(state(&app).selected_count(), 3);

    type_text(&mut app, "be");
    let rows = state(&app).filtered_rows();
    assert_eq!(rows.len(), 1, "only beta matches, got {rows:?}");

    // Uncheck the single visible row; hidden rows keep their state.
    let _ = dispatch(press(KeyCode::Char(' ')), &mut app);
    assert_eq!(state(&app).selected_count(), 2);
    assert_eq!(
        state(&app)
            .selected_rows()
            .iter()
            .map(|row| row.key.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "gamma"]
    );

    // Clearing the filter brings everything back, unchecked row still off.
    let _ = dispatch(press(KeyCode::Backspace), &mut app);
    let _ = dispatch(press(KeyCode::Backspace), &mut app);
    assert_eq!(state(&app).filtered_rows().len(), 3);
    assert_eq!(state(&app).selected_count(), 2);
}

#[test]
fn saving_nothing_asks_for_nothing() {
    let mut app = test_app_with_agent();
    reach_checklist(&mut app, vec![model("alpha"), model("beta")]);
    let effects = dispatch(ctrl('x'), &mut app);
    assert!(effects.is_empty(), "clearing is local, got {effects:?}");
    assert_eq!(state(&app).selected_count(), 0);

    let effects = dispatch(press(KeyCode::Enter), &mut app);
    assert!(
        effects.is_empty(),
        "an empty selection must not touch config, got {effects:?}"
    );
    assert!(state(&app).error.is_some(), "the user is told why");
    assert!(!state(&app).saving);
}

#[test]
fn ctrl_a_then_enter_writes_every_discovered_model() {
    let mut app = test_app_with_agent();
    reach_checklist(&mut app, vec![model("alpha"), model("beta")]);
    let _ = dispatch(ctrl('x'), &mut app);
    assert_eq!(state(&app).selected_count(), 0);
    let _ = dispatch(ctrl('a'), &mut app);
    assert_eq!(state(&app).selected_count(), 2);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    let Effect::UpsertCustomModelsMany { rows, .. } = &effects[0] else {
        panic!("expected a batch save, got {effects:?}");
    };
    assert_eq!(rows.len(), 2);
}

#[test]
fn each_wire_format_records_its_own_backend_and_header() {
    let cases = [
        (0usize, "chat_completions", "bearer"),
        (1, "responses", "bearer"),
        (2, "messages", "x_api_key"),
    ];
    for (index, backend, header) in cases {
        let mut app = test_app_with_agent();
        let mut asked = Vec::new();
        open_wizard(&mut app);
        type_text(&mut app, ADDRESS);
        // The address step prefetches with the default format, so a repeated
        // Enter on the key step must not ask a second time.
        asked.extend(asked_formats(dispatch(press(KeyCode::Enter), &mut app)));
        let effects = dispatch(press(KeyCode::Enter), &mut app);
        assert!(
            effects.is_empty(),
            "the key step reuses the in-flight request"
        );
        for _ in 0..index {
            let _ = dispatch(press(KeyCode::Down), &mut app);
        }
        assert_eq!(state(&app).format().canonical, backend);
        asked.extend(asked_formats(dispatch(press(KeyCode::Enter), &mut app)));
        assert_eq!(
            state(&app).step,
            WizardStep::Models,
            "confirming a format always advances to the checklist"
        );
        assert!(
            asked.iter().any(|format| format == backend),
            "the shell must be asked with the chosen format; asked {asked:?}"
        );
        answer_discovery(&mut app, vec![model("alpha")]);
        let effects = dispatch(press(KeyCode::Enter), &mut app);
        let Effect::UpsertCustomModelsMany {
            api_backend,
            auth_scheme,
            base_url,
            rows,
            api_key,
            ..
        } = &effects[0]
        else {
            panic!("expected a batch save, got {effects:?}");
        };
        assert_eq!(api_backend, backend);
        assert_eq!(auth_scheme, header);
        assert!(api_key.is_none(), "no key was typed");
        let params = upsert_many_params(api_backend, auth_scheme, base_url, None, rows);
        let record = &params["models"][0];
        assert_eq!(record["provider"], "custom");
        assert_eq!(record["api_backend"], backend);
        assert_eq!(record["auth_scheme"], header);
        assert_eq!(
            record["base_url"],
            base_url.as_str(),
            "a custom row must carry the endpoint it came from"
        );
        assert_eq!(record["key"], "alpha");
        assert_eq!(record["context_window"], 128_000);
        assert!(
            record.get("api_key").is_none(),
            "an omitted key must not appear as an empty string: {params}"
        );
    }
}

#[test]
fn a_typed_key_is_carried_once_and_recorded_per_model() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    type_text(&mut app, ADDRESS);
    let _ = dispatch(press(KeyCode::Enter), &mut app);
    type_text(&mut app, "sk-secret-value");
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    assert!(
        matches!(
            &effects[0],
            Effect::DiscoverCustomProvider {
                api_key: Some(_),
                ..
            }
        ),
        "the typed key rides along with discovery, got {effects:?}"
    );
    assert!(
        !format!("{effects:?}").contains("sk-secret-value"),
        "debug output must never contain the key"
    );
    assert!(
        !format!("{:?}", state(&app)).contains("sk-secret-value"),
        "wizard state must never print the key"
    );

    answer_discovery(&mut app, vec![model("alpha"), model("beta")]);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    let Effect::UpsertCustomModelsMany { api_key, rows, .. } = &effects[0] else {
        panic!("expected a batch save, got {effects:?}");
    };
    let exposed = api_key.as_ref().expect("key carried").expose();
    let base_url = state(&app)
        .resolved_base_url()
        .expect("discovery resolved one");
    let params = upsert_many_params("chat_completions", "bearer", &base_url, Some(exposed), rows);
    let models = params["models"].as_array().expect("models array");
    assert_eq!(models.len(), 2);
    for record in models {
        assert_eq!(record["api_key"], "sk-secret-value");
        assert_eq!(record["base_url"], ADDRESS);
    }
}

#[test]
fn a_saved_batch_lands_on_the_summary_step() {
    let mut app = test_app_with_agent();
    reach_checklist(&mut app, vec![model("alpha"), model("beta")]);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    let Effect::UpsertCustomModelsMany { generation, .. } = effects[0] else {
        panic!("expected a batch save, got {effects:?}");
    };
    assert!(state(&app).saving);
    dispatch_task_result(
        TaskResult::CustomModelsUpdated {
            generation,
            stale: false,
            warning: None,
            error: None,
            models: None,
            custom_models: Vec::new(),
        },
        &mut app,
    );
    assert_eq!(state(&app).step, WizardStep::Summary);
    assert_eq!(state(&app).saved_count, 2);
    // Enter on the summary closes the wizard.
    let _ = dispatch(press(KeyCode::Enter), &mut app);
    assert!(app.custom_provider_modal.is_none());
}

#[test]
fn a_failed_save_returns_to_the_checklist() {
    let mut app = test_app_with_agent();
    reach_checklist(&mut app, vec![model("alpha")]);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    let Effect::UpsertCustomModelsMany { generation, .. } = effects[0] else {
        panic!("expected a batch save, got {effects:?}");
    };
    dispatch_task_result(
        TaskResult::CustomModelsUpdated {
            generation,
            stale: false,
            warning: None,
            error: Some("config is read-only".to_owned()),
            models: None,
            custom_models: Vec::new(),
        },
        &mut app,
    );
    assert_eq!(state(&app).step, WizardStep::Models);
    assert!(!state(&app).saving);
    assert_eq!(state(&app).error.as_deref(), Some("config is read-only"));
}

#[test]
fn escape_cancels_from_every_step() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    // 1. Address.
    assert!(
        dispatch(press(KeyCode::Esc), &mut app).is_empty(),
        "closing emits nothing"
    );
    assert!(
        app.custom_provider_modal.is_none(),
        "Esc closes from Address"
    );

    // 2. API key.
    open_wizard(&mut app);
    type_text(&mut app, ADDRESS);
    let _ = dispatch(press(KeyCode::Enter), &mut app);
    assert_eq!(state(&app).step, WizardStep::ApiKey);
    let _ = dispatch(press(KeyCode::Esc), &mut app);
    assert!(
        app.custom_provider_modal.is_none(),
        "Esc closes from ApiKey"
    );

    // 3. Wire format.
    open_wizard(&mut app);
    type_text(&mut app, ADDRESS);
    let _ = dispatch(press(KeyCode::Enter), &mut app);
    let _ = dispatch(press(KeyCode::Enter), &mut app);
    assert_eq!(state(&app).step, WizardStep::WireFormat);
    let _ = dispatch(press(KeyCode::Esc), &mut app);
    assert!(
        app.custom_provider_modal.is_none(),
        "Esc closes from Format"
    );

    // 4. Models, with the filter active: Esc still cancels, not just clears.
    reach_checklist(&mut app, vec![model("alpha"), model("beta")]);
    type_text(&mut app, "al");
    assert!(state(&app).filter_focused);
    let _ = dispatch(press(KeyCode::Esc), &mut app);
    assert!(
        app.custom_provider_modal.is_none(),
        "Esc must cancel even while the filter is focused"
    );

    // 5. Summary.
    reach_checklist(&mut app, vec![model("alpha")]);
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    let Effect::UpsertCustomModelsMany { generation, .. } = effects[0] else {
        panic!("expected a batch save, got {effects:?}");
    };
    dispatch_task_result(
        TaskResult::CustomModelsUpdated {
            generation,
            stale: false,
            warning: None,
            error: None,
            models: None,
            custom_models: Vec::new(),
        },
        &mut app,
    );
    assert_eq!(state(&app).step, WizardStep::Summary);
    let _ = dispatch(press(KeyCode::Esc), &mut app);
    assert!(
        app.custom_provider_modal.is_none(),
        "Esc closes from Summary"
    );
}

#[test]
fn shift_tab_walks_back_without_closing() {
    let mut app = test_app_with_agent();
    reach_checklist(&mut app, vec![model("alpha")]);
    let _ = dispatch(
        Action::CustomProviderWizardKey(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        &mut app,
    );
    assert_eq!(state(&app).step, WizardStep::WireFormat);
    assert!(app.custom_provider_modal.is_some());
}

#[test]
fn the_settings_trigger_row_opens_the_wizard() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::SetCustomProviderWizard(true), &mut app);
    assert!(effects.is_empty());
    assert!(app.custom_provider_modal.is_some());
    // The row is a trigger: it stores nothing, so `false` is a no-op.
    let _ = dispatch(Action::SetCustomProviderWizard(false), &mut app);
    assert!(
        app.custom_provider_modal.is_some(),
        "a reset must not close it"
    );
}

#[test]
fn a_batch_with_no_resolved_endpoint_is_never_written() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    // What a server answer with an empty `base_url` would leave behind: rows to
    // write but no route for them.
    {
        let wizard = state_mut(&mut app);
        wizard.models = vec![model("alpha")];
        wizard.selected = vec![true];
        wizard.base_url = None;
        wizard.step = WizardStep::Models;
    }
    let effects = dispatch(press(KeyCode::Enter), &mut app);
    assert!(
        effects.is_empty(),
        "a `custom` row with no endpoint must not be written, got {effects:?}"
    );
    assert!(
        state(&app).error.is_some(),
        "the user is told why nothing was saved"
    );
    assert!(!state(&app).saving, "the wizard is not left waiting");
}

#[test]
fn typing_the_slash_command_opens_the_wizard_from_chat() {
    // The composer path resolves the command, runs it, and hands the action
    // back to the router; all three hops must land for a typed `/provider`.
    for text in [
        "/provider",
        "/provider add",
        "/provider   ADD",
        "/providers",
        "/custom-provider",
    ] {
        let mut app = test_app_with_agent();
        let _ = dispatch(Action::SendPrompt(text.to_owned()), &mut app);
        assert!(
            app.custom_provider_modal.is_some(),
            "`{text}` must open the wizard from the composer"
        );
        // The wizard owns the keyboard the moment it opens.
        let effects = dispatch(press(KeyCode::Enter), &mut app);
        assert!(
            !format!("{effects:?}").contains("SendPrompt"),
            "the wizard must swallow keys, not send a prompt: {effects:?}"
        );
    }
    // A subcommand that does not exist is refused in chat, and opens nothing.
    let mut app = test_app_with_agent();
    let _ = dispatch(Action::SendPrompt("/provider remove".to_owned()), &mut app);
    assert!(
        app.custom_provider_modal.is_none(),
        "an unknown subcommand must not open the wizard"
    );
}

#[test]
fn the_typed_key_never_appears_in_effects_or_toasts() {
    let mut app = test_app_with_agent();
    open_wizard(&mut app);
    type_text(&mut app, ADDRESS);
    let _ = dispatch(press(KeyCode::Enter), &mut app);
    type_text(&mut app, "sk-abcdefghijklmnopqrstuvwxyz");
    let discovery = dispatch(press(KeyCode::Enter), &mut app);
    let rendered = format!("{discovery:?}");
    assert!(
        !rendered.contains("sk-abcdefghijklmnopqrstuvwxyz"),
        "a typed credential must never be rendered by an effect: {rendered}"
    );
    let toast = app
        .agents
        .get(&AgentId(0))
        .and_then(|agent| agent.toast.as_ref())
        .map(|(text, _)| text.to_string());
    assert!(
        !toast.is_some_and(|text| text.contains("sk-abcdefghijklmnopqrstuvwxyz")),
        "a typed credential must never reach a toast"
    );
    // The wizard's own Debug output is redacted, so a stray log line or panic
    // message cannot leak the typed key.
    let rendered_state = format!("{:?}", state(&app));
    assert!(
        !rendered_state.contains("sk-abcdefghijklmnopqrstuvwxyz"),
        "the wizard state must redact its credential, got {rendered_state}"
    );
    assert_eq!(
        state(&app).credential().expect("key was typed").expose(),
        "sk-abcdefghijklmnopqrstuvwxyz",
        "the key is still carried to the save, just never displayed"
    );
}

#[test]
fn checklist_arrows_move_the_checkmark_and_space_toggles_that_row() {
    let mut app = test_app_with_agent();
    // Discovery checks everything the server offered, so a toggle is an uncheck.
    reach_checklist(
        &mut app,
        vec![model("alpha"), model("beta"), model("gamma")],
    );
    assert_eq!(
        state(&app).focus,
        0,
        "the checklist starts on the first row"
    );

    let _ = dispatch(press(KeyCode::Down), &mut app);
    assert_eq!(state(&app).focus, 1, "Down must move the focus down");

    let _ = dispatch(press(KeyCode::Char(' ')), &mut app);
    let checked: Vec<bool> = state(&app).selected.iter().copied().collect();
    assert_eq!(
        checked,
        vec![true, false, true],
        "Space toggles the focused row, not the first one"
    );

    // Focus clamps at the last row and Up walks back; it never leaves the list.
    let _ = dispatch(press(KeyCode::Down), &mut app);
    let _ = dispatch(press(KeyCode::Down), &mut app);
    assert_eq!(state(&app).focus, 2, "focus clamps at the last model");
    let _ = dispatch(press(KeyCode::Up), &mut app);
    assert_eq!(state(&app).focus, 1);

    // Typing filters instead of moving focus, and resets it to the top.
    type_text(&mut app, "gam");
    assert_eq!(state(&app).focus, 0);
    assert_eq!(state(&app).filtered_rows(), vec![2]);
    assert_eq!(
        state(&app).selected_count(),
        2,
        "filtering must not change what is checked"
    );
}
