use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Context as _;
use gpui::{App, AppContext as _, Window};
use project::DisableAiSettings;
use settings::Settings as _;
use util::{
    ResultExt as _,
    command::{Stdio, new_command},
};
use workspace::Workspace;
use zed_actions::agent::AskAboutPullRequest;

use super::github_api::{GithubClient, GithubContext, PullRequestOverview, ReviewThread};
use super::pr_overview::{fetch_base_ref, show_toast};

pub const DIFF_BYTE_LIMIT: usize = 200_000;
const MINIMUM_FILE_BYTES: usize = 500;

pub(super) fn ai_enabled(cx: &App) -> bool {
    !DisableAiSettings::get_global(cx).disable_ai
}

pub fn ask_ai_about_pull_request(
    workspace: gpui::WeakEntity<Workspace>,
    context: GithubContext,
    number: u64,
    window: &mut Window,
    cx: &mut App,
) {
    let current_branch = workspace
        .upgrade()
        .and_then(|workspace| workspace.read(cx).project().read(cx).active_repository(cx))
        .and_then(|repository| {
            repository
                .read(cx)
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string())
        });
    let toast_workspace = workspace.clone();
    window
        .spawn(cx, async move |cx| {
            let result = cx
                .background_spawn(async move {
                    gather_context(context, number, current_branch.as_deref()).await
                })
                .await;
            workspace
                .update_in(cx, |_, window, cx| match result {
                    Ok(text) => window.dispatch_action(
                        Box::new(AskAboutPullRequest {
                            number,
                            context: text.into(),
                        }),
                        cx,
                    ),
                    Err(error) => show_toast(
                        &toast_workspace,
                        format!("Could not load pull request #{number} for the agent: {error}"),
                        false,
                        cx,
                    ),
                })
                .log_err();
        })
        .detach();
}

async fn gather_context(
    context: GithubContext,
    number: u64,
    current_branch: Option<&str>,
) -> anyhow::Result<String> {
    let client = GithubClient::new(context.working_directory.clone());
    let overview = client.fetch_overview(&context.repository, number).await?;
    let threads = client
        .fetch_review_threads(&context.repository, number)
        .await?;
    let diff = if current_branch == Some(overview.head_ref_name.as_str()) {
        fetch_base_ref(&context.working_directory, &overview.base_ref_name).await?;
        local_diff(&context.working_directory, &overview.base_ref_name).await?
    } else {
        client.pull_request_diff(number).await?
    };
    Ok(build_context(&overview, &diff, &threads, DIFF_BYTE_LIMIT))
}

async fn local_diff(working_directory: &PathBuf, base_ref_name: &str) -> anyhow::Result<String> {
    let output = new_command("git")
        .args(["diff", &format!("origin/{base_ref_name}...HEAD")])
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .output()
        .await
        .context("running git diff")?;
    anyhow::ensure!(
        output.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct FileDiff<'a> {
    path: &'a str,
    text: &'a str,
    additions: usize,
    deletions: usize,
}

fn split_diff(diff: &str) -> Vec<FileDiff<'_>> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts.push(diff.len());
    starts
        .windows(2)
        .filter_map(|window| {
            let text = diff.get(window[0]..window[1])?;
            let header = text.lines().next()?;
            let path = header
                .rsplit_once(" b/")
                .map(|(_, path)| path)
                .unwrap_or(header);
            let mut in_hunk = false;
            let (mut additions, mut deletions) = (0, 0);
            for line in text.lines() {
                if line.starts_with("@@") {
                    in_hunk = true;
                } else if in_hunk {
                    match line.as_bytes().first() {
                        Some(b'+') => additions += 1,
                        Some(b'-') => deletions += 1,
                        _ => {}
                    }
                }
            }
            Some(FileDiff {
                path,
                text,
                additions,
                deletions,
            })
        })
        .collect()
}

fn truncate_to_lines(text: &str, limit: usize) -> (&str, usize) {
    if text.len() <= limit {
        return (text, 0);
    }
    let mut end = 0;
    for line in text.split_inclusive('\n') {
        if end + line.len() > limit {
            break;
        }
        end += line.len();
    }
    let omitted = text[end..].lines().count();
    (&text[..end], omitted)
}

