# Actup: A GitHub Actions Version Update Tool and LSP

## Overview and Motivation

Outdated GitHub Actions in workflows can lead to security vulnerabilities, inconsistent CI behavior, and maintenance
toil[^dev.to/anderson_leite].\
*Actup* is a Rust-based tool (edition 2024) comprising a CLI binary and a Language Server Protocol (LSP) component designed to automate
checking and updating of GitHub Action versions. Its goals are to **detect outdated action versions**, **provide configurable update
policies**, and **seamlessly integrate into both CI pipelines and developer editors**. By leveraging a modern Rust 2024 codebase and the
latest LSP capabilities (as of 2026), actup helps keep workflows up-to-date with minimal manual effort.

## Repository Layout and Technology Stack

The *actup* project is organized as a Rust 2024 workspace with multiple crates:

- `actup-core`: A library crate implementing core logic for parsing workflows, querying GitHub for latest versions, applying version
  policies, and caching. This crate is used by both the CLI and LSP.
- `actup-cli`: A binary crate for the command-line interface. This uses modern CLI libraries (e.g. Clap or similar) for argument parsing and
  outputs results to the console.
- `actup-lsp`: A binary (or library) crate implementing an LSP server using up-to-date libraries (such as `tower-lsp` or `lsp-server`) to
  integrate with editors.

The project uses **Rust 2024 edition** to leverage the latest language improvements. For example, the Cargo manifest specifies
`edition = "2024"`[^medium/selfint/lsp], ensuring the codebase benefits from updated Rust tooling and features. The choice of Rust
guarantees performance and safety, important for a tool that might run on every commit (via CI) or live in an editor providing feedback.

Additionally, the repository may include:

- **Editor/IDE Integration**: Configuration for packaging the LSP into an editor extension (e.g. a VSCode extension) that launches
  `actup-lsp`.
- **GitHub Action**: A reusable GitHub Action workflow or Docker container wrapping `actup-cli`, so organizations can run actup in CI (for
  example, on pull requests) to auto-detect outdated actions. (This is similar in spirit to how *actions-up* can be run in CI[^dev.to/anderson_leite]
  to generate reports on PRs.)

All components share the core logic to avoid divergence in results between CLI and LSP. The use of Rust 2024 and updated dependencies
ensures that actup is maintainable and aligns with current best practices (for instance, the latest LSP spec version 3.17+ is
supported[^medium/selfint/lsp]).

## Actup CLI: Features and Usage

The `actup` CLI provides commands to scan and update workflow files in a repository:

- `actup check`: Scans all GitHub Actions usage in the repository's workflow files and reports any outdated versions. By default, it looks
  at `.github/workflows/**/*.yml` (the standard location for GitHub Actions workflows[^github/dependabot]) and also any referenced reusable
  workflows or action metadata files. It parses each file for `uses: owner/repo@version` references. The output is a list of findings, for
  example:

<!-- -->

    .github/workflows/build.yml: actions/checkout@v2 → **OUTDATED** (Latest: v4)
    .github/workflows/deploy.yml: org/some-action@v1.2.0 → **OUTDATED** (Latest: v1.3.1)
    .github/workflows/deploy.yml: org/legacy-action@abcdef1 (pinned SHA) → **OK** (Up-to-date)
    .github/workflows/test.yml: owner/unknown-action@v1 → **ERROR** (Repository not found)

