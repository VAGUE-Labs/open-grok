//! Five-step wizard that points Open Grok at a user-supplied model server.
//!
//! Steps: server address -> optional API key -> wire format -> model checklist
//! -> saved summary. The modal owns *only* UI state: every RPC is handed back to
//! dispatch as an intent, and dispatch answers with a [`TaskResult`]. Nothing in
//! this module talks to the network, the shell, or the terminal driver.
//!
//! Credential rule: the typed key is edited through [`SecretInput`] (whose
//! `Debug` is redacted) and is rendered as bullets only. It is never echoed into
//! a label, a log line, or the `Debug` rendering of this state.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use unicode_width::UnicodeWidthChar as _;

use crate::input::line_editor::LineEditor;
use crate::settings::SecretInput;
use crate::theme::Theme;
use crate::views::modal_window::{
    ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut, render_modal_window,
};

/// `[model.<key>].provider` written for every model saved by this wizard.
pub const CUSTOM_PROVIDER_ID: &str = "custom";

/// Sentinel shortcut id for non-clickable footer hints.
const SHORTCUT_ID_HINT: usize = usize::MAX;
/// Upper bound on a pasted credential; real keys are far shorter.
const MAX_KEY_CHARS: usize = 4096;

/// The five wizard steps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    /// Free-text server address (the only mandatory field).
    Address,
    /// Optional bearer / `x-api-key` credential for that address.
    ApiKey,
    /// Which wire protocol the address speaks.
    WireFormat,
    /// Checklist of models discovered at that address.
    Models,
    /// What was written, and anything the shell wants the user to know.
    Summary,
}

impl WizardStep {
    pub const ALL: [Self; 5] = [
        Self::Address,
        Self::ApiKey,
        Self::WireFormat,
        Self::Models,
        Self::Summary,
    ];

    pub const fn index(self) -> usize {
        match self {
            Self::Address => 0,
            Self::ApiKey => 1,
            Self::WireFormat => 2,
            Self::Models => 3,
            Self::Summary => 4,
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Address => "Server address",
            Self::ApiKey => "API key (optional)",
            Self::WireFormat => "Wire format",
            Self::Models => "Models",
            Self::Summary => "Saved",
        }
    }

    /// Previous step, or `None` when already on the first step.
    pub fn prev(self) -> Option<Self> {
        Self::ALL
            .get(self.index().saturating_sub(1))
            .copied()
            .filter(|_| self.index() > 0)
    }

    /// Next step, clamped to the last one.
    pub fn next(self) -> Self {
        Self::ALL
            .get(self.index() + 1)
            .copied()
            .unwrap_or(Self::Summary)
    }
}

/// One wire-protocol choice. Mirrors the shell's `CustomWireFormat`; the format,
/// never the host, decides the credential header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireFormatChoice {
    /// Canonical `api_backend` value stored on the model entry.
    pub canonical: &'static str,
    /// Radio label.
    pub label: &'static str,
    /// One-line explanation of what choosing this implies.
    pub hint: &'static str,
    /// Credential header written with this format.
    pub auth_scheme: &'static str,
}

/// The three supported protocols, in display order.
pub const WIRE_FORMATS: [WireFormatChoice; 3] = [
    WireFormatChoice {
        canonical: "chat_completions",
        label: "OpenAI Chat Completions",
        hint: "GET /v1/models - sends the key as `Authorization: Bearer`.",
        auth_scheme: "bearer",
    },
    WireFormatChoice {
        canonical: "responses",
        label: "OpenAI Responses",
        hint: "GET /v1/models - sends the key as `Authorization: Bearer`.",
        auth_scheme: "bearer",
    },
    WireFormatChoice {
        canonical: "messages",
        label: "Anthropic Messages",
        hint: "GET /v1/models - sends the key as `x-api-key`.",
        auth_scheme: "x_api_key",
    },
];

/// A model advertised by the user's server.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct DiscoveredModel {
    pub key: String,
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_window: u64,
}

impl DiscoveredModel {
    /// Label for the checklist row: friendly name when the server gave one.
    pub fn label(&self) -> &str {
        let name = self.name.trim();
        if name.is_empty() { &self.id } else { name }
    }
}

