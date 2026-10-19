# Designing **actup**: A CLI & LSP Tool for Updating GitHub Actions

## Overview and Motivation

**actup** is a dual-component tool (a command-line interface and a Language
Server Protocol service) that helps developers keep their GitHub Actions
up-to-date. It scans a repository's workflow files and composite actions to find
all references to actions (in the form `uses: owner/repo@version`), checks if
newer versions are available, and assists in updating them. This addresses the
tedious maintenance of tracking action updates and improves security by
encouraging pinned versions. Outdated actions pose security risks
(vulnerabilities in old versions) and reproducibility issues, since GitHub
Actions tags (like `v1` or `v2`) are mutable and can be retagged to different
code[^github/azat-io/actions-up]. GitHub itself **does not automatically handle
semantic version ranges** for actions; it simply fetches the exact ref
specified[^xebia/versioning]. Maintainers might not update broad tags (e.g.
`@v3`) to the latest release[^xebia/versioning], so a workflow using
`uses: action@v3` could silently stay on an older release. For these reasons,
**actup** aims to streamline updates and promote best practices (like pinning to
immutable commits for security[^github/security]).

## Repository Structure and Technology (Rust 2024)

The **actup** project is implemented in Rust (Edition 2024) to ensure modern
language features and tooling. The repository is organized as a Cargo workspace
with multiple crates for clear separation of concerns:

- `actup-core` (library crate): Implements core functionality – scanning files,
  parsing references, querying GitHub for versions, and computing update
  recommendations. This crate is used by both the CLI and LSP components,
  ensuring consistent logic.
- `actup` (binary crate): The CLI tool that developers run in their repo to
  check and apply updates.
- `actup-lsp` (binary crate): The language server that editors can use for
  on-the-fly diagnostics and quick-fixes.

Using a workspace allows sharing of models (e.g. a structure representing an
action reference and its latest version) and utilities (HTTP clients, caching
logic, YAML parsing) between the CLI and LSP. Rust 2024 edition provides
improved async support and other language enhancements, which benefit actup's
network calls and concurrency. We use robust libraries: for example, **Clap**
(for CLI argument parsing) to build a user-friendly command interface, and an
async **LSP framework** (such as `tower-lsp` or its 2026 equivalent) to
implement the language server. Both components are developed following current
best practices (as of 2026) for command-line UX and editor integration.

## Discovering Workflow and Action Files

By default, actup scans conventional locations for GitHub Actions usage:

- **Workflow files** in `.github/workflows/*.yml` or `.github/workflows/*.yaml`.
  These define CI workflows and often contain steps using third-party actions.
- **Composite actions** in `.github/actions/*/action.yml` or
  `.github/actions/*/action.yaml`. These are reusable actions defined within the
  repository, which may call other actions via `uses:`
  statements[^github/azat-io/actions-up].
- **Reusable workflow references** inside workflows: actup will detect if a job
  uses a reusable workflow (e.g.
  `uses: org/repo/.github/workflows/reusable.yml@v1`). These references are
  treated similarly to action references.

The tool may allow configuration of additional scan paths if a repository uses
non-standard locations (for example, some self-hosted Git platforms use `.gitea`
or other directories[^github/azat-io/actions-up]), but the above are defaults.

For each discovered file, actup parses the YAML to find all occurrences of the
`uses: ...@...` syntax. It handles all reference types the GitHub Actions runner
supports: a reference can be a **tag** (e.g. `v2` or `v2.3.4`), a **branch
name** (e.g. `@main` or `@release/v1`), or a specific **commit
SHA**[^xebia/versioning]. Local paths (`./`, `../`) and same-repository
references (`$/`) are skipped for actions and reusable workflows: they have no
external version to check, update or pin. Each external action reference is
extracted as a tuple of **{owner}/{repo}** and a **version identifier**
(tag/branch/SHA).

## Version Checking and Update Strategy

For each action reference found, actup-core will determine if a newer version is
available based on the user's update preferences. This involves querying the
source repository of the action for available versions:

- **Fetching available versions**: actup uses the GitHub API (or git data) to
  list tags and releases of the action's repository. It may use an authenticated
  request (if the user provides a GitHub token) to increase rate limits. The
  tool is optimized to avoid duplicate lookups – it will query each unique
  action at most once per run, even if used multiple
  times[^github/azat-io/actions-up]. Results are cached (details in the Caching
  section) to prevent redundant network calls.

