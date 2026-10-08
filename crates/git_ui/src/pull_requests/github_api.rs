use std::fmt;
use std::path::{Path, PathBuf};

use git::GitHostingProviderRegistry;
use gpui::App;
use project::git_store::Repository;
use serde::{Deserialize, de::DeserializeOwned};
use util::command::{Stdio, new_command};

const GH_CANDIDATE_PATHS: &[&str] = &["gh", "/opt/homebrew/bin/gh", "/usr/local/bin/gh"];

#[derive(Debug, Clone, PartialEq)]
pub enum GithubError {
    GhNotInstalled,
    NotAuthenticated(String),
    NotGithubRepository,
    NoPullRequest,
    Graphql(Vec<String>),
    CommandFailed(String),
    InvalidResponse(String),
}

impl fmt::Display for GithubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GithubError::GhNotInstalled => write!(
                formatter,
                "The GitHub CLI (`gh`) is not installed. Install it from https://cli.github.com."
            ),
            GithubError::NotAuthenticated(details) => write!(
                formatter,
                "The GitHub CLI is not authenticated. Run `gh auth login`. {details}"
            ),
            GithubError::NotGithubRepository => write!(formatter, "Not a GitHub repository"),
            GithubError::NoPullRequest => write!(formatter, "No pull request for this branch"),
            GithubError::Graphql(messages) => {
                write!(formatter, "GitHub error: {}", messages.join("; "))
            }
            GithubError::CommandFailed(details) => write!(formatter, "`gh` failed: {details}"),
            GithubError::InvalidResponse(details) => {
                write!(formatter, "Unexpected response from GitHub: {details}")
            }
        }
    }
}

impl std::error::Error for GithubError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GithubRepository {
    pub owner: String,
    pub name: String,
}

impl GithubRepository {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone)]
pub struct GithubContext {
    pub repository: GithubRepository,
    pub working_directory: PathBuf,
}

impl GithubContext {
    pub fn from_repository(repository: &Repository, cx: &App) -> Result<Self, GithubError> {
        let remote_url = repository
            .default_remote_url()
            .ok_or(GithubError::NotGithubRepository)?;
        let provider_registry = GitHostingProviderRegistry::global(cx);
        let (provider, parsed_remote) = git::parse_git_remote_url(provider_registry, &remote_url)
            .ok_or(GithubError::NotGithubRepository)?;
        if provider.name() != "GitHub" {
            return Err(GithubError::NotGithubRepository);
        }
        Ok(Self {
            repository: GithubRepository {
                owner: parsed_remote.owner.to_string(),
                name: parsed_remote.repo.to_string(),
            },
            working_directory: repository.work_directory_abs_path.to_path_buf(),
        })
    }
}

enum Variable {
    Text(String),
    Integer(i64),
}

