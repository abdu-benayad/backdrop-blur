# `backdrop-blur` — Implementation Plan

> **Reads with:** `DESIGN.md` (the *what & why*, signed off 2026-06-05, wgpu-first),
> `STRUCTURE.md` (packaging topology), `NATIVE_BLUR_RESEARCH.md` (feasibility & algorithm — currently at
> `architecture/glass/NATIVE_BLUR_RESEARCH.md`; it holds the exact dual-Kawase tap offsets + the
> picom-vs-KWin half-pixel warning, and **travels into the new repo** as `docs/RESEARCH.md`).
> This doc is the *how & in what order*. Each sub-step is a **compiling, testable increment**.
>
> **Status:** **v1 IMPL increment IMPLEMENTED** (2026-06-05) in the dedicated repo
> `github.com/abdu-benayad/backdrop-blur` (local `/home/abdu/Downloads/backdrop-blur`, unpushed).
> Steps 1 (core + glow gate), 2 (wgpu separable Gaussian), 3 (egui own-loop + winit example) are
> done and committed on `main`, green on every commit, GPU-verified on lavapipe (incl. a real-egui
> own-loop readback). core + wgpu each survived an adversarial multi-lens review (14 + 11 findings,
> acted on). **Deferred follow-ons:** 2b′ (dual-Kawase) and 2d (analytic halo probe).
>
> **As-built divergences from the plan below** (acted on during review): the seam split into
> `BackdropBlur` + a `GrabPass` sub-trait so the wgpu backend stays total (no impossible
> `grab_source` stub); the wgpu `SourceTexture` is a `SourceView { view, size, color_space }`
> carrying what a bare `wgpu::TextureView` can't; the composite draws over the **full target** via
> `@builtin(position)` (real straight-edge AA + clip-registration) rather than a per-rect viewport;
> a generation-counter `debug_assert` implements the K1 guard; wgpu resource creation cannot return
> `Result`, so `BlurError::ResourceCreation` is glow-only and `UnsupportedTarget` is the one
> synchronous wgpu error; the own-loop renders egui **twice** (intermediate + target) rather than a
> blit. MSRV is **1.92** (egui 0.34), not 1.85.
>
> **Post-v1 status (this doc is the v1 record):** `backdrop-blur-glow` + the egui grab-pass path
> are **no longer deferred** — they shipped and are published (see `GLOW_IMPL.md`). Statements
> below that call glow "deferred / not v1" or say there is "no `glow` pin" describe the v1
> *increment*, not the current tree (the workspace now has a `glow` pin and a `backdrop-blur-glow`
> member). The glow `TargetSpec` binds to `FramebufferSize`, not a GLES internal-format (§3).

## 0. What already exists (and what doesn't)

- **Nothing in `backdrop-blur`** — greenfield repo; every file below is new. There is **no CI yet** in the
  host repo; the `ci.yml` in §5 is the new repo's, written as part of step 1.
