use std::collections::BTreeMap;
use std::ops::Range;

use collections::{HashMap, HashSet};
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Task, WeakEntity, Window, px, uniform_list,
};
use ui::{Chip, ListItem, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use super::TogglePullRequestPanel;
use super::github_api::{
    FileChangeType, GithubClient, GithubContext, GithubError,
    PullRequestOverview, PullRequestFile, ReviewThread,
};
use super::pr_ask_ai::{ai_enabled, ask_ai_about_pull_request};
use super::pr_overview::{
    PullRequestOverviewView, open_pull_request_changes_at, state_badge,
};
use super::pr_status::{BranchLookup, BranchPullRequestLookup};

const PULL_REQUEST_CHANGES_PANEL_KEY: &str = "PullRequestChangesPanel";

#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeNode {
    Directory {
        name: String,
        path: String,
        children: Vec<TreeNode>,
    },
    File {
        name: String,
        index: usize,
    },
}

#[derive(Default)]
struct DirectoryBuilder {
    directories: BTreeMap<String, DirectoryBuilder>,
    files: Vec<(String, usize)>,
}

impl DirectoryBuilder {
    fn into_nodes(self, parent_path: &str) -> Vec<TreeNode> {
        let mut directories = Vec::new();
        for (mut name, mut builder) in self.directories {
            while builder.files.is_empty() && builder.directories.len() == 1 {
                let Some((child_name, child)) = builder.directories.pop_first() else {
                    break;
                };
                name = format!("{name}/{child_name}");
                builder = child;
            }
            let path = if parent_path.is_empty() {
                name.clone()
            } else {
                format!("{parent_path}/{name}")
            };
            let children = builder.into_nodes(&path);
            directories.push(TreeNode::Directory {
                name,
                path,
                children,
            });
        }
        directories.sort_by_cached_key(|node| match node {
            TreeNode::Directory { name, .. } | TreeNode::File { name, .. } => name.to_lowercase(),
        });

        let mut files = self.files;
        files.sort_by_cached_key(|(name, _)| name.to_lowercase());
        directories.extend(
            files
                .into_iter()
                .map(|(name, index)| TreeNode::File { name, index }),
        );
        directories
    }
}

fn build_file_tree<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<TreeNode> {
    let mut root = DirectoryBuilder::default();
    for (index, path) in paths.into_iter().enumerate() {
        let mut components = path.split('/').filter(|component| !component.is_empty());
        let Some(mut file_name) = components.next() else {
            continue;
        };
        let mut directory = &mut root;
        for component in components {
            directory = directory
                .directories
                .entry(file_name.to_string())
                .or_default();
            file_name = component;
        }
        directory.files.push((file_name.to_string(), index));
    }
    root.into_nodes("")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VisibleRow {
    Directory {
        name: String,
        path: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        name: String,
        index: usize,
        depth: usize,
    },
}

fn flatten_tree(
    nodes: &[TreeNode],
    collapsed: &HashSet<String>,
    depth: usize,
    rows: &mut Vec<VisibleRow>,
) {
    for node in nodes {
        match node {
            TreeNode::Directory {
                name,
                path,
                children,
            } => {
                let is_collapsed = collapsed.contains(path);
                rows.push(VisibleRow::Directory {
                    name: name.clone(),
                    path: path.clone(),
                    depth,
                    collapsed: is_collapsed,
                });
                if !is_collapsed {
                    flatten_tree(children, collapsed, depth + 1, rows);
                }
            }
            TreeNode::File { name, index } => rows.push(VisibleRow::File {
                name: name.clone(),
                index: *index,
                depth,
            }),
        }
    }
}

struct LoadedPullRequest {
    context: GithubContext,
    overview: PullRequestOverview,
    files: Vec<PullRequestFile>,
    files_error: Option<SharedString>,
    thread_counts: HashMap<String, usize>,
    tree: Vec<TreeNode>,
}

impl LoadedPullRequest {
    fn new(
        context: GithubContext,
        overview: PullRequestOverview,
        files: Result<Vec<PullRequestFile>, GithubError>,
        threads: &[ReviewThread],
    ) -> Self {
        let (files, files_error) = match files {
            Ok(files) => (files, None),
            Err(error) => (Vec::new(), Some(error.to_string().into())),
        };
        let mut thread_counts: HashMap<String, usize> = HashMap::default();
        for thread in threads {
            *thread_counts.entry(thread.path.clone()).or_default() += 1;
        }
        let tree = build_file_tree(files.iter().map(|file| file.path.as_str()));
        Self {
            context,
            overview,
            files,
            files_error,
            thread_counts,
            tree,
        }
    }
}

enum Content {
    Message { text: SharedString, color: Color },
    Loading,
    Loaded(Box<LoadedPullRequest>),
}

pub struct PullRequestChangesPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<project::Project>,
    lookup: Entity<BranchPullRequestLookup>,
    focus_handle: FocusHandle,
    position: DockPosition,
    content: Content,
    loaded_for: Option<(super::github_api::GithubRepository, u64)>,
    collapsed_directories: HashSet<String>,
    visible_rows: Vec<VisibleRow>,
    load_task: Task<()>,
    _subscription: Subscription,
}