/// Successful `open-grok/custom-providers/discover` payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct DiscoverResponse {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub auth_scheme: String,
    #[serde(default)]
    pub models: Vec<DiscoveredModel>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// One confirmed checklist row, ready to become a `[model.<key>]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedModelRow {
    pub key: String,
    pub model: String,
    pub name: String,
    pub context_window: u64,
}

/// Params for `open-grok/custom-models/upsert-many`.
///
/// `base_url` is the address the shell normalized during discovery. It goes on
/// every row: a `custom` row without an endpoint would have nowhere to send
/// requests, and the host name must never be inferred later.
///
/// `api_key` is written onto every row because the shell's record type carries it
/// per entry; an empty key is **omitted** rather than sent as `""` so the shell
/// keeps any `env_key` already configured for that catalog key.
pub fn upsert_many_params(
    api_backend: &str,
    auth_scheme: &str,
    base_url: &str,
    api_key: Option<&str>,
    rows: &[SelectedModelRow],
) -> serde_json::Value {
    let models = rows
        .iter()
        .map(|row| {
            let mut record = serde_json::json!({
                "key": row.key,
                "model": row.model,
                "provider": CUSTOM_PROVIDER_ID,
                "api_backend": api_backend,
                "auth_scheme": auth_scheme,
                "base_url": base_url,
            });
            if !row.name.trim().is_empty() {
                record["name"] = serde_json::Value::String(row.name.clone());
            }
            if row.context_window > 0 {
                record["context_window"] = serde_json::json!(row.context_window);
            }
            if let Some(key) = api_key.filter(|key| !key.is_empty()) {
                record["api_key"] = serde_json::Value::String(key.to_owned());
            }
            record
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "models": models })
}

/// What the wizard is asking dispatch to do after an input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardIntent {
    /// State changed in place; repaint.
    Changed,
    /// Nothing happened.
    Unchanged,
    /// Dismiss the wizard without writing anything.
    Cancel,
    /// Ask the shell for the model list at the current address + format.
    Discover,
    /// Write the checked models as `[model.<key>]` entries.
    Save,
}

/// Everything the wizard remembers while it is open.
///
/// `Debug` is hand-written so the credential can never reach a log through
/// `{state:?}`; it reports only the length of what was typed.
pub struct CustomProviderModalState {
    /// Current wizard step.
    pub step: WizardStep,
    /// Step 1: the raw address exactly as the user typed it.
    pub(crate) address: LineEditor,
    /// Step 3 radio index into [`WIRE_FORMATS`].
    pub format_index: usize,
    /// Models from the last successful discovery, in server order.
    pub models: Vec<DiscoveredModel>,
    /// Per-model selection, parallel to [`Self::models`].
    pub selected: Vec<bool>,
    /// Focused checklist row, indexed into the *filtered* rows.
    pub focus: usize,
    /// Top visible checklist row.
    pub scroll_offset: usize,
    /// Filter text for the checklist.
    pub(crate) filter: LineEditor,
    /// Whether keystrokes edit the filter (turns on when the user types).
    pub filter_focused: bool,
    /// Base URL the shell normalized the address to.
    pub base_url: Option<String>,
    /// Credential header the shell will use for this format.
    pub auth_scheme: Option<String>,
    /// Non-fatal notes from the shell (e.g. "`/v1` was added").
    pub notes: Vec<String>,
    /// Warning from the last save.
    pub warning: Option<String>,
    /// Inline error shown under the failing step.
    pub error: Option<String>,
    /// A discovery RPC is in flight.
    pub discovering: bool,
    /// The `(address, format, has_key)` tuple the in-flight request was built
    /// from, so an identical repeated Enter is not sent twice.
    inflight: Option<(String, String, bool)>,
    /// A save RPC is in flight.
    pub saving: bool,
    /// Models written by the last successful save.
    pub saved_count: usize,
    /// RPC generation; stale results are dropped by dispatch.
    pub generation: u64,
    /// Models the last save was asked to write (used for the summary count).
    pending_save: usize,
    /// Shared custom-models generation of the in-flight save, if any. Results
    /// for any other generation are ignored.
    pub save_generation: Option<u64>,
    /// Credential text. Private on purpose: read it only through
    /// [`Self::credential`], and never print it.
    key: SecretInput,
    /// Chrome state (close button, footer hit areas).
    pub window: ModalWindowState,
}

