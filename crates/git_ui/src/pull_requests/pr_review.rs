use std::sync::Arc;
use std::time::Duration;

use collections::{HashMap, HashSet};
use editor::{
    DiffReviewHandler, DiffReviewSubmission, Editor, SplittableEditor,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    hover_markdown_style,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, EventEmitter, FontWeight,
    IntoElement, PromptLevel, SharedString, Subscription, Task, WeakEntity, Window,
};
use language::{BufferId, Point};
use markdown::{Markdown, MarkdownElement};
use multi_buffer::{Anchor, MultiBufferSnapshot, PathKey, ToPoint as _};
use project::{Project, ProjectPath, git_store::Repository};
use time::OffsetDateTime;
use ui::{Chip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use super::github_api::{
    DiffSide, GithubClient, GithubContext, NewReviewThread, ReviewComment, ReviewEvent,
    ReviewThread,
};
use super::pr_ask_ai::{ai_enabled, ask_ai_about_pull_request};
use super::pr_overview::{PullRequestOverviewView, avatar, format_relative_time, show_toast};
use crate::branch_diff::BranchDiff;

const PENDING_STATE: &str = "PENDING";
const REBUILD_DEBOUNCE: Duration = Duration::from_millis(150);
const CHARACTERS_PER_ROW: usize = 80;
const REPLY_EDITOR_ROWS: u32 = 2;
const CARD_CHROME_ROWS: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewPosition {
    pub line: u32,
    pub side: DiffSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewTarget {
    pub line: u32,
    pub side: DiffSide,
    pub start_line: Option<u32>,
    pub start_side: Option<DiffSide>,
}

pub(crate) fn review_target(start: ReviewPosition, end: ReviewPosition) -> ReviewTarget {
    if start == end {
        return ReviewTarget {
            line: end.line,
            side: end.side,
            start_line: None,
            start_side: None,
        };
    }
    ReviewTarget {
        line: end.line,
        side: end.side,
        start_line: Some(start.line),
        start_side: Some(start.side),
    }
}

pub(crate) fn review_position(
    snapshot: &MultiBufferSnapshot,
    row: u32,
    in_base_editor: bool,
) -> Option<ReviewPosition> {
    let (buffer, buffer_point) = snapshot.point_to_buffer_point(Point::new(row, 0))?;
    let in_deleted_hunk = snapshot.buffer_for_id(buffer.remote_id()).is_none();
    Some(ReviewPosition {
        line: buffer_point.row + 1,
        side: if in_base_editor || in_deleted_hunk {
            DiffSide::Left
        } else {
            DiffSide::Right
        },
    })
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ThreadPlacement {
    pub in_base_editor: bool,
    pub anchor: Anchor,
}

fn clamp_row(row: u32, buffer: &language::BufferSnapshot) -> Option<u32> {
    (row <= buffer.max_point().row).then_some(row)
}

fn end_of_row_anchor(
    snapshot: &MultiBufferSnapshot,
    buffer: &language::BufferSnapshot,
    row: u32,
) -> Option<Anchor> {
    let text_anchor = buffer.anchor_after(Point::new(row, buffer.line_len(row)));
    snapshot.anchor_in_excerpt(text_anchor)
}

fn start_of_file_anchor(snapshot: &MultiBufferSnapshot, buffer_id: BufferId) -> Option<Anchor> {
    snapshot.anchor_in_buffer(language::Anchor::min_for_buffer(buffer_id))
}

pub(crate) fn place_thread(
    thread: &ReviewThread,
    snapshot: &MultiBufferSnapshot,
    buffer_id: BufferId,
    base_snapshot: Option<(&MultiBufferSnapshot, BufferId)>,
) -> Option<ThreadPlacement> {
    let buffer = snapshot.buffer_for_id(buffer_id)?;
    let side = thread.diff_side.unwrap_or(DiffSide::Right);
    let line = thread.line.or(thread.original_line);
    let requested_row = line.and_then(|line| line.checked_sub(1));

    let fallback = || {
        Some(ThreadPlacement {
            in_base_editor: false,
            anchor: start_of_file_anchor(snapshot, buffer_id)?,
        })
    };

    let Some(requested_row) = requested_row else {
        return fallback();
    };

    if side == DiffSide::Left {
        if let Some((base_snapshot, base_id)) = base_snapshot {
            let base_buffer = base_snapshot.buffer_for_id(base_id)?;
            let placed = clamp_row(requested_row, base_buffer)
                .and_then(|row| end_of_row_anchor(base_snapshot, base_buffer, row));
            return Some(match placed {
                Some(anchor) => ThreadPlacement {
                    in_base_editor: true,
                    anchor,
                },
                None => ThreadPlacement {
                    in_base_editor: true,
                    anchor: start_of_file_anchor(base_snapshot, base_id)?,
                },
            });
        }

        let diff = snapshot.diff_for_buffer_id(buffer_id)?;
        let base_text = diff.base_text();
        if let Some(base_row) = clamp_row(requested_row, base_text) {
            let base_start = base_text.point_to_offset(Point::new(base_row, 0));
            let in_deleted_hunk = diff
                .hunks_intersecting_range(
                    language::Anchor::min_max_range_for_buffer(buffer_id),
                    buffer,
                )
                .any(|hunk| {
                    hunk.diff_base_byte_range.start <= base_start
                        && base_start < hunk.diff_base_byte_range.end
                });
            let buffer_point =
                diff.base_text_point_to_buffer_point(Point::new(base_row, 0), buffer);
            if in_deleted_hunk {
                let base_anchor =
                    base_text.anchor_after(Point::new(base_row, base_text.line_len(base_row)));
                let anchor = snapshot
                    .anchor_in_excerpt(buffer.anchor_before(buffer_point))
                    .map(|anchor| anchor.with_diff_base_anchor(base_anchor));
                if let Some(anchor) = anchor {
                    return Some(ThreadPlacement {
                        in_base_editor: false,
                        anchor,
                    });
                }
            } else if let Some(anchor) = clamp_row(buffer_point.row, buffer)
                .and_then(|row| end_of_row_anchor(snapshot, buffer, row))
            {
                return Some(ThreadPlacement {
                    in_base_editor: false,
                    anchor,
                });
            }
        }
        return fallback();
    }

    match clamp_row(requested_row, buffer).and_then(|row| end_of_row_anchor(snapshot, buffer, row))
    {
        Some(anchor) => Some(ThreadPlacement {
            in_base_editor: false,
            anchor,
        }),
        None => fallback(),
    }
}

fn estimate_body_rows(body: &str) -> u32 {
    body.lines()
        .map(|line| line.chars().count().div_ceil(CHARACTERS_PER_ROW).max(1) as u32)
        .sum::<u32>()
        .max(1)
}

fn estimate_thread_rows(thread: &ReviewThread, expanded: bool) -> u32 {
    if !expanded {
        return 2;
    }
    let comment_rows: u32 = thread
        .comments
        .iter()
        .map(|comment| 1 + estimate_body_rows(&comment.body))
        .sum();
    let reply_rows = if thread.viewer_can_reply {
        REPLY_EDITOR_ROWS
    } else {
        0
    };
    1 + comment_rows + reply_rows + CARD_CHROME_ROWS
}

enum ThreadViewEvent {
    HeightChanged,
}

struct ThreadView {
    session: WeakEntity<PullRequestReviewSession>,
    thread: ReviewThread,
    bodies: Vec<Entity<Markdown>>,
    reply_editor: Entity<Editor>,
    expanded: bool,
    busy: bool,
    error: Option<SharedString>,
    fixed_now: Option<OffsetDateTime>,
}

impl EventEmitter<ThreadViewEvent> for ThreadView {}

impl ThreadView {
    fn new(
        session: WeakEntity<PullRequestReviewSession>,
        thread: ReviewThread,
        fixed_now: Option<OffsetDateTime>,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let reply_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(
                REPLY_EDITOR_ROWS as usize,
                REPLY_EDITOR_ROWS as usize,
                window,
                cx,
            );
            editor.set_placeholder_text("Reply...", window, cx);
            editor
        });
        let mut this = Self {
            session,
            expanded: !thread.is_resolved,
            bodies: Vec::new(),
            thread,
            reply_editor,
            busy: false,
            error: None,
            fixed_now,
        };
        this.rebuild_bodies(project, cx);
        this
    }

    fn rebuild_bodies(&mut self, project: &Entity<Project>, cx: &mut Context<Self>) {
        let language_registry = project.read(cx).languages().clone();
        self.bodies = self
            .thread
            .comments
            .iter()
            .map(|comment| {
                let source = SharedString::from(comment.body.clone());
                let language_registry = language_registry.clone();
                cx.new(|cx| Markdown::new(source, Some(language_registry), None, cx))
            })
            .collect();
    }

    fn update_thread(
        &mut self,
        thread: ReviewThread,
        project: &Entity<Project>,
        cx: &mut Context<Self>,
    ) {
        if thread == self.thread {
            return;
        }
        if thread.is_resolved != self.thread.is_resolved {
            self.expanded = !thread.is_resolved;
        }
        let height_before = self.rows();
        self.thread = thread;
        self.rebuild_bodies(project, cx);
        if self.rows() != height_before {
            cx.emit(ThreadViewEvent::HeightChanged);
        }
        cx.notify();
    }

    fn rows(&self) -> u32 {
        estimate_thread_rows(&self.thread, self.expanded)
    }

    fn set_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        if self.expanded != expanded {
            self.expanded = expanded;
            cx.emit(ThreadViewEvent::HeightChanged);
            cx.notify();
        }
    }

    fn pending_comment_count(&self) -> usize {
        self.thread
            .comments
            .iter()
            .filter(|comment| comment.state == PENDING_STATE)
            .count()
    }

    fn finish_action(
        &mut self,
        result: Result<(), String>,
        clear_reply: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        match result {
            Ok(()) => {
                self.error = None;
                if clear_reply {
                    self.reply_editor
                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                }
            }
            Err(message) => self.error = Some(message.into()),
        }
        cx.notify();
    }

    fn reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.reply_editor.read(cx).text(cx).trim().to_string();
        if body.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let thread_id = self.thread.id.clone();
        self.session
            .update(cx, |session, cx| session.reply(thread_id, body, window, cx))
            .log_err();
        cx.notify();
    }

    fn toggle_resolved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let thread_id = self.thread.id.clone();
        let resolved = !self.thread.is_resolved;
        self.session
            .update(cx, |session, cx| {
                session.set_resolved(thread_id, resolved, window, cx)
            })
            .log_err();
        cx.notify();
    }

    fn render_comment(
        &self,
        index: usize,
        comment: &ReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let login = comment
            .author
            .as_ref()
            .map(|author| author.login.clone())
            .unwrap_or_else(|| "ghost".to_string());
        v_flex()
            .w_full()
            .when(index > 0, |this| {
                this.mt_1()
                    .pt_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(avatar(comment.author.as_ref()))
                    .child(Label::new(login).weight(FontWeight::SEMIBOLD))
                    .child(
                        Label::new(format_relative_time(
                            &comment.created_at,
                            self.fixed_now.unwrap_or_else(OffsetDateTime::now_utc),
                        ))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                    )
                    .when(comment.state == PENDING_STATE, |this| {
                        this.child(Chip::new("Pending").label_color(Color::Warning))
                    }),
            )
            .children(self.bodies.get(index).map(|body| {
                div().text_sm().child(MarkdownElement::new(
                    body.clone(),
                    hover_markdown_style(window, cx),
                ))
            }))
            .into_any_element()
    }
}

