# Crash Analysis: MultiBufferOffset out of bounds during completion

## Crash Summary
- **Sentry Issue:** ZED-3YH (https://sentry.io/organizations/zed-dev/issues/7094131516/)
- **Error:** `offset {} is greater than the snapshot.len() {}`
- **Crash Site:** `multi_buffer::MultiBufferOffset::to_offset` in `multi_buffer.rs` line 8255
- **Event Count:** 146 (First seen: 2025-12-07, Last seen: 2026-02-18)

## Stack Trace Analysis

The crash occurs when confirming a completion:
```
editor::Editor::confirm_completion
→ editor::Editor::do_completion
→ editor::Editor::transact
→ multi_buffer::MultiBuffer::edit
→ multi_buffer::MultiBuffer::edit_internal
→ MultiBufferOffset::to_offset (CRASH)
```

## Root Cause

The crash occurs because of a snapshot synchronization issue:

1. In `do_completion`, selection offsets are computed from the current snapshot:
   ```rust
   let selections = self.selections.all::<MultiBufferOffset>(&self.display_snapshot(cx));
   ```

2. These offsets are then used to build edit ranges and passed to `multi_buffer.edit()`.

3. Inside `edit_internal`, `sync_mut(cx)` is called first, which syncs the multi-buffer's snapshot with any pending buffer changes:
   ```rust
   fn edit_internal(...) {
       // ...
       self.sync_mut(cx);  // Updates snapshot from buffer changes
       let edits = edits.into_iter().map(|(range, new_text)| {
           let mut range = range.start.to_offset(self.snapshot.get_mut())  // Uses UPDATED snapshot!
               ..range.end.to_offset(self.snapshot.get_mut());
           // ...
       })
   }
   ```

4. The `to_offset` implementation for `MultiBufferOffset` has an assertion:
   ```rust
   impl ToOffset for MultiBufferOffset {
       fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> MultiBufferOffset {
           assert!(
               *self <= snapshot.len(),
               "offset {} is greater than the snapshot.len() {}",
               self.0,
               snapshot.len().0,
           );
           *self
       }
   }
   ```

5. If the underlying buffer(s) were modified between when the offsets were computed and when `sync_mut` runs, the snapshot can be **shorter** than expected, causing the offset to exceed `snapshot.len()`.

### Why buffer changes can occur

The `buffer_changed_since_sync` flag is set when underlying buffers are modified. These changes can come from:
- Background processes (LSP, formatting, diagnostics)
- Collaborative editing
- Other views of the same buffer
- Async operations completing

Even though completion confirmation runs on the main thread, buffer modifications from other sources can set the `buffer_changed_since_sync` flag, which `sync_mut` then applies.

## Reproduction

The crash can be reproduced by:
1. Opening a multi-buffer view (project search, split diff, etc.)
2. Opening the completion menu
3. Having an async process modify the underlying buffer (e.g., LSP formatting, collab edit)
4. Confirming the completion

A deterministic test that triggers this crash:

```
cargo test -p multi_buffer test_to_offset_with_stale_offset
```

## Suggested Fix

Change the `to_offset` implementation for `MultiBufferOffset` to clamp the offset instead of asserting. This is the minimal, defensive fix that prevents the crash while preserving intended behavior for valid offsets:

```rust
impl ToOffset for MultiBufferOffset {
    fn to_offset<'a>(&self, snapshot: &MultiBufferSnapshot) -> MultiBufferOffset {
        // Clamp to valid range to handle stale offsets from snapshot changes
        MultiBufferOffset(self.0.min(snapshot.len().0))
    }
    // ... rest unchanged
}
```

### Why clamping is appropriate:
1. **Edit semantics**: When an edit range extends past the buffer end, clamping to the end is the natural fallback (equivalent to editing to EOF)
2. **Defensive programming**: Other offset types (like `saturating_sub`) already use clamping semantics
3. **Minimal change**: This doesn't change behavior for valid offsets
4. **Matches existing patterns**: The code already handles `range.start > range.end` by swapping them

### Alternative approaches considered:
- **Use anchors**: Would require refactoring the edit API, higher risk
- **Abort completion**: Poor UX for the user
- **Synchronize snapshot earlier**: Would require changes to the completion flow