impl Default for CustomProviderModalState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CustomProviderModalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomProviderModalState")
            .field("step", &self.step)
            .field("address", &self.address.text())
            .field("format", &self.format().canonical)
            // Never the credential itself - only how much was typed.
            .field("api_key", &format!("<redacted {} chars>", self.key.len()))
            .field("models", &self.models.len())
            .field("selected", &self.selected_count())
            .field("discovering", &self.discovering)
            .field("saving", &self.saving)
            .field("saved_count", &self.saved_count)
            .field("generation", &self.generation)
            .finish()
    }
}

impl CustomProviderModalState {
    /// A fresh wizard on step 1 with nothing filled in.
    pub fn new() -> Self {
        Self {
            step: WizardStep::Address,
            address: LineEditor::default(),
            format_index: 0,
            models: Vec::new(),
            selected: Vec::new(),
            focus: 0,
            scroll_offset: 0,
            filter: LineEditor::default(),
            filter_focused: false,
            base_url: None,
            auth_scheme: None,
            notes: Vec::new(),
            warning: None,
            error: None,
            discovering: false,
            inflight: None,
            saving: false,
            saved_count: 0,
            generation: 0,
            pending_save: 0,
            save_generation: None,
            key: SecretInput::default(),
            window: ModalWindowState::new(),
        }
    }