- **Interpreting semantic versions**: GitHub Actions typically use semantic
  version tags (vMAJOR or vMAJOR.MINOR.PATCH). However, GitHub does *not*
  automatically resolve a partial version to the latest matching tag – e.g.
  `uses: foo/bar@v2` will fetch the tag `v2`, not `v2.3` if that
  exists[^xebia/versioning]. It's up to the action maintainer to keep the `v2`
  tag updated[^xebia/versioning]. actup's logic will simulate "semantic version"
  resolution: if the current reference is a major-only tag like `v2`, actup will
  check if there are any newer `v2.x` tags that the maintainer created but
  perhaps didn't move `v2` to. For example, if a workflow uses
  `actions/checkout@v3` and the latest release is `v3.5.2` but the `v3` tag
  still points to an older commit, actup will recognize `v3.5.2` as a newer
  version. Similarly, if a full version `v3.1.0` is in use and a `v3.2.0`
  exists, that is a newer minor version. Essentially, actup compares the current
  tag against the list of available tags by semantic version ordering.

- **Update preference policies**: The tool is configurable to decide what
  constitutes an update suggestion. The user can specify a policy for allowed
  version bumps:
  - **Patch** – update to the latest patch version only (within the same
    MAJOR.MINOR series).
  - **Minor** – update to the latest minor release within the same major version
    (includes patch updates as well).
  - **Major** – allow updating to a new major version if available.
  - **All (Any)** – always update to the absolute latest version, regardless of
    major version changes.

For example, if an action is at `v2.3.1` and versions up to `v2.5.0` exist:
under a **patch** policy it would suggest at most `v2.3.x` (latest patch), under
**minor** it would suggest `v2.5.0`, and under **major/all** it might suggest
`v3.x` if a v3 is out. These correspond to behaviors similar to other updaters;
for instance, the GitHub Actions Version Updater action allows selecting update
types (major/minor/patch/all)[^github/actions-version-updater].

Additionally, a policy can define how to handle branch-based versions. By
default, actup will **not auto-update** a branch ref like `@main` since that is
meant to float (and pinning it would defeat the purpose of a rolling
branch)[^github/azat-io/actions-up]. Branch references are reported as
"non-pinned" in output, and only updated if explicitly configured (e.g. a user
can opt-in via config to treat branch refs as updateable, perhaps pinning them
to a specific commit or updating to latest commit on that branch).

- **Pinning strategy**: actup strongly advocates immutable references for
  security. By default, it will propose updates that **pin to a full commit
  SHA** instead of a movable tag. Pinning to a commit ensures the action version
  can't change under you[^github/security]. For example, if a workflow uses
  `uses: org/action@v1.2.3`, actup might update it to
  `uses: org/action@<commit-sha>` corresponding to the latest release. This
  follows GitHub's security recommendations ("Pinning an action to a full-length
  commit SHA is currently the only way to use an action as an immutable
  release"[^github/security]). Some users may prefer staying on version tags for
  readability; actup can allow configuring the update output format: **semver
  tag vs. commit SHA**. For instance, a user could set "pinning: false" in
  config to update to the new tag name instead of a SHA. But by default (or at
  least strongly encouraged), commit SHAs with a comment indicating the version
  (e.g. `# v1.2.4`) will be used for maximum
  security[^github/azat-io/actions-up].

- **Composite actions**: If the repository contains composite actions
  (.github/actions), actup will also update any `uses:` lines inside those.
  Composite actions often invoke other actions; keeping those up-to-date is as
  important as updating workflows. The scanning and version resolution apply
  similarly to those files.

- **Handling unresolvable versions**: If the specified version ref in the
  workflow doesn't exist in the source repo (e.g., a tag was mistyped or
  deleted), actup will flag it as an error. The tool will attempt to find the
  closest match or latest release to suggest as a fix. If the failure to resolve
  is due to network issues or API limitations, actup distinguishes that case and
  reports it differently (so as not to incorrectly blame the version string). In
  offline or rate-limited scenarios, it may mark the status as unknown or warn
  that it couldn't verify latest versions.