impl Render for ThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let thread = self.thread.clone();
        let outdated = thread.is_outdated || thread.line.is_none();
        let comment_count = thread.comments.len();

        if !self.expanded {
            return div()
                .id(SharedString::from(format!("review-thread-{}", thread.id)))
                .size_full()
                .px_3()
                .py_1()
                .child(
                    h_flex()
                        .id(SharedString::from(format!("expand-{}", thread.id)))
                        .size_full()
                        .px_2()
                        .gap_2()
                        .items_center()
                        .cursor_pointer()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.elevated_surface_background)
                        .hover(|style| style.bg(colors.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| this.set_expanded(true, cx)))
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(format!(
                                "Resolved · {comment_count} {}",
                                if comment_count == 1 {
                                    "comment"
                                } else {
                                    "comments"
                                }
                            ))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                        ),
                )
                .into_any_element();
        }

        let location = thread
            .line
            .or(thread.original_line)
            .map(|line| format!("{}:{line}", thread.path))
            .unwrap_or_default();
        let can_resolve = thread.viewer_can_resolve;
        let can_reply = thread.viewer_can_reply;
        let busy = self.busy;
        let resolved = thread.is_resolved;

        div()
            .id(SharedString::from(format!("review-thread-{}", thread.id)))
            .size_full()
            .px_3()
            .py_1()
            .child(
                v_flex()
                    .size_full()
                    .overflow_hidden()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .when(resolved, |this| {
                                this.child(
                                    IconButton::new(
                                        SharedString::from(format!("collapse-{}", thread.id)),
                                        IconName::ChevronDown,
                                    )
                                    .icon_size(IconSize::Small)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.set_expanded(false, cx)),
                                    ),
                                )
                            })
                            .when(!location.is_empty(), |this| {
                                this.child(
                                    Label::new(location)
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                )
                            })
                            .when(outdated, |this| {
                                this.child(Chip::new("Outdated").label_color(Color::Warning))
                            })
                            .when(resolved, |this| {
                                this.child(Chip::new("Resolved").label_color(Color::Success))
                            })
                            .when(self.pending_comment_count() > 0, |this| {
                                this.child(Chip::new("Pending").label_color(Color::Warning))
                            })
                            .child(div().flex_1())
                            .when_some(self.error.clone(), |this, error| {
                                this.child(
                                    Label::new(error).color(Color::Error).size(LabelSize::Small),
                                )
                            })
                            .when(can_resolve, |this| {
                                this.child(
                                    Button::new(
                                        SharedString::from(format!("resolve-{}", thread.id)),
                                        if resolved { "Unresolve" } else { "Resolve" },
                                    )
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .disabled(busy)
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.toggle_resolved(window, cx),
                                    )),
                                )
                            }),
                    )
                    .children(
                        thread
                            .comments
                            .iter()
                            .enumerate()
                            .map(|(index, comment)| self.render_comment(index, comment, window, cx))
                            .collect::<Vec<_>>(),
                    )
                    .when(can_reply, |this| {
                        this.child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .items_start()
                                .child(
                                    div()
                                        .flex_1()
                                        .px_2()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(colors.border)
                                        .child(self.reply_editor.clone()),
                                )
                                .child(
                                    Button::new(
                                        SharedString::from(format!("reply-{}", thread.id)),
                                        "Reply",
                                    )
                                    .style(ButtonStyle::Filled)
                                    .disabled(busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.reply(window, cx)),
                                    ),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }
}

