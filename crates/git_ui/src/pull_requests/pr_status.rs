use std::path::PathBuf;
use std::time::Duration;

use collections::HashMap;
use gpui::{App, Context, Empty, Entity, IntoElement, Render, Subscription, Task, Window};
use project::{
    Project,
    git_store::{GitStoreEvent, RepositoryEvent},
};
use ui::{Button, ButtonCommon, ButtonStyle, Clickable, Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use super::OpenCurrentBranchPullRequest;
use super::github_api::{BranchPullRequest, GithubClient, GithubError};

const LOOKUP_DEBOUNCE: Duration = Duration::from_millis(300);

type BranchKey = (PathBuf, String);

pub struct PullRequestStatusItem {
    project: Entity<Project>,
    pull_request: Option<BranchPullRequest>,
    cache: HashMap<BranchKey, BranchPullRequest>,
    lookups_enabled: bool,
    error_logged: bool,
    lookup_task: Task<()>,
    _subscription: Subscription,
}

impl PullRequestStatusItem {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let subscription = cx.subscribe(&git_store, |this, _, event, cx| {
            if matches!(
                event,
                GitStoreEvent::ActiveRepositoryChanged(_)
                    | GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, true)
            ) {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            project,
            pull_request: None,
            cache: HashMap::default(),
            lookups_enabled: true,
            error_logged: false,
            lookup_task: Task::ready(()),
            _subscription: subscription,
        };
        this.refresh(cx);
        this
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_fixture(
        workspace: &Workspace,
        pull_request: BranchPullRequest,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(workspace, cx);
        this.lookups_enabled = false;
        this.lookup_task = Task::ready(());
        this.pull_request = Some(pull_request);
        this
    }

    fn current_branch_key(&self, cx: &App) -> Option<BranchKey> {
        let project = self.project.read(cx);
        if !project.is_local() {
            return None;
        }
        let repository = project.active_repository(cx)?;
        let repository = repository.read(cx);
        let branch = repository.branch.as_ref()?.name().to_string();
        Some((repository.work_directory_abs_path.to_path_buf(), branch))
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.lookups_enabled {
            return;
        }
        let Some(key) = self.current_branch_key(cx) else {
            self.pull_request = None;
            self.lookup_task = Task::ready(());
            cx.notify();
            return;
        };
        if let Some(cached) = self.cache.get(&key) {
            self.pull_request = Some(cached.clone());
            self.lookup_task = Task::ready(());
            cx.notify();
            return;
        }
        self.pull_request = None;
        cx.notify();

        let client = GithubClient::new(key.0.clone());
        self.lookup_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOOKUP_DEBOUNCE).await;
            let result = cx
                .background_spawn(async move { client.current_branch_pull_request().await })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Some(pull_request)) => {
                        this.cache.insert(key, pull_request.clone());
                        this.pull_request = Some(pull_request);
                    }
                    Ok(None) => this.pull_request = None,
                    Err(error) => {
                        this.pull_request = None;
                        this.log_lookup_error(&error);
                    }
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn log_lookup_error(&mut self, error: &GithubError) {
        if !self.error_logged {
            self.error_logged = true;
            log::warn!("could not look up the pull request for the current branch: {error}");
        }
    }
}

impl Render for PullRequestStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pull_request) = &self.pull_request else {
            return Empty.into_any_element();
        };
        let label = format!("PR #{}", pull_request.number);
        Button::new("pull-request-status", label.clone())
            .style(ButtonStyle::Subtle)
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::PullRequest).size(IconSize::Small))
            .aria_label(label)
            .tooltip(|_, cx| {
                Tooltip::for_action("Open Pull Request", &OpenCurrentBranchPullRequest, cx)
            })
            .on_click(cx.listener(|_, _, window, cx| {
                window.dispatch_action(Box::new(OpenCurrentBranchPullRequest), cx);
            }))
            .into_any_element()
    }
}

impl StatusItemView for PullRequestStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