fn fence_for(text: &str) -> String {
    let mut longest = 0;
    let mut current = 0;
    for character in text.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

pub fn build_context(
    overview: &PullRequestOverview,
    diff: &str,
    threads: &[ReviewThread],
    diff_limit: usize,
) -> String {
    let mut output = String::new();
    writeln!(
        output,
        "# Pull request #{}: {}",
        overview.number, overview.title
    )
    .ok();
    writeln!(output, "URL: {}", overview.url).ok();
    writeln!(
        output,
        "Branches: {} -> {}",
        overview.head_ref_name, overview.base_ref_name
    )
    .ok();

    writeln!(output, "\n## Description").ok();
    if overview.body.trim().is_empty() {
        writeln!(output, "(no description)").ok();
    } else {
        writeln!(output, "{}", overview.body.trim()).ok();
    }

    let files = split_diff(diff);
    writeln!(output, "\n## Changed files").ok();
    for file in &files {
        writeln!(
            output,
            "- {} (+{} -{})",
            file.path, file.additions, file.deletions
        )
        .ok();
    }

    writeln!(output, "\n## Diff").ok();
    let mut body = String::new();
    if diff.len() <= diff_limit {
        body.push_str(diff);
    } else {
        let per_file = (diff_limit / files.len().max(1)).max(MINIMUM_FILE_BYTES);
        let mut omitted_files = 0;
        for file in &files {
            if body.len() >= diff_limit {
                omitted_files += 1;
                continue;
            }
            let (text, omitted_lines) = truncate_to_lines(file.text, per_file);
            body.push_str(text);
            if omitted_lines > 0 {
                writeln!(
                    body,
                    "[... {omitted_lines} more lines of {} omitted ...]",
                    file.path
                )
                .ok();
            }
        }
        if omitted_files > 0 {
            writeln!(body, "[... {omitted_files} more files omitted ...]").ok();
        }
        writeln!(
            output,
            "The diff is over {} KB, so each file is truncated.",
            diff_limit / 1000
        )
        .ok();
    }
    let fence = fence_for(&body);
    writeln!(output, "{fence}diff\n{}\n{fence}", body.trim_end()).ok();

    writeln!(output, "\n## Review threads").ok();
    if threads.is_empty() {
        writeln!(output, "(none)").ok();
    }
    for thread in threads {
        let line = thread
            .line
            .or(thread.original_line)
            .map(|line| format!(":{line}"))
            .unwrap_or_default();
        let status = if thread.is_resolved {
            "resolved"
        } else if thread.is_outdated {
            "outdated"
        } else {
            "open"
        };
        writeln!(output, "\n### {}{line} ({status})", thread.path).ok();
        for comment in &thread.comments {
            let author = comment
                .author
                .as_ref()
                .map(|author| author.login.as_str())
                .unwrap_or("ghost");
            writeln!(output, "{author}: {}", comment.body.trim()).ok();
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pull_requests::github_api::{Actor, DiffSide, PullRequestState, ReviewComment};

    fn overview() -> PullRequestOverview {
        PullRequestOverview {
            id: "PR_1".into(),
            number: 7,
            title: "Add retries".into(),
            body: "Retries failed requests.".into(),
            state: PullRequestState::Open,
            is_draft: false,
            url: "https://github.com/octo-org/widgets/pull/7".into(),
            author: None,
            head_ref_name: "retries".into(),
            base_ref_name: "main".into(),
            head_ref_oid: String::new(),
            base_ref_oid: String::new(),
            review_requests: Vec::new(),
            latest_reviews: Vec::new(),
            labels: Vec::new(),
            check_state: None,
            timeline: Vec::new(),
        }
    }

    fn file_diff(path: &str, added_lines: usize) -> String {
        let mut text = format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n"
        );
        for index in 0..added_lines {
            text.push_str(&format!("+line {index}\n"));
        }
        text
    }

    fn thread() -> ReviewThread {
        ReviewThread {
            id: "T1".into(),
            is_resolved: false,
            is_outdated: true,
            path: "src/a.rs".into(),
            diff_side: Some(DiffSide::Right),
            line: None,
            start_line: None,
            original_line: Some(4),
            subject_type: None,
            viewer_can_reply: true,
            viewer_can_resolve: true,
            comments: vec![ReviewComment {
                id: "C1".into(),
                author: Some(Actor {
                    login: "reviewer".into(),
                    avatar_url: None,
                }),
                body: "Cap the delay.".into(),
                created_at: String::new(),
                state: "SUBMITTED".into(),
            }],
        }
    }

    #[test]
    fn context_has_every_section() {
        let diff = file_diff("src/a.rs", 2);
        let context = build_context(&overview(), &diff, &[thread()], DIFF_BYTE_LIMIT);
        for expected in [
            "# Pull request #7: Add retries",
            "https://github.com/octo-org/widgets/pull/7",
            "retries -> main",
            "Retries failed requests.",
            "- src/a.rs (+2 -1)",
            "+line 1",
            "### src/a.rs:4 (outdated)",
            "reviewer: Cap the delay.",
        ] {
            assert!(
                context.contains(expected),
                "missing {expected:?} in\n{context}"
            );
        }
    }

    #[test]
    fn oversized_diff_is_truncated_per_file() {
        let diff = format!("{}{}", file_diff("src/a.rs", 400), file_diff("src/b.rs", 3));
        let context = build_context(&overview(), &diff, &[], 2_000);
        assert!(context.contains("- src/a.rs (+400 -1)"));
        assert!(context.contains("- src/b.rs (+3 -1)"));
        assert!(context.contains("more lines of src/a.rs omitted"));
        assert!(context.contains("diff --git a/src/b.rs"));
        assert!(!context.contains("+line 399"));
    }
}