Each line shows the file and action reference, the current version, and either the latest available version or an error if the reference is
invalid/unresolvable. Colorful output or icons may be used for clarity (e.g. ⚠️ for outdated, ❌ for errors, ✔️ for up-to-date) when in a
TTY. The tool distinguishes update **severity**: e.g., marking major version updates separately because they may introduce breaking
changes[^dev.to/anderson_leite]. Minor/patch updates might be shown with a different symbol than major updates, to indicate relative risk
(for instance, **\"Update available (minor)\"** vs **\"Update available (major!)\"**).

- `actup update` (or `actup fix`): Optionally, a command to automatically update workflow files in-place. This command adheres to the
  configured policy (see **Configuration** section) to decide how far to upgrade each action. It will modify the YAML files, replacing
  version strings or SHAs with the new targets. By default, it might only perform non-breaking updates (patch/minor) unless an explicit flag
  (e.g. `--allow-major`) is provided. It will backup files or require `--yes` confirmation if not in a dry-run mode.

- **Common Flags**:

- `--config <path>`: specify a custom config file (overriding auto-discovery).

- `--dry-run`: show what would be updated, without modifying files (this is useful to get a report in CI without changing code, similar to
  how `actions-up --dry-run` is used[^dev.to/anderson_leite]).

- `--format=json|text`: machine-readable JSON output for scripting versus human-friendly text.

- `--strict` or `--fail-on-outdated`: exit with non-zero status if any outdated actions are found (helpful for CI enforcement).

- `--no-color`: disable colored output (for plain logs).

- **Scanning Scope**: By default, actup scans the standard locations (.github/workflows). It will also detect **reusable workflows**
  references. Reusable workflows are invoked with a syntax like `uses: owner/repo/.github/workflows/reusable.yml@v1` -- actup will treat
  these similarly to actions (the reference points to a workflow file in another repo at a given ref). According to GitHub docs, Dependabot
  also checks for reusable workflow references and updates them[^github/dependabot], so actup will ensure these are not missed.

- **Composite Actions**: If the repository contains local composite actions (i.e., action definitions in YAML with `uses` steps), actup can
  scan those as well. For example, a composite action in `.github/actions/my-action/action.yml` might use steps calling other actions by
  version. Actup will parse such action definition files and flag outdated references there too, ensuring even your custom actions stay
  up-to-date with the actions they rely on. (Note: If your repository *is* an Action repository intended for reuse, actup can still be run
  to check that the action is using latest versions of any base actions it calls.)

- **Performance**: The CLI uses caching (details below) to minimize network calls. When run with `--verbose`, it can log which external
  requests are made or which cache entries are used, helping debug any connectivity issues or confirm that updates are being checked
  properly.

By offering both a **read-only check** and an **auto-fix** command, the CLI lets users incorporate actup into their workflow as they see fit
-- either as a reporting tool (e.g. run it locally or in CI to get a report of outdated actions) or as a maintenance tool to actually bump
versions en masse. This is similar in spirit to existing tools like *actions-up*, which can detect outdated workflow actions and even update
them automatically[^dev.to/anderson_leite], but actup is designed as a native binary with deeper editor integration and more customization.

## Configuration Design (`actup.toml` or YAML)

Actup's behavior is customizable via a configuration file. The configuration governs update policies and preferences so that
*"checks/updates (configurably) the versions"* aligns with project needs. The config file (for example, named `actup.toml` in the repo root)
might look like:

    # actup configuration file
    updatePolicy = "minor"      # possible values: "patch", "minor", "major", "latest"
    pinning = "preserve"        # "preserve" to keep using tags vs sha as originally, "tag" to prefer tags, "commit" to prefer commit SHAs
    allowPrerelease = false     # whether to consider pre-release tags (like v2.0.0-beta) as updates
    ignore = [
      "some-owner/some-action", # actions to ignore completely
      "another/action@v1"       # specific pinned version references to ignore
    ]
    cacheDays = 1               # how many days to cache version info before checking remote again

Key configuration options:

- **Update Policy**: Defines the allowed version bump level. For example, `"patch"` means actup will only suggest or apply patch-level
  updates (e.g., v1.2.3 → v1.2.5) and ignore any minor/major newer versions. `"minor"` allows patch or minor bumps (v1.2 → v1.3) but not
  major (v1 → v2). `"major"` allows all updates including breaking changes. `"latest"` could mean always go to the latest version available
  regardless of version series (this might jump majors). This preference ensures organizations can, for instance, automatically get critical
  patches but delay major upgrades for manual review. (This is analogous to how dependency update tools often let you control what level of
  updates to apply automatically[^dev.to/anderson_leite].)

- **Pinning Strategy**: Controls how version references are written when updating. `"preserve"` means if the workflow originally used a tag
  like `@v2`, actup will continue using tag references for updates (e.g., update to `@v2.3.4` or `@v3` tag as appropriate). If the reference
  was a commit SHA, it will update to a new commit SHA. This preserves the user\'s choice of pinning style. Alternatively, the user might
  force a style: `"tag"` could mean always use version tags (convert any commit SHAs to the corresponding tag if one exists), whereas
  `"commit"` means always pin to an exact Git commit hash (for maximum immutability). For example, if `pinning = "commit"`, and an action
  reference `uses: actions/checkout@v4` is outdated, actup would find the latest v4 commit and replace it with
  `uses: actions/checkout@<commit-hash>` of that release -- ensuring a fully reproducible reference. Conversely, if a user had a SHA pinned
  but prefers tags, actup could replace the SHA with a tag if it matches a published version.

- **Allowed/Blocked Actions**: The `ignore` list (or its inverse `include` list) can refine what to check. You might ignore certain actions
  (maybe internal ones or those managed by other means) to reduce noise. Similarly, one could configure actup to only monitor certain
  actions if desired.

- **Cache Settings**: `cacheDays` (or a more granular TTL per action) defines how long to trust cached version information before hitting
  the network again. This is adjustable in case a user wants to ensure always-fresh data (set 0 to always refresh) or be more lenient if
  running in CI frequently (to avoid rate limits).

- **Config File Discovery**: Actup will look for a config file in standard locations:

- A project-specific config in the repository (e.g. `actup.toml` or `.actup/config.toml` in the root, or perhaps within `.github/`
  directory). It will search upward from the current directory, so it can be placed at the repo root or a workspace root.

- A global user config (e.g. `~/.config/actup/config.toml` following XDG Base Dir on Linux/Mac, or `%AppData%/actup/config.toml` on Windows)
  for user-specific defaults.

- Command-line `--config` can point to any file, which overrides the above.

Configuration is not required -- sane defaults exist. If no config is found, actup might default to, say, checking for any updates but only
auto-updating patch versions unless `--allow-major` is given, and default to preserving pin styles. The config simply makes this behavior
explicit and tweakable per project or user.

Actup's design ensures that a team can set a repo policy via the config (e.g., "only apply non-breaking updates automatically") and
developers' editors and CI will all follow that policy. This avoids inconsistency, and it contrasts with GitHub Dependabot's all-or-nothing
approach of always PR'ing the latest version[^github/dependabot]. For example, Dependabot will open a PR for any new version (including
majors) by default[^github/dependabot], whereas actup can be configured to ignore major bumps and only flag them for manual consideration.

