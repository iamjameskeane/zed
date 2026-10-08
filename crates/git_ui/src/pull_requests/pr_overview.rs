use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use editor::{Editor, hover_markdown_style};
use gpui::{
    AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, SharedString, Task, WeakEntity, Window, px,
};
use language::LanguageRegistry;
use markdown::{Markdown, MarkdownElement};
use project::Project;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ui::{Avatar, Chip, Divider, Headline, HeadlineSize, TintColor, prelude::*};
use util::{
    ResultExt as _,
    command::{Stdio, new_command},
    truncate_and_trailoff,
};
use workspace::{Toast, Workspace, item::Item, notifications::NotificationId};

use super::github_api::{
    Actor, GithubClient, GithubContext, PullRequestOverview, PullRequestState, ReviewEvent,
    TimelineItem,
};
use super::pr_ask_ai::{ai_enabled, ask_ai_about_pull_request};
use super::pr_review::{PullRequestReviewParams, PullRequestReviewSession};
use crate::branch_diff::BranchDiff;

const TAB_TITLE_MAX_LENGTH: usize = 28;

enum LoadState {
    Loading,
    Loaded(Box<LoadedOverview>),
    Failed(SharedString),
}

struct LoadedOverview {
    overview: PullRequestOverview,
    body: Option<Entity<Markdown>>,
    timeline: Vec<TimelineEntry>,
}

struct TimelineEntry {
    id: String,
    author: Option<Actor>,
    created_at: String,
    review_state: Option<String>,
    markdown: Option<Entity<Markdown>>,
}

#[derive(Clone, PartialEq)]
enum ReviewStatus {
    Idle,
    Submitting,
    Failed(SharedString),
}

pub struct PullRequestOverviewView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    context: GithubContext,
    number: u64,
    focus_handle: FocusHandle,
    state: LoadState,
    review_editor: Entity<Editor>,
    review_status: ReviewStatus,
    checking_out: bool,
    load_task: Task<()>,
    submit_task: Task<()>,
}

impl EventEmitter<()> for PullRequestOverviewView {}

impl Focusable for PullRequestOverviewView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PullRequestOverviewView {
    pub fn open(
        workspace: &mut Workspace,
        number: u64,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().clone();
        match github_context(&project, cx) {
            Ok(context) => Self::open_with_context(workspace, context, number, window, cx),
            Err(message) => show_toast(&workspace_handle, message, false, cx),
        }
    }