- **Multiple tags for the same commit**: Occasionally, maintainers might have
  multiple tags pointing to one commit (for example, `v1.0.0` and `v1` might
  initially point to the same commit). actup's update logic will detect if the
  current version's commit hash equals the latest version's commit hash – in
  such a case, it means there is no actual code change, so it won't suggest an
  "update" that only changes the tag alias. This prevents churn if, say, `v1` is
  an alias and a new tag `v1.0.0` is introduced pointing to the same commit;
  actup would recognize that as effectively up-to-date. Only genuine new commits
  trigger an update suggestion.

## Configuration File Design

To customize its behavior, **actup** supports a configuration file (YAML or
TOML) that can live in the repository. We recommend naming it `actup.yaml` (or
`.actup.toml` in Rust style) at the root of the repo. The tool will discover the
config by looking in the current directory and upwards (similar to how other
tools like `cargo` or linters find configs). The config schema includes:

- **Update policy**: The semantic version bump level allowed. This can be a
  single value or a list of allowed release types. For example:
  - `update_policy`: `minor` # options: `patch` | `minor` | `major` | `all`

    This means actup will suggest minor or patch updates, but not jump to a new
    major version. Multiple could be specified (e.g. `["minor", "patch"]`), but
    a single string or enum covers most cases.\
    By default, this might be `all` (to catch everything) or `minor`
    (conservative by not breaking major).

    In GitHub's own updater action, the default was to update any new
    release[^github/actions-version-updater], but users of actup can tailor
    this.

- **Pinning preference**: A boolean or mode setting to control version reference
  style. E.g.:

  - `pin_to`: `commit` # options: `commit` | `tag`

    If set to "commit", actup will convert version tags to full commit SHAs in
    updates (with maybe a comment of the tag for clarity). If "tag", it will use
    the latest tag name. (There could also be a mode "commit-if-major" to only
    pin when doing major updates, etc., but that adds complexity.)

- **Include branches**: By default false, meaning references that point to a
  branch (like `@main` or any branch name) are not considered outdated (since
  they float). If true, actup will treat branch references as candidates for
  updating (likely pinning them to the latest commit of that branch or at least
  warning that they are not stable). For example:

  - `include_branches: false`

- **Ignore list**: A list of actions (owner/repo or specific owner/repo@version
  identifiers) to exclude from checks. This is useful if certain actions should
  not be touched (perhaps they are intentionally kept at an older version due to
  compatibility). For example:

  ```yaml
  ignore_actions:
    - actions/checkout # ignore all versions of this action
    - my-org/legacy-action@v1.0.0 # ignore this specific reference
  ```

- **Custom paths**: If workflows or actions are in non-standard locations,
  config can provide patterns or directories. E.g.:

  ```yaml
  paths:
    - "ci/workflows/*.yml"
    - ".github/custom-actions/*/action.yaml"
  ```

  If not set, actup uses the defaults as mentioned earlier.

- **Cache settings**: Possibly allow configuring cache location or refresh
  interval (though the tool can have sensible defaults). For instance:

  ```yaml
  cache:
    ttl_days: 1 # refresh cached version info if older than 1 day
    path: "~/.cache/actup" # override cache directory
  ```

The configuration file is optional. actup will operate with default behaviors if
no config is present. The CLI can also override config settings with flags (e.g.
`--policy major` or `--no-pin`) for one-off runs. This design ensures a balance
between **consistency** (a project can commit an `actup.yaml` to enforce how
updates are managed) and **flexibility** (individual runs can adjust behavior).

## Caching Mechanism

Fetching version info from remote GitHub repositories introduces latency and
potential rate limiting. **actup** employs a caching layer to store the latest
known version data for actions. The caching strategy includes:

- **Cache Scope**: A global local cache is maintained (for example, under the
  user's home directory in an appropriate cache folder, e.g. `~/.cache/actup` on
  Linux or `%AppData%\Actup\cache` on Windows). This way, if multiple projects
  use the same action (like `actions/checkout`), actup doesn't fetch tags
  repeatedly. A per-repo cache (e.g. in `.git/actup-cache`) could also be used
  for portability, but global cache avoids duplication and benefits multi-repo
  usage. We opt for a global cache by default, with the path configurable as
  noted.

- **Cache Contents**: The cache stores, for each action **{owner}/{repo}**, the
  information about available versions. This might include:

- The latest commit SHA of the default branch (for branch update option).

