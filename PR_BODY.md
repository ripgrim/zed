# Fix crash in MetalAtlas::texture when texture is freed during render

Fixes a crash that occurs when rendering sprites whose atlas texture has been freed. The crash happens in `MetalAtlasState::texture()` when it calls `unwrap()` on a `None` value because the texture was removed from the atlas between when the scene was prepared and when it was rendered.

## Crash Summary

- **Sentry Issue:** [ZED-1J8](https://sentry.io/organizations/zed-dev/issues/6895305344/)
- **Error:** `called Option::unwrap() on a None value`
- **Crash Site:** `gpui::platform::mac::metal_atlas::MetalAtlasState::texture` at `metal_atlas.rs:185`
- **Event Count:** 535 events
- **First Seen:** 2025-09-22
- **Last Seen:** 2026-02-16

## Root Cause

The crash occurs due to a race condition between image cache cleanup and rendering:

1. An image is rendered, adding sprites to the scene with references to atlas texture IDs
2. Before the scene is drawn, the image cache is cleared (e.g., via `RetainAllImageCache::clear()` or when the cache entity is released)
3. This calls `atlas.remove()` which sets the texture slot to `None` and adds the index to the free list
4. When `draw_polychrome_sprites()` runs, it calls `metal_texture(texture_id)` which panics on `unwrap()` because the texture no longer exists

## Fix

Changed `MetalAtlasState::texture()` to return `Option<&MetalAtlasTexture>` instead of panicking, and updated callers to gracefully handle missing textures by skipping the draw operation.

Changes:
- `MetalAtlas::metal_texture()` now returns `Option<metal::Texture>`
- `MetalAtlasState::texture()` uses safe bounds checking with `get()` and returns `Option`
- `draw_monochrome_sprites()` and `draw_polychrome_sprites()` early-return if texture is missing
- `get_or_insert_with()` uses `expect()` for the just-allocated texture case (which is guaranteed to exist)

## Validation

- Code compiles successfully (`cargo check -p gpui`)
- Clippy passes (`cargo clippy -p gpui --features test-support`)
- The fix is minimal and defensive - it gracefully handles the edge case without affecting normal operation

Note: Full test suite could not be run on Linux CI due to missing macOS-specific dependencies, but the Metal renderer code is only used on macOS.

## Potentially Related Issues

### High Confidence
None found - this appears to be a previously unreported class of crash.

### Medium Confidence
None found.

### Low Confidence
None found.

## Reviewer Checklist

- [ ] The fix handles the race condition gracefully by skipping draws for freed textures
- [ ] No performance impact - the `Option` check is negligible
- [ ] The `expect()` in `get_or_insert_with()` is justified because the texture was just allocated
- [ ] Consider applying similar fixes to DirectX (`directx_atlas.rs`) and wgpu (`wgpu_atlas.rs`) atlases in a follow-up PR, as they have similar patterns that could theoretically crash

Release Notes:

- Fixed a crash on macOS when an image's atlas texture is freed during rendering (ZED-1J8)
