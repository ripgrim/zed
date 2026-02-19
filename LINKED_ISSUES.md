# Potentially Related GitHub Issues

## High Confidence
- None found

## Medium Confidence
- [#28586](https://github.com/zed-industries/zed/pull/28586) — Fix bugs with multicursor completions
  - Why: Same code area (completion flow) and similar crash pattern (multibuffer completion crash)
  - Evidence: Fixed "crash when accepting a completion in a multibuffer with multiple cursors"; this crash also involves multibuffer completions
- [#49047](https://github.com/zed-industries/zed/pull/49047) — multi_buffer: Fix "cannot seek backward" crash in summaries_for_anchors
  - Why: Same crate (multi_buffer) with related synchronization issues between stale state and updated excerpts
  - Evidence: Both involve stale data (stale locators vs stale offsets) causing crashes during multi_buffer operations

## Low Confidence
- [#28820](https://github.com/zed-industries/zed/pull/28820) — Fix slice crash in `do_completion`
  - Why: Same function (do_completion) with slice-related crash
  - Evidence: Different crash site (slice indexing vs offset assertion), but same code area

## Reviewer Checklist
- [ ] Confirm High confidence issues should be referenced in PR body
- [ ] Confirm any issue should receive closing keywords (`Fixes #...`)
- [ ] Reject false positives before merge