    pub fn open_with_context(
        workspace: &mut Workspace,
        context: GithubContext,
        number: u64,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().clone();
        let existing = workspace.items_of_type::<Self>(cx).find(|item| {
            let item = item.read(cx);
            item.number == number && item.context.repository == context.repository
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let view = cx.new(|cx| {
            let mut view = Self::new(workspace_handle, project, context, number, window, cx);
            view.load(cx);
            view
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        context: GithubContext,
        number: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let review_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(4, 12, window, cx);
            editor.set_placeholder_text("Leave a review comment", window, cx);
            editor
        });
        Self {
            workspace,
            project,
            context,
            number,
            focus_handle: cx.focus_handle(),
            state: LoadState::Loading,
            review_editor,
            review_status: ReviewStatus::Idle,
            checking_out: false,
            load_task: Task::ready(()),
            submit_task: Task::ready(()),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open_with_fixture(
        workspace: &mut Workspace,
        context: GithubContext,
        overview: PullRequestOverview,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let workspace_handle = workspace.weak_handle();
        let project = workspace.project().clone();
        let view = cx.new(|cx| {
            let mut view = Self::new(
                workspace_handle,
                project,
                context,
                overview.number,
                window,
                cx,
            );
            let loaded = view.build_loaded(overview, cx);
            view.state = LoadState::Loaded(Box::new(loaded));
            view
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    pub fn open_and_focus_review_box(
        workspace: &mut Workspace,
        number: u64,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::open(workspace, number, window, cx);
        let Some(view) = workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).number == number)
        else {
            return;
        };
        let focus_handle = view.read(cx).review_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
    }

    fn client(&self) -> GithubClient {
        GithubClient::new(self.context.working_directory.clone())
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state, LoadState::Loaded(_)) {
            self.state = LoadState::Loading;
        }
        let client = self.client();
        let repository = self.context.repository.clone();
        let number = self.number;
        self.load_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { client.fetch_overview(&repository, number).await })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(overview) => {
                        let loaded = this.build_loaded(overview, cx);
                        this.state = LoadState::Loaded(Box::new(loaded));
                    }
                    Err(error) if matches!(this.state, LoadState::Loaded(_)) => {
                        show_toast(
                            &this.workspace,
                            format!("Could not refresh pull request #{}: {error}", this.number),
                            false,
                            cx,
                        );
                    }
                    Err(error) => this.state = LoadState::Failed(error.to_string().into()),
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn build_loaded(
        &self,
        overview: PullRequestOverview,
        cx: &mut Context<Self>,
    ) -> LoadedOverview {
        let language_registry = self.project.read(cx).languages().clone();
        let body = non_empty_markdown(&overview.body, &language_registry, cx);
        let timeline = overview
            .timeline
            .iter()
            .filter_map(|item| match item {
                TimelineItem::IssueComment {
                    id,
                    author,
                    body,
                    created_at,
                    ..
                } => Some(TimelineEntry {
                    id: id.clone(),
                    author: author.clone(),
                    created_at: created_at.clone(),
                    review_state: None,
                    markdown: non_empty_markdown(body, &language_registry, cx),
                }),
                TimelineItem::PullRequestReview {
                    id,
                    author,
                    body,
                    state,
                    created_at,
                    ..
                } => Some(TimelineEntry {
                    id: id.clone(),
                    author: author.clone(),
                    created_at: created_at.clone(),
                    review_state: Some(state.clone()),
                    markdown: non_empty_markdown(body, &language_registry, cx),
                }),
                TimelineItem::Other => None,
            })
            .collect();
        LoadedOverview {
            overview,
            body,
            timeline,
        }
    }

    fn checkout(&mut self, cx: &mut Context<Self>) {
        if self.checking_out {
            return;
        }
        self.checking_out = true;
        let client = self.client();
        let number = self.number;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { client.checkout_pull_request(number).await })
                .await;
            this.update(cx, |this, cx| {
                this.checking_out = false;
                let (message, autohide) = match result {
                    Ok(()) => (format!("Checked out pull request #{number}"), true),
                    Err(error) => (
                        format!("Could not check out pull request #{number}: {error}"),
                        false,
                    ),
                };
                show_toast(&this.workspace, message, autohide, cx);
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn open_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        let context = self.context.clone();
        let number = self.number;
        self.workspace
            .update(cx, |workspace, cx| {
                open_pull_request_changes_in(workspace, context, number, window, cx);
            })
            .log_err();
    }

    fn submit_review(&mut self, event: ReviewEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.review_status == ReviewStatus::Submitting {
            return;
        }
        let LoadState::Loaded(loaded) = &self.state else {
            return;
        };
        let body = self.review_editor.read(cx).text(cx).trim().to_string();
        if let Err(message) = validate_review_body(event, &body) {
            self.review_status = ReviewStatus::Failed(message.into());
            cx.notify();
            return;
        }
        let pull_request_id = loaded.overview.id.clone();
        let head_oid = loaded.overview.head_ref_oid.clone();
        let client = self.client();
        let repository = self.context.repository.clone();
        let number = self.number;
        self.review_status = ReviewStatus::Submitting;
        self.submit_task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let pending = match client.fetch_pending_review(&repository, number).await? {
                        Some(pending) => pending,
                        None => {
                            client
                                .start_pending_review(&pull_request_id, &head_oid)
                                .await?
                        }
                    };
                    client.submit_review(&pending.id, event, &body).await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.review_editor
                            .update(cx, |editor, cx| editor.set_text("", window, cx));
                        this.review_status = ReviewStatus::Idle;
                        this.load(cx);
                        this.workspace
                            .update(cx, |workspace, cx| {
                                let sessions = workspace
                                    .items_of_type::<BranchDiff>(cx)
                                    .filter_map(|item| item.read(cx).pull_request_review().cloned())
                                    .collect::<Vec<_>>();
                                for session in sessions {
                                    session.update(cx, |session, cx| session.refresh(window, cx));
                                }
                            })
                            .log_err();
                    }
                    Err(error) => {
                        this.review_status = ReviewStatus::Failed(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn render_header(&self, loaded: &LoadedOverview, cx: &mut Context<Self>) -> impl IntoElement {
        let overview = &loaded.overview;
        let (state_label, state_color) = state_badge(overview);
        let url = overview.url.clone();
        let author = overview
            .author
            .as_ref()
            .map(|author| author.login.clone())
            .unwrap_or_else(|| "ghost".to_string());

        v_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(Headline::new(overview.title.clone()).size(HeadlineSize::Large))
                    .child(
                        Label::new(format!("#{}", overview.number))
                            .size(LabelSize::Large)
                            .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Chip::new(state_label)
                            .icon(IconName::PullRequest)
                            .icon_color(state_color)
                            .label_color(state_color),
                    )
                    .child(avatar(overview.author.as_ref()))
                    .child(Label::new(author).weight(FontWeight::SEMIBOLD))
                    .child(Label::new("wants to merge").color(Color::Muted))
                    .child(Chip::new(overview.head_ref_name.clone()))
                    .child(Label::new("into").color(Color::Muted))
                    .child(Chip::new(overview.base_ref_name.clone())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("pull-request-overview-checkout", "Checkout")
                            .style(ButtonStyle::Filled)
                            .disabled(self.checking_out)
                            .on_click(cx.listener(|this, _, _, cx| this.checkout(cx))),
                    )
                    .child(
                        Button::new("pull-request-overview-changes", "Open Changes")
                            .style(ButtonStyle::Filled)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_changes(window, cx)),
                            ),
                    )
                    .when(ai_enabled(cx), |this| {
                        this.child(
                            Button::new("pull-request-overview-ask-ai", "Ask AI")
                                .style(ButtonStyle::Filled)
                                .start_icon(Icon::new(IconName::ZedAssistant).size(IconSize::Small))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    ask_ai_about_pull_request(
                                        this.workspace.clone(),
                                        this.context.clone(),
                                        this.number,
                                        window,
                                        cx,
                                    );
                                })),
                        )
                    })
                    .child(
                        Button::new("pull-request-overview-github", "Open on GitHub")
                            .style(ButtonStyle::Subtle)
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                    .child(
                        Button::new("pull-request-overview-refresh", "Refresh")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                    ),
            )
    }

    fn render_description(
        &self,
        loaded: &LoadedOverview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let content = match &loaded.body {
            Some(body) => div()
                .text_sm()
                .child(MarkdownElement::new(
                    body.clone(),
                    hover_markdown_style(window, cx),
                ))
                .into_any_element(),
            None => Label::new("No description provided.")
                .color(Color::Muted)
                .into_any_element(),
        };
        let author = loaded.overview.author.as_ref();
        card(
            cx,
            h_flex()
                .gap_2()
                .child(avatar(author))
                .child(
                    Label::new(
                        author
                            .map(|author| author.login.clone())
                            .unwrap_or_else(|| "ghost".to_string()),
                    )
                    .weight(FontWeight::SEMIBOLD),
                )
                .child(Label::new("opened this pull request").color(Color::Muted)),
            content,
        )
    }

    fn render_timeline_entry(
        &self,
        entry: &TimelineEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let login = entry
            .author
            .as_ref()
            .map(|author| author.login.clone())
            .unwrap_or_else(|| "ghost".to_string());
        let verb = if entry.review_state.is_some() {
            "reviewed"
        } else {
            "commented"
        };
        let header = h_flex()
            .gap_2()
            .child(avatar(entry.author.as_ref()))
            .child(Label::new(login).weight(FontWeight::SEMIBOLD))
            .child(Label::new(verb).color(Color::Muted))
            .child(
                Label::new(format_relative_time(
                    &entry.created_at,
                    OffsetDateTime::now_utc(),
                ))
                .color(Color::Muted),
            )
            .when_some(entry.review_state.as_deref(), |this, state| {
                let (label, color) = review_state_badge(state);
                this.child(Chip::new(label).label_color(color))
            });
        let body = entry.markdown.as_ref().map(|markdown| {
            div()
                .text_sm()
                .child(MarkdownElement::new(
                    markdown.clone(),
                    hover_markdown_style(window, cx),
                ))
                .into_any_element()
        });
        let mut container = v_flex()
            .id(SharedString::from(format!("timeline-{}", entry.id)))
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .bg(cx.theme().colors().element_background)
                    .child(header),
            );
        if let Some(body) = body {
            container = container
                .child(Divider::horizontal())
                .child(div().p_3().child(body));
        }
        container.into_any_element()
    }

    fn render_sidebar(&self, loaded: &LoadedOverview, cx: &mut Context<Self>) -> impl IntoElement {
        let overview = &loaded.overview;
        let reviewers = reviewer_rows(overview);
        let (check_icon, check_color, check_label) =
            checks_summary(overview.check_state.as_deref());

        v_flex()
            .w(px(240.))
            .flex_none()
            .gap_4()
            .child(sidebar_section(
                "Reviewers",
                if reviewers.is_empty() {
                    Label::new("No reviewers")
                        .color(Color::Muted)
                        .into_any_element()
                } else {
                    v_flex()
                        .gap_1()
                        .children(reviewers.into_iter().map(|(login, label, color)| {
                            h_flex()
                                .justify_between()
                                .gap_2()
                                .child(Label::new(login))
                                .child(Label::new(label).size(LabelSize::Small).color(color))
                        }))
                        .into_any_element()
                },
                cx,
            ))
            .child(sidebar_section(
                "Labels",
                if overview.labels.is_empty() {
                    Label::new("None yet")
                        .color(Color::Muted)
                        .into_any_element()
                } else {
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .children(overview.labels.iter().map(|label| {
                            let (background, foreground) = label_colors(&label.color);
                            div()
                                .px_1p5()
                                .rounded_full()
                                .text_xs()
                                .bg(background)
                                .text_color(foreground)
                                .child(label.name.clone())
                        }))
                        .into_any_element()
                },
                cx,
            ))
            .child(sidebar_section(
                "Checks",
                h_flex()
                    .gap_1p5()
                    .when_some(check_icon, |this, icon| {
                        this.child(Icon::new(icon).size(IconSize::Small).color(check_color))
                    })
                    .child(Label::new(check_label).color(check_color))
                    .into_any_element(),
                cx,
            ))
    }

    fn render_review_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let submitting = self.review_status == ReviewStatus::Submitting;
        v_flex()
            .w_full()
            .gap_2()
            .child(Label::new("Add your review").weight(FontWeight::SEMIBOLD))
            .child(
                div()
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.review_editor.clone()),
            )
            .when_some(
                match &self.review_status {
                    ReviewStatus::Failed(message) => Some(message.clone()),
                    _ => None,
                },
                |this, message| {
                    this.child(
                        Label::new(message)
                            .color(Color::Error)
                            .size(LabelSize::Small),
                    )
                },
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("pull-request-review-comment", "Comment")
                            .style(ButtonStyle::Filled)
                            .disabled(submitting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_review(ReviewEvent::Comment, window, cx);
                            })),
                    )
                    .child(
                        Button::new("pull-request-review-approve", "Approve")
                            .style(ButtonStyle::Tinted(TintColor::Success))
                            .disabled(submitting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_review(ReviewEvent::Approve, window, cx);
                            })),
                    )
                    .child(
                        Button::new("pull-request-review-request-changes", "Request Changes")
                            .style(ButtonStyle::Tinted(TintColor::Error))
                            .disabled(submitting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_review(ReviewEvent::RequestChanges, window, cx);
                            })),
                    )
                    .when(submitting, |this| {
                        this.child(
                            Label::new("Submitting...")
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                    }),
            )
    }
}