    /// The selected wire format.
    pub fn format(&self) -> &'static WireFormatChoice {
        WIRE_FORMATS
            .get(self.format_index)
            .unwrap_or(&WIRE_FORMATS[0])
    }

    /// The address to send to the shell: exactly what the user typed, trimmed of
    /// surrounding blanks only. Normalization is the shell's job.
    pub fn server_address(&self) -> String {
        self.address.text().trim().to_owned()
    }

    /// The typed credential, or `None` when the user skipped the step.
    pub fn credential(&self) -> Option<SecretInput> {
        if self.key.is_empty() {
            None
        } else {
            Some(SecretInput::new(self.key.expose().to_owned()))
        }
    }

    /// Number of checked models.
    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|selected| **selected).count()
    }

    /// Checklist rows that match the current filter, as indices into
    /// [`Self::models`]. Matching is a case-insensitive substring test on the
    /// catalog key, model id, and display name.
    pub fn filtered_rows(&self) -> Vec<usize> {
        let needle = self.filter.text().trim().to_lowercase();
        self.models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                needle.is_empty()
                    || model.key.to_lowercase().contains(&needle)
                    || model.id.to_lowercase().contains(&needle)
                    || model.label().to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Checked models, in server order, ready to write.
    pub fn selected_rows(&self) -> Vec<SelectedModelRow> {
        self.models
            .iter()
            .zip(&self.selected)
            .filter(|(_, selected)| **selected)
            .map(|(model, _)| SelectedModelRow {
                key: model.key.clone(),
                model: model.id.clone(),
                name: model.label().to_owned(),
                context_window: model.context_window,
            })
            .collect()
    }

    /// Store a successful discovery and land on the checklist.
    pub fn apply_discovered(&mut self, response: DiscoverResponse) {
        self.discovering = false;
        self.inflight = None;
        self.error = None;
        self.base_url = Some(response.base_url.clone()).filter(|url| !url.is_empty());
        self.auth_scheme = Some(response.auth_scheme.clone()).filter(|scheme| !scheme.is_empty());
        self.notes = response.notes.clone();
        self.models = response.models.clone();
        // Default to saving everything the server offered; the user can uncheck.
        self.selected = vec![true; self.models.len()];
        self.focus = 0;
        self.scroll_offset = 0;
        // A late result must not yank the user off a text step they are still
        // editing; the checklist is where it shows up.
        if matches!(self.step, WizardStep::WireFormat | WizardStep::Models) {
            self.step = WizardStep::Models;
        }
    }

    /// A failed discovery keeps the user on the address step with the reason
    /// shown inline - the typed address and key stay untouched for a retry.
    pub fn apply_discovery_error(&mut self, message: String) {
        self.discovering = false;
        self.inflight = None;
        self.error = Some(message);
        self.step = WizardStep::Address;
    }

    /// A successful save lands on the summary with the written count.
    pub fn apply_saved(&mut self, warning: Option<String>) {
        self.saving = false;
        self.saved_count = self.pending_save;
        self.pending_save = 0;
        self.warning = warning.filter(|warning| !warning.trim().is_empty());
        self.error = None;
        self.step = WizardStep::Summary;
    }

    /// A failed save returns to the checklist so the user can retry.
    pub fn apply_save_error(&mut self, message: String) {
        self.saving = false;
        self.pending_save = 0;
        self.error = Some(message);
        self.step = WizardStep::Models;
    }

    // -- RPC bookkeeping (called by dispatch) --------------------------------

    /// Invalidate anything in flight. Every edit to the address or the key
    /// calls this, so a result for superseded input can never land.
    pub fn invalidate_inflight(&mut self) {
        self.generation += 1;
        self.discovering = false;
        self.inflight = None;
    }

    /// Mark a discovery as in flight. `None` means "do not send": no address,
    /// or that exact request is already on its way. A different address, wire
    /// format, or key always supersedes whatever is pending.
    pub fn begin_discovery(&mut self) -> Option<(u64, String, String)> {
        let address = self.server_address();
        if address.is_empty() {
            return None;
        }
        let format = self.format().canonical.to_owned();
        // Only whether a key exists is tracked here - never its value.
        let tuple = (address.clone(), format.clone(), !self.key.is_empty());
        if self.inflight.as_ref() == Some(&tuple) {
            return None;
        }
        self.generation += 1;
        self.discovering = true;
        self.inflight = Some(tuple);
        self.error = None;
        Some((self.generation, address, format))
    }

    /// Mark a save as in flight and return the rows to write. An empty vector
    /// means nothing was checked, and dispatch must emit no effect at all.
    pub fn begin_save(&mut self, save_generation: u64) -> Vec<SelectedModelRow> {
        let rows = self.selected_rows();
        if rows.is_empty() {
            self.error = Some("Select at least one model to save".to_owned());
            return Vec::new();
        }
        self.pending_save = rows.len();
        self.saving = true;
        self.save_generation = Some(save_generation);
        self.error = None;
        rows
    }

    /// Resolve an in-flight save. Results for another generation are dropped.
    pub fn finish_save(&mut self, generation: u64, error: Option<&str>, warning: Option<&str>) {
        if !self.saving || self.save_generation != Some(generation) {
            return;
        }
        match error {
            Some(message) => self.apply_save_error(message.to_owned()),
            None => self.apply_saved(warning.map(str::to_owned)),
        }
    }

    /// The credential header to store: the shell's answer when it gave one,
    /// otherwise the default implied by the chosen wire format.
    pub fn effective_auth_scheme(&self) -> String {
        self.auth_scheme
            .clone()
            .filter(|scheme| !scheme.trim().is_empty())
            .unwrap_or_else(|| self.format().auth_scheme.to_owned())
    }

    /// The endpoint discovery resolved, or `None` when it never resolved one.
    pub fn resolved_base_url(&self) -> Option<String> {
        self.base_url.clone().filter(|url| !url.trim().is_empty())
    }

    /// Keep the user on the checklist when there is no endpoint to write.
    pub fn refuse_missing_base_url(&mut self) {
        self.error = Some(
            "No endpoint was resolved for these models. Go back and confirm the address again."
                .to_owned(),
        );
    }

    // -- input ---------------------------------------------------------------

    /// Route one input event. Esc cancels from every step; Back (Shift+Tab, or
    /// Left on a step with no text field) returns to the previous step.
    pub fn handle_key(&mut self, key: &KeyEvent) -> WizardIntent {
        if key.kind == KeyEventKind::Release {
            return WizardIntent::Unchanged;
        }
        if key.code == KeyCode::Esc {
            return WizardIntent::Cancel;
        }
        // Back: Shift+Tab works everywhere, including on text steps where Tab is
        // free; Left backs up on the steps that own no text field.
        let arrow_back = matches!(key.code, KeyCode::Left | KeyCode::Char('h'))
            && matches!(self.step, WizardStep::WireFormat | WizardStep::Summary);
        let back = key.code == KeyCode::BackTab || arrow_back;
        if back && self.step.index() > 0 {
            self.step = self.step.prev().unwrap_or(WizardStep::Address);
            self.error = None;
            return WizardIntent::Changed;
        }
        match self.step {
            WizardStep::Address => self.on_address_key(key),
            WizardStep::ApiKey => self.on_api_key_key(key),
            WizardStep::WireFormat => self.on_format_key(key),
            WizardStep::Models => self.on_models_key(key),
            WizardStep::Summary => self.on_summary_key(key),
        }
    }

    /// Accept a bracketed paste as credential text on the key step.
    pub fn handle_paste(&mut self, text: &str) -> WizardIntent {
        if self.step != WizardStep::ApiKey {
            return WizardIntent::Unchanged;
        }
        self.push_credential_chars(text.chars().filter(|ch| !ch.is_control()));
        self.invalidate_inflight();
        WizardIntent::Changed
    }

    fn on_address_key(&mut self, key: &KeyEvent) -> WizardIntent {
        match key.code {
            KeyCode::Enter | KeyCode::Char('\n') => {
                let address = self.server_address();
                if address.is_empty() {
                    self.error = Some(
                        "Enter the server address, for example https://api.example.com/v1"
                            .to_owned(),
                    );
                    return WizardIntent::Changed;
                }
                if let Some(problem) = embedded_credentials(&address) {
                    self.error = Some(problem);
                    return WizardIntent::Changed;
                }
                self.error = None;
                self.step = WizardStep::ApiKey;
                WizardIntent::Discover
            }
            KeyCode::Char(_)
                if key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::ALT,
                ) =>
            {
                WizardIntent::Unchanged
            }
            _ => match self.address.handle_key(key) {
                crate::input::line_editor::LineEditOutcome::Unhandled => WizardIntent::Unchanged,
                _ => {
                    self.error = None;
                    // A different address means a different server: anything
                    // still coming back describes the old one.
                    self.invalidate_inflight();
                    WizardIntent::Changed
                }
            },
        }
    }

    fn on_api_key_key(&mut self, key: &KeyEvent) -> WizardIntent {
        match key.code {
            KeyCode::Enter | KeyCode::Char('\n') => {
                // An empty key is a deliberate skip: dispatch omits the field.
                self.error = None;
                self.step = WizardStep::WireFormat;
                WizardIntent::Discover
            }
            KeyCode::Backspace => {
                self.pop_credential_char();
                self.invalidate_inflight();
                WizardIntent::Changed
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.key = SecretInput::default();
                self.invalidate_inflight();
                WizardIntent::Changed
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                self.push_credential_chars(std::iter::once(ch));
                self.invalidate_inflight();
                WizardIntent::Changed
            }
            _ => WizardIntent::Unchanged,
        }
    }

    fn on_format_key(&mut self, key: &KeyEvent) -> WizardIntent {
        match key.code {
            KeyCode::Enter | KeyCode::Char('\n') => {
                self.error = None;
                // Discovery is committed here: the format decides the request
                // shape, so this is the first moment it can be sent.
                self.step = WizardStep::Models;
                WizardIntent::Discover
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.format_index = (self.format_index + 1) % WIRE_FORMATS.len();
                WizardIntent::Changed
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.format_index =
                    (self.format_index + WIRE_FORMATS.len() - 1) % WIRE_FORMATS.len();
                WizardIntent::Changed
            }
            KeyCode::Char(digit @ ('1' | '2' | '3')) => {
                self.format_index = digit as usize - '1' as usize;
                WizardIntent::Changed
            }
            _ => WizardIntent::Unchanged,
        }
    }

    fn on_models_key(&mut self, key: &KeyEvent) -> WizardIntent {
        let rows = self.filtered_rows();
        // Typing anywhere in the step filters; Esc still cancels the wizard.
        if let KeyCode::Char(ch) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
            && ch != ' '
            && !matches!(key.code, KeyCode::Down | KeyCode::Up)
        {
            self.filter_focused = true;
            self.filter.handle_key(key);
            self.focus = 0;
            self.scroll_offset = 0;
            return WizardIntent::Changed;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('\n') => {
                if self.selected_count() == 0 {
                    self.error = Some("Select at least one model to save".to_owned());
                    return WizardIntent::Changed;
                }
                // Marking the save in flight (and counting rows) is dispatch's
                // job, because it owns the shared generation counter.
                self.error = None;
                WizardIntent::Save
            }
            KeyCode::Down => {
                if !rows.is_empty() {
                    // Step down and clamp at the last visible row; a plain
                    // `min` here would leave focus where it was.
                    self.focus = (self.focus + 1).min(rows.len() - 1);
                }
                WizardIntent::Changed
            }
            KeyCode::Up => {
                self.focus = self.focus.saturating_sub(1);
                WizardIntent::Changed
            }
            KeyCode::Char(' ') | KeyCode::Tab => {
                if let Some(&model_index) = rows.get(self.focus)
                    && let Some(slot) = self.selected.get_mut(model_index)
                {
                    *slot = !*slot;
                }
                WizardIntent::Changed
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.set_all_filtered(&rows, true);
                WizardIntent::Changed
            }
            KeyCode::Char('x') | KeyCode::Char('u')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.set_all_filtered(&rows, false);
                WizardIntent::Changed
            }
            KeyCode::Backspace => {
                self.filter.handle_key(key);
                self.focus = 0;
                WizardIntent::Changed
            }
            _ => WizardIntent::Unchanged,
        }
    }

    fn on_summary_key(&mut self, key: &KeyEvent) -> WizardIntent {
        match key.code {
            KeyCode::Enter | KeyCode::Char('\n') => WizardIntent::Cancel,
            _ => WizardIntent::Changed,
        }
    }

    fn set_all_filtered(&mut self, rows: &[usize], value: bool) {
        for &model_index in rows {
            if let Some(slot) = self.selected.get_mut(model_index) {
                *slot = value;
            }
        }
    }

    fn push_credential_chars(&mut self, chars: impl Iterator<Item = char>) {
        let mut buffer = self.key.expose().to_owned();
        for ch in chars {
            if buffer.chars().count() >= MAX_KEY_CHARS {
                break;
            }
            buffer.push(ch);
        }
        self.key = SecretInput::new(buffer);
    }

    fn pop_credential_char(&mut self) {
        let mut buffer = self.key.expose().to_owned();
        buffer.pop();
        self.key = SecretInput::new(buffer);
    }

    // -- rendering -----------------------------------------------------------

    /// Footer hints for the current step.
    fn shortcuts(&self) -> Vec<Shortcut<'static>> {
        let mut hints: Vec<Shortcut<'static>> = Vec::new();
        let mut hint = |label: &'static str| {
            hints.push(Shortcut {
                label,
                clickable: false,
                id: SHORTCUT_ID_HINT,
            })
        };
        match self.step {
            WizardStep::Address => {
                hint("type the server URL");
                hint("Enter next");
            }
            WizardStep::ApiKey => {
                hint("type the key (hidden)");
                hint("Enter skip / next");
            }
            WizardStep::WireFormat => {
                hint("\u{2191}\u{2193} choose");
                hint("Enter discover models");
            }
            WizardStep::Models => {
                hint("\u{2191}\u{2193} move");
                hint("space toggle");
                hint("type to filter");
                hint("Ctrl+A all");
                hint("Ctrl+X none");
                hint("Enter save");
            }
            WizardStep::Summary => {
                hint("Enter close");
                hint("\u{21e7}\u{21e5} back");
            }
        }
        if self.step.index() > 0 {
            hint("\u{21e7}\u{21e5} back");
        }
        hints.push(Shortcut {
            label: "Esc cancel",
            clickable: false,
            id: SHORTCUT_ID_HINT,
        });
        hints
    }

    /// Content lines for the current step.
    fn body_lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let step_label = format!(
            "Step {} of {} - {}",
            self.step.index() + 1,
            WizardStep::ALL.len(),
            self.step.title()
        );
        lines.push(Line::from(Span::styled(
            step_label,
            Style::default()
                .fg(theme.accent_running)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));

        match self.step {
            WizardStep::Address => {
                lines.push(Line::from(Span::styled(
                    "Where does this server live?",
                    Style::default().fg(theme.text_secondary),
                )));
                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme.accent_user)),
                    Span::styled(
                        truncate_to_width(self.address.text(), width.saturating_sub(2)),
                        Style::default().fg(theme.text_primary),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "http:// works for a local runtime; /v1 is added when needed.",
                    Style::default().fg(theme.gray_dim),
                )));
            }
            WizardStep::ApiKey => {
                lines.push(Line::from(Span::styled(
                    "API key for this server, or Enter to skip.",
                    Style::default().fg(theme.text_secondary),
                )));
                let masked = "\u{2022}".repeat(self.key.len());
                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme.accent_user)),
                    Span::styled(
                        masked,
                        Style::default().fg(if self.key.is_empty() {
                            theme.gray_dim
                        } else {
                            theme.text_primary
                        }),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "Stored only for this address. Never shown again after you type it.",
                    Style::default().fg(theme.gray_dim),
                )));
            }
            WizardStep::WireFormat => {
                lines.push(Line::from(Span::styled(
                    "Which protocol does it speak?",
                    Style::default().fg(theme.text_secondary),
                )));
                for (index, choice) in WIRE_FORMATS.iter().enumerate() {
                    let picked = index == self.format_index;
                    let radio = if picked {
                        format!("({})", crate::glyphs::check_mark())
                    } else {
                        "( )".to_owned()
                    };
                    let radio_style = if picked {
                        Style::default().fg(theme.accent_success)
                    } else {
                        Style::default().fg(theme.gray_dim)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(radio, radio_style),
                        Span::styled(
                            format!(" {}. ", index + 1),
                            Style::default().fg(theme.gray_dim),
                        ),
                        Span::styled(
                            choice.label.to_owned(),
                            Style::default().fg(theme.text_primary),
                        ),
                    ]));
                    lines.push(Line::from(Span::styled(
                        format!("     {}", choice.hint),
                        Style::default().fg(theme.gray_dim),
                    )));
                }
                if self.discovering {
                    lines.push(Line::from(Span::styled(
                        "Discovering models...",
                        Style::default().fg(theme.accent_running),
                    )));
                }
            }
            WizardStep::Models => {
                let rows = self.filtered_rows();
                lines.push(Line::from(vec![
                    Span::styled("Filter: ", Style::default().fg(theme.gray_bright)),
                    Span::styled(
                        self.filter.text().to_owned(),
                        Style::default().fg(theme.text_primary),
                    ),
                    Span::styled(
                        "\u{2588}",
                        Style::default().fg(if self.filter_focused {
                            theme.accent_user
                        } else {
                            theme.gray_dim
                        }),
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} of {} selected",
                        self.selected_count(),
                        self.models.len()
                    ),
                    Style::default().fg(theme.gray_dim),
                )));
                if self.models.is_empty() {
                    lines.push(Line::from(Span::styled(
                        if self.discovering {
                            "Discovering models from the server...".to_owned()
                        } else {
                            "No models came back from this address.".to_owned()
                        },
                        Style::default().fg(theme.gray_dim),
                    )));
                } else if rows.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "No model matches the filter.",
                        Style::default().fg(theme.gray_dim),
                    )));
                }
                for (row_pos, &model_index) in rows.iter().enumerate() {
                    let Some(model) = self.models.get(model_index) else {
                        continue;
                    };
                    let focused = row_pos == self.focus;
                    let checked = self.selected.get(model_index).copied().unwrap_or(false);
                    let mark = if checked {
                        crate::glyphs::check_mark()
                    } else {
                        " "
                    };
                    let row_bg = if focused {
                        Some(theme.bg_highlight)
                    } else {
                        None
                    };
                    let bracket = |style: Style| with_bg(style, row_bg);
                    let ctx = if model.context_window > 0 {
                        format!(" - {}", model.context_window)
                    } else {
                        String::new()
                    };
                    lines.push(Line::from(vec![
                        Span::styled("[", bracket(Style::default().fg(theme.gray_dim))),
                        Span::styled(
                            mark,
                            bracket(Style::default().fg(if checked {
                                theme.accent_success
                            } else {
                                theme.gray_dim
                            })),
                        ),
                        Span::styled("]", bracket(Style::default().fg(theme.gray_dim))),
                        Span::styled(
                            format!(
                                " {}{}",
                                truncate_to_width(model.label(), width.saturating_sub(12)),
                                ctx
                            ),
                            bracket(Style::default().fg(theme.text_primary)),
                        ),
                    ]));
                }
            }
            WizardStep::Summary => {
                let noun = if self.saved_count == 1 {
                    "model"
                } else {
                    "models"
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "\u{2713} Saved {} {} to your Open Grok config.",
                        self.saved_count, noun
                    ),
                    Style::default()
                        .fg(theme.accent_success)
                        .add_modifier(Modifier::BOLD),
                )));
                if let Some(base_url) = &self.base_url {
                    lines.push(Line::from(vec![
                        Span::styled("  Server  ", Style::default().fg(theme.gray_dim)),
                        Span::styled(base_url.clone(), Style::default().fg(theme.text_primary)),
                    ]));
                }
                lines.push(Line::from(vec![
                    Span::styled("  Format  ", Style::default().fg(theme.gray_dim)),
                    Span::styled(
                        self.format().label.to_owned(),
                        Style::default().fg(theme.text_primary),
                    ),
                ]));
                if let Some(scheme) = &self.auth_scheme {
                    lines.push(Line::from(vec![
                        Span::styled("  Header  ", Style::default().fg(theme.gray_dim)),
                        Span::styled(scheme.clone(), Style::default().fg(theme.text_primary)),
                    ]));
                }
                for note in &self.notes {
                    lines.push(Line::from(Span::styled(
                        format!("  - {note}"),
                        Style::default().fg(theme.gray_bright),
                    )));
                }
                if let Some(warning) = &self.warning {
                    lines.push(Line::from(Span::styled(
                        format!("  Warning: {warning}"),
                        Style::default().fg(theme.accent_running),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "Pick any of them with /model.",
                    Style::default().fg(theme.gray_dim),
                )));
            }
        }

        if let Some(error) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("\u{2717} {error}"),
                Style::default()
                    .fg(theme.accent_error)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        lines
    }
}

