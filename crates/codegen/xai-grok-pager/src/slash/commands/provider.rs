//! `/provider` -- add a model server you supply yourself.
//!
//! `/provider` and `/provider add` both open the five-step wizard. It only
//! opens UI: the address is queried, and config written, by dispatch once the
//! user confirms.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Open the custom-provider wizard.
pub struct ProviderCommand;

impl SlashCommand for ProviderCommand {
    fn name(&self) -> &str {
        "provider"
    }

    fn aliases(&self) -> &[&str] {
        &["providers", "custom-provider"]
    }

    fn description(&self) -> &str {
        "Add a model server you supply (address, key, format, models)"
    }

    fn usage(&self) -> &str {
        "/provider [add]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[add]")
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match args.trim().to_lowercase().as_str() {
            "" | "add" | "new" => CommandResult::Action(Action::OpenCustomProviderWizard),
            other => CommandResult::Error(format!(
                "Unknown subcommand '{other}'. Use `/provider` or `/provider add`."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;

    static BUNDLE: crate::app::bundle::BundleState = crate::app::bundle::BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn make_ctx<'a>(models: &'a ModelState) -> CommandExecCtx<'a> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot::default(),
        }
    }

    #[test]
    fn bare_and_add_both_open_the_wizard() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let cmd = ProviderCommand;
        for args in ["", "add", "  ADD  ", "new"] {
            let result = cmd.run(&mut ctx, args);
            assert!(
                matches!(
                    result,
                    CommandResult::Action(Action::OpenCustomProviderWizard)
                ),
                "args {args:?} should open the wizard, got {result:?}",
            );
        }
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        let models = ModelState::default();
        let mut ctx = make_ctx(&models);
        let result = ProviderCommand.run(&mut ctx, "remove");
        assert!(
            matches!(result, CommandResult::Error(_)),
            "expected an error, got {result:?}",
        );
    }
}
