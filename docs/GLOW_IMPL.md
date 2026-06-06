# IMPL — `backdrop-blur-glow` + the egui grab-pass adapter

The build plan for the design in [`GLOW_DESIGN.md`](./GLOW_DESIGN.md) (read it first; this is the *how and in what order*). Design + the `unsafe` boundary are signed off. Every step leaves `fmt`/`clippy --workspace --all-targets -D warnings`/`test --workspace`/`build` green; the repo works directly on `main`, so green-per-step is the only guard. **Revised after a four-lens adversarial review (20 findings, all acted on — note at the end).**

---

## 0. What already exists (verified against the tree)

- **`backdrop-blur-core`** (`#![forbid(unsafe_code)]`, no GPU dep). `seam.rs`: `BackdropBlur` + `GrabPass: BackdropBlur` (`grab_source(…, region: Region)`). `geometry.rs`: `Region { origin:[u32;2], size:[u32;2], scale }` (**doc'd top-left**, `geometry.rs:43`) + `clip_to`, `ResolvedMask::from_target`, `BlurRequest { source_region, target_rect, strength, tint, corner_radius }`. `error.rs`: `BlurError { ResourceCreation{stage,source}, UnsupportedTarget{format}, GrabFailed{region,source} }` + `BlurStage` (6 wgpu-shaped variants; the only two edit sites are the `Display` match `error.rs:73-80` and the `stage_display_covers_every_variant` array `error.rs:133-145` — there is no other exhaustive match). `material.rs`: `BlurStrength` (the "no notion of levels" sentence at `material.rs:13-14`).
- **`backdrop-blur-wgpu`** (safe, v1, **frozen**). `cache.rs`: the GPU-free math to hoist + the **format-coupled** `SCRATCH_FORMAT` (`Rgba16Float`) and `composite_encode_srgb(format) -> Option<bool>` (`cache.rs:87`; **`None` is the load-bearing "format not on the allowlist" signal**, consumed at `lib.rs:331-334` as `UnsupportedTarget`). The hoist targets (`PingPongKey` `:19`, `resolve_gaussian` `:35`, etc.) are `pub(crate)`. `uniforms.rs`: the Pod params + layout tests. `shaders/*.wgsl`: the WGSL the glow GLSL is **ported** from (not shared); the composite outputs **straight alpha** (`vec4(rgb, coverage)`).
- **`backdrop-blur-egui`** (`#![forbid(unsafe_code)]`). `Cargo.toml` deps `backdrop-blur-wgpu`/`egui-wgpu`/`wgpu` **unconditionally**; `image-snapshots` feature is `[]` (does **not** imply own-loop). `lib.rs:21-24` re-exports — **neutral:** the five core types (`BlurStrength, CornerRadius, LinearRgba, RepaintPolicy, Tint`, line 21); **wgpu-coupled (must gate):** `SourceColorSpace, SourceView, WgpuBlur` (22), `ScreenDescriptor` (23), `FrameInput, OwnLoopRenderer, is_supported_target, strongest_repaint` (24). `own_loop.rs`: `Surface` + a **private** `fn request` (`own_loop.rs:31`, builds a **top-left** `BlurRequest` from `rect.min * ppp`), `SeamContext`, `composite_surfaces`, `strongest_repaint`. The gated test `tests/own_loop_render.rs` hard-imports `backdrop_blur_wgpu` + `wgpu::*` under `#![cfg(feature = "image-snapshots")]` only.
- **Examples** (all workspace-`exclude`d): `glow-gate` (the type-mapping stub this increment supersedes — imports `Region` at `lib.rs:39`, uses it at `grab_source` `lib.rs:131`, `GlPrepared.target_rect` `:81`, the doc `:133-135`, the §3 table `:27`); `egui-wgpu-panel` + `frost-gallery` (both keep `own-loop` by default — neither sets `default-features = false`). CI `examples` job builds **only** glow-gate + egui-wgpu-panel (frost-gallery is **not** in CI).
- **Workspace** `Cargo.toml`: `members = ["crates/*"]` (a new `crates/backdrop-blur-glow/` is auto-included), `exclude = [the three examples]`. The comment "never compile glow/winit" refers to the **winit-pulling examples**, not a crate. **CI** (`ci.yml`): `default-tier` (`clippy --workspace --all-targets` + `test --workspace`), `msrv`, `gpu` (lavapipe, `image-snapshots --test-threads=1`), `examples`.
- **Verification feasibility (demonstrated):** native glow over EGL-surfaceless → headless GL 3.3 + correct `glReadPixels`; headless Chromium → real WebGL2 + `EXT_color_buffer_float` + RGBA16F readback.

---

## 1. Architecture decisions specific to the build

1. **`GlRegion` newtype in core (not a phantom `Region<Space>`).** Lower blast radius — one seam method (`GrabPass::grab_source`, glow-only) + glow-internal types, no churn to frozen wgpu. It guards the two flip-sensitive surfaces (`grab_source`'s coords; the composite uniform's `rect_origin`); the intervening resolution is orientation-agnostic. The single bridge into the shared `BlurRequest` is `GlRegion::into_region()` — a documented *reinterpret*, no arithmetic; **the one line the IMPL review scrutinizes, and the only place a `framebuffer_height − y` could hide.**
2. **The glow crate is a portable-compiling workspace member; only its GPU *tests* are gated (review C1, Option A).** `glow` 0.17 is pure runtime-loaded function pointers with no native-link build script, so the crate compiles on any runner with no GL present. **All** GPU tests live behind a `gl-snapshots` feature, so plain `cargo test --workspace` runs only its **Tier-0** (pure) tests — which is exactly the "must-pass-everywhere, grows per step" backbone. Consequence, stated rather than hidden: `clippy --workspace --all-targets` now lints the unsafe crate — **desirable** (we want clippy + the `deny(undocumented_unsafe_blocks)` lint on it). The stale `Cargo.toml` "never compile glow" comment is rewritten to name the real exclusion (winit examples). The `glow-gate` example is **retired** in Step 2 (the real crate supersedes its type-mapping proof).
3. **Step order keeps green by dependency:** `core` (0) → {egui feature-gating (1), glow crate (2)} → grab-pass renderer (3) → verification (4) → examples (5). (1) and (2) depend only on (0) and are independent.

---

## 2. Dependency graph

```
0  core: GL BlurStage + UnsupportedContext + GlRegion + hoist algorithm math + TargetEncoding   (wgpu green)
├─ 1  backdrop-blur-egui: feature-gating (own-loop default / grab-pass=[]) + spine hoist          (own-loop green)
└─ 2  backdrop-blur-glow: the GL backend (portable member; GPU tests gated); retire glow-gate     (Tier-0 green)
        └─ 3  egui grab-pass: GrabPassRenderer + frost + callback (adds the glow dep here)         (needs 1+2)
              ├─ 4  verification tiers (gl-snapshots native EGL + wasm WebGL2) + CI jobs
              └─ 5  preview eframe-glow example + glow synthetic-FBO gallery
```

---

## Step 0 — core: errors, `GlRegion`, hoisted math

- **0a — error variants.** Add the GL-shaped `BlurStage` variants (`ShaderCompile`, `ProgramLink`, `Framebuffer`, `VertexArray`) — extend the `Display` match (`error.rs:73-80`) and the test array (`:133-145`). **Also add `BlurError::UnsupportedContext { detail: String }` here** (it is a core-`error.rs` edit, so it belongs in this step, not Step 3 where it is merely *constructed*) with its `Display` + a construction test. *Green:* `cargo test -p backdrop-blur-core`.
- **0b — `GlRegion`.** Core newtype `GlRegion { origin_bl:[u32;2], size:[u32;2], scale }` (bottom-left), constructed only from bottom-left pixel inputs (`from_bottom_px([u32;2], [u32;2], Scale)` — named for the GL-origin precondition it enforces; core cannot take egui's `ViewportInPixels`, so the field-mapping + cast happens at the Step-3 call site), with saturating `intersect(&GlRegion)` (the `clip_rect ∩ viewport` step) + `clip_to(extent)` (the framebuffer ∩), **no `from(Region)` flip ctor**, and `into_region() -> Region` (documented reinterpret, the §1 bridge). `GlRegion` also has a bottom-left-marked `Display`, so `BlurError::GrabFailed` carries a `GlRegion` (not a reinterpreted `Region`) — the one place `into_region` would otherwise leak orientation into a human-facing message. Change `GrabPass::grab_source`'s param `region: Region → GlRegion` (`seam.rs:125`). **Update `glow-gate` to keep the gate compiling:** add `GlRegion` to its core import (`lib.rs:39`), change the `grab_source` param (`lib.rs:131`), `GlPrepared.target_rect` (`:81`), and rewrite the flip doc (`:133-135`) + §3 table cell (`:27`) to say *the flip moved to the caller*. *Green:* core tests (intersect/clip, both empty cases); `cargo build --manifest-path examples/glow-gate/Cargo.toml`.
- **0c — hoist the algorithm math.** Move the GPU-free fns from `wgpu/cache.rs` into a new core `algorithm.rs` (`use_dual_kawase`, `KAWASE_THRESHOLD_PX`, `resolve_kawase_levels`, `MAX_KAWASE_LEVELS`, `kawase_level_size`, `kawase_halfpixel`, `resolve_gaussian`/`GaussianKernel`/`MAX_GAUSSIAN_RADIUS`, `backdrop_uv_remap`, `PingPongKey`), exported **`pub`** from core. Move their tests **with only the crate-path prefix adjusted** (`backdrop_blur_core::` → `crate::`/`super::`; **no body/assertion change** — that is the no-behavior-change proof, not a byte-identical move). In `wgpu/cache.rs` consume them via **`pub(crate) use backdrop_blur_core::{…}`** (the hoist targets were `pub(crate)`; a bare `pub use` would widen wgpu's public API — keep them crate-private). Add a core enum **`TargetEncoding { Linear, Srgb }`** (shared vocabulary). Keep `composite_encode_srgb` **and its `wgpu::TextureFormat` allowlist test** in the wgpu backend (format-coupled, cannot move into the GPU-free core), changing only its return to **`Option<TargetEncoding>`** (`Some(false)→Some(Linear)`, `Some(true)→Some(Srgb)`, `None→None`) so the `lib.rs:331-334` `UnsupportedTarget` path is unchanged. The glow backend never calls this allowlist (it derives encode at runtime, Step 2f). Annotate `material.rs:13-14` to record the "no notion of levels" reversal. *Green:* `cargo test --workspace` (wgpu unchanged) + core.

**File changes:** `core/src/{error.rs, gl_region.rs (new), seam.rs, algorithm.rs (new), material.rs, lib.rs}`; `wgpu/src/cache.rs`; `examples/glow-gate/src/lib.rs`.

---

## Step 1 — backdrop-blur-egui: feature-gating + spine hoist

**Breaking public-API change**, deliberate. **own-loop stays the default, so the excluded examples are unaffected** (they don't set `default-features = false`) — no example edits, no `--features own-loop` action needed.

- **1a — features + optional deps.** Mark `backdrop-blur-wgpu`/`egui-wgpu`/`wgpu` `optional = true`; `[features]`: `own-loop = ["dep:backdrop-blur-wgpu","dep:egui-wgpu","dep:wgpu"]` (default), **`grab-pass = []`** (spine-only — it genuinely compiles; the `dep:backdrop-blur-glow` entry is added in **Step 3**, where the crate exists, *not here* — a `dep:` to an absent crate fails manifest parse and reds the whole workspace), and **`image-snapshots = ["own-loop"]`** (the gated test hard-imports wgpu).
- **1b — hoist the neutral spine + gate the rest.** Move the `Surface` **struct** into a feature-neutral `surface.rs` (`RepaintPolicy` stays a neutral core re-export in `lib.rs`, not moved). **`Surface::request` (top-left) is uncallable from a grab-pass build** because its whole `impl Surface { fn request }` lives inside `mod own_loop`, which is module-gated `#[cfg(feature = "own-loop")]` — *module-gated, not a per-fn attribute* — making the relocated-flip bug unrepresentable (review C3). Gate behind `#[cfg(feature = "own-loop")]` in `lib.rs`: `mod own_loop`, and the wgpu-coupled re-exports (`SourceColorSpace, SourceView, WgpuBlur, ScreenDescriptor, FrameInput, OwnLoopRenderer, is_supported_target, strongest_repaint`). **Stay neutral:** the five core re-exports + the hoisted `Surface`. Also gate `tests/own_loop_render.rs` on `all(feature = "image-snapshots", feature = "own-loop")` (defense in depth).
- **1c — both builds green.** `cargo test -p backdrop-blur-egui` (own-loop default, + `--features image-snapshots`) unchanged; `cargo build -p backdrop-blur-egui --no-default-features --features grab-pass` compiles (spine only). **frost-gallery is currently outside CI** — decide: add it to the `examples` job (recommended, it caught real bugs) or note it explicitly stays manual.

**File changes:** `egui/{Cargo.toml, src/lib.rs, src/surface.rs (new), src/own_loop.rs (cfg), tests/own_loop_render.rs (cfg)}`, `ci.yml` (frost-gallery decision).

---

## Step 2 — backdrop-blur-glow: the GL backend

A workspace member (§1.2) under `crates/backdrop-blur-glow/`, depending on `core` + `glow`. **The only `unsafe` crate:** `#![deny(unsafe_op_in_unsafe_fn)]` + `#![deny(clippy::undocumented_unsafe_blocks)]`; every block `// SAFETY:`-commented. GPU tests behind `gl-snapshots`; Tier-0 tests run in the default `--workspace` loop. **Workspace edit:** rewrite the `Cargo.toml` "never compile glow" comment (it meant the winit examples); the new crate needs no `exclude`. **Retire `glow-gate`:** delete the example + its CI `examples` step + (it has no `exclude` entry of its own beyond the list) the `exclude` entry, mirroring §16's spike retirement — the real crate is the type-mapping proof now.

- **2a — `GlProfile`.** `classify(version, glsl_version, extensions, samples) -> GlProfile` is **pure** (Tier 0): shader-version class (`Es300`/`GlDesktop`), `embedded` flag, `renderable_float: { Rgba16F | Srgb8Rgba8 }`, `samples`. `probe(&glow::Context)` is the thin `glGet*` wrapper. *Green:* Tier-0 classify cases.
- **2b — shaders + programs + VAO + destroy.** The GLSL ES 3.00 sources are a **real WGSL→GLSL translation, not transcription** — enumerate the work: `select(a,b,vec3<bool>)` in the transfer functions → `mix(a, b, vec3(lessThanEqual(...)))` (GLSL-ES has no `select`; a wrong port silently shifts gamma); `textureSampleLevel(t,s,uv,0.0)` → `textureLod`; `@builtin(vertex_index)`→`gl_VertexID`, `@builtin(position)`→`gl_FragCoord`; ES `precision highp float; precision highp int;`; and the fullscreen-triangle clip-space-Y, **re-derived** for GL's bottom-left origin, not copied. **Tier-0 guard:** unit-test the GLSL `srgb_to_linear`/`linear_to_srgb` host-side against the WGSL constants at identical sample points (a numeric match) — a non-trivial readback would not catch a gamma slip. The `version_header(class, embedded)` adapter is also Tier-0. `GlowBlur::new` compiles/links + caches programs (failure → `BlurStage::ShaderCompile`/`ProgramLink`), creates the shared VAO. `destroy(&glow::Context)`; `Drop` issues **no GL**, `log_warn`s if not destroyed. *Green:* Tier-0 (header, transfer-function match); **Tier-1**: programs link on a real context.
- **2c — grab_source.** Query `GL_DRAW_FRAMEBUFFER_BINDING` (capture the live target); bind it `GL_READ_FRAMEBUFFER`; if `GlProfile.samples > 0`, `glBlitFramebuffer`-resolve into a **single-sample resolve FBO/texture** (else flat-fallback); **(re)size the grab texture and the resolve target to the clamped `GlRegion` size on any change** (`copyTexSubImage2D` does not allocate — a grown region past a pre-sized texture is `GL_INVALID_VALUE`; size-key them like `PingPongKey`), then `copyTexSubImage2D`. Save/restore read-FBO + bound texture. *Green:* **Tier-1**: grab a known FBO region, read back, assert match; **a grow-region frame**; a bottom-edge case (y-origin).
- **2d — Gaussian path.** `GlPrepared { mask, tint, target_rect: GlRegion, pass: GlBlurPass::Gaussian{…}, scratch keys }`; scratch keyed by `PingPongKey` with **last-frame-used eviction** (pure decision, Tier-0); decode-on-first-sample → linear scratch (`Rgba16F` or `Srgb8Rgba8` fallback). *Green:* Tier-0 eviction; **Tier-1** small-radius properties.
- **2e — dual-Kawase path.** `GlBlurPass::DualKawase { halfpixels }` over the pyramid (the hoisted level/halfpixel math). *Green:* **Tier-1** energy + transition-width.
- **2f — composite (a rewrite, not a port — record the divergence).** Full-screen triangle under **`glViewport(0,0,screen_w,screen_h)`** (override egui's panel viewport), **`GL_SCISSOR_TEST` disabled**, SDF + `backdrop_uv_remap` (bottom-left uniforms). **Premultiplied output contract, written here:** `out_rgb = encode(mix(blurred, tint.rgb, tint.a)) · coverage; out_a = coverage;` with `glBlendFunc(ONE, ONE_MINUS_SRC_ALPHA)` — **encode happens *before* the coverage multiply** (concave encode → wrong order is the overshoot halo), and **`out_a == coverage`** (so `(1−srcα)` matches `(1−coverage)`). This **differs from wgpu's straight-alpha composite — record the divergence**. Encode bit: on web set `encode_srgb = 1` **without** querying (`GL_FRAMEBUFFER_SRGB` is not a WebGL2 enum — `glIsEnabled` would raise `GL_INVALID_ENUM` and pollute error state); on native, `encode_srgb = !glIsEnabled(GL_FRAMEBUFFER_SRGB)` sampled **after binding the captured target** as draw FBO (so it reflects the real target, not egui's pre-callback default). *Green:* **Tier-1** the **premultiplied `assert_no_edge_halo` envelope, both directions**; a straight-edge-AA transition-width assertion (catches a panel-viewport regression); a `clip_rect ∩` bleed check.
- **2g — wire the seam + lifecycle.** Full `BackdropBlur` + `GrabPass`; the **complete** save/restore list (design §11); context-identity stamping. *Green:* **Tier-1** end-to-end frost + a GL-state-unchanged assertion across `record`.

**File changes:** new `crates/backdrop-blur-glow/{Cargo.toml, src/{lib.rs, profile.rs, program.rs, scratch.rs, grab.rs (incl. the MSAA resolve target), composite.rs}, src/shaders/*.glsl, tests/}`; workspace `Cargo.toml` (comment rewrite, `[workspace.dependencies]` entry, drop glow-gate from `exclude`); delete `examples/glow-gate/`; `ci.yml` (drop the glow-gate step).

---

## Step 3 — egui grab-pass adapter

Stays `#![forbid(unsafe_code)]`. **Adds the glow dep here:** `Cargo.toml` `backdrop-blur-glow = { workspace = true, optional = true }` and `grab-pass = ["dep:backdrop-blur-glow"]` (C2 — the dep now exists).

- **3a — `GrabPassRenderer::new(&Arc<glow::Context>) -> Result<Self, BlurError>`** (probe `GlProfile`, build `GlowBlur` behind `Arc<Mutex<…>>`, reject too-old contexts → the **`UnsupportedContext`** added in Step 0a — Step 3 only *constructs* it). `destroy(&glow::Context)` → `GlowBlur::destroy`.
- **3b — `frost(&self, ui, surface)`.** Drive repaint (`Live`→`request_repaint`; `Bounded`→`request_repaint_after`, best-effort). Enqueue an `egui_glow::CallbackFn` capturing an `Arc` clone + the surface. **Inside the callback (the load-bearing construction, named explicitly, C3):** egui's `info.viewport_in_pixels()` / `clip_rect_in_pixels()` return `ViewportInPixels` (five `i32` fields: `left_px, top_px, from_bottom_px, width_px, height_px`), so build each `GlRegion` by mapping those fields — **the `i32 → u32` cast is the named boundary** (egui clamps them non-negative in `from_points`, so the cast is safe): `let vp = info.viewport_in_pixels(); let gl = GlRegion::from_bottom_px([vp.left_px as u32, vp.from_bottom_px as u32], [vp.width_px as u32, vp.height_px as u32], Scale::new(info.pixels_per_point));` then the same for `clip_rect_in_pixels()`, `gl.intersect(&clip)?.clip_to(screen_size_px)?` (each `None` → skip the blur, a no-op). Assemble `BlurRequest { source_region: gl.into_region(), target_rect: gl.into_region(), strength: surface.strength, tint: surface.tint, corner_radius: surface.corner_radius }` — **material fields read straight off `surface`, geometry from the GL-origin region; `Surface::request` is `cfg(own-loop)` and unreachable here.** Then `grab_source → prepare → record`; ran-flag set at **callback entry** (design §7); `debug_assert` paint-thread.
- **3c — host-obligation doc + the CI feature-unification guard** (`cargo tree -i wgpu` empty on `--no-default-features --features grab-pass`). *Green:* `cargo build -p backdrop-blur-egui --no-default-features --features grab-pass`; **Tier-1** a real egui frame frosted through the callback (grab-pass analog of `own_loop_render.rs`), gated on `all(feature="grab-pass", feature="gl-snapshots")`.

**File changes:** `egui/{Cargo.toml, src/grab_pass.rs (new, cfg), src/lib.rs (cfg re-exports), tests/grab_pass_render.rs (new, gated)}`, `ci.yml` (guard step).

---

## Step 4 — verification tiers + CI

- **4a — native `gl-snapshots`.** A test-support EGL-surfaceless harness (`EGL_PLATFORM_SURFACELESS_MESA`, `eglBindAPI(OPENGL)`, 3.3 ctx, `glow::Context::from_loader_function`) behind `gl-snapshots`; deps `khronos-egl` + `libloading` as **`[target.'cfg(not(target_arch = "wasm32"))'.dev-dependencies]`**. Compile-gated, must-pass on the GL CI runner, absent elsewhere. `--test-threads=1`.
- **4b — web tier.** Prereq: **verify `glow::Context::from_webgl2_context` exists in glow 0.17** (design §17 flags it inferred) and that **the glow crate compiles to `wasm32-unknown-unknown`** — add both as explicit green checks. `wasm-bindgen-test` deps under `[target.'cfg(target_arch = "wasm32")'.dev-dependencies]` (the EGL deps and these cannot both be unconditional). **(A)** a static WebGL2 page (the exact GLSL strings) driven by headless Chromium (Playwright), `readPixels` vs the same channel-delta properties; **(B)** `wasm-pack test --headless --chrome` on a WebGL2 canvas. Pinned `--use-angle=swiftshader`; SwiftShader doubles as the GLES3 proxy.
- **4c — CI.** A `gl-native` job (EGL-surfaceless runner, `--features gl-snapshots`); a `web` job (Chrome + wasm32, 4b); the feature-unification guard. The default tier stays GPU-free **because all GPU tests are `gl-snapshots`-gated** (§1.2). One-standard *property* assertions, not committed bytes.

**File changes:** `glow/Cargo.toml` (target-split dev-deps, `gl-snapshots`), `glow/tests/`, `egui/tests/`, a web harness dir, `ci.yml`.

---

## Step 5 — preview example + glow gallery

- A `preview`-gated **eframe-on-glow** example (the spike generalized to the real backend) — the human visual gate (halo, corners, animation, a frosted card in a `ScrollArea`), workspace-excluded.
- The **glow gallery** reuses the **Step 4a EGL harness to frost a synthetic FBO directly via `GlowBlur`** (no egui paint callback exists in a headless driver — the existing `frost-gallery` is own-loop-only and cannot host the grab-pass). It is a `GlowBlur`-on-a-synthetic-backdrop image dump, not a `GrabPassRenderer` driver.

**File changes:** `examples/eframe-glow-panel/` (new, excluded), a glow image-dump (under the glow crate's gated tests or a small excluded example), workspace `exclude`, `ci.yml`.

---

## Risks and mitigations (consolidated)

| Risk | Mitigation |
|---|---|
| A hoisted fn (0c) changes wgpu behavior | Move its tests along (path-prefix only); `cargo test --workspace` green is the proof |
| `GlRegion::into_region` hides a flip | The single audited bridge (§1); `Surface::request` is `cfg(own-loop)`-unreachable from grab-pass, so the flip bug is unrepresentable |
| The unsafe crate enters the portable tier | Intended (clippy + deny-lints on it); all GPU tests `gl-snapshots`-gated so `test --workspace` is Tier-0 only |
| Feature unification re-adds wgpu to kiosk build | CI `cargo tree -i wgpu` guard (3c) |
| `image-snapshots` without `own-loop` | `image-snapshots = ["own-loop"]` (1a) |
| Grab texture / MSAA target undersized | Size-keyed reallocation (2c) + a grow-region Tier-1 test |
| Wrong WGSL→GLSL gamma port | Tier-0 transfer-function numeric match vs the WGSL constants (2b) |
| Premultiplied encode/coverage order | Written output contract + both-direction no-halo envelope (2f) |
| `glIsEnabled(FRAMEBUFFER_SRGB)` on WebGL2 | Web branch sets `encode=1` without the query (2f, m5) |
| Native EGL / web context absent on a runner | Compile-gated, **absent not skipped**; Tier-0 is the must-pass guard |
| Native teardown UB | No GL in `Drop`; explicit `destroy` from `on_exit` (2b) |
| Kiosk-GPU **performance** | Out of scope; deferred on-device benchmark (design §17) |

---

## Verification loop (per step)

`cargo fmt` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test --workspace` (Tier-0 only — GPU tests are gated) → `cargo build --workspace`; GPU steps additionally run the gated `gl-snapshots` tier on the EGL-surfaceless context. **Tier-0** (profile classify, header adapter, transfer-function match, eviction, `GlRegion` math, hoisted math, error variants) is the must-pass-everywhere backbone. Commit per green step; push stays Abdu's lever.

---

## Revision note (post-IMPL-review)

A four-lens adversarial review (ordering/green, file-accuracy, GL/unsafe, completeness) found 20 findings, all verified against the tree and acted on. **Criticals:** C1 the `crates/*` glob makes glow a member — embraced (Option A: portable-compiling member, GPU tests `gl-snapshots`-gated, comment rewritten, glow-gate retired); C2 a `dep:` feature for an absent crate reds the workspace — `grab-pass = []` in Step 1, the dep added in Step 3; C3 the orientation-free split was unspecified — the callback now names each `BlurRequest` field's source and `Surface::request` is `cfg(own-loop)`-unreachable. **Majors:** `image-snapshots = ["own-loop"]` (M1); drop the no-op example action + decide frost-gallery CI (M2); `composite_encode_srgb -> Option<TargetEncoding>` preserving the `UnsupportedTarget` path (M3); grab/MSAA texture reallocation (M4); `UnsupportedContext` moved to Step 0a (M5); the WGSL→GLSL port written as a real task + Tier-0 transfer-function test (M6); the premultiplied output contract written into Step 2f (M7). **Minors m1–m10** folded: `pub(crate) use` not `pub use`; "verbatim"→"path-prefix-only"; the exact re-export gating split; the glow-gate compile-blocking lines; the WebGL2 `glIsEnabled` guard + native sample-after-bind; the wasm cfg-split + entry-point verification; the synthetic-FBO gallery scope; glow-gate retirement; `material.rs:13-14`.