impl EventEmitter<PanelEvent> for PullRequestChangesPanel {}

impl Focusable for PullRequestChangesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PullRequestChangesPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _, cx| Self::new(workspace, cx))
    }

    fn new(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = workspace.weak_handle();
        let lookup = BranchPullRequestLookup::shared(&project, cx);
        cx.new(|cx| {
            let subscription = cx.observe(&lookup, |this: &mut Self, _, cx| this.sync(cx));
            let mut this = Self {
                workspace: workspace_handle,
                project,
                lookup,
                focus_handle: cx.focus_handle(),
                position: DockPosition::Left,
                content: Content::Loading,
                loaded_for: None,
                collapsed_directories: HashSet::default(),
                visible_rows: Vec::new(),
                load_task: Task::ready(()),
                _subscription: subscription,
            };
            this.sync(cx);
            this
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_fixture(
        workspace: &mut Workspace,
        context: GithubContext,
        overview: PullRequestOverview,
        files: Vec<PullRequestFile>,
        threads: Vec<ReviewThread>,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let panel = Self::new(workspace, cx);
        panel.update(cx, |panel, cx| {
            panel.lookup.update(cx, |lookup, cx| {
                lookup.set_fixture(
                    BranchLookup::Found(super::github_api::BranchPullRequest {
                        number: overview.number,
                        url: overview.url.clone(),
                        state: overview.state,
                    }),
                    cx,
                )
            });
            panel.loaded_for = Some((context.repository.clone(), overview.number));
            panel.load_task = Task::ready(());
            let loaded = LoadedPullRequest::new(context, overview, Ok(files), &threads);
            panel.set_loaded(loaded, cx);
        });
        panel
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let state = self.lookup.read(cx).state().clone();
        let active_repository = self.project.read(cx).active_repository(cx);
        let fallback_context = active_repository
            .as_ref()
            .map(|repository| GithubContext::from_repository(repository.read(cx), cx));

        match state {
            BranchLookup::Found(pull_request) => {
                let Some(repository) = pull_request.repository().or_else(|| {
                    fallback_context
                        .as_ref()
                        .and_then(|context| context.as_ref().ok())
                        .map(|context| context.repository.clone())
                }) else {
                    self.set_message(GithubError::NotGithubRepository.to_string(), Color::Muted);
                    return;
                };
                let Some(working_directory) = active_repository
                    .map(|repository| repository.read(cx).work_directory_abs_path.to_path_buf())
                else {
                    self.set_message("No active repository".into(), Color::Muted);
                    return;
                };
                let key = (repository.clone(), pull_request.number);
                if self.loaded_for.as_ref() == Some(&key) {
                    return;
                }
                self.load_pull_request(
                    GithubContext {
                        repository,
                        working_directory,
                    },
                    pull_request.number,
                    cx,
                );
            }
            BranchLookup::Loading => {
                self.loaded_for = None;
                self.load_task = Task::ready(());
                self.content = Content::Loading;
                cx.notify();
            }
            other => {
                let message = match (fallback_context, other) {
                    (None, _) => "No active repository".to_string(),
                    (Some(Err(error)), _) => error.to_string(),
                    (Some(Ok(_)), BranchLookup::NoPullRequest { branch }) => {
                        format!("No pull request for branch {branch}")
                    }
                    (Some(Ok(_)), BranchLookup::Failed(message)) => {
                        self.set_message(message, Color::Error);
                        return;
                    }
                    (Some(Ok(_)), _) => "No branch checked out".to_string(),
                };
                self.set_message(message, Color::Muted);
            }
        }
    }

    fn set_message(&mut self, text: String, color: Color) {
        self.loaded_for = None;
        self.load_task = Task::ready(());
        self.visible_rows.clear();
        self.content = Content::Message {
            text: text.into(),
            color,
        };
    }

    fn load_pull_request(&mut self, context: GithubContext, number: u64, cx: &mut Context<Self>) {
        self.loaded_for = Some((context.repository.clone(), number));
        self.content = Content::Loading;
        self.visible_rows.clear();
        cx.notify();

        let client = GithubClient::new(context.working_directory.clone());
        let repository = context.repository.clone();
        self.load_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let (overview, files, threads) = futures::join!(
                        client.fetch_overview(&repository, number),
                        client.fetch_files(&repository, number),
                        client.fetch_review_threads(&repository, number),
                    );
                    let threads = threads.log_err().unwrap_or_default();
                    overview.map(|overview| (overview, files, threads))
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((overview, files, threads)) => {
                        let loaded = LoadedPullRequest::new(context, overview, files, &threads);
                        this.set_loaded(loaded, cx);
                    }
                    Err(error) => this.set_message(error.to_string(), Color::Error),
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn set_loaded(&mut self, loaded: LoadedPullRequest, cx: &mut Context<Self>) {
        self.collapsed_directories.clear();
        self.content = Content::Loaded(Box::new(loaded));
        self.rebuild_rows();
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.visible_rows.clear();
        if let Content::Loaded(loaded) = &self.content {
            flatten_tree(
                &loaded.tree,
                &self.collapsed_directories,
                0,
                &mut self.visible_rows,
            );
        }
    }

    fn toggle_directory(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed_directories.remove(path) {
            self.collapsed_directories.insert(path.to_string());
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loaded_for = None;
        self.lookup.update(cx, |lookup, cx| lookup.reload(cx));
    }

    fn open_description(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Loaded(loaded) = &self.content else {
            return;
        };
        let context = loaded.context.clone();
        let number = loaded.overview.number;
        self.workspace
            .update(cx, |workspace, cx| {
                PullRequestOverviewView::open_with_context(workspace, context, number, window, cx);
            })
            .log_err();
    }

    fn ask_ai(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Loaded(loaded) = &self.content else {
            return;
        };
        ask_ai_about_pull_request(
            self.workspace.clone(),
            loaded.context.clone(),
            loaded.overview.number,
            window,
            cx,
        );
    }

    fn open_file(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Content::Loaded(loaded) = &self.content else {
            return;
        };
        let Some(file) = loaded.files.get(index) else {
            return;
        };
        let path = file.path.clone();
        let context = loaded.context.clone();
        let number = loaded.overview.number;
        self.workspace
            .update(cx, |workspace, cx| {
                open_pull_request_changes_at(workspace, context, number, Some(path), window, cx);
            })
            .log_err();
    }

    fn render_header(&self, loaded: &LoadedPullRequest, cx: &mut Context<Self>) -> impl IntoElement {
        let overview = &loaded.overview;
        let (state_label, state_color) = state_badge(overview);
        let title = SharedString::from(overview.title.clone());
        v_flex()
            .w_full()
            .gap_1p5()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .id("pull-request-panel-title")
                    .w_full()
                    .gap_1()
                    .tooltip(Tooltip::text(title.clone()))
                    .child(
                        Label::new(format!("#{}", overview.number))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div().min_w_0().flex_1().child(
                            Label::new(title)
                                .size(LabelSize::Small)
                                .weight(gpui::FontWeight::SEMIBOLD)
                                .truncate(),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Chip::new(state_label)
                            .icon(IconName::PullRequest)
                            .icon_color(state_color)
                            .label_color(state_color),
                    )
                    .child(
                        div().min_w_0().flex_1().child(
                            Label::new(format!(
                                "{} \u{2192} {}",
                                overview.head_ref_name, overview.base_ref_name
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Button::new("pull-request-panel-description", "Description")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::FileTextOutlined).size(IconSize::Small))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_description(window, cx)
                            })),
                    )
                    .when(ai_enabled(cx), |this| {
                        this.child(
                            Button::new("pull-request-panel-ask-ai", "Ask AI")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::ZedAssistant).size(IconSize::Small))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.ask_ai(window, cx)
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        IconButton::new("pull-request-panel-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
    }

    fn render_changes_header(&self, loaded: &LoadedPullRequest) -> impl IntoElement {
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .child(Label::new("Changes").size(LabelSize::Small))
            .child(
                Label::new(loaded.files.len().to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Content::Loaded(loaded) = &self.content else {
            return Vec::new();
        };
        let mut elements = Vec::with_capacity(range.len());
        for row_index in range {
            let Some(row) = self.visible_rows.get(row_index) else {
                continue;
            };
            let element = match row {
                VisibleRow::Directory {
                    name,
                    path,
                    depth,
                    collapsed,
                } => {
                    let toggle_path = path.clone();
                    ListItem::new(("pull-request-directory", row_index))
                        .indent_level(*depth)
                        .toggle(!*collapsed)
                        .always_show_disclosure_icon(true)
                        .on_toggle(cx.listener({
                            let toggle_path = toggle_path.clone();
                            move |this, _, _, cx| this.toggle_directory(&toggle_path, cx)
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_directory(&toggle_path, cx)
                        }))
                        .child(
                            Label::new(format!("{name}/"))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                        .into_any_element()
                }
                VisibleRow::File { name, index, depth } => {
                    let Some(file) = loaded.files.get(*index) else {
                        continue;
                    };
                    let file_index = *index;
                    let directory = file
                        .path
                        .rsplit_once('/')
                        .map(|(directory, _)| format!("{directory}/"))
                        .unwrap_or_default();
                    let comment_count = loaded.thread_counts.get(&file.path).copied();
                    ListItem::new(("pull-request-file", row_index))
                        .indent_level(*depth)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_file(file_index, window, cx)
                        }))
                        .tooltip(Tooltip::text(file.path.clone()))
                        .child(
                            h_flex()
                                .gap_1()
                                .min_w_0()
                                .child(
                                    Label::new(file.change_type.letter())
                                        .size(LabelSize::XSmall)
                                        .color(change_type_color(file.change_type)),
                                )
                                .child(Label::new(name.clone()).size(LabelSize::Small).truncate())
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
                                .when_some(comment_count, |this, count| {
                                    this.child(
                                        h_flex()
                                            .gap_0p5()
                                            .child(
                                                Icon::new(IconName::Chat)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Accent),
                                            )
                                            .child(
                                                Label::new(count.to_string())
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Accent),
                                            ),
                                    )
                                })
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
                        .into_any_element()
                }
            };
            elements.push(element);
        }
        elements
    }
}

fn change_type_color(change_type: FileChangeType) -> Color {
    match change_type {
        FileChangeType::Added | FileChangeType::Copied => Color::Created,
        FileChangeType::Deleted => Color::Deleted,
        _ => Color::Modified,
    }
}

fn centered_message(text: impl Into<SharedString>, color: Color) -> impl IntoElement {
    div()
        .w_full()
        .px_3()
        .py_2()
        .child(Label::new(text).size(LabelSize::Small).color(color))
}

impl Render for PullRequestChangesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let container = v_flex()
            .id("pull-request-changes-panel")
            .key_context("PullRequestChangesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background);
        match &self.content {
            Content::Message { text, color } => {
                container.child(centered_message(text.clone(), *color))
            }
            Content::Loading => container.child(centered_message("Loading...", Color::Muted)),
            Content::Loaded(loaded) => {
                let header = self.render_header(loaded, cx);
                let changes_header = self.render_changes_header(loaded);
                let files_error = loaded.files_error.clone();
                let has_files = !loaded.files.is_empty();
                container
                    .child(header)
                    .child(changes_header)
                    .when_some(files_error, |this, error| {
                        this.child(centered_message(error, Color::Error))
                    })
                    .when(!has_files && loaded.files_error.is_none(), |this| {
                        this.child(centered_message("No changed files", Color::Muted))
                    })
                    .when(has_files, |this| {
                        this.child(
                            uniform_list(
                                "pull-request-changed-files",
                                self.visible_rows.len(),
                                cx.processor(Self::render_rows),
                            )
                            .flex_1()
                            .min_h_0(),
                        )
                    })
            }
        }
    }
}

impl Panel for PullRequestChangesPanel {
    fn persistent_name() -> &'static str {
        "PullRequestChangesPanel"
    }

    fn panel_key() -> &'static str {
        PULL_REQUEST_CHANGES_PANEL_KEY
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
        Some("Pull Request")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(TogglePullRequestPanel)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str, path: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode::Directory {
            name: name.into(),
            path: path.into(),
            children,
        }
    }

    fn file(name: &str, index: usize) -> TreeNode {
        TreeNode::File {
            name: name.into(),
            index,
        }
    }

    #[test]
    fn tree_nests_compacts_single_child_chains_and_sorts() {
        let tree = build_file_tree([
            "README.md",
            "src/client/retry.rs",
            "src/client/backoff.rs",
            "src/lib.rs",
            "docs/guide/intro/setup.md",
            "Cargo.toml",
        ]);
        assert_eq!(
            tree,
            vec![
                directory(
                    "docs/guide/intro",
                    "docs/guide/intro",
                    vec![file("setup.md", 4)]
                ),
                directory(
                    "src",
                    "src",
                    vec![
                        directory(
                            "client",
                            "src/client",
                            vec![file("backoff.rs", 2), file("retry.rs", 1)]
                        ),
                        file("lib.rs", 3),
                    ]
                ),
                file("Cargo.toml", 5),
                file("README.md", 0),
            ]
        );
    }

    #[test]
    fn collapsed_directories_hide_their_rows() {
        let tree = build_file_tree(["src/client/retry.rs", "src/lib.rs"]);
        let mut collapsed = HashSet::default();
        collapsed.insert("src/client".to_string());
        let mut rows = Vec::new();
        flatten_tree(&tree, &collapsed, 0, &mut rows);
        assert_eq!(
            rows,
            vec![
                VisibleRow::Directory {
                    name: "src".into(),
                    path: "src".into(),
                    depth: 0,
                    collapsed: false
                },
                VisibleRow::Directory {
                    name: "client".into(),
                    path: "src/client".into(),
                    depth: 1,
                    collapsed: true
                },
                VisibleRow::File {
                    name: "lib.rs".into(),
                    index: 1,
                    depth: 1
                },
            ]
        );
    }
}
