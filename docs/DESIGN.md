# `backdrop-blur` — Design

> **What this is:** a standalone, toolkit-agnostic Rust crate that provides *real
> backdrop blur* (frosted glass / vibrancy) as a reusable GPU capability — the thing
> no Rust GUI toolkit ships today. It is **not** part of `abdu-egui-ui`; it is its own
> repo, and `abdu-egui-ui` would be merely one future consumer.
>
> **Grounding (committed):**
> - Feasibility & algorithm: `architecture/glass/NATIVE_BLUR_RESEARCH.md` (7-angle,
>   12-claim adversarial research). Real backdrop blur is the *grab → blur → composite*
>   pipeline; clean and reusable on wgpu **when you own the render loop** (Bevy bloom proof).
> - Packaging: `architecture/backdrop-blur/STRUCTURE.md` (5-angle, 8-precedent-verified).
>   A workspace of separate published crates, forced by Cargo's additive-feature rule +
>   distinct backend resource types — the egui / wgpu / iced layout.
>
> **Status:** design — revised after a 5-lens adversarial review (5 critical / 17 major
> findings, all in the §4 seam shape and doc contradictions; topology survived). Pending
> Abdu sign-off. The seam is cut at prepare/record joints here and is **not frozen** until
> an IMPL-doc glow sketch confirms the associated types fit the divergent backend (§13).
>
> **As-built update (post-0.1.0):** the glow grab-pass backend and the egui grab-pass path
> are **no longer deferred** — both shipped and are published on crates.io. References below
> that mark glow / grab-pass as "DEFERRED past v1" describe the original v1 *increment plan*,
> not the current state. The seam proved out against the divergent backend (see §4.4 / the
> `seam.rs` "Gate verdict"); `BlurRequest` also gained a sixth field, `opacity` (§4.3).

---

## 0. Scope decision (the v1 increment) and an honest reach statement

The **target** is the workspace (core, wgpu, glow, egui, facade). The **v1 increment** is the
disciplined minimal slice on the safe backend:

> v1 = `backdrop-blur-core` + `backdrop-blur-wgpu` + `backdrop-blur-egui` (own-loop/wgpu
> path) + `examples/egui-wgpu-panel`.

**Iced was evaluated and dropped from the plan** (§7): it is desktop-focused — off-axis from
this project's RTL-first/kiosk/cross-environment aim — and its render API cannot cleanly do
backdrop blur anyway (its `Primitive::render` is write-only; details in §7).

Consequences, stated plainly — including the ones the review forced into the open:

- **v1 is 100% safe Rust.** The egui adapter rides wgpu; `backdrop-blur-glow` (the only
  `unsafe` crate) and the egui *grab-pass* path are **deferred**. No `unsafe` approval is on
  the v1 critical path.
- **v1 exercises neither axis the seam exists for — both are forward-looking.** With Iced gone,
  v1 has exactly **one toolkit (egui) and one backend (wgpu)**, so *both* the backend-agnostic
  and the toolkit-agnostic claims of the seam are **asserted, not proven** in v1. Honestly, v1 is
  "a backdrop-blur capability for egui-own-loop, *structured* so other backends (glow) and
  toolkits are additive crates later." The seam is therefore an explicit
  **forward-compatibility contract**; the IMPL doc sketches the glow `impl` against it *before*
  core is frozen (§13) so the abstraction is validated on paper, not merely declared. (If that
  sketch shows the seam doesn't fit glow, the honest move is to ship v1 as a concrete egui-wgpu
  crate and lift the trait only when the second backend actually lands — see §13.)
- **v1 does not serve the sponsoring use case.** `abdu-egui-ui`'s Glass is an **eframe/glow**
  app, so it consumes *nothing* from v1 — v1 is ecosystem groundwork, not kiosk frosted glass.
  Kiosk frosted glass needs the deferred glow/grab-pass path.
- **The v1 risks are now purely *correctness*, not toolkit feasibility** (the Iced GO/NO-GO is
  gone). Two PARTIAL-in-research risks sit on the egui-wgpu path: the premultiplied-linear
  **edge halo** (§8/§11 probe) and the **kiosk-GPU blur cost** (a benchmark). Both are settled
  by verification (§11), not by a spike.
- **Reach caveat:** the v1 egui path is the *own-loop* path (apps driving `egui-winit` +
  `egui-wgpu` directly), **not** mainstream `eframe`-on-glow apps. Broad eframe reach arrives
  with `backdrop-blur-glow` + the grab-pass path, sequenced after the seam is proven safe.