impl Render for PullRequestOverviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.state {
            LoadState::Loading => {
                centered_message(Label::new("Loading pull request...").color(Color::Muted))
                    .into_any_element()
            }
            LoadState::Failed(message) => centered_message(
                v_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(message.clone()).color(Color::Error))
                    .child(
                        Button::new("pull-request-overview-retry", "Retry")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                    ),
            )
            .into_any_element(),
            LoadState::Loaded(loaded) => v_flex()
                .w_full()
                .max_w(px(1100.))
                .mx_auto()
                .p_6()
                .gap_4()
                .child(self.render_header(loaded, cx))
                .child(Divider::horizontal())
                .child(
                    h_flex()
                        .w_full()
                        .gap_6()
                        .items_start()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_4()
                                .child(self.render_description(loaded, window, cx))
                                .children(
                                    loaded
                                        .timeline
                                        .iter()
                                        .map(|entry| self.render_timeline_entry(entry, window, cx))
                                        .collect::<Vec<_>>(),
                                )
                                .child(Divider::horizontal())
                                .child(self.render_review_box(cx)),
                        )
                        .child(self.render_sidebar(loaded, cx)),
                )
                .into_any_element(),
        };

        div()
            .id("pull-request-overview")
            .key_context("PullRequestOverview")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .child(content)
            .into_any_element()
    }
}