impl Variable {
    fn text(value: impl Into<String>) -> Self {
        Variable::Text(value.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DiffSide {
    Left,
    Right,
}

impl DiffSide {
    fn as_str(self) -> &'static str {
        match self {
            DiffSide::Left => "LEFT",
            DiffSide::Right => "RIGHT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    fn as_str(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BranchPullRequest {
    pub number: u64,
    pub url: String,
    pub state: PullRequestState,
}

impl BranchPullRequest {
    pub fn repository(&self) -> Option<GithubRepository> {
        let path = self.url.split_once("://")?.1;
        let mut segments = path.split('/').skip(1);
        let owner = segments.next().filter(|segment| !segment.is_empty())?;
        let name = segments.next().filter(|segment| !segment.is_empty())?;
        Some(GithubRepository {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Actor {
    pub login: String,
    #[serde(default, rename = "avatarUrl")]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct Connection<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewComment {
    pub id: String,
    pub author: Option<Actor>,
    pub body: String,
    pub created_at: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    pub id: String,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub path: String,
    pub diff_side: Option<DiffSide>,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub subject_type: Option<String>,
    pub viewer_can_reply: bool,
    pub viewer_can_resolve: bool,
    #[serde(deserialize_with = "deserialize_nodes")]
    pub comments: Vec<ReviewComment>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Label {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReviewRequest {
    pub reviewer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestReview {
    pub author: Option<Actor>,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "__typename")]
pub enum TimelineItem {
    IssueComment {
        id: String,
        author: Option<Actor>,
        body: String,
        #[serde(rename = "createdAt")]
        created_at: String,
        url: String,
    },
    PullRequestReview {
        id: String,
        author: Option<Actor>,
        body: String,
        state: String,
        #[serde(rename = "createdAt")]
        created_at: String,
        url: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PullRequestOverview {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub url: String,
    pub author: Option<Actor>,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_oid: String,
    pub review_requests: Vec<String>,
    pub latest_reviews: Vec<LatestReview>,
    pub labels: Vec<Label>,
    pub check_state: Option<String>,
    pub timeline: Vec<TimelineItem>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PendingReview {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewReviewThread {
    pub pull_request_id: String,
    pub pending_review_id: Option<String>,
    pub path: String,
    pub body: String,
    pub line: u32,
    pub side: DiffSide,
    pub start_line: Option<u32>,
    pub start_side: Option<DiffSide>,
}

fn deserialize_nodes<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Connection::<T>::deserialize(deserializer)?.nodes)
}

const THREAD_FIELDS: &str = "id isResolved isOutdated path diffSide line startLine originalLine \
     subjectType viewerCanReply viewerCanResolve \
     comments(first: 50) { nodes { id author { login avatarUrl } body createdAt state } }";

#[derive(Clone)]
pub struct GithubClient {
    working_directory: PathBuf,
}

impl GithubClient {
    pub fn new(working_directory: PathBuf) -> Self {
        Self { working_directory }
    }

    pub async fn viewer_login(&self) -> Result<String, GithubError> {
        #[derive(Deserialize)]
        struct Viewer {
            login: String,
        }
        let output = run_gh(&self.working_directory, &["api", "user"]).await?;
        serde_json::from_str::<Viewer>(&output)
            .map(|viewer| viewer.login)
            .map_err(|error| GithubError::InvalidResponse(error.to_string()))
    }

    pub async fn current_branch_pull_request(
        &self,
    ) -> Result<Option<BranchPullRequest>, GithubError> {
        let output = run_gh(
            &self.working_directory,
            &["pr", "view", "--json", "number,url,state"],
        )
        .await;
        parse_branch_pull_request(output)
    }

    pub async fn checkout_pull_request(&self, number: u64) -> Result<(), GithubError> {
        run_gh(
            &self.working_directory,
            &["pr", "checkout", &number.to_string()],
        )
        .await
        .map(drop)
    }

    pub async fn fetch_overview(
        &self,
        repository: &GithubRepository,
        number: u64,
    ) -> Result<PullRequestOverview, GithubError> {
        let query = "query($owner: String!, $name: String!, $number: Int!) { \
            repository(owner: $owner, name: $name) { pullRequest(number: $number) { \
            id number title body state isDraft url author { login avatarUrl } \
            headRefName baseRefName headRefOid baseRefOid \
            reviewRequests(first: 50) { nodes { requestedReviewer { \
              ... on User { login } ... on Team { name } ... on Mannequin { login } } } } \
            latestReviews(first: 50) { nodes { author { login avatarUrl } state } } \
            labels(first: 50) { nodes { name color } } \
            commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } \
            timelineItems(first: 100, itemTypes: [ISSUE_COMMENT, PULL_REQUEST_REVIEW]) { nodes { \
              __typename \
              ... on IssueComment { id author { login avatarUrl } body createdAt url } \
              ... on PullRequestReview { id author { login avatarUrl } body state createdAt url } } } \
            } } }";
        let response: OverviewResponse = self
            .graphql(query, repository_variables(repository, number))
            .await?;
        let raw = response.repository.pull_request.ok_or_else(|| {
            GithubError::InvalidResponse(format!("pull request #{number} not found"))
        })?;
        Ok(raw.into_overview())
    }

    pub async fn fetch_review_threads(
        &self,
        repository: &GithubRepository,
        number: u64,
    ) -> Result<Vec<ReviewThread>, GithubError> {
        let query = format!(
            "query($owner: String!, $name: String!, $number: Int!) {{ \
             repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ \
             reviewThreads(first: 100) {{ nodes {{ {THREAD_FIELDS} }} }} }} }} }}"
        );
        let response: ThreadsResponse = self
            .graphql(&query, repository_variables(repository, number))
            .await?;
        let pull_request = response.repository.pull_request.ok_or_else(|| {
            GithubError::InvalidResponse(format!("pull request #{number} not found"))
        })?;
        Ok(pull_request.review_threads.nodes)
    }

    pub async fn fetch_pending_review(
        &self,
        repository: &GithubRepository,
        number: u64,
    ) -> Result<Option<PendingReview>, GithubError> {
        let query = "query($owner: String!, $name: String!, $number: Int!) { \
            repository(owner: $owner, name: $name) { pullRequest(number: $number) { \
            reviews(states: PENDING, first: 20) { nodes { id viewerDidAuthor } } } } }";
        let response: PendingReviewsResponse = self
            .graphql(query, repository_variables(repository, number))
            .await?;
        let pull_request = response.repository.pull_request.ok_or_else(|| {
            GithubError::InvalidResponse(format!("pull request #{number} not found"))
        })?;
        Ok(pull_request
            .reviews
            .nodes
            .into_iter()
            .find(|review| review.viewer_did_author)
            .map(|review| PendingReview { id: review.id }))
    }

    pub async fn start_pending_review(
        &self,
        pull_request_id: &str,
        commit_oid: &str,
    ) -> Result<PendingReview, GithubError> {
        let query = "mutation($pullRequestId: ID!, $commitOid: GitObjectID!) { \
            addPullRequestReview(input: { pullRequestId: $pullRequestId, commitOID: $commitOid }) { \
            pullRequestReview { id } } }";
        let response: AddReviewResponse = self
            .graphql(
                query,
                vec![
                    ("pullRequestId", Variable::text(pull_request_id)),
                    ("commitOid", Variable::text(commit_oid)),
                ],
            )
            .await?;
        Ok(response.add_pull_request_review.pull_request_review)
    }

    pub async fn add_review_thread(
        &self,
        thread: &NewReviewThread,
    ) -> Result<ReviewThread, GithubError> {
        let query = format!(
            "mutation($pullRequestId: ID!, $reviewId: ID, $path: String!, $body: String!, \
             $line: Int!, $side: DiffSide!, $startLine: Int, $startSide: DiffSide) {{ \
             addPullRequestReviewThread(input: {{ pullRequestId: $pullRequestId, \
             pullRequestReviewId: $reviewId, path: $path, body: $body, line: $line, side: $side, \
             startLine: $startLine, startSide: $startSide, subjectType: LINE }}) {{ \
             thread {{ {THREAD_FIELDS} }} }} }}"
        );
        let mut variables = vec![
            ("pullRequestId", Variable::text(&thread.pull_request_id)),
            ("path", Variable::text(&thread.path)),
            ("body", Variable::text(&thread.body)),
            ("line", Variable::Integer(i64::from(thread.line))),
            ("side", Variable::text(thread.side.as_str())),
        ];
        if let Some(review_id) = &thread.pending_review_id {
            variables.push(("reviewId", Variable::text(review_id)));
        }
        if let Some(start_line) = thread.start_line {
            variables.push(("startLine", Variable::Integer(i64::from(start_line))));
        }
        if let Some(start_side) = thread.start_side {
            variables.push(("startSide", Variable::text(start_side.as_str())));
        }
        let response: AddThreadResponse = self.graphql(&query, variables).await?;
        response
            .add_pull_request_review_thread
            .thread
            .ok_or_else(|| GithubError::InvalidResponse("thread was not created".into()))
    }

    pub async fn add_thread_reply(
        &self,
        thread_id: &str,
        body: &str,
        pending_review_id: Option<&str>,
    ) -> Result<ReviewComment, GithubError> {
        let query = "mutation($threadId: ID!, $body: String!, $reviewId: ID) { \
            addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, \
            body: $body, pullRequestReviewId: $reviewId }) { \
            comment { id author { login avatarUrl } body createdAt state } } }";
        let mut variables = vec![
            ("threadId", Variable::text(thread_id)),
            ("body", Variable::text(body)),
        ];
        if let Some(review_id) = pending_review_id {
            variables.push(("reviewId", Variable::text(review_id)));
        }
        let response: AddReplyResponse = self.graphql(query, variables).await?;
        response
            .add_pull_request_review_thread_reply
            .comment
            .ok_or_else(|| GithubError::InvalidResponse("reply was not created".into()))
    }

    pub async fn submit_review(
        &self,
        review_id: &str,
        event: ReviewEvent,
        body: &str,
    ) -> Result<(), GithubError> {
        let query = "mutation($reviewId: ID!, $event: PullRequestReviewEvent!, $body: String) { \
            submitPullRequestReview(input: { pullRequestReviewId: $reviewId, event: $event, body: $body }) { \
            pullRequestReview { id } } }";
        self.graphql::<serde_json::Value>(
            query,
            vec![
                ("reviewId", Variable::text(review_id)),
                ("event", Variable::text(event.as_str())),
                ("body", Variable::text(body)),
            ],
        )
        .await
        .map(drop)
    }

    pub async fn delete_review(&self, review_id: &str) -> Result<(), GithubError> {
        let query = "mutation($reviewId: ID!) { \
            deletePullRequestReview(input: { pullRequestReviewId: $reviewId }) { \
            pullRequestReview { id } } }";
        self.graphql::<serde_json::Value>(query, vec![("reviewId", Variable::text(review_id))])
            .await
            .map(drop)
    }

    pub async fn set_thread_resolved(
        &self,
        thread_id: &str,
        resolved: bool,
    ) -> Result<(), GithubError> {
        let mutation = if resolved {
            "resolveReviewThread"
        } else {
            "unresolveReviewThread"
        };
        let query = format!(
            "mutation($threadId: ID!) {{ {mutation}(input: {{ threadId: $threadId }}) {{ \
             thread {{ id isResolved }} }} }}"
        );
        self.graphql::<serde_json::Value>(&query, vec![("threadId", Variable::text(thread_id))])
            .await
            .map(drop)
    }

    async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Vec<(&str, Variable)>,
    ) -> Result<T, GithubError> {
        let mut arguments = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={query}"),
        ];
        for (name, variable) in variables {
            match variable {
                Variable::Text(value) => {
                    arguments.push("-f".to_string());
                    arguments.push(format!("{name}={value}"));
                }
                Variable::Integer(value) => {
                    arguments.push("-F".to_string());
                    arguments.push(format!("{name}={value}"));
                }
            }
        }
        let argument_refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output =
            run_gh_allowing_graphql_errors(&self.working_directory, &argument_refs).await?;
        parse_graphql_response(&output)
    }
}

fn repository_variables(
    repository: &GithubRepository,
    number: u64,
) -> Vec<(&'static str, Variable)> {
    vec![
        ("owner", Variable::text(&repository.owner)),
        ("name", Variable::text(&repository.name)),
        ("number", Variable::Integer(number as i64)),
    ]
}

async fn run_gh(working_directory: &Path, arguments: &[&str]) -> Result<String, GithubError> {
    let output = spawn_gh(working_directory, arguments).await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(classify_failure(&String::from_utf8_lossy(&output.stderr)))
    }
}

// `gh api graphql` exits non-zero when the response carries an `errors` array,
// but still prints the JSON body on stdout.
async fn run_gh_allowing_graphql_errors(
    working_directory: &Path,
    arguments: &[&str],
) -> Result<String, GithubError> {
    let output = spawn_gh(working_directory, arguments).await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() || stdout.trim_start().starts_with('{') {
        Ok(stdout)
    } else {
        Err(classify_failure(&String::from_utf8_lossy(&output.stderr)))
    }
}

async fn spawn_gh(
    working_directory: &Path,
    arguments: &[&str],
) -> Result<std::process::Output, GithubError> {
    for program in GH_CANDIDATE_PATHS {
        let result = new_command(program)
            .args(arguments)
            .current_dir(working_directory)
            .stdin(Stdio::null())
            .output()
            .await;
        match result {
            Ok(output) => return Ok(output),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(GithubError::CommandFailed(error.to_string())),
        }
    }
    Err(GithubError::GhNotInstalled)
}

fn parse_branch_pull_request(
    output: Result<String, GithubError>,
) -> Result<Option<BranchPullRequest>, GithubError> {
    match output {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|error| GithubError::InvalidResponse(error.to_string())),
        Err(GithubError::NoPullRequest) => Ok(None),
        Err(error) => Err(error),
    }
}

fn classify_failure(stderr: &str) -> GithubError {
    let message = stderr.trim().to_string();
    if message.contains("gh auth login") || message.contains("GH_TOKEN") {
        GithubError::NotAuthenticated(String::new())
    } else if message.contains("no pull requests found") {
        GithubError::NoPullRequest
    } else {
        GithubError::CommandFailed(message)
    }
}

#[derive(Deserialize)]
struct GraphqlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlErrorMessage>,
}

#[derive(Deserialize)]
struct GraphqlErrorMessage {
    message: String,
}

fn parse_graphql_response<T: DeserializeOwned>(json: &str) -> Result<T, GithubError> {
    let envelope: GraphqlEnvelope<T> = serde_json::from_str(json)
        .map_err(|error| GithubError::InvalidResponse(error.to_string()))?;
    if !envelope.errors.is_empty() {
        return Err(GithubError::Graphql(
            envelope
                .errors
                .into_iter()
                .map(|error| error.message)
                .collect(),
        ));
    }
    envelope
        .data
        .ok_or_else(|| GithubError::InvalidResponse("response has no data".into()))
}

#[derive(Deserialize)]
struct RepositoryEnvelope<T> {
    repository: PullRequestEnvelope<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestEnvelope<T> {
    pull_request: Option<T>,
}

type ThreadsResponse = RepositoryEnvelope<ThreadsNode>;
type PendingReviewsResponse = RepositoryEnvelope<ReviewsNode>;
type OverviewResponse = RepositoryEnvelope<RawOverview>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadsNode {
    review_threads: Connection<ReviewThread>,
}

#[derive(Deserialize)]
struct ReviewsNode {
    reviews: Connection<RawPendingReview>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPendingReview {
    id: String,
    viewer_did_author: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddReviewResponse {
    add_pull_request_review: AddReviewPayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddReviewPayload {
    pull_request_review: PendingReview,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddThreadResponse {
    add_pull_request_review_thread: AddThreadPayload,
}

#[derive(Deserialize)]
struct AddThreadPayload {
    thread: Option<ReviewThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddReplyResponse {
    add_pull_request_review_thread_reply: AddReplyPayload,
}

#[derive(Deserialize)]
struct AddReplyPayload {
    comment: Option<ReviewComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOverview {
    id: String,
    number: u64,
    title: String,
    body: String,
    state: PullRequestState,
    is_draft: bool,
    url: String,
    author: Option<Actor>,
    head_ref_name: String,
    base_ref_name: String,
    head_ref_oid: String,
    base_ref_oid: String,
    review_requests: Connection<RawReviewRequest>,
    latest_reviews: Connection<LatestReview>,
    labels: Connection<Label>,
    commits: Connection<RawCommitNode>,
    timeline_items: Connection<TimelineItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReviewRequest {
    requested_reviewer: Option<RawReviewer>,
}

#[derive(Deserialize)]
struct RawReviewer {
    login: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawCommitNode {
    commit: RawCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    status_check_rollup: Option<RawRollup>,
}

#[derive(Deserialize)]
struct RawRollup {
    state: String,
}

impl RawOverview {
    fn into_overview(self) -> PullRequestOverview {
        PullRequestOverview {
            id: self.id,
            number: self.number,
            title: self.title,
            body: self.body,
            state: self.state,
            is_draft: self.is_draft,
            url: self.url,
            author: self.author,
            head_ref_name: self.head_ref_name,
            base_ref_name: self.base_ref_name,
            head_ref_oid: self.head_ref_oid,
            base_ref_oid: self.base_ref_oid,
            review_requests: self
                .review_requests
                .nodes
                .into_iter()
                .filter_map(|request| request.requested_reviewer)
                .filter_map(|reviewer| reviewer.login.or(reviewer.name))
                .collect(),
            latest_reviews: self.latest_reviews.nodes,
            labels: self.labels.nodes,
            check_state: self
                .commits
                .nodes
                .into_iter()
                .find_map(|node| node.commit.status_check_rollup)
                .map(|rollup| rollup.state),
            timeline: self
                .timeline_items
                .nodes
                .into_iter()
                .filter(|item| !matches!(item, TimelineItem::Other))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_branch_pull_request() {
        let found = parse_branch_pull_request(Ok(
            r#"{"number":12,"url":"https://github.com/octo-org/widgets/pull/12","state":"OPEN"}"#
                .into(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(found.number, 12);
        assert_eq!(found.state, PullRequestState::Open);
        assert_eq!(
            found.repository(),
            Some(GithubRepository {
                owner: "octo-org".into(),
                name: "widgets".into()
            })
        );

        let missing = classify_failure("no pull requests found for branch \"feature\"");
        assert_eq!(parse_branch_pull_request(Err(missing)), Ok(None));
        assert_eq!(
            parse_branch_pull_request(Err(GithubError::GhNotInstalled)),
            Err(GithubError::GhNotInstalled)
        );
    }

    #[test]
    fn parses_overview() {
        let json = r#"{"data":{"repository":{"pullRequest":{
            "id":"PR_1","number":12,"title":"Add widget","body":"Body text","state":"OPEN",
            "isDraft":false,"url":"https://github.com/o/r/pull/12",
            "author":{"login":"octocat","avatarUrl":null},
            "headRefName":"widget","baseRefName":"main","headRefOid":"abc","baseRefOid":"def",
            "reviewRequests":{"nodes":[
                {"requestedReviewer":{"login":"reviewer"}},
                {"requestedReviewer":{"name":"core-team"}},
                {"requestedReviewer":null}]},
            "latestReviews":{"nodes":[{"author":{"login":"reviewer"},"state":"APPROVED"}]},
            "labels":{"nodes":[{"name":"bug","color":"d73a4a"}]},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]},
            "timelineItems":{"nodes":[
                {"__typename":"IssueComment","id":"C1","author":{"login":"octocat"},
                 "body":"hello","createdAt":"2026-01-01T00:00:00Z","url":"u1"},
                {"__typename":"PullRequestReview","id":"R1","author":null,"body":"",
                 "state":"APPROVED","createdAt":"2026-01-02T00:00:00Z","url":"u2"},
                {"__typename":"SomethingElse"}]}}}}}"#;
        let response: OverviewResponse = parse_graphql_response(json).unwrap();
        let overview = response.repository.pull_request.unwrap().into_overview();
        assert_eq!(overview.state, PullRequestState::Open);
        assert_eq!(overview.review_requests, vec!["reviewer", "core-team"]);
        assert_eq!(overview.check_state.as_deref(), Some("SUCCESS"));
        assert_eq!(overview.labels[0].name, "bug");
        assert_eq!(overview.timeline.len(), 2);
    }

    #[test]
    fn parses_review_threads() {
        let json = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{
            "id":"T1","isResolved":false,"isOutdated":true,"path":"src/a.rs","diffSide":"LEFT",
            "line":null,"startLine":null,"originalLine":7,"subjectType":"LINE",
            "viewerCanReply":true,"viewerCanResolve":false,
            "comments":{"nodes":[{"id":"C1","author":{"login":"octocat"},"body":"nit",
                "createdAt":"2026-01-01T00:00:00Z","state":"PENDING"}]}}]}}}}}"#;
        let response: ThreadsResponse = parse_graphql_response(json).unwrap();
        let threads = response
            .repository
            .pull_request
            .unwrap()
            .review_threads
            .nodes;
        assert_eq!(threads[0].diff_side, Some(DiffSide::Left));
        assert_eq!(threads[0].line, None);
        assert_eq!(threads[0].original_line, Some(7));
        assert!(threads[0].is_outdated);
        assert_eq!(threads[0].comments[0].state, "PENDING");
    }

    #[test]
    fn graphql_errors_become_error_messages() {
        let json = r#"{"data":null,"errors":[{"message":"Could not resolve to a Repository"},
            {"message":"Second problem"}]}"#;
        let error = parse_graphql_response::<serde_json::Value>(json).unwrap_err();
        assert_eq!(
            error,
            GithubError::Graphql(vec![
                "Could not resolve to a Repository".into(),
                "Second problem".into()
            ])
        );
        assert!(error.to_string().contains("Second problem"));
    }
}