---

## 1. Why this exists

A **frosted-glass surface** — a panel whose background is the blurred, tinted copy of the
content behind it (macOS vibrancy / Windows Acrylic / CSS `backdrop-filter`) — is a first-class
capability that **no Rust GUI toolkit provides** (confirmed: GPUI/`window-vibrancy` do
*whole-window OS* vibrancy, Vello does *shape* blur; neither is in-app arbitrary-surface backdrop
blur). The reason is structural: real-time backdrop blur needs render-to-texture + a multi-tap
convolution that no 2D UI renderer exposes as a property.

This crate fills that gap as a **reusable capability**: the algorithm and its GPU backends live
in one place; each toolkit gets a thin adapter. The research validates the bet — the blur itself
is small and solved (≈20 lines of dual-Kawase); the value is in getting the backend-agnostic seam
right and owning the per-toolkit integration glue that every app currently re-derives.

## 2. Boundaries — what it is and is not

**Is:**
- One material — *frosted glass* (backdrop blur + tint + rounded-rect mask) — as a reusable GPU
  pass with a backend-agnostic seam, plus thin per-toolkit adapters.
- wgpu and glow backends (glow deferred past v1). Dual-Kawase blur, linear-space convolution.
- A capability for **surfaces/overlays** (tooltip, dialog, drawer, popover) — a handful visible
  at once, each paying one grab + blur.

**Is not:**
- **Not** a general effects/filter-graph framework. One effect, parameterised.
- **Not** OS/compositor vibrancy (`window-vibrancy`, KDE blur protocol, DWM Acrylic) — whole-window,
  the compositor's job; orthogonal.
- **Not** a renderer. It composites *into* a target the host owns; it never owns the frame.
- **Not** per-cell bulk decoration; dozens of frosted cells stack the cost. Named, unsupported.
- **Not** a promise for closed-renderer toolkits (GPUI, Slint, Makepad) — no render hook; out
  until upstream adds one.
- **Not, in v1, overlapping/stacked frosted surfaces.** v1 supports a **single frosted surface
  over a once-rendered backdrop, with non-overlapping regions** (§5/S1). Ordered multi-surface
  glass — where panel 2 must blur panel 1's *already-composited* result — needs the seam to
  thread "composited-so-far" and is **explicitly deferred** (the design space is named in §9, the
  v1 contract is single-surface).

## 3. The one route this takes

Two routes exist (research): in-app GPU blur vs OS/compositor vibrancy. This crate is **in-app GPU
blur**, **own-the-loop** model: the host renders its UI into an offscreen intermediate texture; the
crate blurs a region of that texture and composites the frosted surface into a target; only the
host's final pass writes the swapchain (never sampled). This is the Bevy-bloom pattern, the only
model both clean and reusable on wgpu. The *grab-pass* model (grabbing the live framebuffer
in-place) is the glow/mainstream-egui path, deferred with `backdrop-blur-glow` — but its type
socket is reserved in the seam now (§4, `grab_source`) so adding it later is not a core rewrite.

## 4. Domain concepts and types (the load-bearing section)

All vocabulary is backend-agnostic and lives in `backdrop-blur-core` — pure, headless,
`#![forbid(unsafe_code)]`.

### 4.1 Coordinate convention (pinned — not an open question)

**The seam speaks physical pixels.** `BlurRequest` carries physical-pixel `Region`s; `Scale` exists
*only* so the backend resolves logical `BlurStrength`/`CornerRadius` to pixels; **the seam never sees
a logical rect.** `source_region` and `target_rect` carry **independent** scales/sizes, because the
own-loop intermediate (e.g. `Rgba16Float`, possibly at a different size) and the swapchain can differ
in DPI — so `Scale` is per-`Region`, not one global factor. This is a frozen contract, removed from
the old §14 open list.

### 4.2 The material — "what kind of glass"

- `BlurStrength(f32)` — logical points of blur radius. The backend resolves it (× the region's
  `Scale`) to a dual-Kawase iteration count + per-pass sampling offset; **there is no closed-form
  sigma↔iterations map** (research), so the backend interpolates offsets — a named IMPL item, but
  the *type* is settled.