- The list of tags (or at least the highest semver tag and latest commit for
  each major version).

- Timestamp when this data was last fetched.

For example, an entry could look like:

```json
"actions/checkout": {
    "latest_release_tag": "v3.5.2",
    "latest_release_sha": "<sha1>",
    "majors": {
        "2": { "latest_tag": "v2.4.0", "latest_sha": "<sha2>" },
        "3": { "latest_tag": "v3.5.2", "latest_sha": "<sha1>" }
    },
    "default_branch": { "name": "main", "latest_sha": "<sha3>" },
    "fetched_at": "2026-01-12T10:00:00Z"
}
```

This is an illustrative schema – in practice, storing the full tag list might be
unnecessary; we can compute latest on fetch and store just the needed data (to
minimize size). Multiple tags pointing to the same commit would naturally have
the same SHA recorded, which we can use to detect the "no change" scenario as
discussed.

- **Cache Invalidation**: actup will refresh the cache for an action if the data
  is older than a certain threshold (e.g., one day by default) or if explicitly
  forced (via a CLI flag or when user triggers a manual refresh in the LSP).
  This TTL balances freshness with not hammering the GitHub API. Also, if a user
  runs `actup` with a `--no-cache` flag, it will ignore cached data and fetch
  everything anew.

- **Storage**: The cache is stored in a simple format (JSON or TOML) for easy
  inspection. We avoid using the `.git` directory for caching to not interfere
  with version control; instead, the cache lives outside the repo, though an
  alternative mode could allow project-local caching for isolated environment
  use-cases.

- **Concurrency & Consistency**: Both the CLI and LSP components will utilize
  this cache. To avoid race conditions (for example, the LSP checking an action
  version in the background while the CLI is also updating it), access to the
  cache can be synchronized via file locks or in-memory locks if both are
  running simultaneously. In most cases, they won't collide, but it's a
  consideration if a user has an editor (LSP) open and also runs the CLI.

## Handling Special Cases

GitHub Actions versioning has quirks that actup handles gracefully:

- **Floating tags and outdated majors**: If an action uses a major-only tag like
  `@v1`, and the maintainer has released `v1.5.0` but not moved the `v1` tag,
  actup will still detect the newer version. It treats the numeric part of tags
  semantically, rather than trusting tags to be up-to-date[^xebia/versioning].
  In this case, actup would suggest updating to `v1.5.0` (or its commit) even
  though `v1` tag hasn't changed. This ensures you're not stuck on an old
  release due to a lazy maintainer.

- **Branch references**: As noted, these are by default skipped or noted but not
  auto-fixed (since some teams intentionally use a branch like `@main` to always
  run the latest code of an action). If `include_branches` is enabled, actup
  will consider the latest commit on that branch as the "update". For example,
  if `uses: org/action@dev-branch` is in the workflow, actup can fetch the HEAD
  commit of `dev-branch` and offer to pin to that commit. This gives the choice
  of locking it for stability. Otherwise, it simply warns that the reference is
  floating.

- **Missing tags**: If a workflow tries to use a partial version (like `@v2.1`)
  which is not a real tag (as in the Xebia blog example, `v3.1` for checkout
  doesn't exist[^xebia/versioning]), the runner would error. actup will flag
  this as an error and suggest a correction (perhaps the user intended `v3.1.0`
  or just `v3`). This is a user error scenario that actup can catch early via
  its LSP diagnostics.

- **Multiple tags on same commit**: When comparing current vs latest, actup as
  mentioned checks commit hashes. If the "latest" tag is just an alias to the
  same commit as the current tag, it will not prompt an update. If an action
  hasn't released a new commit in a long time, actup will show it as up-to-date,
  even if the maintainer introduced a new tag pointing to the same commit (no
  functional difference).

- **Pre-releases and non-semver tags**: Some action repositories might use
  pre-release tags (like `v2.0.0-beta` or arbitrary tags not following semver).
  actup by default will focus on stable semver tags. It could be configured to
  consider pre-release versions if the user wants (e.g., an option
  `allow_prerelease: true`). Otherwise, it will ignore tags that look like
  pre-releases for update suggestions, unless the current version is itself a
  pre-release. If an action doesn't use semver at all (rare, but say they use
  date-based tags), actup will treat versions lexicographically or by release
  date to find "latest".

