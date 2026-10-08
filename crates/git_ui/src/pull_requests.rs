pub mod github_api;
pub mod pr_overview;
pub mod pr_panel;
pub mod pr_review;

use gpui::{App, actions};
use schemars::JsonSchema;
use serde::Deserialize;
use workspace::Workspace;

pub use pr_panel::PullRequestPanel;

actions!(
    pull_requests,
    [
        /// Toggles focus on the pull requests panel.
        TogglePanel,
        /// Prompts for a pull request number and opens that pull request.
        OpenPullRequest,
        /// Prompts for a pull request number and checks that pull request out with `gh`.
        CheckoutPullRequest,
    ]
);

/// Opens the changes of a pull request in a branch diff against its base branch.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = pull_requests)]
pub struct OpenPullRequestChanges {
    pub number: u64,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
            workspace.toggle_panel_focus::<PullRequestPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &OpenPullRequest, window, cx| {
            prompt_for_pull_request_number(workspace, pr_panel::NumberPromptMode::Open, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenPullRequestChanges, window, cx| {
            pr_overview::open_pull_request_changes(workspace, action.number, window, cx);
        });
        workspace.register_action(|workspace, _: &CheckoutPullRequest, window, cx| {
            prompt_for_pull_request_number(
                workspace,
                pr_panel::NumberPromptMode::Checkout,
                window,
                cx,
            );
        });
    })
    .detach();
}

fn prompt_for_pull_request_number(
    workspace: &mut Workspace,
    mode: pr_panel::NumberPromptMode,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    workspace.open_panel::<PullRequestPanel>(window, cx);
    if let Some(panel) = workspace.panel::<PullRequestPanel>(cx) {
        panel.update(cx, |panel, cx| panel.show_number_prompt(mode, window, cx));
    }
}