- `Tint(LinearRgba)` — the glass film; alpha is film opacity. The **blur convolution runs in linear
  light** — but that is a *job the backend must do*, not a free property: egui-wgpu renders in **gamma
  space** and writes sRGB-encoded values into its target *regardless of the format* (egui#3168), so the
  own-loop intermediate the backend samples holds **gamma-encoded** pixels. The backend therefore
  **sRGB→linear-decodes on sample, convolves in linear, re-encodes at composite** (or samples through an
  `*_Srgb` view so the sampler linearizes). Convolving the intermediate's bytes directly *is* the
  gamma-naive halo — so "linear" is a named decode step (IMPL §2b/M7), not an assumption. The
  **premultiplied-vs-straight edge convention at the translucent rounded-rect boundary is separately
  PARTIAL** and *not* frozen here; the §11 snapshot probe settles it (§8), on top of a correctly
  linearized base.
- `CornerRadius(f32)` — logical points; resolves to a physical-px radius **clamped to
  `min(region.width, region.height) / 2`** (no radius-overshoot artifacts).

### 4.3 Geometry and the request

- `Region { origin: [u32; 2], size: [u32; 2], scale: Scale }` — a physical-pixel rectangle, carrying
  its own logical→physical `Scale`.
- `ResolvedMask { half_extents: [f32; 2], corner_radius_px: f32 }` — what **core computes** for the
  shader: the clamped physical corner radius + rect half-extents. The WGSL/GLSL shader only evaluates
  the standard rounded-rect SDF from these; this split makes §11's headless test concrete (test the
  clamp + logical→physical resolution in core, no GPU; the per-pixel SDF stays in the shader).
- `BlurRequest` — the one backend-agnostic bundle crossing the seam:
  ```rust
  pub struct BlurRequest {
      pub source_region: Region,   // where the backdrop lives in `source` (physical px + scale)
      pub target_rect:   Region,   // where to composite the frosted surface in `target`
      pub strength:      BlurStrength,
      pub tint:          Tint,
      pub corner_radius: CornerRadius,
      pub opacity:       Opacity,    // surface-global fade [0,1]; default 1.0 (added post-design)
  }
  ```

### 4.4 The seam — a two-phase trait (M1/M2/M3/S6)

One trait, implemented once per backend. It is split into **`prepare` (uploads + allocation, holds
device + queue) and `record` (command recording, holds only the encoder + target)** because the GPU
backends demand it: **wgpu** uploads uniforms/textures through the **Queue** (not the encoder), so the
upload phase needs the queue and the record phase needs the encoder; **glow** is immediate-mode
(`prepare` = grab + upload via the context, `record` = draws). The split keeps each phase honest about
the resources it actually holds, and leaves the door open for any future host whose render lifecycle is
itself two-phase. The grab-pass producer lives in a **separate `GrabPass` trait** (below) that only
grab-pass backends implement, so the own-loop wgpu backend never has to stub a `grab_source` it cannot
perform — the seam stays total (added after the 1d review; replaces an impossible "wgpu default impl").
The associated types are the backend's resource universe — distinct per backend, which is exactly why the
traits are **not object-safe** and backends are **separate crates** (the `wgpu-types` → `wgpu-hal`
model). Static dispatch, monomorphised.

