## Crash Summary

**Sentry Issue:** [ZED-3YH](https://sentry.io/organizations/zed-dev/issues/7094131516/) (146 events)

The application crashes with `offset {} is greater than the snapshot.len() {}` when confirming a code completion. The crash occurs in `MultiBufferOffset::to_offset` which asserts that the offset is within bounds of the snapshot.

## Root Cause

When `edit_internal` is called in `MultiBuffer`, it first syncs pending buffer changes via `sync_mut()`. If the underlying buffer(s) were modified between when the edit offsets were computed and when `sync_mut` runs, the snapshot can become shorter than expected, causing offsets to exceed `snapshot.len()`.

The sequence:
1. `do_completion` computes selection offsets from the current snapshot
2. Buffer changes occur (from LSP, collaboration, async operations, etc.)
3. `multi_buffer.edit()` is called, which triggers `sync_mut()` updating the snapshot
4. `to_offset()` assertion fails because offsets are now out of bounds

## Fix

Changed `ToOffset for MultiBufferOffset` to clamp the offset to the valid range instead of asserting:

```rust
impl ToOffset for MultiBufferOffset {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> MultiBufferOffset {
        MultiBufferOffset(self.0.min(snapshot.len().0))
    }
    // ...
}
```

This is the minimal defensive fix that:
- Preserves behavior for valid offsets
- Gracefully handles stale offsets by clamping to buffer end
- Matches similar patterns used elsewhere (e.g., `saturating_sub`)

## Validation

- Clippy passes (`./script/clippy -p multi_buffer`)
- Code compiles successfully
- Added test cases:
  - `test_to_offset_clamps_stale_offset` - verifies clamping behavior
  - `test_edit_with_stale_offset_after_buffer_shrinks` - verifies edit works with stale offsets

## Potentially Related Issues

**Medium Confidence:**
- [#28586](https://github.com/zed-industries/zed/pull/28586) — Fix bugs with multicursor completions (same code area)
- [#49047](https://github.com/zed-industries/zed/pull/49047) — multi_buffer: Fix "cannot seek backward" crash (related synchronization issues)

**Low Confidence:**
- [#28820](https://github.com/zed-industries/zed/pull/28820) — Fix slice crash in `do_completion`

## Reviewer Checklist

- [ ] Verify clamping behavior is appropriate for all callers of `to_offset`
- [ ] Confirm no other `ToOffset` implementations need similar treatment
- [ ] Consider if logging should be added for debugging (offset was clamped)
- [ ] Check if this fix might hide other bugs that should be addressed differently

Release Notes:

- Fixed a crash that could occur when confirming code completions while background processes modify the buffer