struct PlacedBlock {
    thread_id: String,
    editor: WeakEntity<Editor>,
    block_id: CustomBlockId,
}

pub struct PullRequestReviewParams {
    pub workspace: WeakEntity<Workspace>,
    pub project: Entity<Project>,
    pub repository: Entity<Repository>,
    pub context: GithubContext,
    pub number: u64,
    pub title: SharedString,
    pub pull_request_id: String,
    pub head_oid: String,
}

pub struct PullRequestReviewSession {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    git_repository: Entity<Repository>,
    context: GithubContext,
    number: u64,
    title: SharedString,
    pull_request_id: String,
    head_oid: String,
    splittable: Entity<SplittableEditor>,
    threads: Vec<ReviewThread>,
    pending_review_id: Option<String>,
    fixed_now: Option<OffsetDateTime>,
    thread_views: HashMap<String, Entity<ThreadView>>,
    blocks: Vec<PlacedBlock>,
    observed_base_editor: Option<EntityId>,
    refresh_task: Task<()>,
    rebuild_task: Task<()>,
    multibuffer_subscriptions: Vec<Subscription>,
    _splittable_subscription: Subscription,
}

#[cfg(any(test, feature = "test-support"))]
pub fn open_review_with_fixture(
    workspace: &mut Workspace,
    repository: Entity<Repository>,
    base_ref: SharedString,
    context: GithubContext,
    threads: Vec<ReviewThread>,
    pending_review_id: Option<String>,
    now: OffsetDateTime,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let params = PullRequestReviewParams {
        workspace: workspace.weak_handle(),
        project: project.clone(),
        repository: repository.clone(),
        context,
        number: 412,
        title: "Retry failed requests with exponential backoff".into(),
        pull_request_id: "fixture".into(),
        head_oid: "fixture".into(),
    };
    BranchDiff::deploy_branch_diff_with_base_ref_then(
        workspace,
        project,
        repository,
        base_ref,
        None,
        window,
        cx,
        move |branch_diff, window, cx| {
            PullRequestReviewSession::attach_with_fixture(
                &branch_diff,
                params,
                threads,
                pending_review_id,
                now,
                window,
                cx,
            );
        },
    );
}