```rust
/// Implemented by each GPU backend (WgpuBlur now; GlowBlur later). Holds the backend's
/// cached resources (per-(size,format,levels) ping-pong chains + pipelines) across frames.
/// v1 status: a forward-compatibility contract exercised by one backend (wgpu); the glow
/// `impl` is sketched in the IMPL doc before core is frozen, to validate the abstraction.
pub trait BackdropBlur {
    type Device;        // wgpu::Device         | glow::Context
    type Queue;         // wgpu::Queue          | ()            (glow uploads via Device in prepare)
    type Encoder;       // wgpu::CommandEncoder | glow::Context (the immediate-mode draw handle)
    type SourceTexture; // wgpu::TextureView    | glow::Texture     (sampleable backdrop)
    type Target;        // wgpu::TextureView    | glow framebuffer  (composite destination)
    type TargetFormat;  // wgpu::TextureFormat  | FramebufferSize (glow, as-built: the composite
                        //   viewport SIZE, not a color format — glow never introspects the target's
                        //   internal format; see the seam.rs "Gate verdict". The slot's real role is
                        //   "what prepare needs to know about the target", which is backend-specific.
    type Prepared;      // opaque, OWNED per-call handle (no borrow of self) carrying the resolved
                        // payload (offsets, tint, mask, rect, the resource keys) from prepare -> record

    /// Phase 1 — has device + queue. Allocates/keys the ping-pong chain, lazily builds & caches
    /// pipelines (the fixed-scratch down/up pipelines once; the COMPOSITE pipeline per `target_format`,
    /// since wgpu bakes the fragment-target format into the pipeline at creation — M3/M8), resolves the
    /// payload (offsets, tint, mask, rect) into `Prepared`. Returns an OWNED handle (no borrow of self)
    /// so `record` need not immediately follow.
    /// **No-op:** a zero-sized/offscreen `request.source_region` returns `Ok(None)` — valid input, not
    /// an error (reconciles §4.5); `record` is then simply not called. (Hence `Option`.)
    fn prepare(
        &mut self,
        device: &Self::Device,
        queue: &Self::Queue,
        source: &Self::SourceTexture,
        target_format: Self::TargetFormat,
        request: &BlurRequest,
    ) -> Result<Option<Self::Prepared>, BlurError>;

    /// Phase 2 — has only the encoder + target. Records down → up → composite for a prior
    /// `prepare` in the same frame, **consuming the handle** (as-built: recording the same
    /// surface twice is a use-after-move compile error, not a prose rule). `source != target`
    /// is a contract (S2); wgpu forbids sampling the texture it writes. The immediate-mode
    /// backend restores GL state it touched (bound FBO, viewport, blend func, texture units)
    /// so it leaves state as found (§10).
    fn record(
        &self,
        encoder: &mut Self::Encoder,
        target: &Self::Target,
        prepared: Self::Prepared,
    ) -> Result<(), BlurError>;
}

/// The grab-pass socket — implemented *in addition to* `BackdropBlur` only by backends that must
/// extract a sampleable source from a live framebuffer (glow; the deferred mainstream-egui path).
/// Own-loop backends (wgpu) do NOT implement it — they receive an already-sampleable source — so
/// `BackdropBlur` stays total and wgpu never stubs a method it cannot perform. This is the socket
/// that keeps glow additive (S6); making it a separate trait (not a method on every backend) was
/// the 1d-review fix for the impossible "wgpu default impl hands it through".
pub trait GrabPass: BackdropBlur {
    type Framebuffer;   // glow framebuffer (the grab READ source); wgpu never implements GrabPass

    /// Blit + MSAA-resolve the `region` out of the live `framebuffer` into a sampleable
    /// SourceTexture. **As-built divergence (supersedes this doc's original K5):** `region` is a
    /// `GlRegion` — already in GL bottom-left coordinates, constructed via `from_bottom_px` — so
    /// `grab_source` performs NO read-origin flip. The v1 sketch placed the flip inside this
    /// method; the adapter now builds the whole request bottom-left, so a flip here would be a
    /// *double* flip. The y-orientation is carried by the type, not by an arithmetic step
    /// (see `core/src/seam.rs` + `gl_region.rs` — the shipped source is ground truth here).
    fn grab_source(
        &mut self,
        device: &Self::Device,
        queue: &Self::Queue,
        framebuffer: &Self::Framebuffer,
        region: GlRegion,
    ) -> Result<Self::SourceTexture, BlurError>;
}
```

- **v1 contract is serial `prepare` → `record` per surface** (single-surface scope, §2). The handle is
  **owned and consumed by `record`** (as-built), so double-record is unrepresentable — no prose rule
  needed. What remains prose: because the ping-pong scratch is shared, **two surfaces are not
  prepared-then-both-recorded** in v1 — each is prepared and recorded before the next; the backends
  guard the stale-handle case with a generation `debug_assert`. Genuine multi-surface batching
  (overlapping glass) is deferred (§9) and would need a per-call scratch discriminator, not just the
  owned handle (K1).
- **Cache key is a newtype, not size alone (S5):** `PingPongKey { size, levels }` keys the fixed-format
  (`Rgba16Float`) scratch chain (`levels` = dual-Kawase mip depth, a function of `BlurStrength × Scale`).
  The **composite pipeline is keyed separately by `TargetFormat`** (M8) — the down/up scratch is always
  the internal format, only the final composite matches the caller's target. `BlurError::UnsupportedTarget`
  is an explicit allowlist check, distinct from wgpu's must-match-format validation (M8).