impl Item for PullRequestOverviewView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &gpui::App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &gpui::App) -> SharedString {
        match &self.state {
            LoadState::Loaded(loaded) => format!(
                "#{} {}",
                self.number,
                truncate_and_trailoff(&loaded.overview.title, TAB_TITLE_MAX_LENGTH)
            )
            .into(),
            _ => format!("#{}", self.number).into(),
        }
    }
}

pub fn open_pull_request_changes(
    workspace: &mut Workspace,
    number: u64,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match github_context(workspace.project(), cx) {
        Ok(context) => open_pull_request_changes_in(workspace, context, number, window, cx),
        Err(message) => show_toast(&workspace.weak_handle(), message, false, cx),
    }
}

pub fn open_pull_request_changes_in(
    workspace: &mut Workspace,
    context: GithubContext,
    number: u64,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = workspace.weak_handle();
    let project = workspace.project().clone();
    let Some(repository) = project.read(cx).git_store().read(cx).active_repository() else {
        show_toast(&workspace_handle, "No active repository".into(), false, cx);
        return;
    };
    let current_branch = repository
        .read(cx)
        .branch
        .as_ref()
        .map(|branch| branch.name().to_string());
    let client = GithubClient::new(context.working_directory.clone());
    let background_context = context.clone();

    cx.spawn_in(window, async move |_, cx| {
        let result = cx
            .background_spawn(async move {
                let overview = client
                    .fetch_overview(&background_context.repository, number)
                    .await?;
                if current_branch.as_deref() != Some(overview.head_ref_name.as_str()) {
                    client.checkout_pull_request(number).await?;
                }
                fetch_base_ref(
                    &background_context.working_directory,
                    &overview.base_ref_name,
                )
                .await?;
                anyhow::Ok(overview)
            })
            .await;
        match result {
            Ok(overview) => {
                workspace_handle
                    .update_in(cx, |workspace, window, cx| {
                        let session_workspace = workspace.weak_handle();
                        let session_project = project.clone();
                        let session_repository = repository.clone();
                        BranchDiff::deploy_branch_diff_with_base_ref_then(
                            workspace,
                            project,
                            repository,
                            format!("origin/{}", overview.base_ref_name).into(),
                            None,
                            window,
                            cx,
                            move |branch_diff, window, cx| {
                                PullRequestReviewSession::attach(
                                    &branch_diff,
                                    PullRequestReviewParams {
                                        workspace: session_workspace,
                                        project: session_project,
                                        repository: session_repository,
                                        context,
                                        number,
                                        title: overview.title.into(),
                                        pull_request_id: overview.id,
                                        head_oid: overview.head_ref_oid,
                                    },
                                    window,
                                    cx,
                                );
                            },
                        );
                    })
                    .log_err();
            }
            Err(error) => {
                show_toast(
                    &workspace_handle,
                    format!("Could not open changes for pull request #{number}: {error}"),
                    false,
                    cx,
                );
            }
        }
    })
    .detach();
}

