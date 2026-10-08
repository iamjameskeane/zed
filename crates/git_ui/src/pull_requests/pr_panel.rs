use collections::HashMap;
use editor::Editor;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Task, WeakEntity, Window, px,
};
use project::Project;
use ui::{Checkbox, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use super::TogglePanel;
use super::github_api::{
    FileChangeType, GithubClient, GithubContext, GithubError, PullRequestFile, PullRequestList,
    PullRequestListKind, PullRequestSummary, ViewedState,
};
use super::pr_overview::PullRequestOverviewView;

const PULL_REQUEST_PANEL_KEY: &str = "PullRequestPanel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberPromptMode {
    Open,
    Checkout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRequestPanelEvent {
    OpenPullRequest { number: u64, url: String },
}

enum LoadState<T> {
    Idle,
    Loading,
    Loaded(T),
    Failed(SharedString),
}

struct Section {
    kind: PullRequestListKind,
    collapsed: bool,
    state: LoadState<PullRequestList>,
}

struct ExpandedPullRequest {
    pull_request_id: String,
    files: LoadState<Vec<PullRequestFile>>,
    _task: Task<()>,
}

pub struct PullRequestPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    position: DockPosition,
    sections: Vec<Section>,
    expanded: HashMap<u64, ExpandedPullRequest>,
    has_refreshed: bool,
    number_input: Entity<Editor>,
    number_prompt: Option<NumberPromptMode>,
    number_prompt_error: Option<SharedString>,
    refresh_tasks: Vec<Task<()>>,
}

impl EventEmitter<PanelEvent> for PullRequestPanel {}
impl EventEmitter<PullRequestPanelEvent> for PullRequestPanel {}

