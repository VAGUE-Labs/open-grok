//! Handlers for the custom-provider wizard (`/provider`, Settings -> Models).
//!
//! The wizard is pure UI state in
//! [`crate::views::custom_provider_modal`]. This module is the thin
//! translation layer: keystroke in, [`Effect`]s out; [`TaskResult`] in, wizard
//! state out. It performs no I/O itself - the address is untrusted third-party
//! infrastructure, and only the shell may talk to it.

use crossterm::event::KeyEvent;

use crate::app::actions::Effect;
use crate::app::app_view::AppView;
use crate::views::custom_provider_modal::{CustomProviderModalState, WizardIntent};

pub(in crate::app::dispatch) type WizardEffects = Vec<Effect>;

/// Open the wizard. Always a fresh one: a half-typed attempt from an earlier
/// open must not leak into the next.
pub(in crate::app::dispatch) fn open_custom_provider_wizard(app: &mut AppView) -> WizardEffects {
    app.custom_provider_modal = Some(CustomProviderModalState::new());
    vec![]
}

/// Drop the wizard, and with it the typed credential.
pub(in crate::app::dispatch) fn close_custom_provider_wizard(app: &mut AppView) -> WizardEffects {
    app.custom_provider_modal = None;
    vec![]
}

/// Settings trigger row. The row stores nothing, so `false` is a no-op.
pub(in crate::app::dispatch) fn set_custom_provider_wizard(
    app: &mut AppView,
    open: bool,
) -> WizardEffects {
    if open {
        return open_custom_provider_wizard(app);
    }
    vec![]
}

/// Route one keystroke to the open wizard.
pub(in crate::app::dispatch) fn custom_provider_key(
    app: &mut AppView,
    key: KeyEvent,
) -> WizardEffects {
    let Some(state) = app.custom_provider_modal.as_mut() else {
        return vec![];
    };
    // Compute the intent before touching `app` again: the wizard is part of it.
    let intent = state.handle_key(&key);
    handoff(app, intent)
}

/// Route a bracketed paste (long keys are pasted, not typed).
pub(in crate::app::dispatch) fn custom_provider_paste(
    app: &mut AppView,
    text: &str,
) -> WizardEffects {
    let Some(state) = app.custom_provider_modal.as_mut() else {
        return vec![];
    };
    let intent = state.handle_paste(text);
    handoff(app, intent)
}

fn handoff(app: &mut AppView, intent: WizardIntent) -> WizardEffects {
    match intent {
        WizardIntent::Unchanged | WizardIntent::Changed => vec![],
        WizardIntent::Cancel => close_custom_provider_wizard(app),
        WizardIntent::Discover => custom_provider_discover(app),
        WizardIntent::Save => custom_provider_save(app),
    }
}

/// Ask the shell what the typed address serves. Nothing is written to config
/// here - discovery is read-only by construction.
pub(in crate::app::dispatch) fn custom_provider_discover(app: &mut AppView) -> WizardEffects {
    let Some(state) = app.custom_provider_modal.as_mut() else {
        return vec![];
    };
    // The credential is read once, here, for this one request. It is never
    // logged, never cached in the pager, and never sent anywhere but the
    // address the user typed.
    let api_key = state.credential();
    let Some((generation, server_address, format)) = state.begin_discovery() else {
        return vec![];
    };
    vec![Effect::DiscoverCustomProvider {
        generation,
        server_address,
        format,
        api_key,
    }]
}

/// Write the checked models on the shared custom-models mutation lane, so the
/// usual staleness guard, config write, and settings refresh all apply.
pub(in crate::app::dispatch) fn custom_provider_save(app: &mut AppView) -> WizardEffects {
    let Some(state) = app.custom_provider_modal.as_mut() else {
        return vec![];
    };
    // Every row needs the endpoint the models came from. If discovery never
    // resolved one, say so instead of writing a `custom` row with no route.
    let Some(base_url) = state.resolved_base_url() else {
        state.refuse_missing_base_url();
        return vec![];
    };
    // Nothing checked means no write, and the shared generation counter stays
    // untouched: an empty save must not mark a real mutation from Settings
    // stale. The step machine already refuses Enter, so this is the backstop.
    if state.selected_count() == 0 {
        return vec![];
    }
    let api_key = state.credential();
    let api_backend = state.format().canonical.to_owned();
    let auth_scheme = state.effective_auth_scheme();
    let generation = super::settings::setters::begin_custom_models_mutation();
    let rows = state.begin_save(generation);
    if rows.is_empty() {
        // Zero selections: nothing to write, so the shell is not asked at all.
        return vec![];
    }
    vec![Effect::UpsertCustomModelsMany {
        generation,
        api_backend,
        auth_scheme,
        base_url,
        api_key,
        rows,
    }]
}

/// Discovery finished: land the user on the checklist, or keep them on the
/// address step with the reason shown inline.
pub(in crate::app::dispatch) fn handle_custom_provider_discovered(
    app: &mut AppView,
    generation: u64,
    error: &Option<String>,
    response: &Option<crate::views::custom_provider_modal::DiscoverResponse>,
) -> WizardEffects {
    let Some(state) = app.custom_provider_modal.as_mut() else {
        return vec![];
    };
    if generation != state.generation {
        // A result for superseded input must never overwrite the checklist.
        return vec![];
    }
    match (error, response) {
        (None, Some(response)) => state.apply_discovered(response.clone()),
        (Some(message), _) => state.apply_discovery_error(message.clone()),
        (None, None) => {
            state.apply_discovery_error("The server returned no models and no reason.".to_owned())
        }
    }
    vec![]
}

/// A save finished. Called from the shared `CustomModelsUpdated` lane, which
/// the wizard deliberately rides so config writes and refreshes behave exactly
/// like any other custom-model mutation.
pub(in crate::app::dispatch) fn note_custom_provider_save(
    app: &mut AppView,
    generation: u64,
    error: &Option<String>,
    warning: &Option<String>,
) {
    if let Some(state) = app.custom_provider_modal.as_mut() {
        state.finish_save(generation, error.as_deref(), warning.as_deref());
    }
}