pub(super) async fn fetch_base_ref(
    working_directory: &Path,
    base_ref_name: &str,
) -> anyhow::Result<()> {
    let output = new_command("git")
        .args(["fetch", "origin", base_ref_name])
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .output()
        .await
        .context("running git fetch")?;
    anyhow::ensure!(
        output.status.success(),
        "git fetch origin {base_ref_name} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

pub(super) fn github_context(
    project: &Entity<Project>,
    cx: &gpui::App,
) -> Result<GithubContext, String> {
    let repository = project
        .read(cx)
        .git_store()
        .read(cx)
        .active_repository()
        .ok_or_else(|| "No active repository".to_string())?;
    GithubContext::from_repository(repository.read(cx), cx).map_err(|error| error.to_string())
}

pub(super) fn show_toast<C: AppContext>(
    workspace: &WeakEntity<Workspace>,
    message: String,
    autohide: bool,
    cx: &mut C,
) {
    struct PullRequestOverviewToast;
    workspace
        .update(cx, |workspace, cx| {
            let toast = Toast::new(
                NotificationId::unique::<PullRequestOverviewToast>(),
                message,
            );
            workspace.show_toast(if autohide { toast.autohide() } else { toast }, cx);
        })
        .log_err();
}

fn non_empty_markdown(
    source: &str,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut Context<PullRequestOverviewView>,
) -> Option<Entity<Markdown>> {
    if source.trim().is_empty() {
        return None;
    }
    let source = SharedString::from(source.to_string());
    let language_registry = language_registry.clone();
    Some(cx.new(|cx| Markdown::new(source, Some(language_registry), None, cx)))
}

fn validate_review_body(event: ReviewEvent, body: &str) -> Result<(), &'static str> {
    match event {
        ReviewEvent::Approve => Ok(()),
        ReviewEvent::Comment | ReviewEvent::RequestChanges if body.trim().is_empty() => {
            Err("Write a comment before submitting this review.")
        }
        ReviewEvent::Comment | ReviewEvent::RequestChanges => Ok(()),
    }
}

pub(super) fn format_relative_time(timestamp: &str, now: OffsetDateTime) -> String {
    let Ok(parsed) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return timestamp.to_string();
    };
    let seconds = (now - parsed).whole_seconds();
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        "just now".to_string()
    } else if hours < 1 {
        format!("{minutes}m ago")
    } else if days < 1 {
        format!("{hours}h ago")
    } else if days < 30 {
        format!("{days}d ago")
    } else if days < 365 {
        format!("{}mo ago", days / 30)
    } else {
        format!("{}y ago", days / 365)
    }
}

