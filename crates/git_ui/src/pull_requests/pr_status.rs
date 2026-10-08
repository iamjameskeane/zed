use std::path::PathBuf;
use std::time::Duration;

use collections::HashMap;
use gpui::{
    App, Context, Empty, Entity, EntityId, Global, IntoElement, Render, Subscription, Task,
    WeakEntity, Window,
};
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

#[derive(Debug, Clone, PartialEq)]
pub enum BranchLookup {
    Unavailable,
    Loading,
    NoPullRequest { branch: String },
    Failed(String),
    Found(BranchPullRequest),
}

#[derive(Default)]
struct LookupRegistry(HashMap<EntityId, WeakEntity<BranchPullRequestLookup>>);

impl Global for LookupRegistry {}

pub struct BranchPullRequestLookup {
    project: Entity<Project>,
    state: BranchLookup,
    cache: HashMap<BranchKey, BranchPullRequest>,
    lookups_enabled: bool,
    error_logged: bool,
    lookup_task: Task<()>,
    _subscription: Subscription,
}

impl BranchPullRequestLookup {
    pub fn shared(project: &Entity<Project>, cx: &mut App) -> Entity<Self> {
        let existing = cx
            .default_global::<LookupRegistry>()
            .0
            .get(&project.entity_id())
            .and_then(|lookup| lookup.upgrade());
        if let Some(existing) = existing {
            return existing;
        }
        let lookup = cx.new(|cx| Self::new(project.clone(), cx));
        let registry = cx.default_global::<LookupRegistry>();
        registry.0.retain(|_, lookup| lookup.upgrade().is_some());
        registry
            .0
            .insert(project.entity_id(), lookup.downgrade());
        lookup
    }

    fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
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
            state: BranchLookup::Unavailable,
            cache: HashMap::default(),
            lookups_enabled: true,
            error_logged: false,
            lookup_task: Task::ready(()),
            _subscription: subscription,
        };
        this.refresh(cx);
        this
    }

    pub fn state(&self) -> &BranchLookup {
        &self.state
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_fixture(&mut self, state: BranchLookup, cx: &mut Context<Self>) {
        self.lookups_enabled = false;
        self.lookup_task = Task::ready(());
        self.state = state;
        cx.notify();
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

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.current_branch_key(cx) {
            self.cache.remove(&key);
        }
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.lookups_enabled {
            return;
        }
        let Some(key) = self.current_branch_key(cx) else {
            self.state = BranchLookup::Unavailable;
            self.lookup_task = Task::ready(());
            cx.notify();
            return;
        };
        if let Some(cached) = self.cache.get(&key) {
            self.state = BranchLookup::Found(cached.clone());
            self.lookup_task = Task::ready(());
            cx.notify();
            return;
        }
        self.state = BranchLookup::Loading;
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
                        this.state = BranchLookup::Found(pull_request);
                    }
                    Ok(None) => this.state = BranchLookup::NoPullRequest { branch: key.1 },
                    Err(error) => {
                        this.log_lookup_error(&error);
                        this.state = BranchLookup::Failed(error.to_string());
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

pub struct PullRequestStatusItem {
    lookup: Entity<BranchPullRequestLookup>,
    _subscription: Subscription,
}

impl PullRequestStatusItem {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let lookup = BranchPullRequestLookup::shared(workspace.project(), cx);
        let subscription = cx.observe(&lookup, |_, _, cx| cx.notify());
        Self {
            lookup,
            _subscription: subscription,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_fixture(
        workspace: &Workspace,
        pull_request: BranchPullRequest,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = Self::new(workspace, cx);
        this.lookup.update(cx, |lookup, cx| {
            lookup.set_fixture(BranchLookup::Found(pull_request), cx)
        });
        this
    }
}

impl Render for PullRequestStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let BranchLookup::Found(BranchPullRequest { number, .. }) = *self.lookup.read(cx).state()
        else {
            return Empty.into_any_element();
        };
        let label = format!("PR #{number}");
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