struct SessionReviewHandler {
    session: WeakEntity<PullRequestReviewSession>,
}

impl DiffReviewHandler for SessionReviewHandler {
    fn button_labels(&self, cx: &App) -> Vec<SharedString> {
        let has_pending_review = self
            .session
            .read_with(cx, |session, _| session.pending_review_id.is_some())
            .unwrap_or(false);
        vec![
            if has_pending_review {
                "Add Review Comment"
            } else {
                "Start Review"
            }
            .into(),
            "Add Comment".into(),
        ]
    }

    fn submit(&self, submission: DiffReviewSubmission, window: &mut Window, cx: &mut App) {
        self.session
            .update(cx, |session, cx| {
                session.submit_comment(submission, window, cx)
            })
            .log_err();
    }
}

impl PullRequestReviewSession {
    pub fn attach(
        branch_diff: &Entity<BranchDiff>,
        params: PullRequestReviewParams,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(existing) = branch_diff.read(cx).pull_request_review().cloned() {
            existing.update(cx, |session, cx| {
                session.head_oid = params.head_oid;
                session.refresh(window, cx);
            });
            return;
        }
        let splittable = branch_diff.read(cx).editor(cx);
        let session = cx.new(|cx| {
            let mut session = Self::new(params, splittable, cx);
            session.refresh(window, cx);
            session
        });
        branch_diff.update(cx, |branch_diff, cx| {
            branch_diff.set_pull_request_review(session, cx);
        });
    }

    fn new(
        params: PullRequestReviewParams,
        splittable: Entity<SplittableEditor>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&splittable, |this, _, cx| this.sync_editors(cx));
        let mut this = Self {
            workspace: params.workspace,
            project: params.project,
            git_repository: params.repository,
            context: params.context,
            number: params.number,
            title: params.title,
            pull_request_id: params.pull_request_id,
            head_oid: params.head_oid,
            splittable,
            threads: Vec::new(),
            pending_review_id: None,
            fixed_now: None,
            thread_views: HashMap::default(),
            blocks: Vec::new(),
            observed_base_editor: None,
            refresh_task: Task::ready(()),
            rebuild_task: Task::ready(()),
            multibuffer_subscriptions: Vec::new(),
            _splittable_subscription: subscription,
        };
        this.sync_editors(cx);
        this
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn attach_with_fixture(
        branch_diff: &Entity<BranchDiff>,
        params: PullRequestReviewParams,
        threads: Vec<ReviewThread>,
        pending_review_id: Option<String>,
        now: OffsetDateTime,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let splittable = branch_diff.read(cx).editor(cx);
        let session = cx.new(|cx| {
            let mut session = Self::new(params, splittable, cx);
            session.fixed_now = Some(now);
            session.apply(threads, pending_review_id, window, cx);
            session
        });
        branch_diff.update(cx, |branch_diff, cx| {
            branch_diff.set_pull_request_review(session, cx);
        });
    }

