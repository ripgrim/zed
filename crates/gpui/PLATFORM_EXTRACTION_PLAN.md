# Plan: Extracting GPUI Platform Implementations

## Goal

Make it easier to iterate on platform implementations by:
1. Making platform traits public so external crates can implement them
2. Creating a `gpui_platform` convenience crate so consumers don't need `#[cfg]` dances
3. Eventually moving platform impls (mac, linux, windows) into their own crates

## Non-goal

Making gpui itself fully platform-agnostic. gpui will keep its platform dependencies
(e.g. for screen capture, which uses platform-specific types like `scap`). The point
is that the platform *implementations* live outside gpui, not that gpui has zero
platform awareness.

## Current State

- `platform.rs` defines traits: `Platform`, `PlatformWindow`, `PlatformDisplay`, `PlatformDispatcher`, `PlatformTextSystem`, `PlatformAtlas`
- `Platform`, `PlatformWindow`, `PlatformTextSystem`, `PlatformAtlas` are `pub(crate)`
- `PlatformDisplay`, `PlatformDispatcher` are already `pub`
- Platform impls live in `src/platform/{mac,linux,windows,test}/`
- `current_platform(headless: bool) -> Rc<dyn Platform>` is `pub(crate)`, has 3 cfg'd versions
- `Application::new()` calls `current_platform(false)` internally
- `App::new_app()` already takes `Rc<dyn Platform>` — the right seam exists

## Phase 1: Make traits and types public in gpui

Change visibility from `pub(crate)` to `pub`:

### Traits
- `Platform`
- `PlatformWindow`
- `PlatformTextSystem`
- `PlatformAtlas`

### Structs/enums used in trait signatures
- `RequestFrameOptions` (and its fields)
- `WindowParams` (and its fields)
- `PlatformInputHandler`
- `AtlasKey`, `AtlasTile`, `AtlasTextureId`, `AtlasTextureKind`, `TileId`
- `NoopTextSystem`

### Functions
- `current_platform` — make `pub`
- `get_gamma_correction_ratios` — keep `pub(crate)`, not part of the trait surface

### Notes
- Adding `#[allow(missing_docs)]` to newly-public items is acceptable for now since `platform.rs` is under `#![deny(missing_docs)]`.
- Do NOT change any logic, just visibility.

## Phase 2: Add `Application::with_platform()`

In `app.rs`, add a constructor:

```rust
pub fn with_platform(platform: Rc<dyn Platform>) -> Self {
    Self(App::new_app(
        platform,
        Arc::new(()),
        Arc::new(NullHttpClient),
    ))
}
```

This lets external code provide a platform without touching `current_platform`.

## Phase 3: Create `gpui_platform` crate

Create `crates/gpui_platform/` that:
- Depends on `gpui`
- Re-exports `gpui::current_platform` as its own `current_platform`
- Re-exports the `Platform` trait
- Consumers depend on `gpui_platform` instead of doing cfg gating themselves

```
crates/gpui_platform/
├── Cargo.toml
└── src/
    └── gpui_platform.rs
```

The crate is intentionally thin — just a facade. Later, when platform impls are extracted into their own crates, `gpui_platform` will depend on `gpui_macos`, `gpui_linux`, `gpui_windows` (behind cfg) and wire up `current_platform` from those.

Add to workspace `Cargo.toml` members and `[workspace.dependencies]`.

## Phase 4 (future): Extract platform impls into separate crates

Not in this PR. Rough shape:
- `gpui_macos` — everything in `platform/mac/`
- `gpui_linux` — everything in `platform/linux/` (wayland, x11, headless)
- `gpui_windows` — everything in `platform/windows/`
- `gpui_blade` — shared blade renderer used by linux + macos-blade
- Keep `platform/test` inside gpui (tightly coupled to test infra)

These crates depend on `gpui` for trait definitions. No circular deps because gpui doesn't depend on them.

## OS-specific trait methods (known issue for Phase 4)

The `Platform` and `PlatformWindow` traits have `#[cfg(target_os)]` methods:
- `read_from_find_pasteboard` / `write_to_find_pasteboard` — macOS only
- `read_from_primary` / `write_to_primary` — Linux only
- `get_raw_handle` — Windows only

When extracting impls, these should become either:
- Platform-specific extension traits (e.g. `MacPlatformExt`)
- Or methods with default no-op impls (some already are)

This is a Phase 4 concern, not blocking Phase 1-3.

## Screen capture API caveat

The `Platform` trait has screen-capture methods (`is_screen_capture_supported`,
`screen_capture_sources`) gated behind `#[cfg(feature = "screen-capture")]` that
use platform-specific types from `scap`. These stay in gpui — platform impl crates
will implement the trait methods, but the feature flag and `scap` dependency remain
in gpui. This is acceptable because the goal is extracting *implementations*, not
purging all platform deps from gpui.

## Order of operations

1. Phase 1: visibility changes in `platform.rs` — one commit
2. Phase 2: `Application::with_platform()` — one commit
3. Phase 3: create `gpui_platform` crate — one commit
4. Verify: `cargo check` on all targets, run gpui tests