fn state_badge(overview: &PullRequestOverview) -> (&'static str, Color) {
    match overview.state {
        PullRequestState::Merged => ("Merged", Color::Accent),
        PullRequestState::Closed => ("Closed", Color::Error),
        PullRequestState::Open if overview.is_draft => ("Draft", Color::Muted),
        PullRequestState::Open => ("Open", Color::Success),
    }
}

fn review_state_badge(state: &str) -> (&'static str, Color) {
    match state {
        "APPROVED" => ("Approved", Color::Success),
        "CHANGES_REQUESTED" => ("Changes requested", Color::Error),
        "COMMENTED" => ("Commented", Color::Muted),
        "DISMISSED" => ("Dismissed", Color::Muted),
        "PENDING" => ("Pending", Color::Warning),
        _ => ("Reviewed", Color::Muted),
    }
}

fn reviewer_rows(overview: &PullRequestOverview) -> Vec<(String, &'static str, Color)> {
    let mut rows = overview
        .latest_reviews
        .iter()
        .map(|review| {
            let (label, color) = review_state_badge(&review.state);
            let login = review
                .author
                .as_ref()
                .map(|author| author.login.clone())
                .unwrap_or_else(|| "ghost".to_string());
            (login, label, color)
        })
        .collect::<Vec<_>>();
    for requested in &overview.review_requests {
        if !rows.iter().any(|(login, _, _)| login == requested) {
            rows.push((requested.clone(), "Awaiting review", Color::Warning));
        }
    }
    rows
}