## Version Detection and Caching Mechanism

**Identifying Latest Versions**: For each action reference found, actup needs to determine if a newer version exists. This involves
interacting with the remote GitHub repository of the action. The process is:

1. **Parse the current reference**: Determine if it's a semantic version tag (e.g. `v2.3.4`), a major tag (`v2`), a branch name (`@main` or
   custom branch), or a commit SHA.

2. **Fetch repository tags/versions**: Using GitHub's API (or git methods), retrieve the list of available versions. This can include:

3. Listing Git tags (especially those matching the semver pattern). The GitHub API provides endpoints for listing tags or releases. For
   example, actup might use the REST API `GET /repos/{owner}/{repo}/tags` to get tags and commit SHAs, or the GraphQL API to get the latest
   release for a given major version.

4. Checking if the reference is a branch: in that case, get the latest commit SHA of that branch (to compare if the branch has moved).

5. If the reference is already a commit SHA, one strategy is to see if that SHA corresponds to a known tag or release, and if not, we might
   consider it as a pinned commit. For pinned commits, \"outdated\" could mean the action repo has new commits on the default branch or a
   tagged release that the user might want to move to (this is tricky; typically pinned commits are considered static, but actup could
   optionally inform if there's a later tag or commit available).

6. **Determine the \"latest\" allowed version**: Based on the config's updatePolicy, filter the available versions. For example, if current
   is v2.3.4 and updatePolicy is \"minor\", actup will consider any v2.x version \>2.3.4 (2.4.0, 2.5.0, etc) but ignore v3+. It will pick
   the highest 2.x version available as the target. If updatePolicy is \"major\" or \"latest\", it would consider v3 if present and decide
   the highest version overall.

7. **Compare**: If the target (latest allowed) version is different (newer commit) than the current, mark as outdated. If it's the same
   commit (even if tag name differs), consider it up-to-date. This addresses cases where multiple tags point to the same commit: for
   example, if `v1.2.3` and `v1.2.4` tags both refer to commit X (perhaps the maintainer moved a tag or made a mistake), and the user is on
   `v1.2.3`, actup would see that the \"latest\" tag is effectively the same code and not flag it. Conversely, if `v1` is a floating tag
   that currently points to `v1.2.3` commit and a `v1.2.4` exists but the maintainer hasn't updated the `v1` tag, a user on `@v1` might
   *not* automatically get the new patch. Actup in that scenario could flag that a specific newer tag exists (suggesting the user pin to
   `v1.2.4` or expect the maintainer to move `v1`). Essentially, actup treats the combination of tag and commit as the version state. It
   warns if there\'s a newer commit/tag that **would be fetched** if the reference was updated.

8. **Branches**: If the action is referenced by a branch (e.g. `@main` or `@dev`), the concept of \"latest version\" is usually not
   applicable since the branch is a moving target. In this case, actup by default will not consider a branch reference \"outdated\" in the
   semantic version sense (since it always pulls latest on each run). However, many organizations discourage using branches for actions due
   to stability/security concerns. Thus, actup can optionally flag branch references as **policy violations** or warnings (configurable).
   For example, the config might allow a setting `warnOnBranch = true` to mark any non-pinned reference with a warning like \"Unpinned
   action reference; consider using a tag or commit for stability.\" This way, users get a nudge to pin versions.

9. **Rate Limiting and Auth**: GitHub's API has rate limits. Actup will use **unauthenticated requests** by default (sufficient for moderate
   use, but limited to 60 requests/hour). To handle large repositories or org-wide scans, actup allows providing a GitHub token (via env var
   or config) to increase limits. The caching strategy (below) also mitigates hitting these limits by reusing data.