    pub(crate) fn number(&self) -> u64 {
        self.number
    }

    fn client(&self) -> GithubClient {
        GithubClient::new(self.context.working_directory.clone())
    }

    fn editors(&self, cx: &App) -> (Entity<Editor>, Option<Entity<Editor>>) {
        let splittable = self.splittable.read(cx);
        (
            splittable.rhs_editor().clone(),
            splittable.lhs_editor().cloned(),
        )
    }

    fn sync_editors(&mut self, cx: &mut Context<Self>) {
        let (rhs_editor, lhs_editor) = self.editors(cx);
        let handler: Arc<dyn DiffReviewHandler> = Arc::new(SessionReviewHandler {
            session: cx.weak_entity(),
        });
        for editor in std::iter::once(&rhs_editor).chain(lhs_editor.as_ref()) {
            if !editor.read(cx).has_diff_review_handler() {
                editor.update(cx, |editor, cx| {
                    editor.set_diff_review_handler(Some(handler.clone()), cx);
                    editor.set_show_diff_review_button(true, cx);
                });
            }
        }

        let base_editor_id = lhs_editor.as_ref().map(|editor| editor.entity_id());
        if base_editor_id != self.observed_base_editor || self.multibuffer_subscriptions.is_empty()
        {
            self.observed_base_editor = base_editor_id;
            self.multibuffer_subscriptions = std::iter::once(&rhs_editor)
                .chain(lhs_editor.as_ref())
                .map(|editor| {
                    let multibuffer = editor.read(cx).buffer().clone();
                    cx.subscribe(&multibuffer, |this, _, event: &multi_buffer::Event, cx| {
                        match event {
                            multi_buffer::Event::BufferRangesUpdated { .. }
                            | multi_buffer::Event::BuffersRemoved { .. }
                            | multi_buffer::Event::BufferDiffChanged
                            | multi_buffer::Event::DiffHunksToggled => this.schedule_rebuild(cx),
                            _ => {}
                        }
                    })
                })
                .collect();
            self.schedule_rebuild(cx);
        }
    }