impl Focusable for PullRequestPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PullRequestPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(workspace, window, cx)
        })
    }

    fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = workspace.weak_handle();
        cx.new(|cx| {
            let number_input = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Pull request number", window, cx);
                editor
            });
            Self {
                workspace: workspace_handle,
                project,
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                sections: PullRequestListKind::ALL
                    .into_iter()
                    .map(|kind| Section {
                        kind,
                        collapsed: false,
                        state: LoadState::Idle,
                    })
                    .collect(),
                expanded: HashMap::default(),
                has_refreshed: false,
                number_input,
                number_prompt: None,
                number_prompt_error: None,
                refresh_tasks: Vec::new(),
            }
        })
    }

    fn github_context(&self, cx: &App) -> Result<GithubContext, GithubError> {
        let repository = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .active_repository()
            .ok_or(GithubError::NotGithubRepository)?;
        GithubContext::from_repository(repository.read(cx), cx)
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.has_refreshed = true;
        self.refresh_tasks.clear();

        let context = match self.github_context(cx) {
            Ok(context) => context,
            Err(error) => {
                let message = SharedString::from(error.to_string());
                for section in &mut self.sections {
                    section.state = LoadState::Failed(message.clone());
                }
                cx.notify();
                return;
            }
        };

        for section in &mut self.sections {
            section.state = LoadState::Loading;
        }
        for kind in PullRequestListKind::ALL {
            let client = GithubClient::new(context.working_directory.clone());
            let repository = context.repository.clone();
            let task = cx.spawn(async move |this, cx| {
                let result = cx
                    .background_spawn(
                        async move { client.list_pull_requests(&repository, kind).await },
                    )
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(section) = this
                        .sections
                        .iter_mut()
                        .find(|section| section.kind == kind)
                    {
                        section.state = match result {
                            Ok(list) => LoadState::Loaded(list),
                            Err(error) => LoadState::Failed(error.to_string().into()),
                        };
                    }
                    cx.notify();
                })
                .log_err();
            });
            self.refresh_tasks.push(task);
        }
        cx.notify();
    }

    pub fn show_number_prompt(
        &mut self,
        mode: NumberPromptMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.number_prompt = Some(mode);
        self.number_prompt_error = None;
        self.number_input
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.number_input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn hide_number_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.number_prompt = None;
        self.number_prompt_error = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn cancel_number_prompt(
        &mut self,
        _: &menu::Cancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hide_number_prompt(window, cx);
    }

    fn confirm_number_prompt(
        &mut self,
        _: &menu::Confirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mode) = self.number_prompt else {
            return;
        };
        let text = self.number_input.read(cx).text(cx);
        let Ok(number) = text.trim().trim_start_matches('#').parse::<u64>() else {
            self.number_prompt_error = Some("Enter a pull request number".into());
            cx.notify();
            return;
        };
        let context = match self.github_context(cx) {
            Ok(context) => context,
            Err(error) => {
                self.number_prompt_error = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        self.hide_number_prompt(window, cx);
        match mode {
            NumberPromptMode::Checkout => self.checkout_pull_request(number, context, cx),
            NumberPromptMode::Open => {
                let url = format!(
                    "https://github.com/{}/pull/{number}",
                    context.repository.full_name()
                );
                self.open_pull_request(number, url, window, cx);
            }
        }
    }

    fn open_pull_request(
        &mut self,
        number: u64,
        url: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace
            .update(cx, |workspace, cx| {
                PullRequestOverviewView::open(workspace, number, window, cx);
            })
            .log_err();
        cx.emit(PullRequestPanelEvent::OpenPullRequest { number, url });
    }

    fn checkout_pull_request(
        &mut self,
        number: u64,
        context: GithubContext,
        cx: &mut Context<Self>,
    ) {
        let client = GithubClient::new(context.working_directory);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { client.checkout_pull_request(number).await })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => this.show_toast(format!("Checked out pull request #{number}"), true, cx),
                Err(error) => this.show_toast(
                    format!("Could not check out pull request #{number}: {error}"),
                    false,
                    cx,
                ),
            })
            .log_err();
        })
        .detach();
    }

    fn show_toast(&self, message: String, autohide: bool, cx: &mut Context<Self>) {
        struct PullRequestToast;
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let toast = Toast::new(NotificationId::unique::<PullRequestToast>(), message);
            workspace.show_toast(if autohide { toast.autohide() } else { toast }, cx);
        });
    }

    fn toggle_section(&mut self, kind: PullRequestListKind, cx: &mut Context<Self>) {
        if let Some(section) = self
            .sections
            .iter_mut()
            .find(|section| section.kind == kind)
        {
            section.collapsed = !section.collapsed;
            cx.notify();
        }
    }

    fn toggle_pull_request(&mut self, pull_request: &PullRequestSummary, cx: &mut Context<Self>) {
        if self.expanded.remove(&pull_request.number).is_some() {
            cx.notify();
            return;
        }
        let context = match self.github_context(cx) {
            Ok(context) => context,
            Err(error) => {
                self.expanded.insert(
                    pull_request.number,
                    ExpandedPullRequest {
                        pull_request_id: pull_request.id.clone(),
                        files: LoadState::Failed(error.to_string().into()),
                        _task: Task::ready(()),
                    },
                );
                cx.notify();
                return;
            }
        };
        let number = pull_request.number;
        let client = GithubClient::new(context.working_directory);
        let repository = context.repository;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { client.fetch_files(&repository, number).await })
                .await;
            this.update(cx, |this, cx| {
                if let Some(expanded) = this.expanded.get_mut(&number) {
                    expanded.files = match result {
                        Ok(files) => LoadState::Loaded(files),
                        Err(error) => LoadState::Failed(error.to_string().into()),
                    };
                    cx.notify();
                }
            })
            .log_err();
        });
        self.expanded.insert(
            number,
            ExpandedPullRequest {
                pull_request_id: pull_request.id.clone(),
                files: LoadState::Loading,
                _task: task,
            },
        );
        cx.notify();
    }

    fn set_file_viewed(&mut self, number: u64, path: String, viewed: bool, cx: &mut Context<Self>) {
        let Some(pull_request_id) = self
            .expanded
            .get(&number)
            .map(|expanded| expanded.pull_request_id.clone())
        else {
            return;
        };
        let context = match self.github_context(cx) {
            Ok(context) => context,
            Err(error) => {
                self.show_toast(error.to_string(), false, cx);
                return;
            }
        };
        self.update_viewed_state(number, &path, viewed, cx);
        let client = GithubClient::new(context.working_directory);
        cx.spawn(async move |this, cx| {
            let result = {
                let path = path.clone();
                cx.background_spawn(async move {
                    client
                        .set_file_viewed(&pull_request_id, &path, viewed)
                        .await
                })
                .await
            };
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.update_viewed_state(number, &path, !viewed, cx);
                    this.show_toast(format!("Could not update viewed state: {error}"), false, cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn update_viewed_state(
        &mut self,
        number: u64,
        path: &str,
        viewed: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(ExpandedPullRequest {
            files: LoadState::Loaded(files),
            ..
        }) = self.expanded.get_mut(&number)
            && let Some(file) = files.iter_mut().find(|file| file.path == path)
        {
            file.viewer_viewed_state = if viewed {
                ViewedState::Viewed
            } else {
                ViewedState::Unviewed
            };
            cx.notify();
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(Label::new("Pull Requests").size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("pull-request-checkout", IconName::GitBranch)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Checkout Pull Request by Number"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_number_prompt(NumberPromptMode::Checkout, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("pull-request-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
    }

    fn render_number_prompt(
        &self,
        mode: NumberPromptMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hint = match mode {
            NumberPromptMode::Open => "Open pull request number, press Enter",
            NumberPromptMode::Checkout => "Check out pull request number, press Enter",
        };
        v_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .on_action(cx.listener(Self::confirm_number_prompt))
            .on_action(cx.listener(Self::cancel_number_prompt))
            .child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted))
            .child(
                div()
                    .h_7()
                    .px_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.number_input.clone()),
            )
            .when_some(self.number_prompt_error.clone(), |this, error| {
                this.child(
                    Label::new(error)
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
    }

    fn render_section(
        &self,
        section_index: usize,
        section: &Section,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let kind = section.kind;
        let count = match &section.state {
            LoadState::Loaded(list) => Some(list.total_count),
            _ => None,
        };
        let mut container = v_flex().w_full().child(
            ListItem::new(("pull-request-section", section_index))
                .toggle(!section.collapsed)
                .on_toggle(cx.listener(move |this, _, _, cx| this.toggle_section(kind, cx)))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(kind, cx)))
                .child(Label::new(kind.title()).size(LabelSize::Small))
                .end_slot::<AnyElement>(count.map(|count| {
                    Label::new(count.to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .into_any_element()
                })),
        );
        if section.collapsed {
            return container;
        }
        match &section.state {
            LoadState::Idle | LoadState::Loading => {
                container = container.child(status_label("Loading...", Color::Muted));
            }
            LoadState::Failed(message) => {
                container = container.child(status_label(message.clone(), Color::Error));
            }
            LoadState::Loaded(list) if list.pull_requests.is_empty() => {
                container = container.child(status_label("No pull requests", Color::Muted));
            }
            LoadState::Loaded(list) => {
                for pull_request in &list.pull_requests {
                    container = container.child(self.render_pull_request(pull_request, cx));
                }
            }
        }
        container
    }

    fn render_pull_request(
        &self,
        pull_request: &PullRequestSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let number = pull_request.number;
        let expanded = self.expanded.get(&number);
        let toggle_target = pull_request.clone();
        let open_target = pull_request.clone();
        let github_url = pull_request.url.clone();
        let author = pull_request
            .author
            .as_ref()
            .map(|author| author.login.clone())
            .unwrap_or_default();

        v_flex()
            .w_full()
            .child(
                ListItem::new(("pull-request", number as usize))
                    .indent_level(1)
                    .toggle(expanded.is_some())
                    .on_toggle(cx.listener(move |this, _, _, cx| {
                        this.toggle_pull_request(&toggle_target, cx);
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_pull_request(
                            open_target.number,
                            open_target.url.clone(),
                            window,
                            cx,
                        );
                    }))
                    .tooltip(Tooltip::text(pull_request.title.clone()))
                    .child(
                        h_flex()
                            .gap_1()
                            .min_w_0()
                            .child(
                                Label::new(format!("#{number}"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(pull_request.title.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .when(pull_request.is_draft, |this| {
                                this.child(
                                    Label::new("Draft")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Warning),
                                )
                            }),
                    )
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(author)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new(
                                    ("pull-request-github", number as usize),
                                    IconName::ArrowUpRight,
                                )
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text("Open on GitHub"))
                                .on_click(move |_, _, cx| cx.open_url(&github_url)),
                            ),
                    ),
            )
            .when_some(expanded, |this, expanded| {
                this.child(self.render_files(number, expanded, cx))
            })
    }

    fn render_files(
        &self,
        number: u64,
        expanded: &ExpandedPullRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        match &expanded.files {
            LoadState::Idle | LoadState::Loading => {
                status_label("Loading files...", Color::Muted).into_any_element()
            }
            LoadState::Failed(message) => {
                status_label(message.clone(), Color::Error).into_any_element()
            }
            LoadState::Loaded(files) if files.is_empty() => {
                status_label("No changed files", Color::Muted).into_any_element()
            }
            LoadState::Loaded(files) => v_flex()
                .w_full()
                .children(files.iter().enumerate().map(|(index, file)| {
                    let path = file.path.clone();
                    let viewed = file.viewer_viewed_state == ViewedState::Viewed;
                    let (directory, file_name) = match file.path.rsplit_once('/') {
                        Some((directory, file_name)) => {
                            (format!("{directory}/"), file_name.to_string())
                        }
                        None => (String::new(), file.path.clone()),
                    };
                    ListItem::new(SharedString::from(format!(
                        "pull-request-file-{number}-{index}"
                    )))
                    .indent_level(2)
                    .start_slot(
                        Checkbox::new(
                            SharedString::from(format!("pull-request-viewed-{number}-{index}")),
                            viewed.into(),
                        )
                        .on_click(cx.listener(
                            move |this, state: &ToggleState, _, cx| {
                                this.set_file_viewed(number, path.clone(), state.selected(), cx);
                            },
                        )),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .min_w_0()
                            .child(
                                Label::new(file.change_type.letter())
                                    .size(LabelSize::XSmall)
                                    .color(change_type_color(file.change_type)),
                            )
                            .child(Label::new(file_name).size(LabelSize::Small).truncate())
                            .child(
                                Label::new(directory)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .end_slot(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(format!("+{}", file.additions))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Created),
                            )
                            .child(
                                Label::new(format!("-{}", file.deletions))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Deleted),
                            ),
                    )
                }))
                .into_any_element(),
        }
    }
}

fn status_label(message: impl Into<SharedString>, color: Color) -> impl IntoElement {
    div()
        .px_4()
        .py_1()
        .child(Label::new(message).size(LabelSize::Small).color(color))
}

fn change_type_color(change_type: FileChangeType) -> Color {
    match change_type {
        FileChangeType::Added | FileChangeType::Copied => Color::Created,
        FileChangeType::Deleted => Color::Deleted,
        _ => Color::Modified,
    }
}

impl Render for PullRequestPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sections = self
            .sections
            .iter()
            .enumerate()
            .map(|(index, section)| self.render_section(index, section, cx).into_any_element())
            .collect::<Vec<_>>();
        let number_prompt = self
            .number_prompt
            .map(|mode| self.render_number_prompt(mode, cx).into_any_element());

        v_flex()
            .id("pull-request-panel")
            .key_context("PullRequestPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .children(number_prompt)
            .child(
                v_flex()
                    .id("pull-request-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(sections),
            )
    }
}

impl Panel for PullRequestPanel {
    fn persistent_name() -> &'static str {
        "PullRequestPanel"
    }

    fn panel_key() -> &'static str {
        PULL_REQUEST_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Pull Requests")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(TogglePanel)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if active && !self.has_refreshed {
            self.refresh(cx);
        }
    }
}
