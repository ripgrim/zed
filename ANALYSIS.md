# Crash Analysis: MetalAtlas texture lookup on None value

## Crash Summary
- **Sentry Issue:** ZED-1J8 (https://sentry.io/organizations/zed-dev/issues/6895305344/)
- **Error:** `called Option::unwrap() on a None value`
- **Crash Site:** `gpui::platform::mac::metal_atlas::MetalAtlasState::texture` in `metal_atlas.rs:185`
- **Event Count:** 535 events
- **Versions Affected:** 0.218.6+stable and likely others

## Root Cause

The crash occurs in `MetalAtlasState::texture()` at line 185:
```rust
fn texture(&self, id: AtlasTextureId) -> &MetalAtlasTexture {
    let textures = match id.kind {
        crate::AtlasTextureKind::Monochrome => &self.monochrome_textures,
        crate::AtlasTextureKind::Polychrome => &self.polychrome_textures,
        crate::AtlasTextureKind::Subpixel => unreachable!(),
    };
    textures[id.index as usize].as_ref().unwrap()  // <-- CRASH HERE
}
```

The call chain is:
1. `MetalRenderer::draw_polychrome_sprites()` (metal_renderer.rs:1009)
2. `MetalAtlas::metal_texture()` (metal_atlas.rs:26)
3. `MetalAtlasState::texture()` (metal_atlas.rs:185)

### Data Flow Analysis

The issue is a race condition between image cache cleanup and rendering:

1. **Image rendering:** When an image is rendered, it's added to the sprite atlas via `get_or_insert_with()`. The texture is stored at `textures[index]` and the `AtlasTile` contains a `texture_id` referencing this index.

2. **Scene batching:** The `Scene` collects sprites during the paint phase. Each `PolychromeSprite` contains a `tile` with a `texture_id`.

3. **Image removal:** When an image cache is cleared or an image is removed (e.g., via `RetainAllImageCache::clear()` or when the cache entity is released), `drop_image()` is called which invokes `atlas.remove()`.

4. **The bug:** In `metal_atlas.rs:remove()`, when a texture's reference count reaches zero:
   ```rust
   if texture.is_unreferenced() {
       textures.free_list.push(id.index as usize);
       lock.tiles_by_key.remove(key);
   }
   ```
   The texture slot is set to `None` (via `texture_slot.take()`) and the index is added to the free list for reuse.

5. **Crash:** During the same frame or shortly after, `draw_polychrome_sprites()` is called with a `texture_id` that was just freed. The lookup `textures[id.index].as_ref().unwrap()` panics because the slot is now `None`.

### Why This Happens

The problem is that the `Scene` holds `AtlasTextureId` references that can become stale when:
- The image cache is cleared mid-frame
- The image cache entity is released (via `observe_release`)
- Multiple windows share the same atlas but have different lifetimes

The `remove()` function aggressively frees textures without ensuring no pending renders reference them.

## Reproduction

The crash is difficult to reproduce deterministically as it requires specific timing between:
1. An image being rendered (adding to scene)
2. The image cache being cleared or released
3. The frame being drawn

A test case would need to:
1. Create a window with an image element
2. Trigger a render cycle that adds polychrome sprites to the scene
3. Clear the image cache before `draw()` is called
4. Call `draw()` which will attempt to render the now-freed texture

Command to run reproduction test:
```
cargo test -p gpui test_atlas_texture_removal_during_render
```

## Suggested Fix

The safest fix is to make `MetalAtlasState::texture()` return an `Option<&MetalAtlasTexture>` instead of panicking, and have callers handle the case where a texture no longer exists. This is a defensive approach that gracefully handles stale texture references.

### Option A: Graceful degradation in texture lookup (Recommended)

Modify `MetalAtlas::metal_texture()` to return `Option<metal::Texture>` and skip rendering sprites whose textures have been freed:

```rust
pub(crate) fn metal_texture(&self, id: AtlasTextureId) -> Option<metal::Texture> {
    self.0.lock().texture(id).map(|t| t.metal_texture.clone())
}

fn texture(&self, id: AtlasTextureId) -> Option<&MetalAtlasTexture> {
    let textures = match id.kind {
        crate::AtlasTextureKind::Monochrome => &self.monochrome_textures,
        crate::AtlasTextureKind::Polychrome => &self.polychrome_textures,
        crate::AtlasTextureKind::Subpixel => unreachable!(),
    };
    textures.get(id.index as usize).and_then(|t| t.as_ref())
}
```

Then in `draw_polychrome_sprites()` and `draw_monochrome_sprites()`, early return if the texture is missing.

### Option B: Defer texture removal until frame end

Keep textures alive until the end of the current frame by batching removals. This is more complex but preserves the current API.

### Recommended Approach

Option A is preferred because:
1. Minimal code changes
2. Gracefully handles edge cases
3. No performance impact (missing texture = skip draw)
4. Consistent with defensive programming principles
