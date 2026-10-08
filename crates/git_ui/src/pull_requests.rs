pub mod github_api;
pub mod pr_ask_ai;
pub mod pr_changes_panel;
pub mod pr_overview;
pub mod pr_review;
pub mod pr_status;

use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Window,
    actions,
};
use schemars::JsonSchema;
use serde::Deserialize;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use github_api::{BranchPullRequest, GithubClient, GithubContext, GithubError};
use pr_overview::{PullRequestOverviewView, show_toast};

pub use pr_changes_panel::PullRequestChangesPanel;
pub use pr_status::{BranchLookup, PullRequestStatusItem};

actions!(
    pull_requests,
    [
        /// Opens the pull request of the branch checked out in the active repository
        /// and shows its changed files in the Pull Request panel.
        OpenCurrentBranchPullRequest,
        /// Toggles focus on the Pull Request panel.
        TogglePullRequestPanel,
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
        workspace.register_action(|workspace, _: &OpenCurrentBranchPullRequest, window, cx| {
            open_current_branch_pull_request(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &TogglePullRequestPanel, window, cx| {
            workspace.toggle_panel_focus::<PullRequestChangesPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &OpenPullRequest, window, cx| {
            prompt_for_pull_request_number(workspace, NumberPromptMode::Open, window, cx);
        });
        workspace.register_action(|workspace, action: &OpenPullRequestChanges, window, cx| {
            pr_overview::open_pull_request_changes(workspace, action.number, window, cx);
        });
        workspace.register_action(|workspace, _: &CheckoutPullRequest, window, cx| {
            prompt_for_pull_request_number(workspace, NumberPromptMode::Checkout, window, cx);
        });
    })
    .detach();
}

pub(crate) fn open_current_branch_pull_request(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = workspace.weak_handle();
    let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
        show_toast(&workspace_handle, "No active repository".into(), false, cx);
        return;
    };
    let (working_directory, branch_name, fallback_repository) = {
        let repository = repository.read(cx);
        (
            repository.work_directory_abs_path.to_path_buf(),
            repository
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string()),
            GithubContext::from_repository(repository, cx)
                .ok()
                .map(|context| context.repository),
        )
    };
    workspace.focus_panel::<PullRequestChangesPanel>(window, cx);

    let lookup = pr_status::BranchPullRequestLookup::shared(workspace.project(), cx);
    if let BranchLookup::Found(pull_request) = lookup.read(cx).state().clone() {
        open_overview(
            workspace,
            pull_request,
            working_directory,
            fallback_repository,
            window,
            cx,
        );
        return;
    }

    let client = GithubClient::new(working_directory.clone());
    cx.spawn_in(window, async move |_, cx| {
        let result = cx
            .background_spawn(async move { client.current_branch_pull_request().await })
            .await;
        workspace_handle
            .update_in(cx, |workspace, window, cx| match result {
                Ok(Some(pull_request)) => open_overview(
                    workspace,
                    pull_request,
                    working_directory,
                    fallback_repository,
                    window,
                    cx,
                ),
                Ok(None) => {
                    let message = match branch_name {
                        Some(branch_name) => format!("No pull request for branch {branch_name}"),
                        None => "No pull request for the current checkout".to_string(),
                    };
                    show_toast(&workspace.weak_handle(), message, true, cx);
                }
                Err(error) => {
                    show_toast(&workspace.weak_handle(), error.to_string(), false, cx);
                }
            })
            .log_err();
    })
    .detach();
}

fn open_overview(
    workspace: &mut Workspace,
    pull_request: BranchPullRequest,
    working_directory: std::path::PathBuf,
    fallback_repository: Option<github_api::GithubRepository>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repository) = pull_request.repository().or(fallback_repository) else {
        show_toast(
            &workspace.weak_handle(),
            GithubError::NotGithubRepository.to_string(),
            false,
            cx,
        );
        return;
    };
    let context = GithubContext {
        repository,
        working_directory,
    };
    PullRequestOverviewView::open_with_context(workspace, context, pull_request.number, window, cx);
    workspace.focus_panel::<PullRequestChangesPanel>(window, cx);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberPromptMode {
    Open,
    Checkout,
}

fn prompt_for_pull_request_number(
    workspace: &mut Workspace,
    mode: NumberPromptMode,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, |window, cx| {
        NumberPrompt::new(workspace_handle, mode, window, cx)
    });
}

struct NumberPrompt {
    workspace: gpui::WeakEntity<Workspace>,
    mode: NumberPromptMode,
    input: Entity<Editor>,
    error: Option<SharedString>,
}

impl NumberPrompt {
    fn new(
        workspace: gpui::WeakEntity<Workspace>,
        mode: NumberPromptMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Pull request number", window, cx);
            editor
        });
        Self {
            workspace,
            mode,
            input,
            error: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        let Ok(number) = text.trim().trim_start_matches('#').parse::<u64>() else {
            self.error = Some("Enter a pull request number".into());
            cx.notify();
            return;
        };
        let mode = self.mode;
        self.workspace
            .update(cx, |workspace, cx| match mode {
                NumberPromptMode::Open => {
                    PullRequestOverviewView::open(workspace, number, window, cx)
                }
                NumberPromptMode::Checkout => checkout_pull_request(workspace, number, cx),
            })
            .log_err();
        cx.emit(DismissEvent);
    }
}

fn checkout_pull_request(workspace: &mut Workspace, number: u64, cx: &mut Context<Workspace>) {
    let workspace_handle = workspace.weak_handle();
    let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
        show_toast(&workspace_handle, "No active repository".into(), false, cx);
        return;
    };
    let client = GithubClient::new(repository.read(cx).work_directory_abs_path.to_path_buf());
    cx.spawn(async move |_, cx| {
        let result = cx
            .background_spawn(async move { client.checkout_pull_request(number).await })
            .await;
        match result {
            Ok(()) => show_toast(
                &workspace_handle,
                format!("Checked out pull request #{number}"),
                true,
                cx,
            ),
            Err(error) => show_toast(
                &workspace_handle,
                format!("Could not check out pull request #{number}: {error}"),
                false,
                cx,
            ),
        }
    })
    .detach();
}

impl ModalView for NumberPrompt {}
impl EventEmitter<DismissEvent> for NumberPrompt {}

impl Focusable for NumberPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for NumberPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = match self.mode {
            NumberPromptMode::Open => "Open pull request number, press Enter",
            NumberPromptMode::Checkout => "Check out pull request number, press Enter",
        };
        v_flex()
            .key_context("PullRequestNumberPrompt")
            .elevation_3(cx)
            .w(rems(24.))
            .p_2()
            .gap_1()
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(Label::new(hint).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .h_7()
                    .px_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.input.clone()),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }
}
