//! Mutating tools: `updateFile` and `updateAllUsesToVersion`.

use std::{fs, io, ops::Range, path::Path};

use actup_core::{
    ActionRef, ActionRepoId, Config, EditValidationError, FileKind, Info, SourceFormat, Uses,
    UsesLocation, classify_uses, discover_files, line_spans, parse_action_path,
    patch::{
        EditKind, EditOutcome, FileEdits, PendingEdit, SkipReason, Splices, retag_edit,
        retarget_edit, sha_edit,
    },
    policy::Style,
    provider::Registry,
    rewrite_atomic, version,
};
use rust_mcp_sdk::schema::schema_utils::CallToolError;
use serde::Serialize;

use crate::{
    args::{self, check_written_ref, repo_id},
    confine::Root,
    diff,
    report::{Reply, Served, Status},
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Site {
    path: String,
    line: usize,
    provider: String,
    host: String,
    action: String,
}

#[derive(Debug, Serialize)]
struct Span {
    start: usize,
    end: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Change {
    #[serde(flatten)]
    site: Site,
    range: Span,
    from: String,
    to: String,
    before: String,
    after: String,
}

#[derive(Debug, Serialize)]
struct Unchanged {
    #[serde(flatten)]
    site: Site,
    #[serde(rename = "ref")]
    reference: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum SkipCause {
    Ignored,
    Unplaceable,
    Overlap,
    OutOfBounds,
}

impl From<SkipReason> for SkipCause {
    fn from(reason: SkipReason) -> Self {
        match reason {
            SkipReason::Unplaceable => Self::Unplaceable,
            SkipReason::Overlap => Self::Overlap,
            SkipReason::OutOfBounds => Self::OutOfBounds,
        }
    }
}

#[derive(Debug, Serialize)]
struct Skipped {
    #[serde(flatten)]
    site: Site,
    reason: SkipCause,
}

#[derive(Debug, Serialize)]
struct Unresolved {
    #[serde(flatten)]
    site: Site,
    error: String,
}

#[derive(Debug, Serialize)]
struct FileError {
    path: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct LineRevert {
    path: String,
    line: usize,
    current: String,
    original: String,
}

#[derive(Debug, Default, Serialize)]
struct Reversal {
    diff: String,
    lines: Vec<LineRevert>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Outcome {
    dry_run: bool,
    written: Vec<String>,
    changes: Vec<Change>,
    unchanged: Vec<Unchanged>,
    skipped: Vec<Skipped>,
    unresolvable: Vec<Unresolved>,
    errors: Vec<FileError>,
    diff: String,
    reversal: Reversal,
}

impl Outcome {
    const fn status(&self) -> Status {
        if !self.unresolvable.is_empty() {
            Status::Unresolvable
        } else if self.changes.is_empty() {
            Status::UpToDate
        } else {
            Status::Outdated
        }
    }

    fn reply(self, served: Option<&Served>) -> Result<Reply, CallToolError> {
        let failed = !self.errors.is_empty();
        let reply = Reply::new(served, self.status(), self)?;
        Ok(if failed { reply.failing() } else { reply })
    }
}

struct Label {
    site: Site,
    range: Range<usize>,
    from: String,
    to: String,
    expected: String,
}

struct Planned {
    content: String,
    splices: Splices,
    changes: Vec<Change>,
    unchanged: Vec<Unchanged>,
    skipped: Vec<Skipped>,
}

fn served_for(registry: &Registry, action: &ActionRef) -> Served {
    registry.find(action.host()).map_or_else(
        || {
            Served::unserved(
                action
                    .host()
                    .or_else(|| registry.default_host())
                    .unwrap_or_default(),
            )
        },
        Served::by,
    )
}

fn site(path: &str, loc: &UsesLocation, served: Served) -> Site {
    Site {
        path: path.to_string(),
        line: loc.line(),
        provider: served.provider,
        host: served.host,
        action: loc.value().to_string(),
    }
}

fn label(site: Site, edit: &PendingEdit, loc: &UsesLocation, from: &str, to: &str) -> Label {
    let EditKind::ReplaceRange { range, .. } = &edit.kind;
    let base = loc
        .value()
        .split_once('@')
        .map_or_else(|| loc.value(), |(base, _)| base);
    Label {
        site,
        range: range.clone(),
        from: from.to_string(),
        to: to.to_string(),
        expected: format!("{base}@{to}"),
    }
}

fn plan(original: &str, edits: FileEdits<Label>) -> Result<Planned, String> {
    let plan = edits.plan();
    let content = plan.content();
    let old_spans = line_spans(original);
    let new_spans = line_spans(&content);
    if old_spans.len() != new_spans.len() {
        return Err("edits changed the number of lines".to_string());
    }
    let after = plan.splices.format().extract_uses(&content);
    let mut planned = Planned {
        changes: Vec::new(),
        unchanged: Vec::new(),
        skipped: Vec::new(),
        content: String::new(),
        splices: plan.splices,
    };
    for (label, outcome) in plan.entries {
        if let EditOutcome::Skipped(reason) = outcome {
            planned.skipped.push(Skipped {
                site: label.site,
                reason: reason.into(),
            });
            continue;
        }
        let line = label.site.line;
        let (Some(old), Some(new)) = (
            old_spans
                .get(line - 1)
                .and_then(|s| original.get(s.clone())),
            new_spans.get(line - 1).and_then(|s| content.get(s.clone())),
        ) else {
            return Err(format!("line {line} is out of range"));
        };
        if !after
            .iter()
            .any(|loc| loc.line() == line && loc.value() == label.expected)
        {
            return Err(format!(
                "line {line}: the edited line does not hold `uses: {}`",
                label.expected
            ));
        }
        if old == new {
            planned.unchanged.push(Unchanged {
                site: label.site,
                reference: label.from,
            });
        } else {
            planned.changes.push(Change {
                site: label.site,
                range: Span {
                    start: label.range.start,
                    end: label.range.end,
                },
                from: label.from,
                to: label.to,
                before: old.to_string(),
                after: new.to_string(),
            });
        }
    }
    planned.content = content;
    Ok(planned)
}

enum Refusal {
    Io(io::Error),
    Changed,
    Invalid(EditValidationError),
}

impl From<io::Error> for Refusal {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl Refusal {
    fn message(&self, shown: &str) -> String {
        match self {
            Self::Io(e) => format!("writing {shown} failed: {e}"),
            Self::Changed => format!("edit refused: {shown} changed since scan"),
            Self::Invalid(e) => format!("edit refused: {shown} after edit: {e}"),
        }
    }
}

fn write(
    root: &Path,
    path: &Path,
    planned: &Planned,
    validate: bool,
) -> Result<(String, String), Refusal> {
    let mut texts = (String::new(), String::new());
    rewrite_atomic(root, path, |current| -> Result<String, Refusal> {
        let content = planned.splices.apply(current).ok_or(Refusal::Changed)?;
        if validate {
            planned
                .splices
                .format()
                .validate_edit(current, &content)
                .map_err(Refusal::Invalid)?;
        }
        texts = (current.to_string(), content.clone());
        Ok(content)
    })?;
    Ok(texts)
}

fn commit(
    outcome: &mut Outcome,
    root: &Root,
    path: &Path,
    shown: &str,
    original: &str,
    mut planned: Planned,
    validate: bool,
) {
    outcome.unchanged.append(&mut planned.unchanged);
    outcome.skipped.append(&mut planned.skipped);
    if planned.changes.is_empty() {
        return;
    }
    let (before, after) = if outcome.dry_run {
        (original.to_string(), planned.content.clone())
    } else {
        match write(root.path(), path, &planned, validate) {
            Ok(texts) => texts,
            Err(refusal) => {
                outcome.errors.push(FileError {
                    path: shown.to_string(),
                    error: refusal.message(shown),
                });
                return;
            }
        }
    };
    if !outcome.dry_run {
        outcome.written.push(shown.to_string());
    }
    if let Some(forward) = diff::render(shown, &before, &after) {
        outcome.diff.push_str(&forward);
    }
    if let Some(backward) = diff::render(shown, &after, &before) {
        outcome.reversal.diff.push_str(&backward);
    }
    for change in &planned.changes {
        let line = change.site.line;
        if outcome
            .reversal
            .lines
            .last()
            .is_some_and(|last| last.path == shown && last.line == line)
        {
            continue;
        }
        outcome.reversal.lines.push(LineRevert {
            path: shown.to_string(),
            line,
            current: change.after.clone(),
            original: change.before.clone(),
        });
    }
    outcome.changes.extend(planned.changes);
}

fn same_repo(a: &ActionRepoId, b: &ActionRepoId, default_host: &str) -> bool {
    a.host()
        .unwrap_or(default_host)
        .eq_ignore_ascii_case(b.host().unwrap_or(default_host))
        && a.owner() == b.owner()
        && a.repo() == b.repo()
}

fn refused(message: impl std::fmt::Display) -> CallToolError {
    CallToolError::from_message(message.to_string())
}

fn refuse_skips(skipped: &[Skipped]) -> Result<(), CallToolError> {
    if skipped.is_empty() {
        return Ok(());
    }
    let skips: Vec<String> = skipped
        .iter()
        .map(|skip| format!("line {}: edit skipped ({:?})", skip.site.line, skip.reason))
        .collect();
    Err(refused(format!(
        "{}; nothing was written",
        skips.join("; ")
    )))
}

fn remote_values(format: SourceFormat, content: &str) -> Vec<(UsesLocation, ActionRef)> {
    format
        .extract_uses(content)
        .into_iter()
        .filter_map(|loc| match loc.classify()? {
            Uses::Local => None,
            Uses::Action(action) | Uses::Invalid { action, .. } => Some((loc, action)),
        })
        .collect()
}

/// The edit that writes `reference` over the ref at `loc`, if the rewritten value parses.
///
/// A SHA drops the version of a trailing version comment and keeps its note.
/// Any other ref takes the place of that version.
fn ref_edit(loc: &UsesLocation, reference: &str) -> Result<PendingEdit, String> {
    let value = loc.value();
    let Some((base, _)) = value.split_once('@') else {
        return Err(format!("`{value}` has no `@`"));
    };
    let unplaceable = || format!("`{value}` is escaped or spans lines; edit it by hand");
    if loc.ref_range().is_none() {
        return Err(unplaceable());
    }
    let rewritten = format!("{base}@{reference}");
    let action = match classify_uses(&rewritten) {
        Uses::Action(action) => action,
        Uses::Invalid { error, .. } => return Err(format!("`{rewritten}`: {error}")),
        Uses::Local => return Err(format!("`{rewritten}` names no remote action")),
    };
    let edit = if action.is_sha() {
        sha_edit(loc, reference)
    } else {
        retag_edit(loc, reference)
    };
    edit.ok_or_else(unplaceable)
}

/// `updateFile`: replace the refs of named `uses:` values in one file, all or none.
///
/// # Errors
///
/// Returns an error result when an argument is invalid, the path leaves the
/// root, an edit names no single `uses:` value, the edits span more than one
/// host, or the edited file fails validation.
pub fn update_file(
    tool: &str,
    registry: &Registry,
    root: &Root,
    args: &args::UpdateFile,
) -> Result<Reply, CallToolError> {
    let invalid = |message: String| CallToolError::invalid_arguments(tool, Some(message));
    if args.edits.is_empty() {
        return Err(invalid("edits must not be empty".to_string()));
    }
    let path = root.file(&args.path).map_err(refused)?;
    let shown = root.relative(&path);
    let original = fs::read_to_string(&path).map_err(|e| refused(format!("{shown}: {e}")))?;
    let format = SourceFormat::from_path(&path);
    let default_host = registry.default_host().unwrap_or_default();
    let locations = remote_values(format, &original);

    let mut edits = FileEdits::new(path.clone(), original.clone(), format);
    let mut unchanged = Vec::new();
    let mut problems = Vec::new();
    let mut servers: Vec<Served> = Vec::new();
    for edit in &args.edits {
        let named = parse_action_path(&edit.action)
            .map_err(|e| invalid(format!("action `{}`: {e}", edit.action)))?;
        check_written_ref("ref", &edit.reference).map_err(invalid)?;
        let line = edit.line.get();
        let matches: Vec<&(UsesLocation, ActionRef)> = locations
            .iter()
            .filter(|(loc, action)| {
                let id = action.to_repo_id();
                loc.line() == line
                    && same_repo(&id, &named, default_host)
                    && id.subpath() == named.subpath()
            })
            .collect();
        let [(loc, action)] = matches.as_slice() else {
            problems.push(format!(
                "line {line}: {} `uses:` value(s) name `{}`",
                matches.len(),
                edit.action
            ));
            continue;
        };
        let served = served_for(registry, action);
        if !servers.contains(&served) {
            servers.push(served.clone());
        }
        let site = site(&shown, loc, served);
        if action.ref_str() == edit.reference {
            unchanged.push(Unchanged {
                site,
                reference: edit.reference.clone(),
            });
            continue;
        }
        let pending = match ref_edit(loc, &edit.reference) {
            Ok(pending) => pending,
            Err(problem) => {
                problems.push(format!("line {line}: {problem}"));
                continue;
            }
        };
        let label = label(site, &pending, loc, action.ref_str(), &edit.reference);
        edits.add(pending, label);
    }
    if !problems.is_empty() {
        return Err(refused(problems.join("; ")));
    }
    let [served] = servers.as_slice() else {
        let hosts: Vec<&str> = servers.iter().map(|s| s.host.as_str()).collect();
        return Err(invalid(format!(
            "edits name actions on more than one host ({}); call updateFile once per host",
            hosts.join(", ")
        )));
    };

    let mut planned = plan(&original, edits).map_err(refused)?;
    refuse_skips(&planned.skipped)?;
    if args.validate_yaml && !planned.changes.is_empty() {
        format
            .validate_edit(&original, &planned.content)
            .map_err(|e| refused(format!("{shown} after edit: {e}")))?;
    }
    planned.unchanged.extend(unchanged);
    let mut outcome = Outcome {
        dry_run: args.dry_run,
        ..Outcome::default()
    };
    commit(
        &mut outcome,
        root,
        &path,
        &shown,
        &original,
        planned,
        args.validate_yaml,
    );
    outcome.reply(Some(served))
}

fn retarget(
    loc: &UsesLocation,
    action: &ActionRef,
    target: &Info,
    style: Style,
) -> Option<(PendingEdit, String)> {
    let edit = retarget_edit(loc, action.is_sha(), target, style)?;
    let to = if style.writes_sha(action.is_sha()) {
        target.sha().to_string()
    } else {
        target.version().to_string()
    };
    Some((edit, to))
}

struct Job<'a> {
    registry: &'a Registry,
    config: &'a Config,
    root: &'a Root,
    wanted: &'a ActionRepoId,
    target: Info,
    default_host: &'a str,
}

impl Job<'_> {
    fn update(&self, file: &Path, outcome: &mut Outcome) -> Result<(), FileError> {
        let path = self.root.contain(file).map_err(|refusal| FileError {
            path: file.display().to_string(),
            error: refusal.to_string(),
        })?;
        let shown = self.root.relative(&path);
        let fail = |error: String| FileError {
            path: shown.clone(),
            error,
        };
        let original = fs::read_to_string(&path).map_err(|e| fail(e.to_string()))?;
        let format = SourceFormat::from_path(&path);
        let mut edits = FileEdits::new(path.clone(), original.clone(), format);
        for loc in format.extract_uses(&original) {
            let (action, error) = match loc.classify() {
                None | Some(Uses::Local) => continue,
                Some(Uses::Action(action)) => (action, None),
                Some(Uses::Invalid { action, error }) => (action, Some(error)),
            };
            let id = action.to_repo_id();
            if !same_repo(&id, self.wanted, self.default_host)
                || self
                    .wanted
                    .subpath()
                    .is_some_and(|p| id.subpath() != Some(p))
            {
                continue;
            }
            let site = site(&shown, &loc, served_for(self.registry, &action));
            if let Some(error) = error {
                outcome.unresolvable.push(Unresolved {
                    site,
                    error: error.to_string(),
                });
                continue;
            }
            if self.config.should_ignore(&action) {
                outcome.skipped.push(Skipped {
                    site,
                    reason: SkipCause::Ignored,
                });
                continue;
            }
            let style = self.config.policy_for(&action).pin;
            let Some((pending, to)) = retarget(&edits.writable(&loc), &action, &self.target, style)
            else {
                outcome.skipped.push(Skipped {
                    site,
                    reason: SkipCause::Unplaceable,
                });
                continue;
            };
            let label = label(site, &pending, &loc, action.ref_str(), &to);
            edits.add(pending, label);
        }
        let planned = plan(&original, edits).map_err(fail)?;
        if !planned.changes.is_empty() {
            format
                .validate_edit(&original, &planned.content)
                .map_err(|e| fail(format!("edit refused: {shown} after edit: {e}")))?;
        }
        commit(outcome, self.root, &path, &shown, &original, planned, true);
        Ok(())
    }
}

const fn scans(kind: FileKind, args: &args::UpdateAll) -> bool {
    match kind {
        FileKind::Workflow | FileKind::Custom => args.include_workflows,
        FileKind::CompositeAction => args.include_composite_actions,
    }
}

/// `updateAllUsesToVersion`: move every matching reference under a directory to one ref.
///
/// # Errors
///
/// Returns an error result when an argument is invalid or the directory leaves the root.
pub async fn update_all(
    tool: &str,
    registry: &Registry,
    config: &Config,
    root: &Root,
    args: &args::UpdateAll,
) -> Result<Reply, CallToolError> {
    let invalid = |message: String| CallToolError::invalid_arguments(tool, Some(message));
    let target = &args.target;
    let wanted = repo_id(
        target.host.as_deref(),
        &target.owner,
        &target.repo,
        target.path.as_deref(),
    )
    .map_err(invalid)?;
    check_written_ref("targetRef", &args.target_ref).map_err(invalid)?;
    if version::Version::parse(&args.target_ref).is_sha() {
        return Err(invalid(format!(
            "targetRef `{}` is a commit SHA; pass a tag or branch and the provider resolves its commit",
            args.target_ref
        )));
    }
    let workspace = root.dir(&args.workspace_root).map_err(refused)?;

    let default_host = registry.default_host().unwrap_or_default();
    let Some(provider) = registry.find(wanted.host()) else {
        let host = wanted.host().unwrap_or(default_host).to_string();
        return Ok(Reply::failure(
            &Served::unserved(&host),
            &actup_core::Error::ProviderNotFound { host },
        ));
    };
    let served = Served::by(provider);
    let sha = match provider
        .refresh_ref_to_sha(&wanted.without_subpath(), &args.target_ref)
        .await
    {
        Ok(sha) => sha,
        Err(e) => return Ok(Reply::failure(&served, &e)),
    };
    let job = Job {
        registry,
        config,
        root,
        wanted: &wanted,
        target: Info::new(version::Version::parse(&args.target_ref), sha.as_str()),
        default_host,
    };
    let mut outcome = Outcome {
        dry_run: args.dry_run,
        ..Outcome::default()
    };
    for file in discover_files(&workspace, &config.scan.extra_paths) {
        if scans(file.kind(), args)
            && let Err(error) = job.update(file.path(), &mut outcome)
        {
            outcome.errors.push(error);
        }
    }
    outcome.reply(Some(&served))
}

#[cfg(test)]
mod tests {
    use actup_core::{
        BoxFuture,
        provider::{ActionYml, Kind, Provider, Sha40},
    };
    use serde_json::{Value, json};

    use super::*;

    fn loc(line_body: &str) -> Result<(UsesLocation, ActionRef), Box<dyn std::error::Error>> {
        let found = SourceFormat::Yaml.extract_uses(line_body);
        let [loc] = found.as_slice() else {
            return Err(format!("expected one `uses:` in {line_body:?}").into());
        };
        let action = actup_core::parse_uses(loc.value())?.ok_or("local action")?;
        Ok((loc.clone(), action))
    }

    fn target() -> Info {
        Info::new(version::Version::parse("v4.2.0"), "b".repeat(40))
    }

    fn edited(line_body: &str, reference: &str) -> Result<String, String> {
        let found = actup_core::extract_uses_from_yaml(line_body);
        let [loc] = found.as_slice() else {
            return Err(format!("expected one `uses:` in {line_body:?}"));
        };
        let edit = ref_edit(loc, reference)?;
        let EditKind::ReplaceRange { range, new } = &edit.kind;
        assert_eq!(edit.comment, None, "{line_body:?}");
        let mut line = line_body.to_string();
        line.replace_range(range.clone(), new);
        Ok(line)
    }

    #[test]
    fn ref_edit_refreshes_version_comments_and_keeps_notes() {
        let sha = "b".repeat(40);
        let cases = [
            ("- uses: o/r@a~1", "v1", "- uses: o/r@v1".to_string()),
            (
                "- uses: o/r@v4.0.0 # v4.0.0 keep",
                "v4.2.0",
                "- uses: o/r@v4.2.0 # v4.2.0 keep".to_string(),
            ),
            (
                "- uses: o/r@v4.0.0 # 1st step",
                "v4.2.0",
                "- uses: o/r@v4.2.0 # 1st step".to_string(),
            ),
            (
                "- uses: o/r@v4.0.0 # v4.0.0 keep",
                &sha,
                format!("- uses: o/r@{sha} # keep"),
            ),
            (
                "- uses: o/r@v4.0.0 # v4.0.0",
                &sha,
                format!("- uses: o/r@{sha}"),
            ),
            (
                "- uses: o/r@v4.0.0 # 1st step",
                &sha,
                format!("- uses: o/r@{sha} # 1st step"),
            ),
        ];
        for (line_body, reference, expected) in cases {
            assert_eq!(edited(line_body, reference), Ok(expected), "{line_body:?}");
        }
    }

    #[test]
    fn ref_edit_requires_an_at_and_a_value_that_parses() {
        assert_eq!(
            edited("- uses: o/r", "v1"),
            Err("`o/r` has no `@`".to_string())
        );
        assert_eq!(
            edited("- uses: ./x@a", "v1"),
            Err("`./x@v1` names no remote action".to_string())
        );
        assert_eq!(
            edited("- uses: \"o/r@a\\x7e1\"", "v1"),
            Err("`o/r@a~1` is escaped or spans lines; edit it by hand".to_string())
        );
        assert_eq!(
            edited("- uses: o/r/../x@a~1", "v1"),
            Err("`o/r/../x@v1`: invalid action reference 'o/r/../x@v1': path segment `..` is empty, `.` or `..`".to_string())
        );
    }

    #[test]
    fn retarget_follows_the_pin_style() -> Result<(), Box<dyn std::error::Error>> {
        let (tag_loc, tag) = loc("- uses: a/b@v4.0.0\n")?;
        let sha_line = format!("- uses: a/b@{}\n", "a".repeat(40));
        let (sha_loc, sha) = loc(&sha_line)?;
        let to = |loc: &UsesLocation, action: &ActionRef, style: Style| {
            retarget(loc, action, &target(), style).map(|(_, to)| to)
        };
        assert_eq!(
            to(&tag_loc, &tag, Style::Preserve).as_deref(),
            Some("v4.2.0")
        );
        assert_eq!(to(&tag_loc, &tag, Style::Sha), Some("b".repeat(40)));
        assert_eq!(to(&sha_loc, &sha, Style::Preserve), Some("b".repeat(40)));
        assert_eq!(to(&sha_loc, &sha, Style::Tag).as_deref(), Some("v4.2.0"));
        Ok(())
    }

    #[test]
    fn retarget_refreshes_the_version_comment_of_a_tag() -> Result<(), Box<dyn std::error::Error>> {
        let sha = "b".repeat(40);
        for (style, content, written) in [
            (
                Style::Preserve,
                "- uses: a/b@v4.0.0 # v4.0.0 keep\n",
                "- uses: a/b@v4.2.0 # v4.2.0 keep\n".to_string(),
            ),
            (
                Style::Preserve,
                "- uses: a/b@v4.0.0 # mine\n",
                "- uses: a/b@v4.2.0 # mine\n".to_string(),
            ),
            (
                Style::Preserve,
                "- uses: a/b@v4.0.0 # 1st step\n",
                "- uses: a/b@v4.2.0 # 1st step\n".to_string(),
            ),
            (
                Style::Sha,
                "- uses: a/b@v4.0.0 # 1st step\n",
                format!("- uses: a/b@{sha} # 1st step\n"),
            ),
            (
                Style::Sha,
                "- { uses: a/b@v4.0.0, with: { x: 1 } }\n",
                format!("- {{ uses: a/b@{sha}, with: {{ x: 1 }} }}\n"),
            ),
            (
                Style::Sha,
                "- { uses: 'a/b@v4.0.0' }\n",
                format!("- {{ uses: 'a/b@{sha}' }} # v4.2.0\n"),
            ),
        ] {
            let (l, action) = loc(content)?;
            let (edit, to) = retarget(&l, &action, &target(), style).ok_or("no edit")?;
            let site = Site {
                path: "f".into(),
                line: 1,
                provider: "github".into(),
                host: "github.com".into(),
                action: l.value().into(),
            };
            let mut edits = FileEdits::new("f".into(), content.to_string(), SourceFormat::Yaml);
            edits.add(edit.clone(), label(site, &edit, &l, action.ref_str(), &to));
            assert_eq!(plan(content, edits)?.content, written, "{style}");
        }
        Ok(())
    }

    #[test]
    fn retarget_keeps_the_closer() -> Result<(), Box<dyn std::error::Error>> {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        for (open, close) in [("\"", "\""), ("'", "'"), ("{ uses: ", " }")] {
            let uses = if open.starts_with('{') { "" } else { "uses: " };
            for (line, style, expected) in [
                (
                    format!("- {uses}{open}a/b@{old}{close} # v4.0.0\n"),
                    Style::Preserve,
                    format!("- {uses}{open}a/b@{new}{close} # v4.2.0\n"),
                ),
                (
                    format!("- {uses}{open}a/b@{old}{close}\n"),
                    Style::Preserve,
                    format!("- {uses}{open}a/b@{new}{close} # v4.2.0\n"),
                ),
                (
                    format!("- {uses}{open}a/b@{old}{close} # v4.0.0\n"),
                    Style::Tag,
                    format!("- {uses}{open}a/b@v4.2.0{close}\n"),
                ),
            ] {
                let (l, action) = loc(&line)?;
                let (edit, to) =
                    retarget(&l, &action, &target(), style).ok_or("no retarget edit")?;
                let site = Site {
                    path: "f".into(),
                    line: 1,
                    provider: "github".into(),
                    host: "github.com".into(),
                    action: action.to_string(),
                };
                let labelled = label(site, &edit, &l, action.ref_str(), &to);
                let mut edits = FileEdits::new("f".into(), line.clone(), SourceFormat::Yaml);
                edits.add(edit, labelled);
                assert_eq!(plan(&line, edits)?.content, expected, "{line:?} {style}");
            }
        }
        Ok(())
    }

    #[test]
    fn retarget_writes_no_comment_after_other_flow_keys() -> Result<(), Box<dyn std::error::Error>>
    {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        for (line, expected) in [
            (
                format!("- {{ uses: a/b@{old}, with: {{ x: 1 }} }}\n"),
                format!("- {{ uses: a/b@{new}, with: {{ x: 1 }} }}\n"),
            ),
            (
                format!("- {{ uses: a/b@{old}, with: {{ x: 1 }} }} # v4.0.0\n"),
                format!("- {{ uses: a/b@{new}, with: {{ x: 1 }} }} # v4.2.0\n"),
            ),
        ] {
            let (l, action) = loc(&line)?;
            let (edit, to) =
                retarget(&l, &action, &target(), Style::Preserve).ok_or("no retarget edit")?;
            let site = Site {
                path: "f".into(),
                line: 1,
                provider: "github".into(),
                host: "github.com".into(),
                action: action.to_string(),
            };
            let labelled = label(site, &edit, &l, action.ref_str(), &to);
            let mut edits = FileEdits::new("f".into(), line.clone(), SourceFormat::Yaml);
            edits.add(edit, labelled);
            assert_eq!(plan(&line, edits)?.content, expected, "{line:?}");
        }
        Ok(())
    }

    #[test]
    fn retarget_skips_a_value_not_written_verbatim() -> Result<(), Box<dyn std::error::Error>> {
        let (escaped, action) = loc("- uses: \"a/b@v4.0.\\x30\"\n")?;
        assert!(retarget(&escaped, &action, &target(), Style::Preserve).is_none());
        Ok(())
    }

    #[test]
    fn plan_reports_overlaps_and_refuses_broken_lines() -> Result<(), Box<dyn std::error::Error>> {
        let content = "- uses: a/b@v1\n";
        let (l, action) = loc(content)?;
        let site = || Site {
            path: "f".into(),
            line: 1,
            provider: "github".into(),
            host: "github.com".into(),
            action: "a/b@v1".into(),
        };
        let first = PendingEdit::replace_range(1, 12..14, "v2".into());
        let second = PendingEdit::replace_range(1, 12..14, "v3".into());
        let mut edits = FileEdits::new("f".into(), content.to_string(), SourceFormat::Yaml);
        edits.add(
            first.clone(),
            label(site(), &first, &l, action.ref_str(), "v2"),
        );
        edits.add(
            second.clone(),
            label(site(), &second, &l, action.ref_str(), "v3"),
        );
        let planned = plan(content, edits)?;
        assert!(planned.changes.is_empty());
        assert!(matches!(
            planned.skipped.as_slice(),
            [
                Skipped {
                    reason: SkipCause::Overlap,
                    ..
                },
                Skipped {
                    reason: SkipCause::Overlap,
                    ..
                }
            ]
        ));

        let breaking = PendingEdit::replace_range(1, 12..14, "v2\n".into());
        let mut edits = FileEdits::new("f".into(), content.to_string(), SourceFormat::Yaml);
        edits.add(breaking.clone(), label(site(), &breaking, &l, "v1", "v2"));
        assert!(plan(content, edits).is_err());

        let unparsed = PendingEdit::replace_range(1, 8..9, "\"".into());
        let mut edits = FileEdits::new("f".into(), content.to_string(), SourceFormat::Yaml);
        edits.add(unparsed.clone(), label(site(), &unparsed, &l, "v1", "v1"));
        assert!(plan(content, edits).is_err());
        Ok(())
    }

    #[test]
    fn skip_causes_cover_every_writer_limit() -> Result<(), Box<dyn std::error::Error>> {
        let names: Vec<serde_json::Value> = [
            SkipCause::Ignored,
            SkipReason::Unplaceable.into(),
            SkipReason::Overlap.into(),
            SkipReason::OutOfBounds.into(),
        ]
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()?;
        assert_eq!(names, ["ignored", "unplaceable", "overlap", "outOfBounds"]);
        Ok(())
    }
    fn planned(content: &str, range: Range<usize>, new: &str) -> Planned {
        let mut edits = FileEdits::new("f".into(), content.to_string(), SourceFormat::Yaml);
        edits.add(PendingEdit::replace_range(2, range, new.to_string()), ());
        let plan = edits.plan();
        Planned {
            content: plan.content(),
            splices: plan.splices,
            changes: Vec::new(),
            unchanged: Vec::new(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn write_keeps_changes_made_elsewhere_since_scan() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        fs::write(&path, "steps:\n  - uses: a/b@v1\nname: renamed\n")?;
        let planned = planned("steps:\n  - uses: a/b@v1\nname: a\n", 14..16, "v2");
        let Ok((before, after)) = write(dir.path(), &path, &planned, true) else {
            panic!("write refused");
        };
        assert_eq!(before, "steps:\n  - uses: a/b@v1\nname: renamed\n");
        assert_eq!(after, "steps:\n  - uses: a/b@v2\nname: renamed\n");
        assert_eq!(fs::read_to_string(&path)?, after);
        Ok(())
    }

    #[test]
    fn write_refuses_a_line_changed_since_scan() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let changed = "steps:\n  - uses: a/b@main\n";
        fs::write(&path, changed)?;
        let planned = planned("steps:\n  - uses: a/b@v1\n", 14..16, "v2");
        let Err(refusal) = write(dir.path(), &path, &planned, true) else {
            panic!("write must refuse a changed line");
        };
        assert_eq!(
            refusal.message("ci.yml"),
            "edit refused: ci.yml changed since scan"
        );
        assert_eq!(fs::read_to_string(&path)?, changed);
        Ok(())
    }

    #[test]
    fn write_validates_the_edit_only_when_asked() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let content = "steps:\n  - uses: a/b@v1\n";
        fs::write(&path, content)?;
        let planned = planned(content, 14..16, "v2\n  - [");
        let Err(refusal) = write(dir.path(), &path, &planned, true) else {
            panic!("write must refuse invalid YAML");
        };
        let message = refusal.message("ci.yml");
        assert!(
            message.starts_with("edit refused: ci.yml after edit: invalid YAML: "),
            "{message}"
        );
        assert_eq!(fs::read_to_string(&path)?, content);

        assert!(write(dir.path(), &path, &planned, false).is_ok());
        assert_eq!(
            fs::read_to_string(&path)?,
            "steps:\n  - uses: a/b@v2\n  - [\n"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn write_refuses_a_symlink_that_leaves_the_root() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let content = "steps:\n  - uses: a/b@v1\n";
        let target = outside.path().join("ci.yml");
        fs::write(&target, content)?;
        let path = root.path().join("ci.yml");
        std::os::unix::fs::symlink(&target, &path)?;
        let planned = planned(content, 14..16, "v2");
        let Err(refusal) = write(root.path(), &path, &planned, true) else {
            panic!("write must refuse a file outside the root");
        };
        assert_eq!(
            refusal.message("ci.yml"),
            format!(
                "writing ci.yml failed: resolves outside {}",
                root.path().display()
            )
        );
        assert_eq!(fs::read_to_string(&target)?, content);
        Ok(())
    }

    #[test]
    fn write_reports_an_io_failure() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("missing").join("ci.yml");
        let planned = planned("steps:\n  - uses: a/b@v1\n", 14..16, "v2");
        let Err(refusal) = write(dir.path(), &path, &planned, true) else {
            panic!("write into a missing directory must fail");
        };
        let message = refusal.message("ci.yml");
        assert!(message.starts_with("writing ci.yml failed: "), "{message}");
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn commit_records_a_refused_write_as_a_file_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let scanned = "steps:\n  - uses: a/b@v1\n";
        fs::write(&path, "steps:\n  - uses: a/b@main\n")?;
        let (l, action) = loc("  - uses: a/b@v1\n")?;
        let site = Site {
            path: "ci.yml".into(),
            line: 2,
            provider: "github".into(),
            host: "github.com".into(),
            action: "a/b@v1".into(),
        };
        let edit = PendingEdit::replace_range(2, 14..16, "v2".into());
        let mut edits = FileEdits::new(path.clone(), scanned.to_string(), SourceFormat::Yaml);
        edits.add(edit.clone(), label(site, &edit, &l, action.ref_str(), "v2"));
        let planned = plan(scanned, edits)?;
        let mut outcome = Outcome::default();
        let root = Root::new(dir.path())?;
        commit(&mut outcome, &root, &path, "ci.yml", scanned, planned, true);
        let [error] = outcome.errors.as_slice() else {
            panic!("expected one file error: {:?}", outcome.errors);
        };
        assert_eq!(error.error, "edit refused: ci.yml changed since scan");
        assert_eq!(outcome.written, Vec::<String>::new());
        assert!(outcome.changes.is_empty());
        assert_eq!(outcome.diff, "");
        Ok(())
    }

    #[test]
    fn commit_writes_nothing_when_edits_overlap() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let content = "steps:\n  - uses: a/b@v1\n  - uses: c/d@v1\n";
        fs::write(&path, content)?;
        let mut edits = FileEdits::new(path.clone(), content.to_string(), SourceFormat::Yaml);
        for (line, range, to) in [(2, 14..16, "v2"), (2, 15..16, "3"), (3, 14..16, "v2")] {
            let (l, action) = loc(content.lines().nth(line - 1).ok_or("no line")?)?;
            let site = Site {
                path: "ci.yml".into(),
                line,
                provider: "github".into(),
                host: "github.com".into(),
                action: l.value().into(),
            };
            let edit = PendingEdit::replace_range(line, range, to.into());
            edits.add(edit.clone(), label(site, &edit, &l, action.ref_str(), to));
        }
        let planned = plan(content, edits)?;
        let mut outcome = Outcome::default();
        let root = Root::new(dir.path())?;
        commit(&mut outcome, &root, &path, "ci.yml", content, planned, true);

        let skipped: Vec<(usize, &str)> = outcome
            .skipped
            .iter()
            .map(|skip| (skip.site.line, skip.site.action.as_str()))
            .collect();
        assert_eq!(skipped, [(2, "a/b@v1"), (2, "a/b@v1"), (3, "c/d@v1")]);
        assert!(
            outcome
                .skipped
                .iter()
                .all(|skip| matches!(skip.reason, SkipCause::Overlap))
        );
        assert!(outcome.changes.is_empty());
        assert_eq!(outcome.written, Vec::<String>::new());
        assert!(outcome.errors.is_empty());
        assert_eq!(outcome.diff, "");
        assert_eq!(fs::read_to_string(&path)?, content);
        Ok(())
    }

    struct Resolving;

    impl Provider for Resolving {
        fn kind(&self) -> Kind {
            Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            let sha = Sha40::new("b".repeat(40)).ok_or_else(|| actup_core::Error::Internal {
                context: "test".into(),
                message: "sha".into(),
                source: None,
            });
            Box::pin(async move { sha })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    fn update_all_args(host: Option<&str>) -> Result<args::UpdateAll, serde_json::Error> {
        serde_json::from_value(json!({
            "workspaceRoot": ".",
            "match": { "host": host, "owner": "actions", "repo": "checkout" },
            "targetRef": "v4.2.0",
            "dryRun": true,
        }))
    }

    fn body(reply: Reply) -> Result<(Option<bool>, Value), Box<dyn std::error::Error>> {
        let result = reply.into_result();
        let body = result.structured_content.ok_or("structured content")?;
        Ok((result.is_error, Value::Object(body)))
    }

    #[tokio::test]
    async fn update_all_skips_ignored_refs_and_files_outside_the_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join("repo/.github/workflows"))?;
        fs::write(
            dir.path().join("repo/.github/workflows/ci.yml"),
            "steps:\n  - uses: actions/checkout@v4.0.0\n  - uses: actions/checkout@v3\n",
        )?;
        let outside = "steps:\n  - uses: actions/checkout@v3\n";
        fs::write(dir.path().join("outside.yml"), outside)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            dir.path().join("outside.yml"),
            dir.path().join("repo/link.yml"),
        )?;
        let root = Root::new(&dir.path().join("repo"))?;
        let mut config = Config::default();
        config.scan.ignore = vec!["actions/checkout@v4.0.0".parse()?];
        config.scan.extra_paths = vec!["link.yml".parse()?];
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(Resolving));

        let reply = update_all("t", &registry, &config, &root, &update_all_args(None)?)
            .await
            .map_err(|e| e.to_string())?;
        let (is_error, body) = body(reply)?;
        assert_ne!(is_error, Some(true));
        assert_eq!(
            body["skipped"],
            json!([{
                "path": ".github/workflows/ci.yml",
                "line": 2,
                "provider": "github",
                "host": "github.com",
                "action": "actions/checkout@v4.0.0",
                "reason": "ignored",
            }])
        );
        assert_eq!(body["changes"][0]["line"], json!(3));
        assert_eq!(body["changes"][0]["to"], json!("v4.2.0"));
        assert_eq!(body["errors"], json!([]));
        assert_eq!(body["changes"].as_array().map(Vec::len), Some(1));
        assert_eq!(fs::read_to_string(dir.path().join("outside.yml"))?, outside);
        Ok(())
    }

    #[tokio::test]
    async fn update_all_reports_an_unserved_host() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let root = Root::new(dir.path())?;
        let mut registry = Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(Resolving));
        let args = update_all_args(Some("ghe.corp.com"))?;
        let reply = update_all("t", &registry, &Config::default(), &root, &args)
            .await
            .map_err(|e| e.to_string())?;
        let (is_error, body) = body(reply)?;
        assert_eq!(is_error, Some(true));
        assert_eq!(
            body,
            json!({
                "provider": "unknown",
                "host": "ghe.corp.com",
                "status": "unknown",
                "error": "no provider configured for host: ghe.corp.com",
            })
        );
        Ok(())
    }
}