- **The glow grab-pass is proven** in `abdu-egui-ui/examples/tooltip_blur_spike.rs`
  (`GlBlur { grab, tex_a/b, fbo_a/b, blur_prog, comp_prog, vao, size }`: grab region → ping-pong blur →
  composite → rebind egui's FBO). v1 does **not** use it (glow deferred); **step 1d's gate maps it against
  the trait** (§3). It uses a separable Gaussian — which v1 also ships first (S1).
- **The repo's hard test invariant (LESSONS):** *the default `cargo test` builds and runs on any machine,
  GPU or not.* wgpu bundles no software rasterizer — `request_adapter` returns `None` on a GPU-less box
  without a system lavapipe ICD — so **every test that creates a `wgpu::Device` is feature-gated**
  (`image-snapshots`) and run only on a lavapipe CI host with `--test-threads=1`. This plan honors that as
  a **two-tier** model (§8); it is the single most violated assumption a naive port would make.

## 1. IMPL-level architecture decisions

| Decision | Choice | Why |
|---|---|---|
| Repo | new dedicated repo; cargo **virtual-manifest workspace** | STRUCTURE §1/§6. |
| Edition / MSRV | `edition = "2024"`, `rust-version = "1.92"` (= max(1.85 edition floor, 1.87 wgpu 29, 1.92 egui/egui-wgpu 0.34)), `resolver = "3"`; `rust-toolchain.toml` pins the dev/CI toolchain (1.95.0), a dedicated CI job verifies the 1.92 floor | edition 2024 ⇒ Rust ≥ 1.85, but egui 0.34 pushes it to 1.92 (K6 realized). |
| v1 crates | `backdrop-blur-core`, `backdrop-blur-wgpu`, `backdrop-blur-egui` (libraries) + `examples/egui-wgpu-panel` (winit) | DESIGN §0/§13. glow/facade/2nd-toolkit deferred. |
| Test tiers | **default tier** = pure logic + a fake-backend wiring test, **GPU-free, runs anywhere**; **gated tier** = everything that builds a `wgpu::Device`, behind `image-snapshots`, lavapipe + `--test-threads=1` | LESSONS invariant; M1/M2 (the review's #1 finding). |
| Trait-or-not | decided at **1d** by the glow paper-sketch gate (§3); else ship concrete `wgpu`/`egui`, lift the seam at glow-time | DESIGN §13 — a one-backend v1 doesn't earn a trait by itself. |
| Blur algorithm | **separable Gaussian first** (2b — proven by the spike, sufficient at tooltip/dialog size per research), **dual-Kawase as a gated follow-on** (2b′) with a unit test pinning the exact tap offsets | S1; de-risks the first pixel + the half-pixel double-apply hazard out of the critical path. |
| Color | **linear-light blur with an explicit sRGB→linear decode at the egui sample boundary** (egui renders gamma-space — M7); edge alpha convention behind a shader switch, **selected** by the 2d probe (S3) | DESIGN §4.2; egui#3168. |
| unsafe | **none in v1** (`#![forbid(unsafe_code)]` on core + wgpu; bytemuck via **derives**, never `unsafe impl`) | review REJECT confirmed forbid holds. |

## 2. Data flow

DESIGN §6 (the egui own-loop frame-ordering contract), with the review's correction (M4): the egui
intermediate render pass is opened in its own scope and **dropped before the encoder is touched again**
(`RenderPass::forget_lifetime` makes a live-pass encoder op a *runtime* panic, not a compile error), and
the frame ends in **one** `queue.submit` chaining egui's returned command buffers with the blur's.

## 3. THE GATE — step 1d's glow paper-sketch (before freezing the trait)

Write the `GlowBlur` `impl` as signatures-only Rust (`todo!()` bodies) in a **throwaway `examples/` or a
`#[cfg(test)]` doc module that depends on `glow` as a dev-dependency** — never in `backdrop-blur-core`,
which stays GPU-dep-free. Map the spike's proven `GlBlur` onto the **final** `BackdropBlur` (after M6
froze `prepare -> Result<Option<Prepared>, _>` and the owned `Prepared`). It fits iff every cell maps with
no extra method and no `()` in a load-bearing slot:

| `BackdropBlur` item | wgpu | glow (from the spike) |
|---|---|---|
| `Device` / `Queue` | `wgpu::Device` / `wgpu::Queue` | `glow::Context` / `()` (uploads via the context) |
| `Encoder` | `wgpu::CommandEncoder` | `glow::Context` (immediate draw handle) |
| `Framebuffer` / `SourceTexture` | `()` / `wgpu::TextureView` | `glow::Framebuffer` / `glow::Texture` |
| `Target` / `TargetSpec` | `wgpu::TextureView` / `wgpu::TextureFormat` | `glow::Framebuffer` / **`FramebufferSize`** (the composite viewport, **not** a color format — as built) |
| `Prepared` (OWNED) | resolved offsets/tint/mask/rect + resource keys | **same payload** — glow's `prepare` *resolves* params into `Prepared`; it does **not** "upload" (immediate-mode GL binds uniforms at draw, in `record`) — K2 |
| `grab_source(fb, region)` | default: hand the intermediate through | `copy_tex_image_2d` from `fb` for `region` → grab tex |
| origin convention | top-left sample | **bottom-left grab** (`copy_tex_image_2d`); the flip lives **inside** glow's `grab_source` — confirms no extra trait method (K5) |

**Decision rule:** all cells ✓ → **keep the trait**. Any contortion (a method glow needs that the trait
lacks; an associated type wgpu can't supply; `()` standing for a load-bearing resource) → **drop the trait
for v1**, ship inherent `WgpuBlur` + concrete `backdrop-blur-egui`, lift core's seam when glow lands.
Record the sketch + decision in core's module docs. (Expectation: it fits — the gate makes that *verified*.)

## 4. Build sequence (each sub-step ends on the §8 tier that applies — not a weaker `cargo build`)

### Step 1 — `backdrop-blur-core` (+ the §3 gate)

1a. **Workspace skeleton.** Root virtual `Cargo.toml`: `members = ["crates/*"]` and the winit example
   **`exclude`d from the default members** (or a separate `members` entry built only in its own CI job —
   M2), `resolver = "3"`, `[workspace.package]` (edition/rust-version/license), `[workspace.dependencies]`
   pinning `wgpu`, `egui`, `egui-wgpu` (**no `glow` pin** — dead until the deferred glow crate, M3).
   `crates/backdrop-blur-core` with `#![forbid(unsafe_code)]`, empty `lib.rs`. `rust-toolchain.toml` pins
   the MSRV. *Green:* `cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo build`.

1b. **Domain types + pure tests** (the only headless-GPU-free unit tier). `BlurRadius`, `LinearRgba`,
   `Tint`, `CornerRadius`, `Scale`, `Region`, `BlurRequest`, `ResolvedMask`, `BlurError` (+ `BlurStage`,
   the boxed `BackendError`), `RepaintPolicy`. Pure fns + `{fn}_{scenario}` tests:
   `BlurRadius × Scale → (levels, per-pass offsets)`; `CornerRadius → ResolvedMask` with the
   `min(extent)/2` clamp; **`Region::is_empty_or_offscreen` predicate** (the no-op *predicate* lives here;
   the no-op *behavior* — `prepare → Ok(None)` — is asserted in the gated wgpu tier 2b, since core has no
   `prepare` to call — M6). *Green:* `cargo test -p backdrop-blur-core` (GPU-free, runs anywhere).

1c. **The seam traits** (DESIGN §4.4, post-M6): `BackdropBlur` with associated types incl. **owned
   `Prepared`**, `prepare -> Result<Option<Self::Prepared>, BlurError>`, `record`; plus the additive
   **`GrabPass: BackdropBlur`** trait carrying `Framebuffer` + `grab_source` (split out post-1d-review so
   the own-loop wgpu backend stays total). Doc contracts (`source != target`, state-restore,
   composite-keyed-by-format). **Freeze the no-op shape (`Option`) and the boxed error `source` here — they
   are types the gate sketches against, not comments.**

1d. **THE GATE (§3).** The glow signature sketch against the frozen trait, in `examples/glow_gate.rs` or a
   `glow`-dev-dep test module. *Green:* the sketch compiles; decision recorded in core docs. **If it fails,
   stop and revise/drop the trait before step 2.**

### Step 2 — `backdrop-blur-wgpu` (`WgpuBlur`)

2a. **Crate + skeleton** (fold into 2b if a bodiless commit would trip clippy — S5/C4). `#![forbid(unsafe_code)]`,
   deps `core` + `wgpu` + `bytemuck` (derives only). `WgpuBlur::new(&Device)` — **no `target_spec` arg**
   (format is per-`prepare`, M3). `impl BackdropBlur` with `todo!()` bodies; unused params `_`-prefixed,
   not-yet-read fields carry the repo's self-clearing `#[expect(dead_code, reason = …)]`. *Green:* the §8
   default tier (clippy `-D warnings` included — no red from the skeleton).

2b. **Separable Gaussian, end-to-end** (S1 — the proven path to a first pixel). Fixed **`Rgba16Float`**
   scratch chain keyed by `PingPongKey { size, levels }`; **sRGB→linear decode on sample** in the blur
   shader (egui's intermediate is gamma-encoded — M7), convolve, leave linear for composite. The
   **GPU-uniform struct lives here, not core** (S2): `#[repr(C)] #[derive(Pod, Zeroable)]` with explicit
   `_pad` fields (the WGSL 16-byte-rounding landmine), plus a `size_of`/offset unit test. `prepare`
   allocates/keys + resolves the payload (returning `Ok(None)` for the empty/offscreen region — M6);
   `record` encodes the separable passes. *Green:* **gated tier** —
   `cargo test -p backdrop-blur-wgpu --features image-snapshots -- --test-threads=1` (lavapipe): chain
   builds, a flat source blurs to a non-trivial CPU-readback. Default `cargo test` here = the pure
   cache-key/uniform-layout logic only.

2b′. **Dual-Kawase** (the production algorithm) as a **separate increment**, behind the kiosk benchmark
   (DESIGN §11). Port the exact published ARM/scenefx 5-tap down / 8-tap up offsets; a **pure unit test
   pins those offsets** (a wrong-weight blur still passes a "non-trivial output" readback — the offset test
   is the real guard) and resolves the picom-vs-KWin half-pixel double-apply convention (research). Swap it
   in behind a radius threshold. *Green:* gated tier + the new offset unit test (default tier).

2c. **Composite.** `composite.wgsl`: sample the blurred chain, evaluate the rounded-rect SDF from
   `ResolvedMask`, apply `Tint`, **re-encode to the target's color space**, blend in. The **alpha
   convention (premult vs straight) is a shader switch (define/uniform), defaulted but not frozen** (S3) —
   2d *selects* it without a 2c rewrite. The **composite pipeline is keyed by `TargetSpec`** (M8); the
   down/up pipelines stay fixed-scratch. Handle the sRGB target rule (`add_srgb_suffix` / `view_formats`).
   `BlurError::UnsupportedTarget` is an explicit allowlist, **separate** from wgpu's must-match-format
   validation. *Green:* gated tier — composites into a `Target` distinct from `source` (`source != target`).

2d. **Snapshot + edge-halo probe** (gated, lavapipe, `--test-threads=1`). Commit
   `panel-over-backdrop-{ltr,rtl}.png`. The **halo gate is an analytic oracle, not an eyeball** (K3): a
   CPU-readback numeric assertion on a boundary ROI (max channel delta below tolerance) decides + freezes
   premult-vs-straight; the PNG is a human-facing visual with a generous threshold, explicitly *not* the
   gate. *Green:* `cargo test -p backdrop-blur-wgpu --features image-snapshots -- --test-threads=1`.

### Step 3 — `backdrop-blur-egui` (own-loop, wgpu) — v1's last step

3a. **Crate + own-loop helper.** Deps `core` + `backdrop-blur-wgpu` + `egui` +
   **`egui-wgpu = { workspace = true, default-features = false }`** (NO `winit` in the library — M3),
   `own-loop` feature. The helper implements the corrected DESIGN §6 contract (M4):
   1. `update_texture` for deltas; `update_buffers(encoder, …)` → keep its `Vec<CommandBuffer>`.
   2. egui intermediate render pass in **its own scope, dropped** before any further encoder use.
   3. blur `prepare`/`record` (its own `begin_render_pass` into the swapchain/2nd target).
   4. `encoder.finish()`, then **one** `queue.submit(egui_bufs.into_iter().chain([main]))`.
   Build the `ScreenDescriptor` (both `update_buffers` and `render` need it) and reconcile its
   `pixels_per_point` with the per-`Region` `Scale`. *Green:* default tier (`-p backdrop-blur-egui`).

3b. **`Surface` API + `RepaintPolicy`** + an **observable wiring test** (M5). Default/headless:
   (a) a `kittest` test asserts the `Surface` places its rect and emits **no AccessKit node** (the
   decoration contract); (b) a **recording fake `impl BackdropBlur`** (trivial under static dispatch) whose
   `prepare`/`record` push into a `Vec` the test reads — asserting exactly one `prepare`+`record` per
   placed surface, and `Ok(None)` → no `record` for an offscreen surface. Real wgpu `prepare`/`record` is
   the gated tier only. *Green:* default tier `cargo test -p backdrop-blur-egui`.

3c. **`examples/egui-wgpu-panel`** (own crate, winit). The example enables `egui-wgpu/winit` + `egui-winit`
   (the only place winit enters). A frosted panel over moving content, blur on/off + radius A/B, a
   vsync-off per-frame blur-cost readout (the spike's UX, on wgpu — feeds the §11 benchmark). *Green:*
   `cargo build -p egui-wgpu-panel` (run is manual — needs a display); its own `clippy`. *(Optional, K4: a
   feature-gated lavapipe render-snapshot of the example's output is the only true proof the own-loop
   integration — which IS the product — works end-to-end; weigh against scope.)*

**Deferred (not v1):** `backdrop-blur-glow` + egui grab-pass (kiosk/mainstream reach, `unsafe`,
Abdu-approval-gated); `backdrop-blur` facade (≥2 sub-crates); any non-egui toolkit adapter.

## 5. File change summary

```
backdrop-blur/
  Cargo.toml                              [workspace] virtual; members=["crates/*"], example excluded; resolver 3; [workspace.deps] wgpu/egui/egui-wgpu (no glow)
  rust-toolchain.toml                     pin dev/CI toolchain (1.95.0); declared MSRV = 1.92 = max(1.85, wgpu 1.87, egui 1.92)
  docs/RESEARCH.md                        NATIVE_BLUR_RESEARCH.md carried in (tap offsets, half-pixel warning)
  .github/workflows/ci.yml                default tier (any runner) + gated lavapipe tier (mesa-vulkan-drivers, --test-threads=1) + separate example build
  crates/
    backdrop-blur-core/                   #![forbid(unsafe)]; deps: thiserror ONLY (no bytemuck — the GPU-uniform struct lives in -wgpu per S2)
      src/{lib,material,geometry,error,liveness,seam}.rs   types + the BackdropBlur trait + Region no-op predicate
    backdrop-blur-wgpu/                    #![forbid(unsafe)]; deps: core, wgpu, bytemuck; feat image-snapshots
      src/{lib,cache,uniforms}.rs          WgpuBlur, impl BackdropBlur, PingPongKey; the #[repr(C)] GPU-uniform struct + layout test live in uniforms.rs (as built)
      src/shaders/{gaussian,downsample,upsample,composite}.wgsl   include_str! + create_shader_module
      tests/snapshot.rs                    lavapipe panel + analytic halo probe (feature-gated, --test-threads=1)
    backdrop-blur-egui/                    deps: core, backdrop-blur-wgpu, egui, egui-wgpu(default-features=false); feat own-loop
      src/{lib,own_loop}.rs                Surface, RepaintPolicy, the §6 frame-ordering helper
      tests/wiring.rs                      default tier: kittest a11y-node-absence + recording-fake-backend call count
  examples/egui-wgpu-panel/{Cargo.toml,src/main.rs}   winit; enables egui-wgpu/winit + egui-winit
```

## 6. Dependency graph (build order = the steps)

```
backdrop-blur-core ──► backdrop-blur-wgpu ──► backdrop-blur-egui ──► examples/egui-wgpu-panel
   (step 1)              (step 2/2b′)           (step 3)               (step 3c)
```
Strict: core has no GPU dep; no sibling knows another; **only the example pulls winit** (M2/M3).

## 7. Risks & mitigations

- **The trait doesn't fit glow** (the core bet). → 1d's paper gate proves/disproves it before any wgpu
  code; failure triggers the documented concrete-v1 fallback.
- **`forget_lifetime` runtime panic** (encoder touched while egui pass alive — a *runtime* trap a CI build
  won't catch). → M4's drop-the-pass-then-single-submit contract, with a per-step comment; the optional 3c
  render-snapshot (K4) is the only thing that actually *executes* the path.
- **Gamma/sRGB** (egui writes gamma-space → naive convolution halos). → M7's explicit decode-on-sample /
  re-encode-on-composite; called out in DESIGN §4.2, not assumed.
- **Default `cargo test` needs a GPU** (the repo's cardinal sin). → M1's two-tier gating; the only
  always-on GPU-free signals are the core unit tests + the fake-backend wiring test.
- **Wrong dual-Kawase tap weights pass a "non-trivial output" readback.** → S1's offset unit test is the
  real guard; Gaussian-first keeps the first pixel off this risk.
- **WGSL uniform layout mismatch silently misreads the mask.** → S2's explicit `_pad` struct + `size_of`
  test, in the wgpu crate, derives only (keeps `forbid(unsafe)`).
- **lavapipe nondeterminism / SIGSEGV.** → `--test-threads=1`, `SnapshotOptions` thresholds, static-scene
  rule (LESSONS); the GPU tier is Linux/lavapipe-pinned and documented as a prerequisite.

## 8. Verification — two tiers (the review's load-bearing correction)

**Default tier — runs on ANY machine, spins up zero GPU contexts** (the repo invariant):
```
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings   # library crates only; portable BECAUSE no lib pulls winit
cargo test  --workspace                                  # core unit tests + the egui fake-backend wiring test
cargo build --workspace
```
**Gated GPU tier — lavapipe CI host only** (`mesa-vulkan-drivers`, `VK_ICD_FILENAMES=…/lvp_icd.json`,
`WGPU_BACKEND=vulkan`):
```
cargo test -p backdrop-blur-wgpu --features image-snapshots -- --test-threads=1   # chain/composite readback + snapshots + halo oracle
```
**The winit example — built separately** (its own job; never in the portable `--all-targets` loop):
```
cargo build  -p egui-wgpu-panel
cargo clippy -p egui-wgpu-panel -- -D warnings
```
Every sub-step (1a…3c) ends green on **the tier that applies to it** (1b/3b default; 2b…2d gated; 3c the
example build) — not a weaker per-step `cargo build`. Fresh repo, no PR diff: green-on-every-commit is the
only guard.