    fn schedule_rebuild(&mut self, cx: &mut Context<Self>) {
        self.rebuild_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REBUILD_DEBOUNCE).await;
            this.update(cx, |this, cx| this.rebuild_blocks(cx))
                .log_err();
        });
    }

    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client();
        let repository = self.context.repository.clone();
        let number = self.number;
        self.refresh_task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let threads = client.fetch_review_threads(&repository, number).await?;
                    let pending = client.fetch_pending_review(&repository, number).await?;
                    anyhow::Ok((threads, pending))
                })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok((threads, pending)) => {
                    this.apply(threads, pending.map(|pending| pending.id), window, cx)
                }
                Err(error) => this.show_error(
                    format!(
                        "Could not load review threads for #{}: {error}",
                        this.number
                    ),
                    cx,
                ),
            })
            .log_err();
        });
    }

    fn show_error(&self, message: String, cx: &mut Context<Self>) {
        show_toast(&self.workspace, message, false, cx);
    }

    fn apply(
        &mut self,
        threads: Vec<ReviewThread>,
        pending_review_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_review_id = pending_review_id;
        let live_ids: HashSet<&str> = threads.iter().map(|thread| thread.id.as_str()).collect();
        self.thread_views
            .retain(|id, _| live_ids.contains(id.as_str()));
        for thread in &threads {
            match self.thread_views.get(&thread.id) {
                Some(view) => {
                    let project = self.project.clone();
                    view.update(cx, |view, cx| {
                        view.update_thread(thread.clone(), &project, cx)
                    });
                }
                None => {
                    let session = cx.weak_entity();
                    let project = self.project.clone();
                    let thread = thread.clone();
                    let fixed_now = self.fixed_now;
                    let id = thread.id.clone();
                    let view = cx.new(|cx| {
                        ThreadView::new(session, thread, fixed_now, &project, window, cx)
                    });
                    cx.subscribe(
                        &view,
                        |this, view, event: &ThreadViewEvent, cx| match event {
                            ThreadViewEvent::HeightChanged => this.resize_block_for(&view, cx),
                        },
                    )
                    .detach();
                    self.thread_views.insert(id, view);
                }
            }
        }
        self.threads = threads;
        self.rebuild_blocks(cx);
        cx.notify();
    }

    fn resize_block_for(&mut self, view: &Entity<ThreadView>, cx: &mut Context<Self>) {
        let (thread_id, rows) = view.read_with(cx, |view, _| (view.thread.id.clone(), view.rows()));
        for placed in self
            .blocks
            .iter()
            .filter(|placed| placed.thread_id == thread_id)
        {
            if let Some(editor) = placed.editor.upgrade() {
                let mut heights = HashMap::default();
                heights.insert(placed.block_id, rows);
                editor.update(cx, |editor, cx| editor.resize_blocks(heights, None, cx));
            }
        }
    }

    fn path_keys_by_repo_path(&self, cx: &App) -> HashMap<String, PathKey> {
        let (rhs_editor, _) = self.editors(cx);
        let multibuffer = rhs_editor.read(cx).buffer().clone();
        let git_repository = self.git_repository.read(cx);
        let snapshot = multibuffer.read(cx).snapshot(cx);
        snapshot
            .buffers_with_paths()
            .filter_map(|(buffer_snapshot, path_key)| {
                let buffer = multibuffer.read(cx).buffer(buffer_snapshot.remote_id())?;
                let repo_path = repo_path_for_buffer(&buffer, git_repository, cx)?;
                Some((repo_path, path_key.clone()))
            })
            .collect()
    }

    fn rebuild_blocks(&mut self, cx: &mut Context<Self>) {
        let mut stale: HashMap<EntityId, (Entity<Editor>, HashSet<CustomBlockId>)> =
            HashMap::default();
        for placed in self.blocks.drain(..) {
            if let Some(editor) = placed.editor.upgrade() {
                stale
                    .entry(editor.entity_id())
                    .or_insert_with(|| (editor, HashSet::default()))
                    .1
                    .insert(placed.block_id);
            }
        }
        for (editor, block_ids) in stale.into_values() {
            editor.update(cx, |editor, cx| editor.remove_blocks(block_ids, None, cx));
        }

        let (rhs_editor, lhs_editor) = self.editors(cx);
        let path_keys = self.path_keys_by_repo_path(cx);
        let rhs_snapshot = rhs_editor.read(cx).buffer().read(cx).snapshot(cx);
        let lhs_snapshot = lhs_editor
            .as_ref()
            .map(|editor| editor.read(cx).buffer().read(cx).snapshot(cx));

        let mut new_blocks: Vec<(Entity<Editor>, String, BlockProperties<Anchor>)> = Vec::new();
        for thread in &self.threads {
            let Some(view) = self.thread_views.get(&thread.id) else {
                continue;
            };
            let Some(path_key) = path_keys.get(&thread.path) else {
                continue;
            };
            let Some(buffer_id) = rhs_snapshot
                .buffers_with_paths()
                .find(|(_, key)| *key == path_key)
                .map(|(buffer, _)| buffer.remote_id())
            else {
                continue;
            };
            let base = lhs_snapshot.as_ref().and_then(|snapshot| {
                let base_id = snapshot
                    .buffers_with_paths()
                    .find(|(_, key)| *key == path_key)
                    .map(|(buffer, _)| buffer.remote_id())?;
                Some((snapshot, base_id))
            });
            let Some(placement) = place_thread(thread, &rhs_snapshot, buffer_id, base) else {
                continue;
            };
            let editor = match (&lhs_editor, placement.in_base_editor) {
                (Some(lhs_editor), true) => lhs_editor.clone(),
                _ => rhs_editor.clone(),
            };
            let render_view = view.clone();
            let rows = view.read(cx).rows();
            new_blocks.push((
                editor,
                thread.id.clone(),
                BlockProperties {
                    placement: BlockPlacement::Below(placement.anchor),
                    height: Some(rows),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |_| render_view.clone().into_any_element()),
                    priority: 0,
                },
            ));
        }

        for (editor, thread_id, properties) in new_blocks {
            let block_ids = editor.update(cx, |editor, cx| {
                editor.insert_blocks([properties], None, cx)
            });
            if let Some(block_id) = block_ids.into_iter().next() {
                self.blocks.push(PlacedBlock {
                    thread_id,
                    editor: editor.downgrade(),
                    block_id,
                });
            }
        }
    }

    fn locate(
        &self,
        submission: &DiffReviewSubmission,
        cx: &App,
    ) -> Result<(String, ReviewTarget), String> {
        let (_, lhs_editor) = self.editors(cx);
        let in_base_editor = lhs_editor
            .as_ref()
            .is_some_and(|editor| editor.entity_id() == submission.editor.entity_id());
        let snapshot = submission.editor.read(cx).buffer().read(cx).snapshot(cx);
        let start_buffer = submission.range.start.buffer_id();
        let end_buffer = submission.range.end.buffer_id();
        let (Some(buffer_id), true) = (start_buffer, start_buffer == end_buffer) else {
            return Err("Select lines from a single file to comment on.".to_string());
        };
        let path_key = snapshot
            .path_for_buffer(buffer_id)
            .cloned()
            .ok_or_else(|| "Could not find the file for this comment.".to_string())?;
        let path = self
            .path_keys_by_repo_path(cx)
            .into_iter()
            .find_map(|(path, key)| (key == path_key).then_some(path))
            .ok_or_else(|| "This file is not part of the pull request.".to_string())?;
        let start_row = submission.range.start.to_point(&snapshot).row;
        let end_row = submission.range.end.to_point(&snapshot).row;
        let position = |row| {
            review_position(&snapshot, row, in_base_editor)
                .ok_or_else(|| "Could not map this line to the pull request.".to_string())
        };
        Ok((
            path,
            review_target(position(start_row)?, position(end_row)?),
        ))
    }

    fn submit_comment(
        &mut self,
        submission: DiffReviewSubmission,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (path, target) = match self.locate(&submission, cx) {
            Ok(located) => located,
            Err(message) => {
                self.show_error(message, cx);
                return;
            }
        };
        let start_review = submission.button_index == 0;
        let client = self.client();
        let pull_request_id = self.pull_request_id.clone();
        let head_oid = self.head_oid.clone();
        let existing_review_id = self.pending_review_id.clone();
        let body = submission.comment;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let created_review = match &existing_review_id {
                        Some(_) => None,
                        None => Some(
                            client
                                .start_pending_review(&pull_request_id, &head_oid)
                                .await?,
                        ),
                    };
                    let review_id = existing_review_id
                        .or_else(|| created_review.as_ref().map(|review| review.id.clone()));
                    let thread = NewReviewThread {
                        pull_request_id,
                        pending_review_id: review_id.clone(),
                        path,
                        body,
                        line: target.line,
                        side: target.side,
                        start_line: target.start_line,
                        start_side: target.start_side,
                    };
                    if let Err(error) = client.add_review_thread(&thread).await {
                        if let Some(review) = &created_review {
                            client.delete_review(&review.id).await.log_err();
                        }
                        return Err(error.into());
                    }
                    if !start_review && let Some(review) = &created_review {
                        client
                            .submit_review(&review.id, ReviewEvent::Comment, "")
                            .await?;
                    }
                    anyhow::Ok(())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    this.show_error(format!("Could not post comment: {error}"), cx);
                }
                this.refresh(window, cx);
            })
            .log_err();
        })
        .detach();
    }

    fn reply(
        &mut self,
        thread_id: String,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client();
        let pending_review_id = self.pending_review_id.clone();
        let request_thread_id = thread_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    client
                        .add_thread_reply(&request_thread_id, &body, pending_review_id.as_deref())
                        .await
                })
                .await
                .map(drop)
                .map_err(|error| error.to_string());
            this.update_in(cx, |this, window, cx| {
                this.finish_thread_action(&thread_id, result, true, window, cx)
            })
            .log_err();
        })
        .detach();
    }

    fn set_resolved(
        &mut self,
        thread_id: String,
        resolved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client();
        let request_thread_id = thread_id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    client
                        .set_thread_resolved(&request_thread_id, resolved)
                        .await
                })
                .await
                .map_err(|error| error.to_string());
            this.update_in(cx, |this, window, cx| {
                this.finish_thread_action(&thread_id, result, false, window, cx)
            })
            .log_err();
        })
        .detach();
    }

    fn finish_thread_action(
        &mut self,
        thread_id: &str,
        result: Result<(), String>,
        clear_reply: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let succeeded = result.is_ok();
        if let Some(view) = self.thread_views.get(thread_id) {
            view.update(cx, |view, cx| {
                view.finish_action(result, clear_reply, window, cx)
            });
        }
        if succeeded {
            self.refresh(window, cx);
        }
    }

    fn pending_comment_count(&self) -> usize {
        self.threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .filter(|comment| comment.state == PENDING_STATE)
            .count()
    }

    fn submit_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let number = self.number;
        self.workspace
            .update(cx, |workspace, cx| {
                PullRequestOverviewView::open_and_focus_review_box(workspace, number, window, cx)
            })
            .log_err();
    }

    fn discard_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review_id) = self.pending_review_id.clone() else {
            return;
        };
        let count = self.pending_comment_count();
        let answer = window.prompt(
            PromptLevel::Warning,
            "Discard pending review?",
            Some(&format!(
                "{count} pending {} will be deleted from GitHub.",
                if count == 1 { "comment" } else { "comments" }
            )),
            &["Discard", "Cancel"],
            cx,
        );
        let client = self.client();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let result = cx
                .background_spawn(async move { client.delete_review(&review_id).await })
                .await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    this.show_error(format!("Could not discard the review: {error}"), cx);
                }
                this.refresh(window, cx);
            })
            .log_err();
        })
        .detach();
    }
}