- **Private or internal actions**: If a workflow references an action in a
  private repository (within the same organization, for example), actup can
  handle it if provided a token with access. The mechanism is the same, but
  users must supply credentials (via env var or config) so the tool can query
  those repos. The LSP would need to be configured with the token as well (maybe
  via environment or an editor setting) to resolve private action versions.

## Command-Line Interface (CLI) Design

The **actup** CLI is designed for both interactive use by developers and
automation in CI pipelines. Key aspects of the CLI include:

- **Usage and Commands**: The primary mode is simply running `actup` in the root
  of a repository. This triggers a scan and outputs a report of any outdated
  actions. By default, it operates in a check-and-interactive-update mode:

  - **Scan**: List all discovered actions with their current version and latest
    available version.
  - **Report**: Show which ones are outdated, and if so, the version they can be
    updated to (respecting the configured update policy). For each outdated
    action, indicate if the new version is a major bump (possibly with a warning
    symbol or note).
  - **Interactive prompt**: The user can choose which updates to apply. For
    example, actup might present a list like:

    ```console
    1. actions/checkout (v3 -> v3.5.2)
    2. org/setup-tool (v1.0.0 -> v1.2.0) [major]
    3. actions/upload-artifact (v2.2.4 -> v3.0.0) [major]

    Select actions to update (enter numbers or 'all'):
    ```

    The user can select specific ones or all. This interactive selection is
    inspired by existing tools[^github/azat-io/actions-up].

After selection, actup will update the YAML files in-place (modifying the
version strings accordingly). It will replace tags with either new tags or SHAs
as configured. It might also add a comment after a SHA to indicate which version
that was (for readability in raw YAML). The changes can then be reviewed by the
developer (in version control diffs).

- **Batch (auto) mode**: Users can bypass interactivity (for CI or scripting) by
  flags like `--yes` (apply all updates) or `--check` (just exit with non-zero
  if any updates are available, useful for CI to fail a build or remind to run
  actup). An auto-update mode (`actup -y`) will apply all suggested updates and
  possibly even open a pull request if run in CI context (similar to how some
  actions do it[^github/actions-version-updater], though in our case, using
  actup in CI would typically just commit to a branch or so if configured).

- **Dry Run and Output**: With `--dry-run`, actup will perform all checks and
  print the would-be changes *without* modifying any
  files[^github/azat-io/actions-up]. This is useful to see what it would do (and
  can be the default in CI to only report). The output format in dry run could
  be colored terminal text or a summary table (e.g., listing each action and
  "current -> new version"). There could also be an option to output a JSON
  report of the findings for other tooling to consume.

- **CLI Options Summary**: Using Clap, we would define options such as:

  - `--yes/-y`: Apply all updates without prompt (non-interactive).
  - `--dry-run`: Don't write changes, just list outdated actions.
  - `--config <file>`: Specify a config file path if not using the default
    discovery.
  - `--policy <level>`: Override the update policy (patch/minor/major/all) for
    this run.
  - `--include-branches`: Temporarily treat branch refs as updatable (overriding
    config default to skip)[^github/azat-io/actions-up].
  - `--no-pin`: If the default is to pin to SHAs, this could disable pinning for
    the run (or vice versa `--pin` to enforce pinning if default config is
    tags).
  - `--update-cache`: Force refresh of all cached data, ignoring the TTL.
  - `--no-cache`: Do not use cache at all (fetch everything fresh).
  - Possibly filtering options like `--action <name>` to only check/update a
    specific action or `--workflow <path>` to only scan certain files, though
    these are nice-to-have.

- **Example workflow**: A typical usage might be:

  ```console
  $ actup
  Found 5 actions in 3 workflow files.
  - actions/checkout@v2 — New version available: v3.5.2
  - actions/setup-node@v3 — Up-to-date ✔
  - org/foo-action@v1.2.0 — New version available: v1.4.0 (minor update)
  - ./github/actions/my-composite/action.yml -> uses: actions/upload-artifact@v2.2.4 — New major version v3.0.0 available
  - actions/unknown@v1 — ❌ Version not found (possibly removed or typo)

  Update all outdated actions? [y/N]
  ```

  If the user agrees (or ran with -y), it would proceed to edit those files. If
  one of the updates is major, actup might print a warning that a breaking
  change could occur (and perhaps require confirmation unless `--yes` is used,
  or mark it clearly in a PR).