- **Threading (C1):** the blurrer is single-threaded, frame-serial (`&mut self` in prepare, `&self`
  in record), **not** required to be `Send`/`Sync` in v1; a multi-threaded render loop owns one per
  render thread. A shared blurrer is deliberate future work (YAGNI).

### 4.5 The error type — the return half of the contract (M6)

`BlurError` is a `thiserror` enum living in **dependency-free core**, so it **cannot name a backend
error type** (`wgpu::*`/`glow::*` live in the GPU crates core forbids). It therefore carries a **boxed
trait-object source** — still a typed `Error` value that composes with `?`/`#[source]`, *not* a
flattened `String` model. Each `Display` is a complete sentence with recovery context. **A
zero-sized/offscreen region is a no-op (`prepare` returns `Ok(None)`), not an error** (valid input) — so
there is *no* `ZeroSizedRegion` variant.

```rust
type BackendError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum BlurError {
    #[error("failed to create the {stage} while preparing the blur")]
    ResourceCreation { stage: BlurStage, #[source] source: BackendError },
    #[error("target format {format} is not a supported render target for the blur composite")]
    UnsupportedTarget { format: String }, // deliberate String exception: core cannot name wgpu::TextureFormat,
                                          // so the backend captures format!("{fmt:?}") at the boundary.
    #[error("the grab source could not be produced from the framebuffer for region {region}")]
    GrabFailed { region: GlRegion, #[source] source: BackendError },   // GlRegion (bottom-left), as-built
    #[error("the GL context does not support the blur backend: {detail}")]
    UnsupportedContext { detail: String },  // as-built addition (glow construction-time gate);
                                            // same documented String exception as UnsupportedTarget
    // … (one variant per real GPU-fault scenario in §9; no no-op-as-error variants)
}

#[derive(Debug, Clone, Copy)]
pub enum BlurStage { PingPongTexture, DownsamplePipeline, UpsamplePipeline, CompositePipeline, UniformBuffer, BindGroup,
                     ShaderCompile, ProgramLink, Framebuffer, VertexArray /* GL stages, as-built */ }
```

`ResourceCreation.stage` localises a 3 AM kiosk failure to the exact resource that died.

### 4.6 Liveness — a typed adapter obligation, not a footnote (S3)

Backdrop freshness is a *correctness* property, not a perf knob, and it does **not** generalise across
toolkits (research §7: egui is reactive-by-default — it does *not* repaint when behind-surface content
changes; there is no region invalidation; "fresh as long as the host repaints" can be *zero* frames).
So the adapter API carries a domain type:

```rust
pub enum RepaintPolicy { Static, Live, Bounded(Duration) }
```

`Static` (default) for dialogs/tooltips over still content; `Live` (idle-power cost named) **required**
for glass over animating content; `Bounded` for periodic. The adapter — not core — drives the host's
`request_repaint`; §6 shows the decision point and the "repaint-continuously-then-settle-static"
sequence the research prescribes.

> **As-built honesty (two caveats):** (1) the policy gates the outgoing *repaint request* only — the
> blur work itself (grab/blur/composite) runs on **every** `render_frame`/paint call regardless of
> policy, so `Bounded` saves power only when the host is otherwise idle, and a host that repaints for
> unrelated reasons pays full blur cost per frame. (2) On the **own-loop** path the adapter calls
> `ctx.request_repaint()` *outside* an egui pass; for a manual winit host that signal is only
> observable via egui's repaint plumbing (`set_request_repaint_callback` or the next `FullOutput`'s
> `viewport_output` delay) — `render_frame` neither returns the decision nor installs the callback,
> so the liveness contract currently depends on host wiring the doc does not specify. Open design
> question, not yet resolved.

## 5. Workspace topology (summary; full rationale in `STRUCTURE.md`)

```
backdrop-blur/                 (new dedicated repo; virtual-manifest workspace; edition 2024, MSRV 1.92)
├── backdrop-blur-core   seam trait + material/geometry/error/liveness vocabulary — #![forbid(unsafe)]
├── backdrop-blur-wgpu   wgpu impl of the seam (WGSL)               — safe          [v1]
├── backdrop-blur-glow   glow impl (GLSL/GLES) — the ONLY unsafe crate   [DEFERRED past v1]
├── backdrop-blur-egui   egui adapter: own-loop(→wgpu) [v1] + grab-pass(→glow) [deferred]
└── backdrop-blur        optional thin facade (additive re-exports)  [deferred until ≥2 subcrates]
```

