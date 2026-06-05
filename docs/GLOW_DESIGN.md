# Design — `backdrop-blur-glow` + the egui grab-pass adapter

**Status:** design (pre-IMPL), revised through two adversarial review rounds (a five-lens review, then a focused correctness verification of the fixes). 13 + 17 findings acted on; see the revision note at the end.

The increment that gives **eframe-on-glow apps — and the cage kiosk — real frosted glass**, the use case v1 (own-loop/wgpu) deliberately did not serve (DESIGN §0).

**Two decisions locked with Abdu before this doc:**
1. **Scope = "crate now, migrate next."** This increment builds and verifies the reusable `backdrop-blur-glow` backend + the egui grab-pass adapter. The `abdu-egui-ui` frosted-Glass work is *specified here* (§16) but built as the next increment — and §16 makes clear it is a **new component**, not a wiring swap.
2. **GL targets = native GL 3.3-core + GLES 3.0 + WebGL2/wasm**, all three a verified surface this increment. WebGL1 is out.

Grounded in five research passes (the working spike; the wgpu backend; the verified egui_glow 0.34.3 API; and *demonstrated* native + web pixel-test feasibility). Divergences from the spike or v1 are called out with rationale (the repo's "record each divergence" rule).

---

## 1. Why this exists

A frosted-glass panel must show a **blurred copy of whatever is behind it**. Three facts force the design:

- **The kiosk runs eframe-on-glow under `cage`.** cage implements no compositor-side blur protocol, and a single fullscreen app has nothing behind it for a compositor to blur. The only route is **in-app**: grab the backdrop from the GL framebuffer, blur it, composite it back.
- **eframe owns the render loop.** Unlike the v1 own-loop path, an eframe app never yields the loop. The only hook is an **egui paint callback** running raw GL on egui's live context — the *grab-pass*.
- **v1 reserved the socket but did not fill it.** The seam (`BackdropBlur` + `GrabPass`) was compile-proven against glow in v1 (`examples/glow-gate`), so this increment is **additive, not a rewrite**.

A working proof exists — `abdu-egui-ui/examples/tooltip_blur_spike.rs` grabs egui's framebuffer with `copyTexImage2D` inside a glow paint callback, blurs, composites back. **But the spike is a feasibility probe, not a correctness reference:** it blurs in the wrong color space, has no rounded corners, no blend, leaves scissor/blend/program/VAO dirty (it survives only because egui_glow re-runs `prepare_painting` after each callback — confirmed at painter.rs), and `.expect()`s on GL failure. This increment keeps the spike's load-bearing trick and rebuilds everything around it correctly.

---

## 2. The user flow (plain English, end to end)

The consumer is an **eframe app built with the glow renderer** (`Renderer::Glow` + the `glow` feature). **Precondition:** only then is `cc.gl` populated. eframe's web backend *defaults to wgpu* (WebGPU→WebGL2); a consumer who wants the grab-pass on web must opt into `Renderer::Glow`, forgoing WebGPU. If `cc.gl` is `None`, the grab-pass is unavailable and the consumer draws a flat tint (§12).

1. **At startup** the app builds **one** `GrabPassRenderer` from `cc.gl: &Arc<glow::Context>`. It owns the cached GL resources (programs, per-size scratch, grab texture) and lives for the app's lifetime.
2. **Each frame, while laying out the UI**, for each glass panel the app calls `renderer.frost(ui, surface)` — *before* painting that panel's own foreground (the **host obligation**: the frost callback must be enqueued ahead of the panel's fill, or the grab would include the panel's own content). `frost` requests a repaint if the surface is live, and enqueues an egui paint callback at the panel's z-position.
3. **When egui paints**, its glow painter walks the draw list in z-order; by the time it reaches the panel's callback, **everything behind the panel is already in the framebuffer**. The callback fires with the live GL context and:
   - saves the GL state it will touch and **disables `GL_SCISSOR_TEST`** (egui enters the callback with scissor enabled, clamped to the panel's clip rect — left on, it would hard-clip the blur's edge AA);
   - **grabs** the panel region — clamped to `viewport ∩ clip_rect ∩ framebuffer` — out of the live framebuffer into a *separate* texture (copy-before-sample);
   - **blurs** it in linear light (Gaussian small radius, dual-Kawase large), in ping-pong scratch it owns;
   - **composites** the tinted, rounded-rect-masked, premultiplied result back over the panel under a **full-framebuffer viewport**;
   - **restores** the saved state so egui keeps painting unaffected.
4. egui paints the panel's **own foreground** on top.

The app writes no GL. Everything unsafe is inside `backdrop-blur-glow`.

---

## 3. Boundaries

**In scope:** `backdrop-blur-glow` (the only `unsafe` crate); the egui **grab-pass adapter** (`GrabPassRenderer` + `frost`) added to `backdrop-blur-egui` behind a feature; a **core refactor** hoisting the GPU-free algorithm math + GL-shaped error variants (§15); three GL targets from one glow shader source, **verified** native (EGL-surfaceless `glReadPixels`) and web (headless-Chromium WebGL2 readback).

**Out of scope (named, deferred):** the `abdu-egui-ui` frosted-Glass component (§16, next increment, its own design→IMPL→review gate); WebGL1/GL2.1; compositor-side blur; the **on-device kiosk-GPU performance benchmark** and any perf tuning it mandates — every blur budget in the research is desktop/2015-mobile, so cost on the weak kiosk GPU is unverified (§17). Correctness is settled here; the budget is not.

---

## 4. Architecture: three crates, three responsibilities

```
backdrop-blur-core      pure vocabulary + ALL backend-agnostic resolution math   (#![forbid(unsafe_code)])
        ▲                       ▲
backdrop-blur-wgpu      backdrop-blur-glow      two sibling backends, one seam
  (safe, v1, frozen)      (the ONLY unsafe crate, this increment)
        ▲                       ▲
        └───────────┬───────────┘
            backdrop-blur-egui      egui adapters, feature-gated:
                                      feature own-loop (default) → OwnLoopRenderer (→ wgpu)   [v1]
                                      feature grab-pass          → GrabPassRenderer (→ glow)  [this increment]
```

- **core** owns every decision that does not name a GPU type: strength→radius, Gaussian-vs-Kawase selection and the level/threshold/half-pixel math, the UV remap, the rounded-rect mask, the color-encoding *policy*, the error/liveness vocabulary, and (after §15) the algorithm math v1 left in the wgpu crate plus GL-shaped `BlurStage` variants.
- **backdrop-blur-glow** owns *everything GL*: cached programs + scratch, the capability probe, a GLSL-ES-3.00 shader set + per-target version header, the grab, the blur passes, the composite, and the GL state save/restore. The **only** crate not `forbid(unsafe_code)`.
- **backdrop-blur-egui** gains the grab-pass adapter behind a `grab-pass` feature and stays `#![forbid(unsafe_code)]`. Making this crate feature-gated is a **breaking refactor**, not glue — priced in §7/M5.

**Coupling, named:** the egui adapter touches a backend only through the `BackdropBlur`/`GrabPass` traits + core vocabulary. Feature flags decide the concrete backend.

---

## 5. The seam: how glow plugs in (and the coordinate convention)

Unchanged from v1; this is the first real implementation of its glow side.

- **`BackdropBlur`** (per-backend cached resources). glow: `Device = Encoder = glow::Context` (immediate mode), `Queue = ()`, `SourceTexture = glow::Texture` (the grab), `Target = glow::Framebuffer` (egui's FBO), `TargetFormat = u32`, `Prepared = GlPrepared` (owned).
  - `prepare(ctx, (), &source, target_format, &request) -> Result<Option<GlPrepared>, BlurError>` — resolve the payload, pick/allocate scratch, lazily build+cache programs. `Ok(None)` when the clamped region is empty.
  - `record(ctx, &target_fbo, &prepared) -> Result<(), BlurError>` — run the blur passes + composite, **and leave GL state as found** (§11). Two clauses are load-bearing, not mere restore-list entries: **scissor is disabled for the whole record**, and the composite draws under a **full-framebuffer viewport** (§10).
- **`GrabPass: BackdropBlur`** (glow-only).
  - `grab_source(ctx, (), &framebuffer, region) -> Result<glow::Texture, BlurError>` — explicitly bind the live target as `GL_READ_FRAMEBUFFER` (never trust the incoming binding, which may be a stale scratch FBO), then `copyTexSubImage2D` the **clamped** region into a pre-sized grab texture (MSAA-resolve blit first if needed).

**The coordinate convention (load-bearing — got two reviews wrong before this).** On the glow path the callback derives a **GL bottom-left** `Region` from `info.viewport_in_pixels()` (its `from_bottom_px` field is already GL-origin) and builds the **entire** `BlurRequest` — `target_rect` *and* `source_region` — in bottom-left coords. `Surface::request`'s top-left rect is used only for **orientation-free** resolution (strength, repaint), never to build the composite geometry. Consequences:
- `grab_source` consumes the bottom-left region directly and performs **no internal flip** — a **deliberate divergence** from the v1 `GrabPass` seam doc (`seam.rs:118-119`, which placed the flip *inside* `grab_source`) and the glow-gate table; **both are updated in the same step** so the trait contract and the design agree. Rationale: `from_bottom_px` already yields GL coords at the call site, so a flip inside `grab_source` would be a *double* flip.
- `rect_origin`, `source_region`, the SDF, and `backdrop_uv_remap` then all operate in **one consistent bottom-left system** — the grab texture's v=0 row is the framebuffer's bottom row (a `copyTexSubImage2D` from a bottom-left FB), matching `rect_uv.y` increasing upward, so nothing is upside-down and a partially-clipped panel samples the right rows.
- **Type smell, flagged for IMPL:** `Region`'s core convention is top-left (`geometry.rs:43`); the grab path interprets it bottom-left. The IMPL should weigh a distinct GL-origin newtype to avoid one type silently carrying two conventions.

---

## 6. Data model — the domain types glow introduces

Reused **unchanged** from core: `BlurRequest`, `ResolvedMask`, `Region` (+ `clip_to`), `Scale`, `Tint`/`LinearRgba`, `BlurStrength`, `CornerRadius`, `BlurError`, `RepaintPolicy`, and (after §15) the algorithm-resolution functions + the extended `BlurStage`. `BlurError::GrabFailed` was reserved for this path.

New types, all in `backdrop-blur-glow`:

- **`GlowBlur`** — the cached, cross-frame GL resources: blur programs (Gaussian + Kawase down/up), the composite program, per-size scratch chains, the grab texture, the shared VAO. Implements `BackdropBlur` + `GrabPass`. Sole owner of GL object lifetimes; `Drop` deletes them (against a live context only — see context-loss below).
- **`GlProfile`** — the **typed** capability record, resolved once at construction (not a bag of bools): the shader-version class (`Es300` / `GlDesktop`), whether the context is embedded (drives `precision` qualifiers), the **renderable-float tier** as an enum (`Rgba16F` when `EXT_color_buffer_float`/core; else `Srgb8Rgba8` fallback — so the *scratch* format is a match, not a branch), and the default-FB sample count (the MSAA guard).
- **`GlScratch`** — one ping-pong slot: a `glow::Texture` (sampled) + its `glow::Framebuffer` (drawn). Chains keyed by core `PingPongKey { size, levels }`.
- **`GlPrepared`** — the owned per-call payload: resolved mask, tint, target rect (bottom-left), and the **typed** algorithm parameters `GlBlurPass::{ Gaussian { sigma, taps }, DualKawase { halfpixels: Vec<[f32; 2]> } }` (mirroring wgpu's `PreparedBlur` enum — *not* a bare `Vec<f32>`), plus scratch keys. Borrows nothing from `GlowBlur`.

**Scratch eviction (not Drop-only).** The cache is keyed by `(size, levels)`; a dragged/DPI-changing panel would otherwise accumulate one chain per distinct size and leak VRAM. `GlowBlur` evicts **last-frame-used**: each chain records the frame it was last touched; chains untouched for N frames are deleted. The eviction *decision* is a pure function unit-tested in Tier 0; resize and DPI change are triggers.

**Context-loss identity.** On web a `webglcontextlost`/`restored` cycle hands a *new* `glow::Context` while `GlowBlur` holds resources minted on the dead one — and `Drop` would delete against a dead context (UB). Resources are **stamped with a context identity**; on mismatch `GlowBlur` discards stale handles *without* GL deletes and rebuilds, and `frost` falls back to flat between lost and restored.

**The `Arc<Mutex<GlowBlur>>` is a bound-satisfier, not a concurrency model.** `egui_glow::CallbackFn` requires `Send + Sync + 'static` and `prepare` needs `&mut self`, so the shared handle is a `Mutex`. The seam is frame-serial single-threaded; the Mutex is never contended. It is documented as such; the callback carries a `debug_assert` it runs on the fixed paint thread (off-thread GL is UB). The poisoned-mutex path is handled (flat fallback + `log_err`), never silently skipped.

---

## 7. The egui grab-pass adapter (`GrabPassRenderer`)

A **structural sibling** of `OwnLoopRenderer`, not a copy.

- **`GrabPassRenderer::new(gl: &Arc<glow::Context>) -> Result<Self, BlurError>`** — probes `GlProfile`, builds `GlowBlur`, refuses too-old contexts (`UnsupportedContext`). Holds `GlowBlur` behind the `Arc<Mutex<…>>` bound-satisfier (§6).
- **`frost(&self, ui: &mut egui::Ui, surface: Surface)`** — resolves the orientation-free parts of `surface`; drives repaint (below); and enqueues an `egui::PaintCallback` wrapping an `egui_glow::CallbackFn` that captures an `Arc` clone of `GlowBlur` and the surface. The callback computes the GL-origin `Region`/`BlurRequest` from `info.viewport_in_pixels()` and runs the §2.3 sequence.

**The shared spine must be hoisted, and the own-loop-only parts named.** v1's `Surface`, `Surface::request`, `RepaintPolicy`, `composite_surfaces`, `SeamContext`, `strongest_repaint` live in the **wgpu-gated `own_loop` module**; a `grab_pass` module cannot reach them and they vanish when wgpu is absent. So:
- **Hoist** `Surface`, `Surface::request` (→ `pub(crate)`), and `RepaintPolicy` into a **feature-neutral** module compiling under either feature.
- **`composite_surfaces`, `SeamContext`, and `strongest_repaint` are own-loop-only** (the earlier "reused verbatim" framing was wrong). The grab-pass runs **one callback per surface** and relies on egui's viewport-wide repaint coalescing, so it never holds all surfaces at once. **Note:** `strongest_repaint` is `pub` and re-exported (`lib.rs:24`), so gating it behind `own-loop` is part of the **M5 breaking public-API change**, not a silent internal removal.

**Repaint policy — honest about egui's coalescing.** Per the project's research (egui issues 3109/3931), `request_repaint_after` honors only the *smallest* duration viewport-wide, any per-frame `request_repaint` overrides it, and it is a one-shot wake. So the adapter offers two clean policies — **`Live`** (re-`request_repaint` every frame → re-grab every frame) and **`Static`** (grab once; holds, with an explicit *stale-backdrop* caveat) — and accepts **`Bounded(d)`** as **best-effort**: `frost` re-issues `request_repaint_after(d)` every alive frame, effective cadence `max(d, viewport_min)`. The staleness mode of each is in the §12 table; `Bounded`'s limit is a §17 risk.

**Callback-ran detection (precise protocol).** egui_glow **drops** a callback whose payload is not the *exact* `egui_glow::CallbackFn` type the consumer's eframe was compiled against, emitting an **uncatchable egui-side `log::warn!`** (painter.rs:442) — the host gets no `Result`. So: `frost` clears an `Arc<AtomicBool>` *only when it actually enqueues* (`prepare` returned `Some`); the callback sets it on entry; the adapter warns **once** next frame iff `{enqueued ∧ prepare was Some ∧ flag still false}` — so a closed or clipped-to-nothing panel never false-positives. The ran-flag is the *sufficient* programmatic guard; pinning `egui_glow` to the consumer's eframe minor is best-effort (a library cannot enforce the consumer's dep tree), and two `egui_glow` copies in the tree is the residual mode the flag catches.

**Feature-gating — the exact, breaking edits (M5).** `backdrop-blur-egui` today depends on `wgpu`/`egui-wgpu`/`backdrop-blur-wgpu` *unconditionally* and re-exports wgpu/egui-wgpu types **and `strongest_repaint`** at the crate root with no `cfg`. The refactor: mark those three deps `optional = true`; define `own-loop` (default) and `grab-pass` feature→dep maps; gate `mod own_loop` and **all** of those re-exports behind `#[cfg(feature = "own-loop")]` (a semver-relevant public-API-shape change); both glow consumers — web *and* kiosk — build `default-features = false, features = ["grab-pass"]`. **One crate vs a split `backdrop-blur-egui-glow`:** kept one feature-gated crate (STRUCTURE intent + small hoisted spine), re-confirmed *after* pricing the cfg-gated re-export surface above — that cost, not aesthetics, decides it (§17).

---

## 8. One glow shader source, three GL targets

The glow backend's blur/composite shaders are written **once in GLSL ES 3.00**; only the `#version` header and the ES-only precision lines change per target. (These are the glow crate's *own* GLSL files — separate from the wgpu crate's WGSL, which is untouched.)

We **mirror egui_glow's `ShaderVersion` *pattern*** — read `SHADING_LANGUAGE_VERSION`, branch on the major version — but emit our **own** header values (we do not reuse `ShaderVersion::get`'s output, which would hand back `#version 140` for desktop):

| Target | `#version` header | precision lines |
|---|---|---|
| Desktop GL 3.3-core | `#version 140` | omitted (desktop ignores them) |
| GLES 3.0 (kiosk) | `#version 300 es` | `precision highp float;` **and** `precision highp int;` |
| WebGL2 (wasm) | `#version 300 es` | `precision highp float;` **and** `precision highp int;` |

`#version 140` is what egui_glow itself emits on desktop (proven to compile in the same eframe-glow context our callback runs in) and supports everything the body needs: `in`/`out`, `texture()`, `textureLod()`, and `gl_VertexID` (GLSL 1.40). `highp float` is **mandatory** (not `mediump`) for the SDF coverage math — some weak kiosk GLES drivers default `mediump` and band the corner AA; `highp int` covers the `gl_VertexID` integer math in the oversized-triangle vertex shader on strict GLES3 drivers. The body is ES-3.00-shaped, so it needs no `varying`/`gl_FragColor` legacy branch. The version class is part of `GlProfile`; the header adapter is ~20 lines.

---

## 9. WebGL2 specifics (the web-only decisions)

Shader-and-readback feasibility is **probe-verified** (headless Chromium: WebGL2 + `EXT_color_buffer_float`, render-to-RGBA16F + `readPixels` exact, `copyTexImage2D` grab `NO_ERROR`). The eframe-on-glow-on-wasm *integration* is verified **by analogy** to native (same glow API) — Tier-2(B) (§14) is what actually proves it.

- **Two surfaces, two formats — do not conflate.** The **grab** texture is **always RGBA8** on *all* targets (egui renders 8-bit gamma regardless of format — egui#3168 — so RGBA8 is the precision floor, not a fallback). The **blur scratch** is **RGBA16F** when `GlProfile`'s renderable-float tier is `Rgba16F`, and degrades to an **sRGB-encoded RGBA8 scratch** (`Srgb8Rgba8` tier — decode→tap→re-encode each pass, perceptually-uniform 8-bit, logged) only when float is not renderable: web without `EXT_color_buffer_float`, or a float-incapable kiosk. The float-tier enum keys the scratch format; the grab format is fixed.
- **The composite outputs premultiplied alpha (web *requires* it).** A web canvas is `premultipliedAlpha: true` and **eframe owns the context attributes** — we cannot opt out. §10 shows why glow's premultiplied composite is provably halo-free and needs *no* change to the wgpu backend.
- **No automatic sRGB on the default framebuffer** — true on native too (§10): the composite always manually encodes.
- **VAOs are mandatory.** WebGL2/GL-core have no usable default VAO; even the attribute-less fullscreen-triangle draw needs a bound non-zero VAO. `GlowBlur` owns one; **VAO + activeTexture** join the save/restore list.
- **Feedback is a hard error** (`INVALID_OPERATION`), not UB — the same `source ≠ target` contract; copy-before-sample satisfies it.

---

## 10. Color + composite correctness

The spike convolves sRGB bytes directly (gamma-wrong, dark halos). The crate ports the wgpu backend's correct model — non-negotiable for production:

- **Linear light.** egui renders **gamma-encoded** values regardless of format (egui#3168), so the grab is sRGB-encoded 8-bit (RGBA8, §9). The first blur sample **decodes sRGB→linear**; all convolution is linear; the composite **re-encodes linear→sRGB on write**.
- **`encode_srgb = 1` ALWAYS on the default framebuffer (C2).** The default framebuffer the callback draws into is **not in sRGB-encode mode**: GL defaults `GL_FRAMEBUFFER_SRGB` to disabled and egui_glow never *enables* it on this path — on native it additionally *disables* it when `ARB_framebuffer_sRGB` is present (painter.rs:326-327); on web the disable is compile-gated off (painter.rs:177) and WebGL2 has no such enable. So the composite always manually encodes; the wgpu backend's format-allowlist (which keys on a swapchain `TextureFormat`) does **not** carry over — glow hard-wires `encode_srgb = 1` and never introspects the `u32` internal format. *Confirm-at-IMPL caveat:* egui_glow's `pp_fb_extent` offscreen path (painter.rs:131) *can* enable framebuffer sRGB, but that is the post-process target, not the default FB the grab-pass composites into; a Tier-1/Tier-2 GLES assertion guards against a double-encode if a kiosk driver surprises us.

**The composite coordinate model (C1 — corrected after verification).** The composite is a **full-screen triangle**, reusing the wgpu composite's SDF + `backdrop_uv_remap` math (ported to GLSL), with uniforms fed in **GL bottom-left coords** (§5) so the same logic runs without a per-fragment flip. Three load-bearing clauses:
- **Full-framebuffer viewport.** egui pre-sets the GL viewport to the *panel rect* before the callback (painter.rs:431). A full-screen NDC triangle under a panel-sized viewport would rasterize fragments **only inside** the panel — every fragment `coverage == 1`, and the **outer half of the analytic AA band is never generated**, silently killing straight-edge AA. So the composite **must set `glViewport(0,0, screen_size_px.x, screen_size_px.y)`** (overriding egui's panel viewport; the prior viewport is saved/restored), making `gl_FragCoord` carry true full-framebuffer window coords that match `rect_origin`.
- **Scissor disabled** for the whole record (§5/§11), so the AA band is not clipped to egui's per-primitive scissor box.
- **Clip in two steps.** `grab_region = viewport_in_pixels ∩ clip_rect_in_pixels` (two *distinct* `PaintCallbackInfo` rects — the `clip_rect` intersection is what stops a `ScrollArea`-scrolled panel from bleeding past its scroll clip), then `.clip_to(screen_size_px)` for the framebuffer bound (this is what stops `copyTexSubImage2D` reading past the framebuffer edge → no dark fringe). `backdrop_uv_remap` maps the panel rect onto the clamped grab exactly as wgpu maps onto a clipped source. Empty intersection → flat fallback.

**Premultiplied is glow-side only — wgpu is genuinely untouched (M1, simplified after verification).** glow's composite is a **separate GLSL file** from wgpu's WGSL, so there is **no shared shader and no flag**: glow's GLSL composite outputs **premultiplied** (`out_rgb = encode(linear_color) · coverage`, alpha `= coverage`) paired with `glBlendFunc(ONE, ONE_MINUS_SRC_ALPHA)`; wgpu's WGSL composite stays **exactly as shipped** (straight alpha, its baked `over_blend` `SrcAlpha, 1−SrcA`). They produce **algebraically identical output**: both give `out = encode(linear_color)·coverage + dst·(1−coverage)` — a coverage-weighted lerp in *encoded* space between the (locally constant) edge color and the destination. Halo-free under three conditions, stated so an IMPL cannot over-claim:
1. **encode then multiply by coverage** (compute `rgb·coverage` in *linear* light then encode and you reintroduce the gamma/coverage halo — sRGB encode is concave, so `encode(C·cov) > encode(C)·cov`, an edge overshoot);
2. **output alpha also equals coverage** (so the blend's `(1−srcα)` term matches `(1−coverage)`);
3. the **edge color is locally constant** (a straight edge over a locally-flat backdrop — the *same* scope as the §2d oracle, local not global).

Because the output expression is identical, wgpu's frozen straight-alpha §2d snapshot remains a valid regression guard for the shared *algebra* — but the **premultiplied path itself runs only on glow/web**, so it gets its own assertion (§14). (The earlier "matches egui_glow's blend" claim is corrected: egui uses `blend_func_separate` with a distinct *alpha* func; ours matches on **color**, which is all that matters on the opaque default FB.)

---

## 11. The `unsafe` boundary — for Abdu's explicit approval

The first `unsafe` crate in the project, isolated hard:

- **All `unsafe` lives in `backdrop-blur-glow`.** core, wgpu, and the egui adapter stay `#![forbid(unsafe_code)]`. The glow crate carries `#![deny(unsafe_op_in_unsafe_fn)]`; **every block gets a `// SAFETY:`**.
- **The precondition is uniform and met by construction:** every GL call needs a *current* `glow::Context`. Inside an egui_glow paint callback it is current; the test harness makes it current via `eglMakeCurrent`. No `unsafe` for borrow/lifetime workarounds — only the irreducible GL FFI.
- **The state save/restore list** (`record`/`grab_source` leave state as found): **bound draw FBO, bound read FBO, viewport (set to full framebuffer for the composite, then restored — §10), `SCISSOR_TEST` enable + box (disabled during record, restored), blend enable + func/equation, active texture unit + bound textures on used units, and VAO.** egui_glow re-establishes its *own* program/VAO/blend/scissor/viewport via `prepare_painting` after each callback, so the one truly load-bearing restore is the **framebuffer binding** — but the crate restores the full list rather than depending on egui's re-init. `copyTexSubImage2D` is independent of `PIXEL_STORE`.
- **MSAA guard.** A startup `GL_SAMPLES` query records the default-FB sample count in `GlProfile`. If multisampled, the grab `glBlitFramebuffer`-resolves into a single-sample FBO first; if it cannot, the frame **falls back to flat** rather than corrupting.

**No `unsafe` is written until Abdu signs off this design and the IMPL doc.**

---

## 12. Error paths as scenarios

| Scenario | Handling |
|---|---|
| `cc.gl == None` (consumer on eframe's default wgpu renderer) | `GrabPassRenderer` not constructible; consumer draws flat. Documented precondition (§2). |
| Context too old (WebGL1 / not GLES3-capable) | `GrabPassRenderer::new` → `BlurError::UnsupportedContext`; flat fallback. |
| `EXT_color_buffer_float` absent (web) | Degrade the *scratch* to sRGB-RGBA8 + logged warning (§9). Not an error. |
| Default framebuffer multisampled | Blit-resolve; if impossible, flat fallback for that frame. |
| Region clamps to empty (offscreen / scrolled out / zero-area) | `prepare` → `Ok(None)` → no-op; egui untouched; no false ran-flag warning. |
| Shader compile / link / FBO-incomplete | `BlurError::ResourceCreation { stage }` with a **GL-shaped** `BlurStage` (§15); flat fallback; **never panic**. |
| Callback never ran (version skew) | Precise ran-flag protocol (§7) → warn once. |
| Web context lost (`webglcontextlost`) | Context-identity mismatch → discard stale handles without GL deletes, rebuild on restore, flat in between (§6). |
| Poisoned shared-state mutex | `log_err` + flat fallback. |

Every `Err` has a handler ending in a flat fallback. The promise to the consumer: **frost or flat, never crash.**

---

## 13. Contracts as promises

- **State-as-found:** after `record`/`grab_source`, GL state is exactly what egui left.
- **No feedback:** the live framebuffer is never simultaneously sampled and drawn.
- **Clipped honestly:** grab and composite stay within `viewport ∩ clip_rect ∩ framebuffer`.
- **Linear light, always-encode:** convolution is gamma-correct; the composite manually re-encodes (`encode_srgb = 1`) on native and web.
- **One glow shader source:** identical GLSL ES 3.00 on GL 3.3 / GLES 3.0 / WebGL2 via header adaptation.
- **wgpu untouched:** glow's premultiplied composite is a separate GLSL file producing pixels algebraically identical to wgpu's frozen straight-alpha WGSL; no wgpu edit, no shared flag.
- **Total failure model:** every failure is a `Result` ending in flat fallback; no panic, no silent swallow.
- **No leaks:** scratch is evicted last-frame-used; `Drop` deletes the rest (against a live context only).

---

## 14. Verification loop (defined before any code)

The portable, **must-pass-everywhere** guard is Tier 0. The GPU tiers are **compile-gated** and must pass **on the CI runner that enables them**; they are *absent* (not runtime-skipped) elsewhere — matching the existing `image-snapshots` precedent (which `.expect()`s a renderer and is simply not compiled on runners without one). No novel runtime-skip mechanism.

- **Tier 0 — GPU-free, any machine (`cargo test -p backdrop-blur-core`).** The hoisted algorithm math (exact tap offsets/weights, level/threshold/half-pixel, UV remap, mask coverage), the eviction decision, `GlProfile` resolution, the color-encoding policy, and every error variant. Catches the recurring bugs (wrong weights/flip/color space). Must pass on every runner.
- **Tier 1 — native GL pixel gate (compile-gated `gl-snapshots`).** *Proven feasible here:* raw `glow` over **EGL-surfaceless** brings up a hardware GL 3.3 context headless and `glReadPixels` round-trips (shader-draw confirmed). Assert **channel-delta properties** (the existing wgpu pattern: `r>30 ∧ b>30`, energy band, transition-width band, the no-halo envelope) — **not** committed-PNG equality (three rasterizers cannot agree byte-for-byte). **Required new assertions:** a **straight-edge-AA** check (transition-width band on a *non-corner* edge — catches a regression to a panel-clipped viewport, §10) and a **bottom-framebuffer-edge partial-clip** check (the edge a y-orientation bug exposes, which a top-edge test would miss). The enabling CI job guarantees the context. `--test-threads=1`.
- **Tier 2 — web pixel gate (compile-gated CI job).** *Proven feasible:* headless Chromium runs real ANGLE WebGL2 + `EXT_color_buffer_float`; render-to-RGBA16F + `readPixels` exact. **(A)** a static WebGL2 page running the *exact* GLSL ES 3.00 strings, asserting the same channel-delta properties; **(B)** `wasm-pack test --headless --chrome` running the *real* glow backend (proves the Rust path: header adapter, VAO/state, capability branch, context-loss). Tolerance + a pinned renderer (`--use-angle=swiftshader`) so GPU-less CI agrees; SwiftShader doubles as a **GLES3-correctness proxy** (catches `highp→mediump` banding and the float fallback).
- **The premultiplied no-halo assertion is load-bearing and its content is pinned.** The §2d oracle runs only on wgpu/native with straight alpha, so it **never exercises the premultiplied path** — the glow/web tiers' assertion is the *sole* guard against the "linear-multiply-then-encode" ordering bug (§10). It is the **same `assert_no_edge_halo` envelope** (every edge-band pixel within `[min(edge,dst), max(edge,dst)] + TOL`), run on a **premultiplied glow/web readback**, in **both** directions (bright-over-dark for overshoot, dark-over-bright for undershoot) — not a weak "edge looks blurry" check.
- **One standard, by property not by bytes.** Native and web run the *same* property assertions on each surface; cross-surface agreement is "both satisfy the same numeric properties," not "both match the same committed bytes." Any committed reference image is perceptual/eyeballed and out of the must-pass numeric path.
- **Human gate.** A `preview` eframe example (the spike generalized to the real backend) for perceptual checks (halo, corners, animation, a frosted card in a `ScrollArea`); the headless PNG gallery extends to glow.

---

## 15. Prerequisite refactor (IMPL step 0): hoist into core, keep wgpu green

v1 left the **GPU-free** Kawase/Gaussian math inside `backdrop-blur-wgpu/src/cache.rs` (`use_dual_kawase`, `KAWASE_THRESHOLD_PX`, `resolve_kawase_levels` + the log2 quantization, `kawase_level_size`, `kawase_halfpixel`, `resolve_gaussian`/`GaussianKernel`, `backdrop_uv_remap`, `PingPongKey`). Glow needs it identically; duplicating it is a real divergence hazard.

**Step 0, before any glow code:**
- **Move that math into `backdrop-blur-core`.** Keep only the two **format-coupled** items (`SCRATCH_FORMAT`, `composite_encode_srgb`) in each backend, expressing the *encoding policy* as a core enum (`TargetEncoding`) each backend maps its own format onto.
- **Extend `BlurStage` in core with GL-shaped variants** (`ShaderCompile`, `ProgramLink`, `Framebuffer`, `VertexArray`) alongside the existing wgpu ones, so glow init failures map honestly (a link failure must not log as `DownsamplePipeline`).
- **`git mv` the `cache.rs` unit tests verbatim into core** so the green run *is* the proof wgpu's behavior is unchanged; annotate the **`BlurStrength` doc block (`material.rs:7-21`, the "no notion of levels" sentence at `:12`)** in the same commit to record the deliberate reversal — justified because a *second backend now exists*.

wgpu is updated to consume from core and stays green at every step.

---

## 16. The `abdu-egui-ui` frosted-Glass increment (specified now, built next)

**Correction (C3):** there is **no GL blur wired into any `abdu-egui-ui` component today.** `src/widgets/overlay/glass.rs` is a flat pass-through pane; `tooltip_blur_spike.rs` is a standalone `preview`-gated example no component consumes; `glass/DESIGN.md` scopes v1 as flat/safe-only with the real blur deferred. So the next increment is **the first real frosted-Glass component**, not a wiring swap — and it gets **its own design→IMPL→review gate**.

What that increment covers (new design, not this doc):
- A frosted-Glass component mapping its `(variant, tone, locale)` tokens onto a `Surface { rect, strength, tint, corner_radius, repaint }` — the **only** coupling to this crate. RTL/mirroring stays at the Glass surface layer (the blur is geometry-symmetric).
- Renderer lifecycle (one `GrabPassRenderer` from `cc.gl` at startup), the **host obligation** (enqueue `frost` *before* the panel's own fill), the fallback-to-flat UX when `cc.gl` is `None` or the context is unsupported, and the repaint/staleness policy.
- **Deleting `tooltip_blur_spike.rs`** as a superseded example.

The genuinely wiring-only part is narrow — `cc.gl → GrabPassRenderer::new`, `frost(ui, surface)` at the paint site. Everything else is component design. The `Surface` API is the contract that keeps the app from writing GL.

---

## 17. Risks and open questions

- **Kiosk-GPU performance is UNVERIFIED.** All budgets are desktop/2015-mobile; per-frame cost on the weak kiosk GPU is unknown and only on-device measurement settles it. Mitigation: dual-Kawase's near-constant cost + downsampled scratch. Can force a perf-tuning follow-on. **Named, not hidden.**
- **Kiosk GLES3 *correctness*.** `EXT_color_buffer_float` absence (→ RGBA8 scratch), 6-level pyramid fill-rate, silent `highp→mediump`, and the `pp_fb_extent` sRGB caveat (§10) affect whether the *reference* reproduces on embedded GLES3. SwiftShader is a GLES3 proxy in Tier 2; real-device confirmation is deferred with the perf benchmark.
- **SwiftShader pixel drift on GPU-less CI.** Handled by property (not byte) assertions + a pinned renderer.
- **`Bounded` repaint is best-effort** (egui issue 3109, §7). Live/Static are the honest first-class policies.
- **Coordinate-convention divergence (§5).** The bottom-left flip moves out of `grab_source` to the callback; the `GrabPass` seam doc + glow-gate table are updated to match, and `Region` carrying two conventions is flagged for a possible GL-origin newtype.
- **One-crate-vs-split (§7).** Decided one feature-gated crate, re-confirmed after pricing the cfg-gated re-export refactor (incl. the public `strongest_repaint`).
- **A few API facts are inferred, not source-quoted** (eframe's exact web context attributes; glow's `from_webgl2_context` use in 0.34). High-confidence; re-verified at implementation, and Tier-2(B) exercises the real path.

---

## 18. What this does NOT cover

- The `abdu-egui-ui` frosted-Glass component (next increment; §16; its own gate).
- WebGL1 / GL 2.1 / the legacy shader interface.
- Compositor-side blur (cage has none).
- An on-device performance benchmark and any perf tuning it mandates (§17).
- Multi-surface batching beyond one-callback-per-surface.

---

## Revision note (post-review)

**Round 1 — five-lens adversarial review (13 findings, all acted on):** C1 the composite/scissor/clip model; C2 `encode_srgb` always; C3 §16 is greenfield component design, not a wiring swap; M1 premultiplied; M2/M5 the egui-adapter feature-gating + spine hoist as explicit breaking refactor; M3 `Bounded` best-effort; M4 GL-shaped `BlurStage`; M6 the `cc.gl == None` case; M7 Tier 1 compile-gated; plus typed `GlBlurPass`/`GlProfile`, scratch eviction, web context-loss, the precision/read-FBO/ran-flag clusters.

**Round 2 — focused correctness verification of the fixes (17 findings, all acted on):** the real catch was that a full-screen composite triangle under egui's **panel-sized viewport** loses straight-edge AA — fixed by overriding to a **full-framebuffer viewport** (§10/§11). The clean simplification: glow's GLSL composite is a **separate file** from wgpu's WGSL, so glow emits premultiplied with **no wgpu edit and no shared flag** (§10/§13), and the premultiplied output is **algebraically identical** to the frozen straight-alpha path under encode-then-coverage. Also: the whole `BlurRequest` is built bottom-left on the glow path (not just `rect_origin`); `clip_rect ∩` is a step distinct from `clip_to`; §8 desktop header is `#version 140` (mirroring egui_glow's pattern, not reusing its output, not `330 core`); the `encode_srgb` justification reworded (GL defaults it off + egui never enables on this path, with the `pp_fb_extent` caveat); the premultiplied no-halo assertion content pinned in §14; `strongest_repaint` is a public re-export (part of the M5 break); "silently no-ops" → an uncatchable `log::warn!`; the `material.rs` cite corrected.