- **Safety and Commit**: actup will not automatically commit changes by default
  (it will leave them staged or prompt the user to review). In automation, one
  could run `actup -y && git commit -am "Update GitHub Actions versions"` as
  part of a release process. In CI use (like a scheduled workflow), actup could
  be run inside a GitHub Action (someone could wrap the CLI in an action
  container) to open pull requests with updates – similar to the marketplace
  updater[^github/actions-version-updater], but that is outside the scope of the
  core design (which focuses on the tool itself). Still, we ensure that the
  CLI's non-interactive mode can be used in such a context.

## Editor Integration via LSP (`actup-lsp`)

The companion language server, **actup-lsp**, provides real-time feedback and
automated fixes in code editors (VSCode, Neovim, etc.) for workflow files. By
implementing the Language Server Protocol, it can be plugged into any
LSP-compatible editor. Key features of the LSP server:

- **Activation**: The server is configured to activate for YAML files in paths
  like `.github/workflows/` or any file that appears to be a GitHub Actions
  workflow or action (we might use file name heuristics like those containing
  `uses:` and `jobs:` keys). Alternatively, a VSCode extension or editor setting
  can restrict it to known workflow file patterns to avoid interfering with
  generic YAML editing. Once active, it will maintain a background actup-core
  process to analyze files.

- **Diagnostics (Squiggly underlines)**: actup-lsp provides **diagnostics** for
  each action reference in an open file:

- If an action is **outdated** according to the policy, the server reports a
  diagnostic on the `uses: ...@version` range. This could be a warning-level
  diagnostic with a message like *"GitHub Action is out-of-date: newer version
  **v1.4.0** available"* (for example). The outdated version text (the part
  after `@`) would have a squiggly underline in the editor.

- If an action's version ref is **unresolvable** (e.g., the tag doesn't exist or
  network failure), the server shows an error-level diagnostic on that line:
  *"Invalid or unknown action version: tag not found"*. However, as noted, if
  the reason is a transient network issue or API limit, the message might be
  *"Unable to check version (network error)"* and possibly at a lower severity
  or no diagnostic, to avoid false positives. Essentially, definite problems
  (like a 404 on that ref) are errors[^xebia/versioning], while uncertain ones
  due to connectivity are either not reported or shown as informational.

- If an action is current (no update), no diagnostic is emitted for it.
  Possibly, we could mark it with a subtle icon or so, but generally LSP
  diagnostics are for issues.

The LSP continuously updates diagnostics as the file changes or at a set
interval. It likely will not spam the GitHub API on every keystroke; instead, it
can use the caching mechanism. For example, upon opening a file, the LSP fetches
needed data (using cache if available) and then caches it in memory for the
editing session. If the user edits the version string, the LSP can re-check that
single action. To avoid excessive calls, we might debounce requests or limit
checks to when the document is saved or idle.

- **Code Actions (Quick Fixes)**: One of the most powerful features is offering
  fixes for outdated or error diagnostics. When the user sees a squiggly
  underline under an outdated action, they can invoke a quick-fix: actup-lsp
  will present an option to **"Update to latest version"**. Selecting that will
  automatically edit the document to replace the version part. If configured to
  pin to SHA, the fix will insert the commit SHA of the latest version (and
  possibly append a comment with the version). For example,
  `uses: actions/checkout@v2` would change to `uses: actions/checkout@<abc123>`
  with a comment `# v3.5.2`. If the user's preference is tags, it might replace
  to `@v3.5.2`.

In cases where a major version update is available but the user policy is to
avoid major bumps, the LSP might still offer it as a *separate* code action with
a warning (e.g., "Upgrade to v2.0.0 (major version)") so the user can
consciously apply it if desired. Alternatively, the config policy could be
strictly enforced (i.e., LSP only suggests up to minor by default), but giving
the option in-editor can be helpful. If multiple upgrade paths exist (say minor
vs major), multiple code actions can be provided.

For an **unresolvable reference** diagnostic, a quick fix might suggest either
removing it (not usually desired) or if it's a partial tag issue, suggest the
closest valid reference. For example, if `uses: foo/bar@v1.2` is not a real tag,
and the repository has v1.2.0, the fix could offer "Change to v1.2.0".