fn checks_summary(state: Option<&str>) -> (Option<IconName>, Color, &'static str) {
    match state {
        Some("SUCCESS") => (Some(IconName::Check), Color::Success, "All checks passed"),
        Some("FAILURE") | Some("ERROR") => {
            (Some(IconName::XCircle), Color::Error, "Some checks failed")
        }
        Some("PENDING") | Some("EXPECTED") => (
            Some(IconName::CountdownTimer),
            Color::Warning,
            "Checks in progress",
        ),
        _ => (None, Color::Muted, "No checks"),
    }
}

fn label_colors(hex: &str) -> (Hsla, Hsla) {
    let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0x808080);
    let red = ((value >> 16) & 0xff) as f32;
    let green = ((value >> 8) & 0xff) as f32;
    let blue = (value & 0xff) as f32;
    let luminance = (0.299 * red + 0.587 * green + 0.114 * blue) / 255.;
    let foreground = if luminance > 0.6 {
        gpui::black()
    } else {
        gpui::white()
    };
    (gpui::rgb(value).into(), foreground)
}

pub(super) fn avatar(actor: Option<&Actor>) -> gpui::AnyElement {
    match actor.and_then(|actor| actor.avatar_url.clone()) {
        Some(url) => Avatar::new(SharedString::from(url))
            .size(px(20.))
            .into_any_element(),
        None => {
            let initial = actor
                .and_then(|actor| actor.login.chars().next())
                .map(|character| character.to_uppercase().to_string())
                .unwrap_or_default();
            h_flex()
                .size(px(20.))
                .flex_none()
                .justify_center()
                .rounded_full()
                .bg(gpui::hsla(0., 0., 0.5, 0.3))
                .child(Label::new(initial).size(LabelSize::XSmall))
                .into_any_element()
        }
    }
}

fn centered_message(content: impl IntoElement) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_6()
        .child(content)
}

fn card(
    cx: &Context<PullRequestOverviewView>,
    header: impl IntoElement,
    body: impl IntoElement,
) -> impl IntoElement {
    v_flex()
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border)
        .child(
            div()
                .px_3()
                .py_2()
                .bg(cx.theme().colors().element_background)
                .child(header),
        )
        .child(Divider::horizontal())
        .child(div().p_3().child(body))
}

fn sidebar_section(
    title: &'static str,
    content: gpui::AnyElement,
    cx: &Context<PullRequestOverviewView>,
) -> impl IntoElement {
    v_flex()
        .gap_1p5()
        .pb_3()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            Label::new(title)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .weight(FontWeight::SEMIBOLD),
        )
        .child(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn relative_time_unit_boundaries() {
        let now = OffsetDateTime::parse("2026-06-15T12:00:00Z", &Rfc3339).unwrap();
        let ago = |duration: Duration| {
            let timestamp = (now - duration).format(&Rfc3339).unwrap();
            format_relative_time(&timestamp, now)
        };
        assert_eq!(ago(Duration::seconds(59)), "just now");
        assert_eq!(ago(Duration::seconds(60)), "1m ago");
        assert_eq!(ago(Duration::minutes(59)), "59m ago");
        assert_eq!(ago(Duration::minutes(60)), "1h ago");
        assert_eq!(ago(Duration::hours(23)), "23h ago");
        assert_eq!(ago(Duration::hours(24)), "1d ago");
        assert_eq!(ago(Duration::days(29)), "29d ago");
        assert_eq!(ago(Duration::days(30)), "1mo ago");
        assert_eq!(ago(Duration::days(364)), "12mo ago");
        assert_eq!(ago(Duration::days(365)), "1y ago");
        assert_eq!(ago(Duration::seconds(-30)), "just now");
        assert_eq!(format_relative_time("not a date", now), "not a date");
    }

    #[test]
    fn review_body_is_required_except_for_approval() {
        assert!(validate_review_body(ReviewEvent::Approve, "").is_ok());
        assert!(validate_review_body(ReviewEvent::Comment, "  \n").is_err());
        assert!(validate_review_body(ReviewEvent::RequestChanges, "").is_err());
        assert!(validate_review_body(ReviewEvent::Comment, "text").is_ok());
        assert!(validate_review_body(ReviewEvent::RequestChanges, "text").is_ok());
    }
}
