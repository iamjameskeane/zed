# Upstream sync

This fork adds GitHub pull request review to Zed. Keep the fork on the Dev release channel so `script/install-fork` installs `/Applications/Zed Dev.app`.

## 3 November 2026

| Item | Value |
| --- | --- |
| Stable release | [v1.26.2](https://github.com/zed-industries/zed/releases/tag/v1.26.2), published 28 October 2026 |
| Upstream commit | `c4273271092b0287f7d181f616bd5171e0a2f70c` |
| Previous fork head | `2ddf32a1b6` on `pr-review` |
| Previous upstream base | `b47a4ca595` |
| Method | Rebase the 13 fork commits onto the stable release |
| Conflicts | None |
| Main checkout snapshot | `wip/pr-changes-2026-11-03` at `5eb0b88d36fdc3513343efdb22c9be65fd9c0252` |

The stable tags now use `v1`, so a `v0.*` filter does not find the latest release. The initial `fetch upstream --tags` updated refs but rejected the existing moving `nightly` and `collab-staging` tags. A targeted stable-tag fetch avoids overwriting them.

The earlier sync branch, `sync/upstream-2026-10-28`, records a rebase onto `37faef81f7` in its reflog. Its named sync note was absent from both checkouts and branch trees. This note records the method for the next sync.

The main checkout's uncommitted patch changes how files open from the PR changes panel. It does not add folder folding. It remains on the WIP branch and is not included here.

## Repeat the sync

1. Save any uncommitted work without changing the running checkout. Use `git stash create` and give the returned commit a WIP branch.
2. Read the latest non-prerelease GitHub release. Fetch that exact tag from `upstream`.
3. In an isolated worktree, rebase the fork commits onto that tag. Do not change the running main checkout.
4. Retain the Dev channel, the fork features and the install scripts.
5. Run the targeted checks and tests under the `zed-build` machine lock, with six build jobs.
6. Run `script/install-fork`, then open a PR to `pr-review`. James reviews and merges it.

## Validation

Pass: `cargo check -p git_ui -p agent_ui -j 6` on macOS arm64. The check includes the folder folding work that follows this sync.

Pending: focused PR tests and one `script/install-fork` build.