- **Workspace Edits (Bulk fixes)**: LSP supports workspace-wide edits. actup-lsp
  can provide a code action at the file or folder level (perhaps triggered from
  the editor's command palette) to **"Update all actions in this
  workflow/repository"**. This would apply all recommended updates in one go.
  For instance, in VS Code, a code action on the file (like a lightbulb at top)
  could do "Upgrade all outdated actions in this file", applying multiple line
  edits. We could even extend that to all open files or all workflow files in
  the workspace, though automatically editing multiple files from a single
  action might require the editor's explicit approval (some LSPs allow it with a
  WorkspaceEdit containing changes to many files). This essentially parallels
  the CLI's batch update, but from within the editor context.

- **Implementation**: actup-lsp is built on an async runtime (Tokio) to handle
  multiple requests and perform background network calls without freezing the
  editor. The Rust LSP library (e.g. `tower-lsp`) provides a framework where we
  implement handlers for document changes, open, save, etc. On file open or
  save, the server triggers a re-scan and version check via actup-core, then
  sends diagnostics. The heavy lifting (network calls to GitHub) is done on
  background tasks, and results are cached, so subsequent openings or edits are
  faster. We also must handle LSP's asynchronous nature – e.g., if the user is
  offline or API is slow, we may initially mark diagnostics as unknown and
  update them when results come in.

- **LSP Configuration**: The language server may read the same config file
  (`actup.yaml`) to honor user preferences (so it knows the update policy and
  pinning setting). Additionally, the editor extension might allow specifying a
  GitHub token for the LSP to use (to avoid rate limiting, especially since
  editors could check frequently). Security-wise, the token would be loaded into
  the LSP process environment or via an LSP setting, not stored in the repo.

- **User Experience**: By integrating with editors, developers get immediate
  feedback when writing or reviewing workflow files. Outdated actions are
  highlighted proactively (squiggly underline as a warning rather than having to
  run a separate check). The quick-fix workflow (similar to how code analyzers
  suggest fixes) makes updating as simple as a click, reducing the friction of
  the recommended practice of pinning to SHAs. If an update might introduce
  breaking changes (major bump), the LSP could include a note in the diagnostic
  or the code action text (e.g., "major version update – check release notes").
  This ensures developers are aware.

- **Example Scenario**: A developer opens a pull request that modifies a
  workflow file. Their editor (with actup-lsp) immediately flags that
  `uses: actions/cache@v2` is outdated – the diagnostic says "actions/cache v3
  is available". The developer clicks the lightbulb and chooses "Update to
  v3.4.0" – the YAML is edited to
  `uses: actions/cache@<sha-of-v3.4.0> # v3.4.0`. They save the file, and the
  rest of CI will use the updated action. This tight feedback loop encourages
  keeping actions current as part of normal development.

## Conclusion

In summary, **actup** combines a powerful CLI tool and an editor assistant to
automate the detection and updating of GitHub Actions versions. It addresses
real problems in managing workflows: ensuring security (via updated, pinned
versions) and saving developer time. By scanning all workflows and composite
actions[^github/azat-io/actions-up], consulting the latest releases (with
caching for efficiency), and offering configurable rules for
updates[^github/actions-version-updater], actup provides a comprehensive
solution. The Rust implementation (edition 2024) ensures performance and
reliability, while the LSP integration follows modern guidance for developer
tools (as of 2026) to seamlessly blend into development environments. Through
**actup**, teams can confidently use third-party actions, knowing that they will
be alerted to updates and can apply them safely with minimal effort, whether via
a terminal prompt or a quick fix in their IDE.

[^github/azat-io/actions-up]: GitHub - azat-io/actions-up: Interactive CLI tool
    to update GitHub Actions to latest versions with SHA pinning\
    <https://github.com/azat-io/actions-up>

[^xebia/versioning]: How GitHub Actions Versioning System Works | Xebia\
    <https://xebia.com/blog/how-github-actions-versioning-system-works/>

[^github/security]: Secure use reference - GitHub Docs\
    <https://docs.github.com/en/actions/reference/security/secure-use>

[^github/actions-version-updater]: GitHub Actions Version Updater · Actions ·
    GitHub Marketplace · GitHub\
    <https://github.com/marketplace/actions/github-actions-version-updater>