**Caching Strategy**: To avoid repeatedly querying GitHub on every run or for every file, actup implements caching of version info:

- The cache could be stored on the local filesystem, e.g. under the user's cache directory (`~/.cache/actup/` on Linux, or appropriate
  OS-specific cache dir). Within this, entries might be keyed by action repository. For example, `~/.cache/actup/actions_owner_repo.json`
  could store the last known version info for `owner/repo`. This JSON might include the latest version per major series, or a full list of
  tags with timestamps.

- Alternatively (or additionally), a project-local cache might be kept (for example in `.git/actup-cache` or `.github/actup-cache.json`). A
  local cache could be beneficial for CI usage: if the cache file is checked into the repo or persisted between CI runs, it could prevent
  each CI run from hammering the API. However, storing in the repository has downsides (staleness, merge conflicts), so by default the cache
  is likely just on local machine.

- When actup runs, for each distinct `owner/repo` it encounters, it will:

- Check if we have a cached entry and if it's fresh (not older than `cacheDays`). If valid, use that to determine latest versions.

- If no cache or expired, query GitHub API for that repo's tags/releases, then update the cache with the new info (including a timestamp).

- The cache stores minimal necessary info: possibly the highest version for each major (so we can quickly decide if a major bump exists) and
  the highest overall, plus mapping of tag names to commit SHAs for comparing equality. Storing commit SHAs allows actup to know if e.g.
  `v1` tag moved since last check or if two different tags are actually the same commit.

- **Cache Invalidation**: The `cacheDays` config (or a more explicit TTL) controls when to refresh. Users can also manually refresh by
  running `actup update --no-cache` or similar. The LSP (which runs continuously) might periodically refresh cache in the background or on a
  new editor session to ensure it isn\'t showing very stale data.

