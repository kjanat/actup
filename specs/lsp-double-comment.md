# F8: LSP Double-Comment on Pin-to-SHA

Status: Design complete, not implemented

## Bug

When `[pin] comment = true`, pinning a ref that already has a trailing YAML comment produces a double comment:

```yaml
# Before
- uses: actions/checkout@v4 # existing comment

# After (broken)
- uses: actions/checkout@abc123...def # v4.2.0 # existing comment
```

`format_pin_text()` (`code_actions.rs:291`) unconditionally appends `# {ref}` to the SHA. The `TextEdit` covers only `ref_range` (the `v4`
after `@`), so any existing trailing text survives. CLI is protected by `PendingEdit::apply`'s `!result.contains('#')` guard
(`common.rs:173-178`); LSP has no equivalent.

## Affected Sites

All three pin-edit construction sites in `code_actions.rs`:

| Site | Line    | Action                           |
| ---- | ------- | -------------------------------- |
| A    | 144-146 | Diagnostic "Update & pin to SHA" |
| B    | 171-173 | Diagnostic "Pin current to SHA"  |
| C    | 345-347 | Cursor "Pin to SHA"              |

Each does: `TextEdit { range: ref_range, new_text: format_pin_text(...) }`

## Root Cause

`ref_range` ends at the end of the parsed value (before any space/comment). The parser (`parser.rs:146-157`) splits on whitespace for
unquoted values, so trailing `# comment` is excluded from `value` and therefore from `ref_range`. Editing only `ref_range` leaves the
trailing text intact on the line.

## Existing Infrastructure

`ActionCheckEntry.value_range` (`diagnostics.rs:69`) is already populated but tagged `#[allow(dead_code)]` with doc comment "Reserved for
future comment-aware edits." This is exactly the field needed for the fix.

## Proposed Fix

### Option A: Extend edit range to end-of-line (preferred)

When `pin_comment = true`, extend the edit range from `ref_range` to `(ref_range.start, end-of-line)`. This replaces the ref AND any
trailing comment in one edit, then `format_pin_text` appends the clean comment.

Implementation:

1. Pass the document text (or at least the line) to the pin-action builders
2. For each entry, compute end-of-line column from the document
3. Use `Range { start: ref_range.start, end: Position { line, character: eol_col } }`
4. This removes the old trailing comment and replaces it with the new one

Pros: Simple, complete, handles all quoting edge cases. Cons: Requires access to the document text in code action generation.

### Option B: Use `value_range` to detect existing comments

Check whether content exists between `value_range.end` and end-of-line that contains `#`. If so, suppress the comment in `format_pin_text`.

Implementation:

1. Pass document text + `value_range` to `format_pin_text` or a wrapper
2. Extract text after `value_range.end` on the same line
3. If it contains `#`, set `pin_comment = false` for this entry

Pros: Minimal edit range (ref only), existing comment preserved. Cons: Less clean (old comment may be stale/misleading after pin).

### Option C: Extend edit to cover trailing comment

Use `value_range.end` to `end-of-line` as a secondary edit range, replacing the trailing comment with the pin comment.

More complex, less clear benefit over Option A.

## Quoting Edge Case

Quoted values (`"actions/checkout@v4"`) have `ref_range` inside the quotes. Inserting `sha # ref` inside quotes produces invalid YAML:
`"actions/checkout@abc123 # v4.2.0"` (the `#` is literal, not a comment).

Fix: When the value is quoted, `pin_comment` should be forced to `false`, or the edit should extend beyond the closing quote. This requires
knowing whether the original value was quoted (currently not tracked in `ActionCheckEntry`).

Suggested: Add `is_quoted: bool` to `ActionCheckEntry` or `LocatedAction`, sourced from the parser's `quote_offset > 0`. When quoted, always
suppress the pin comment.

## Test Plan

1. Unquoted + no existing comment + `pin_comment=true` -> appends `# ref`
2. Unquoted + existing comment + `pin_comment=true` -> replaces/suppresses
3. Quoted + `pin_comment=true` -> no comment appended (or correct placement)
4. Snapshot tests for each scenario
5. Integration test: full LSP round-trip with trailing comment in YAML