fn repo_path_for_buffer(
    buffer: &Entity<language::Buffer>,
    repository: &Repository,
    cx: &App,
) -> Option<String> {
    let file = buffer.read(cx).file()?;
    let project_path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };
    let repo_path = repository.project_path_to_repo_path(&project_path, cx)?;
    Some(repo_path.as_unix_str().to_string())
}

impl Render for PullRequestReviewSession {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.pending_comment_count();
        let has_pending_review = self.pending_review_id.is_some();
        let colors = cx.theme().colors();
        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .bg(colors.title_bar_background)
            .border_b_1()
            .border_color(colors.border)
            .child(
                Label::new(format!("PR #{} · {}", self.number, self.title))
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD)
                    .truncate(),
            )
            .when(ai_enabled(cx), |this| {
                this.child(
                    Button::new("pull-request-ask-ai", "Ask AI")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::ZedAssistant).size(IconSize::XSmall))
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
            .child(div().flex_1())
            .when(has_pending_review, |this| {
                this.child(
                    Label::new(format!(
                        "Pending review · {count} {}",
                        if count == 1 { "comment" } else { "comments" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    Button::new("pending-review-discard", "Discard")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.discard_review(window, cx)),
                        ),
                )
                .child(
                    Button::new("pending-review-submit", "Submit Review")
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.submit_review(window, cx)),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer_diff::BufferDiff;
    use gpui::TestAppContext;
    use language::Buffer;
    use multi_buffer::MultiBuffer;

    const BASE_TEXT: &str = "a\nb\nc\nd\n";
    const NEW_TEXT: &str = "a\nc\nd\ne\n";

    fn thread(
        side: Option<DiffSide>,
        line: Option<u32>,
        original_line: Option<u32>,
    ) -> ReviewThread {
        ReviewThread {
            id: "thread".into(),
            is_resolved: false,
            is_outdated: line.is_none(),
            path: "file.txt".into(),
            diff_side: side,
            line,
            start_line: None,
            original_line,
            subject_type: None,
            viewer_can_reply: true,
            viewer_can_resolve: true,
            comments: Vec::new(),
        }
    }

    fn diff_snapshot(cx: &mut TestAppContext) -> (MultiBufferSnapshot, BufferId) {
        let buffer = cx.new(|cx| Buffer::local(NEW_TEXT, cx));
        let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());
        let diff = cx.new(|cx| {
            BufferDiff::new_with_base_text(BASE_TEXT, &buffer.read(cx).text_snapshot(), cx)
        });
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.add_diff(diff, cx);
            multibuffer.expand_diff_hunks(vec![Anchor::Min..Anchor::Max], cx);
        });
        (
            multibuffer.read_with(cx, |multibuffer, cx| multibuffer.snapshot(cx)),
            buffer_id,
        )
    }

    #[test]
    fn review_target_maps_single_and_multi_line_ranges() {
        let right = |line| ReviewPosition {
            line,
            side: DiffSide::Right,
        };
        let left = |line| ReviewPosition {
            line,
            side: DiffSide::Left,
        };
        assert_eq!(
            review_target(right(4), right(4)),
            ReviewTarget {
                line: 4,
                side: DiffSide::Right,
                start_line: None,
                start_side: None,
            }
        );
        assert_eq!(
            review_target(left(2), right(5)),
            ReviewTarget {
                line: 5,
                side: DiffSide::Right,
                start_line: Some(2),
                start_side: Some(DiffSide::Left),
            }
        );
    }

    #[gpui::test]
    fn review_position_distinguishes_deleted_rows(cx: &mut TestAppContext) {
        let (snapshot, _) = diff_snapshot(cx);
        // Rows: 0 "a", 1 "-b", 2 "c", 3 "d", 4 "+e"
        let position = |row, in_base_editor| review_position(&snapshot, row, in_base_editor);
        assert_eq!(
            position(1, false),
            Some(ReviewPosition {
                line: 2,
                side: DiffSide::Left
            })
        );
        assert_eq!(
            position(2, false),
            Some(ReviewPosition {
                line: 2,
                side: DiffSide::Right
            })
        );
        assert_eq!(
            position(4, false),
            Some(ReviewPosition {
                line: 4,
                side: DiffSide::Right
            })
        );
        assert_eq!(
            position(2, true),
            Some(ReviewPosition {
                line: 2,
                side: DiffSide::Left
            })
        );
    }

    #[gpui::test]
    fn threads_anchor_to_their_side_and_fall_back_when_outdated(cx: &mut TestAppContext) {
        let (snapshot, buffer_id) = diff_snapshot(cx);
        let placed_row = |thread: &ReviewThread| {
            let placement = place_thread(thread, &snapshot, buffer_id, None).expect("placement");
            placement.anchor.to_point(&snapshot).row
        };

        assert_eq!(placed_row(&thread(Some(DiffSide::Right), Some(2), None)), 2);
        assert_eq!(placed_row(&thread(Some(DiffSide::Left), Some(2), None)), 1);
        assert_eq!(placed_row(&thread(Some(DiffSide::Left), Some(1), None)), 0);
        assert_eq!(placed_row(&thread(Some(DiffSide::Right), None, Some(3))), 3);
        assert_eq!(
            placed_row(&thread(Some(DiffSide::Right), None, Some(99))),
            0
        );
    }

    #[test]
    fn thread_height_tracks_collapse_and_reply_box() {
        let mut resolved = thread(Some(DiffSide::Right), Some(1), None);
        resolved.is_resolved = true;
        resolved.comments.push(ReviewComment {
            id: "comment".into(),
            author: None,
            body: "one\ntwo".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            state: "SUBMITTED".into(),
        });
        let collapsed = estimate_thread_rows(&resolved, false);
        let expanded = estimate_thread_rows(&resolved, true);
        resolved.viewer_can_reply = false;
        assert_eq!(collapsed, 2);
        assert!(expanded > collapsed);
        assert!(estimate_thread_rows(&resolved, true) < expanded);
    }
}