This caching approach is crucial for performance and avoiding rate limits, especially if *actup-lsp* is providing real-time feedback in an
editor (you wouldn\'t want it to call the GitHub API for every keystroke). Instead, it will likely do one fetch per action repo per day
(configurable), caching results across all open projects.

## Handling Special Cases and Version Nuances

Real-world GitHub Action versioning can be messy. Actup is designed to handle several tricky scenarios:

- **Floating Major Tags**: Many actions use a strategy of moving a major tag (e.g. `v1`) to always point to the latest minor/patch in that
  series. If a workflow uses `uses: foo/bar@v1`, they will automatically get updates whenever `v1` tag is retargeted by the maintainer.
  Actup's default stance is that such a reference is *not* outdated as long as the tag currently encompasses the latest code for major 1.
  However, if the maintainer has released `v1.2.0` and updated `v1` to point to it, a user referencing `@v1` is inherently up-to-date. If
  the maintainer releases `v2.0.0`, the `v1` tag won't move (stays on latest v1.x), so there *is* a newer major version out there. Depending
  on the updatePolicy, actup might:

- If policy allows major: flag `v1` -\> `v2` update available.

- If not, perhaps just inform that a new major exists but is not auto-updated due to policy.

- **Tags not updated**: In some cases, maintainers do *not* update the floating tag. For example, they might leave `v1` pegged to the
  initial release and expect users to manually bump to `v1.1.0`, etc. If actup finds that `v1` tag in the repo is still at an old commit
  while there are newer `v1.x` tags, it will flag that the reference `v1` is effectively outdated (even though `v1` itself *is* the
  reference). It may suggest the specific latest tag. This scenario is subtle: essentially, actup detects that the user is on a broad tag
  that hasn't moved to the latest patch. The tool can either treat that as outdated (since a newer release in that series exists) or
  possibly give a separate warning \"maintainer released v1.1.0 but \'v1\' tag is still at v1.0.0 -- consider pinning the new version.\"

- **Multiple Tags for Same Commit**: If an action repo has multiple tags that point to the same commit (e.g., `v2.0.0` and `v2.0` might both
  exist, or a maintainer might create both `v1.4` and `v1.4.0` tags for convenience), actup handles this by comparing commit hashes. It
  avoids suggesting an update when the code is identical. For example, if a workflow uses `@v1.4` and the maintainer also has `v1.4.0` (same
  commit) and no newer code, actup won't incorrectly suggest that `v1.4.0` is newer -- it will recognize they are the same underlying
  version. This prevents redundant or oscillating updates.

- **Pre-releases and Non-Semver**: Some action tags might be dates (`2023-07-01`) or non-semver strings. By default, actup focuses on semver
  patterns, but it could be extended to handle other schemes. Pre-releases (like `v2.0.0-beta`) might be ignored unless explicitly allowed
  via config (`allowPrerelease`). If allowed, actup could consider a beta as an update (useful if the user is already on a pre-release
  track).

- **Unresolvable References**: If an action reference is broken (e.g., repository does not exist or has been renamed, or the tag/branch
  doesn't exist), actup reports this. In CLI, it marks as error; in LSP, it will show a red underline and an error diagnostic. This helps
  catch typos or cases where an action was yanked. For example, `uses: actions/checKout@v3` (typo in \"checkout\") would result in an error
  \"Action not found\". The tool may attempt some heuristics (like suggesting the correct casing if that's the issue, since GitHub
  owner/repo names are case-insensitive but actions require correct casing in workflows).

- **Network Issues & Offline Mode**: If the GitHub API is unreachable (network down or rate-limited), actup should fail gracefully. In CLI,
  it could warn \"Could not fetch latest version info at this time\" and possibly exit with a specific code. In LSP, it could either use
  cache only or mark diagnostics in a way to indicate they might be stale. It will not mark everything as error (to avoid false positives
  when offline). Perhaps a notification is presented to the user if the LSP cannot reach GitHub (e.g., \"Actup: Using cached data, updates
  may be outdated due to network issues\").

## Composite Actions and Reusable Workflows

Actup fully supports composite actions and reusable workflows:

- **Composite Actions**: As noted, if you have multiple composite actions in one repository, they all share the same version tags (since git
  tags apply to the entire repo)[^earthly/composite-actions]. Actup's version detection accounts for this: a new tag will be considered an
  update for *all* actions in that repo. For example, if `org/composite-repo@v1` is used (and that repo contains `actionA` and `actionB` as
  composite actions), when the maintainer releases `v1.1`, both `actionA` and `actionB` are effectively at v1.1 now, even if maybe only one
  of them changed. Actup does not try to distinguish which composite changed; it simply tracks the repo version. This is in line with
  GitHub's model where the whole repository is versioned as a unit[^earthly/composite-actions].

- **Reusable Workflows**: These are workflows defined in one repo and invoked in another via `uses:`. They behave similarly to composite
  actions in terms of referencing (`owner/repo/path/file.yml@ref`). Actup treats them the same as any action reference: it will check if a
  newer commit or tag exists for that reusable workflow file. Usually, reusable workflows might be versioned with tags or rely on the
  repository's tags. Actup will ensure that if your workflow calls a reusable workflow at `@v1`, it catches if there\'s a `v2` or updated
  `v1` tag.

By including composite actions and reusable workflows in the scan, actup provides a comprehensive update check for all GitHub Action-related
references in a repository. This is more thorough than some built-in tools; for instance, Dependabot does handle reusable
workflows[^github/dependabot], and actup matches that capability, while giving you local control over how and when to update.

## Actup LSP: Editor Integration and Guidance

The companion **actup-LSP** brings these checks directly into the developer's editor/IDE. It\'s essentially a language server focused on
GitHub Actions workflow files (YAML). By using the Language Server Protocol, actup-LSP can provide real-time diagnostics and fixes in any
LSP-compatible editor (VSCode, Neovim with LSP client, etc.). This approach follows the modern trend of using LSP for specialized static
analysis beyond programming language syntax[^medium/selfint/lsp]. The LSP operates roughly as follows:

- **Activation**: The LSP server activates for files matching the typical GitHub Actions workflow patterns (e.g., any YAML in a
  `.github/workflows` directory, or perhaps any file named `action.yml`/`workflow.yml`). The VSCode extension (for example) would register
  these file globs and spawn actup-lsp for those.

- **Diagnostics (Squiggly Underlines)**: When a workflow file is opened or saved, the LSP parses it (using the same core logic as the CLI)
  and identifies any outdated or invalid action references. It then sends diagnostic messages to the editor. Examples of diagnostics:

- **Warning** on an outdated version: e.g., underlining `@v2` in `uses: actions/checkout@v2` with a message \"Newer version available:
  v4[^dev.to/anderson_leite]\". This would likely be a *warning* severity if it\'s a minor/patch update, or maybe *information* severity for
  patch, and *warning* for major updates -- severity can be configurable.

- **Error** on an invalid reference: e.g., underlining the whole `uses: owner/unknown@v1` with \"Action repository not found\" or
  `uses: foo/bar@badtag` \"Version tag \'badtag\' not found\". Errors indicate something that definitely should be fixed by the user (typo
  or repository moved).

- **Hint/Info** on a policy issue: e.g., a faint underline on `@main` (branch reference) with a tooltip \"Unpinned action reference --
  locking to a version is recommended for stability.\" This can be informational, depending on user settings.

The diagnostics update live as you edit. If you change a version in the file, the LSP will re-evaluate and clear or update the diagnostic
accordingly. This immediate feedback loop ensures developers catch outdated actions even before pushing code. It's akin to having a linter
that reminds you of dependency updates. In fact, similar tools exist for dependencies (for example, the *Depramanager* VSCode extension
highlights outdated dependencies in config files[^github/depramanager]), and actup-LSP provides that capability specifically for GitHub
Actions versions.

- **Code Actions (Quick Fixes)**: Perhaps the most powerful feature of the LSP is offering one-click fixes. When an outdated action version
  diagnostic is present, actup-LSP registers a **Code Action** for it. In editors like VSCode, this appears as a lightbulb or quick-fix
  suggestion. The user can click \"Update to latest version\" (or a specific version). Actup-LSP will then apply the edit to the text,
  changing `@v2` to `@v4` (or whatever the appropriate target per config). This uses the config policy: e.g., if policy is minor-only and a
  major is available, the quick fix might say \"Update to v2.3.4\" (latest minor) instead of v3. If multiple fixes are possible (say both a
  minor and a major are available and policy allows major but with caution), the LSP might offer a choice of versions in the quick action
  menu.

Additionally, a code action could be provided to **pin to SHA** for security. For example, if the config/policy suggests pinning, the quick
fix could read \"Update to v1.2.3 (commit 1a2b3c4)\" which replaces the reference with a full length SHA. This gives developers flexibility
to choose how to update.

- **Bulk Fixes**: The LSP could implement a custom command to fix all outdated actions in the file or even the workspace. For instance, an
  editor command \"Actup: Update all actions in workspace\" could instruct the server to iterate through all open (or all known) workflow
  files and apply updates. This might generate multiple edits; LSP supports WorkspaceEdit that can modify multiple files in one go. This
  essentially mirrors running `actup update` from the CLI, but initiated in-editor for convenience.

- **Hover/Completion**: Although not asked, the LSP could also provide a *hover tooltip* on an action reference to show additional info,
  like \"Current version: v1.2.0; Latest: v1.3.1 (minor update available)\". It might even list the release date of the new version or
  changelog link if available, to help the developer decide on update (some of this data could be fetched from GitHub releases info).
  Autocomplete for action versions is another possible feature: when typing `uses: actions/checkout@`, the LSP might suggest `v4` if it
  knows v4 is latest, or suggest tags like `v3`, `v2` that exist. However, these are extended features beyond the core ask of version
  checking/updating.

- **LSP Configuration**: The LSP will read the same config file (actup.toml) for consistency. Additionally, editor-specific settings could
  override or complement this. For example, a developer could have an editor setting to temporarily silence warnings, or the organization's
  actup.toml might be enforced. The LSP should honor the effective config so that it doesn\'t, say, suggest a major update when the policy
  forbids it.

- **Performance in LSP**: To avoid lag in the editor, the LSP must use caching and possibly asynchronous checks. On opening a file, it might
  initially use cached info to mark anything known, and then in the background refresh any data that\'s stale. The diagnostics might update
  after a short delay once new data comes in. This asynchronous design ensures that editors remain responsive. The use of Rust and
  asynchronous networking (with e.g. Tokio under the hood) helps keep the LSP fast.

- **Resilience**: The LSP handles the same error cases gracefully. If offline, it may either use last known info or show a notification
  rather than marking everything as error. It will also coalesce diagnostics to not overwhelm the user; e.g., if 5 actions in a file are
  outdated, it will list each but the editor can show them individually.

By integrating with LSP, actup essentially becomes a **real-time advisor** for CI workflow maintenance. This is a step beyond periodic tools
like Dependabot, giving immediate insight. It aligns with the idea that LSP can power cross-cutting static analysis tools[^medium/selfint/lsp]
-- here, the static analysis is on configuration files (YAML) to enforce DevOps best practices.

## CLI and LSP Synergy

Both the CLI and LSP provide overlapping functionality in different contexts. They share a core, ensuring consistency. Some usage scenarios
and how they complement each other:

- A developer coding a workflow gets instant feedback from **actup-LSP** about outdated actions and can fix them on the spot (with quick
  actions).
- Periodically, a CI pipeline might run `actup check` (perhaps via a scheduled job or on pull requests) to fail builds if critical updates
  are missing. For example, if a severe security issue is discovered in an action version, the team could bump the updatePolicy to allow
  that patch and let actup auto-fix or at least alert in CI. This automation is similar to the approach described by Anderson Leite, where
  outdated actions across many repos were detected and even auto-PR\'ed using a tool[^dev.to/anderson_leite] -- actup could facilitate the
  same at an individual repo level or be scripted across repos.
- When a new major release of some action comes out, Dependabot might open a PR; with actup-LSP, developers would also start seeing warnings
  in their editor if they\'re on the old major, prompting a more proactive discussion whether to upgrade.

In summary, the CLI is great for **automation and batch updates**, while the LSP is great for **interactive, on-the-fly guidance**. They
ensure that at no point a workflow file falls far behind the latest recommended versions without the team knowing.

## Conclusion

Actup and actup-LSP together form a comprehensive solution for managing GitHub Action versions. By designing the tool with a modern Rust
2024 codebase and following latest LSP practices, we ensure it is efficient and easy to integrate. The configurable nature (update policies,
pinning preferences) addresses the nuanced needs of different projects -- whether you want ultra-conservative updates or to always live on
the bleeding edge of actions. With caching and careful handling of edge cases like floating tags and composite actions, actup provides
intelligent feedback rather than naive version bumps.

This design takes inspiration from existing solutions (e.g. GitHub Dependabot's automated updates[^github/dependabot] and community tools
like actions-up[^dev.to/anderson_leite]) but goes further by integrating into developers' daily workflows via an LSP. The result is a
developer-friendly tool that keeps CI workflows secure, up-to-date, and consistent across projects with minimal manual effort -- a
significant productivity and security win for teams using GitHub Actions.

---

[^dev.to/anderson_leite]: Automating GitHub Actions Updates Across Your Entire Organization - DEV Community\
    <https://dev.to/anderson_leite/automating-github-actions-updates-across-your-entire-organization-17mj>

[^medium/selfint/lsp]: LSP outside the editor. Using the language server protocol in... | by Tom Selfin | Medium\
    <https://medium.com/@selfint/lsp-outside-the-editor-431f77a9a4be>

[^github/dependabot]: Keeping your actions up to date with Dependabot - GitHub Docs\
    <https://docs.github.com/en/code-security/how-tos/secure-your-supply-chain/secure-your-dependencies/keeping-your-actions-up-to-date-with-dependabot>

[^earthly/composite-actions]: Understanding and Using Composite Actions in GitHub - Earthly Blog\
    <https://earthly.dev/blog/composite-actions-github/>

[^github/depramanager]: GitHub - Okerew/depramanager: The Depramanager is a Visual Studio Code (VSCode) extension designed to help
    developers manage dependencies\
    <https://github.com/Okerew/depramanager>
