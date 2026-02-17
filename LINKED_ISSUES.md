# Linked Issues for ZED-1J8

## Summary
ZED-1J8 is a crash in `MetalAtlasState::texture()` when attempting to render a sprite whose atlas texture has been freed. The crash occurs when `Option::unwrap()` is called on a `None` value at `metal_atlas.rs:185`.

## Search Strategy
Searched for issues related to:
- Atlas texture crashes
- Metal renderer crashes
- Image cache crashes
- Polychrome/monochrome sprite rendering issues
- `drop_image` related crashes

## Potentially Related Issues

### High Confidence
None found via GitHub issue search. This appears to be a previously unreported class of crash.

### Medium Confidence
None found. The specific interaction between image cache cleanup and rendering timing hasn't been documented in existing issues.

### Low Confidence
None found that directly relate to this crash pattern.

## Notes
- This crash has 535 events in Sentry, indicating it's a relatively common issue
- First seen: 2025-09-22, Last seen: 2026-02-16
- The crash affects macOS users specifically (Metal renderer)
- Similar crashes could theoretically occur on Windows (DirectX) and Linux (wgpu) as they have similar atlas implementations, but the Sentry data shows this specific crash is from macOS

## Recommendation
This appears to be a new/unique crash that should be fixed without waiting for linked issues. The fix should also be applied to the DirectX and wgpu atlas implementations for consistency, as they have similar `texture()` functions that could panic.