/// Reject an address that smuggles credentials in the URL; the wizard has a
/// dedicated key step, and a URL-embedded key would be echoed everywhere.
fn embedded_credentials(address: &str) -> Option<String> {
    let after_scheme = address.split_once("://").map_or(address, |rest| rest.1);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    if authority.contains('@') {
        return Some(
            "Remove the credentials from the URL - paste the key on the next step instead."
                .to_owned(),
        );
    }
    None
}

/// Truncate to `width` display columns, appending an ellipsis when shortened.
fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let advance = ch.width().unwrap_or(0);
        if used + advance > width.saturating_sub(1) {
            out.push('\u{2026}');
            return out;
        }
        used += advance;
        out.push(ch);
    }
    out
}

/// Apply the focused-row highlight background to a span style.
fn with_bg(style: Style, bg: Option<ratatui::style::Color>) -> Style {
    match bg {
        Some(bg) => style.bg(bg),
        None => style,
    }
}

/// Render the wizard centered over `area`.
pub fn render_custom_provider_modal(
    buf: &mut Buffer,
    area: Rect,
    state: &mut CustomProviderModalState,
    theme: &Theme,
    compact: bool,
) {
    let shortcuts = state.shortcuts();
    let title = format!(
        "Add a custom provider - step {}/{}",
        state.step.index() + 1,
        WizardStep::ALL.len()
    );
    let config = ModalWindowConfig {
        title: &title,
        tabs: None,
        shortcuts: &shortcuts,
        sizing: ModalSizing {
            width_pct: 0.72,
            max_width: 96,
            min_width: 52,
            v_margin: 4,
            footer_lines: 1,
            ..ModalSizing::default()
        }
        .with_compact(compact),
        fold_info: None,
    };
    let Some(areas) = render_modal_window(buf, area, &mut state.window, &config, theme) else {
        return;
    };
    let content = areas.content;
    if content.width == 0 || content.height == 0 {
        return;
    }
    let lines = state.body_lines(content.width as usize, theme);
    // Keep the focused checklist row inside the viewport.
    let visible = content.height.saturating_sub(2) as usize;
    if state.step == WizardStep::Models && visible > 0 {
        let rows = state.filtered_rows();
        let header_rows = 2;
        if state.focus + header_rows >= state.scroll_offset + visible {
            state.scroll_offset = state.focus + header_rows + 1 - visible;
        }
        if rows.is_empty() {
            state.scroll_offset = 0;
        }
    }
    let skipped = if state.step == WizardStep::Models {
        // Skip the two header lines plus the scrolled-away rows.
        2 + state.scroll_offset
    } else {
        0
    };
    let window: Vec<Line<'static>> = lines.into_iter().skip(skipped).collect();
    Paragraph::new(window).render(content, buf);
}