egui is the **only planned toolkit adapter**; Iced is dropped (§7). Any future toolkit that exposes a
sampleable-backdrop hook is an *additive* adapter crate against the stable seam — that slot is what the
separate-adapter-crate pattern keeps open, not a present obligation. Backends are separate crates
(distinct public resource types + Cargo's additive-feature rule); adapters are separate crates
(egui/tracing precedent — a future non-egui user never compiles egui's tree); core is a types/seam crate
every other crate depends on; examples are their own crates under `examples/` (keeps eframe/winit out of
every published library's dep graph).

## 6. Data flow — the egui own-loop adapter (`backdrop-blur-egui`, wgpu)

The host drives `egui-winit` + `egui-wgpu` directly (not eframe). The adapter's per-frame order is a
**contract** (S2/S4), because each step is panic-prone if reordered:

1. `Renderer::update_texture` for all texture deltas.
2. `Renderer::update_buffers` **first**, then **submit the `Vec<CommandBuffer>` it returns** (egui-wgpu
   does *not* auto-submit). Skipping/reordering this panics.
3. Begin a render pass whose color attachment is the **offscreen intermediate** (caller-chosen — the
   CONFIRMED `egui-wgpu::Renderer::render(render_pass, …)` seam, no fork), then call `render` (needs
   `RenderPass::forget_lifetime` for the `'static` bound). This renders the UI **up to but not
   including the frosted surface's foreground**.
4. For the frosted surface: `prepare` (samples the intermediate as `source`) then `record` into the
   **swapchain or a second target** — **never the intermediate it samples** (`source != target`; wgpu
   forbids read==write). The backend owns the internal blur ping-pong scratch; the host owns only the
   final target.
5. The surface's own border/content is painted **after** the composite, so the blur samples only the
   backdrop, not foreground drawn over it (the Backdrop-Root rule — S2).
6. The adapter decides `request_repaint` per the surface's `RepaintPolicy` (§4.6) and presents.

This keeps the crate owning **only the background**; content + a11y stay the host's.

> **v1 as-built (steps 3 & 5):** the adapter does **not** split the egui frame into backdrop and
> foreground passes — it renders the *same* tessellated frame into both the intermediate and the
> target (no second post-composite pass). The Backdrop-Root rule is therefore a **host obligation**,
> documented on `FrameInput::paint_jobs`: the host must not paint a frosted surface's own
> background/fill into the jobs, or the blur (source_region == target_rect) samples the panel's own
> fill instead of the content behind it. The surface's foreground is painted by the host in its own
> later pass. Also: `render_frame` itself drives `request_repaint` from the surfaces' `RepaintPolicy`
> (per §4.6), taking the host's `&egui::Context`; and `OwnLoopRenderer::new` rejects non-Unorm
> (sRGB/HDR) targets, since the adapter pins the decode-in-shader gamma model.

## 7. Iced — evaluated and dropped (the recorded decision)

Iced was a candidate second toolkit; it is **dropped from the plan** for two independent reasons:

1. **Off-axis from this project.** Iced is desktop-focused. This work's north star is RTL-first,
   kiosk, and cross-environment (web/Wayland/embedded); a desktop-centric toolkit is not where the
   reach matters here.
2. **Its render API cannot cleanly do backdrop blur anyway.** The verified
   `Primitive::render(&self, &Pipeline, &mut CommandEncoder, target: &TextureView, clip_bounds)` hands
   a custom primitive the **write** target (the same surface texture the UI composites onto) and
   **nothing else** — no device, no queue at render time, and **no sampleable view of the scene behind
   it**; wgpu forbids sampling the texture you are writing. The bare primitive path therefore *cannot*
   sample a backdrop. The only escape — having the Iced *application* render its scene to an offscreen
   intermediate — requires restructuring the app's render topology, and unlike egui
   (`egui-wgpu::Renderer::render` takes a caller-chosen attachment — CONFIRMED) **no analogous verified
   `iced_wgpu` hook exists**. So even that path was unproven.

The seam loses nothing by this: it never depended on Iced specifics, and the two-phase prepare/record
shape stands on the wgpu Queue/Encoder split and glow's immediate mode alone (§4.4). A future toolkit
that *does* expose a sampleable-backdrop hook drops in as an additive adapter crate.

## 8. Algorithm (summary; full treatment in `NATIVE_BLUR_RESEARCH.md`)

- **Dual-Kawase (dual-filter) down/up-sample** — 5-tap downsample, 8-tap upsample, the
  production-compositor standard (KWin, picom). ~2.8 ms @1080p on a 2015 tiler vs 23–42 ms naive
  Gaussian. A linear-sampled separable Gaussian is the fallback for a single small fixed radius.
- **Linear-space convolution** (CONFIRMED). The **edge alpha convention** (premultiplied vs straight at
  the translucent rounded-rect boundary) is **FROZEN to straight alpha for the wgpu composite**
  (2026-06-05, IMPL §2d). **As-built divergence:** the glow composite outputs **premultiplied** alpha
  (`out_rgb = encode(...) · coverage; out_a = coverage` with `ONE, ONE_MINUS_SRC_ALPHA`) — a recorded,
  deliberate per-backend split, not an accident (GLOW_IMPL §2f; its Tier-1 halo probe pins that path). The
  composite's coverage is **analytic** (the rounded-rect SDF, not a filtered alpha texture) and the edge
  color is constant, so the "over" blend is monotonic — there is no premultiplied/gamma halo to avoid.
  The §2d analytic oracle (`backdrop-blur-wgpu/tests/snapshot.rs::translucent_panel_edge_has_no_halo`)
  proved this on a high-contrast edge in both directions (bright-over-black, dark-over-white) and pins it
  against regression; the research's halo concern applies to filtered color-with-alpha, which this path
  never does.
- Shaders are **ported, not bound** — re-implemented from published ARM/scenefx values, not copied from
  GPLv3 picom. No Skia/libplacebo dependency.

## 9. Error and degradation scenarios

- **Zero-sized / offscreen region** → `prepare` returns `Ok(())`-equivalent no-op (no allocation, no
  grab); **never an error** (M7).
- **Resource creation fails** → `Err(BlurError::ResourceCreation { stage, .. })`; the adapter logs and
  the surface renders without frost that frame. Never panics.
- **Unsupported target format** → `Err(UnsupportedTarget)` from the lazy per-format pipeline build (M3).
- **Resize / DPI change** → the `PingPongKey { size, levels }` chain is rebuilt; stale keys age out
  (the composite pipeline is keyed *separately* by target format — §4.4; the scratch key carries no format).
- **Stale backdrop over animating content** → governed by the surface's `RepaintPolicy` (§4.6), a typed
  obligation, **not** an assumption that the host repaints.
- **Overlapping / stacked frosted surfaces** → **out of v1** (§2). The seam blurs non-overlapping
  regions; ordered stacked glass (panel 2 blurring panel 1's composited result) needs the seam to thread
  "composited-so-far" — designed as future work via either an ordered `&[BlurRequest]` the backend
  sequences against one evolving buffer, or in-seam target read-back (the Bevy `post_process_write`
  precedent). Named, not silently assumed-independent.
- **MSAA / grab-pass landmines** (multisampled read FBO, callback type-identity, GLES `#version 300 es`)
  → all live in `backdrop-blur-glow`, **deferred**; the `grab_source` socket (§4.4) is their reserved home.

## 10. Contracts / promises

- *`prepare` allocates/uploads and `record` composites a tinted, rounded-rect-masked frosted surface
  into a target distinct from its source — touching only resources it owns and leaving GPU state as
  found (the immediate-mode backend explicitly saves/restores bound FBO, viewport, blend func, texture
  units).*
- *`backdrop-blur-core` is pure, headless-testable, `#![forbid(unsafe_code)]`, with no GPU dependency —
  the one crate that cannot break a backend.*
- *A consumer compiles exactly the backends and toolkits it names — a wgpu user never builds glow's
  `unsafe`; a future non-egui adapter never forces egui's tree on others.*
- *The crate owns only a surface's background; content, foreground, and a11y stay the host's.*
- *Adding a backend or toolkit is a new crate against the stable seam — validated by the IMPL-doc glow
  sketch before core is frozen, not merely asserted.*

## 11. Verification

- **Core unit tests** (headless, no GPU): `BlurStrength × Scale → iteration/offset interpolation; the
  `CornerRadius` clamp + logical→physical resolution producing `ResolvedMask`; `Region` clipping; the
  linear-space tint conversion. The pure heart. (The per-pixel SDF is the shader's; core's testable part
  is the resolved params — S9.)
- **Backend tests** need a GPU: rendered against **lavapipe** for determinism (this repo's pattern).
  **As-built:** these are **analytic property assertions on rendered pixels** (energy preservation,
  edge registration, the **edge-halo probe** over a high-contrast edge in both directions), *not*
  committed golden-image files — no reference PNGs exist or are diffed. The glow twin runs the same
  class of oracles behind `gl-snapshots` on an EGL-surfaceless harness.
- **Per-adapter example crates** are the visual proof (frosted panel over moving content, blur on/off A/B).
- **Kiosk-cost benchmark** (named PARTIAL risk): dual-Kawase vs separable Gaussian at tooltip/dialog size,
  to convert the "performant on the deployment GPU" unknown into a measured fact.
- **CI** (as-built) runs a GPU-free default tier (`cargo test --workspace`), plus **deliberately narrow
  gated jobs** — `image-snapshots` (lavapipe), `gl-snapshots` (EGL-surfaceless), a
  `--no-default-features --features grab-pass` job with the `cargo tree -i wgpu` feature-unification
  guard — builds each example manifest, and verifies the MSRV (1.92) in a dedicated job. **Not**
  `--all-features` (the gated tiers are mutually exclusive slices by design). No feature-powerset
  explosion (backend split is crate-level).

## 12. What this does NOT cover

- Real glass on the **grab-pass / glow** path (mainstream eframe-glow) — deferred to
  `backdrop-blur-glow`; socket reserved (§4.4), not in v1.
- **OS/compositor vibrancy** — orthogonal, `window-vibrancy`'s job.
- **Closed-renderer toolkits** (GPUI, Slint, Makepad) — no render hook; out until upstream.
- **A general filter/effects graph** — one material only.
- **Overlapping/stacked ordered glass** — out of v1 (§2/§9); design space named, contract is single-surface.

## 13. Implementation sequence (high-level; the IMPL doc details each)

1. **`backdrop-blur-core`** — the two-phase seam trait + vocabulary + error + liveness + pure unit tests.
   **Before freezing the trait:** the IMPL doc sketches the `GlowBlur impl` (grab_source + the
   immediate-mode `Encoder = glow::Context` record) against it to validate the abstraction on the
   genuinely divergent backend — the seam's whole justification. The associated types are driven by the
   **known backend set** (wgpu + glow). *Decision gate:* if the glow sketch shows the trait does not
   fit without contortion, ship v1 as a **concrete `backdrop-blur-wgpu`/-`egui` pair (no trait)** and
   lift the seam only when glow actually lands — a one-backend v1 does not earn a trait by itself.
2. **`backdrop-blur-wgpu`** — `WgpuBlur`: dual-Kawase WGSL, `PingPongKey`-keyed chains, per-format
   pipeline cache, the linear-space composite with `ResolvedMask` SDF + tint. Lavapipe snapshot + the
   edge-halo probe.
3. **`backdrop-blur-egui`** (own-loop path) — the egui-wgpu adapter implementing the §6 frame-ordering
   contract + `RepaintPolicy` + `examples/egui-wgpu-panel`. **This is v1's last step.**

Deferred behind the stable seam: `backdrop-blur-glow` + egui grab-pass (mainstream/kiosk reach,
`unsafe`, Abdu-approval-gated); the `backdrop-blur` facade (once ≥2 sub-crates earn it); any future
non-egui toolkit adapter.

## 14. Open questions for IMPL (trimmed — the review closed several)

- **Trait-or-no-trait for v1** (§13 step 1 gate) — does the paper glow sketch justify the seam now, or
  ship a concrete one-backend v1 and lift the trait when glow lands? Decided at step 1, not before.
- **Continuous `BlurStrength` → per-pass offsets** — no closed-form sigma↔iterations; the backend
  interpolates offsets (research open-question; the *type* is settled, the curve is IMPL).
- **Edge alpha convention** (§8) — straight vs premultiplied linear at the glass boundary; the §11
  snapshot probe decides, not assumption.
- **Kiosk-GPU blur cost** — the benchmark in §11 converts the PARTIAL perf risk to a number.
- **Crate name** — `backdrop-blur` is descriptive/discoverable; confirm before publishing.

*(Closed by the review and now contracts, not questions: the coordinate/`Scale` convention (§4.1), the
seam's prepare/record shape and all associated types (§4.4), the `BlurError` enum (§4.5), the
read-after-write `source != target` rule (§6), the MSRV (1.92, STRUCTURE §6).)*
