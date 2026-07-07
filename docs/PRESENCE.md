# Presence (surface-global fade coverage) — design + IMPL note

> Presence is the surface-global fade dial — what a host reaching for "opacity" actually wants: an
> increment on the shipped v1 that adds one material parameter, `Presence`, fading a frosted surface
> **toward transparency as a whole** — the blurred backdrop dissolves back to the untouched destination as
> presence → 0. Motivated by a real consumer (abdu-egui-ui's `Scrim`: a modal dialog fades in/out, and the
> frosted backdrop must fade *with* it). Today there is no such knob: `Tint.alpha` is the film mix (blur vs
> tint color) and `BlurRadius` is the blur amount — neither makes the whole surface *present-or-absent*. A
> consumer that scales those to fake a fade gets a frost that **pops in at full presence** (the composite
> *replaces* the destination; it does not blend by a master factor). This adds the missing factor.

## The one-line insight

The composite **already alpha-blends the frosted result over the destination** using the rounded-rect
`coverage` as the blend weight (WGSL straight-alpha `vec4(rgb, coverage)` over; GLSL premultiplied
`vec4(rgb*coverage, coverage)` with `ONE, ONE_MINUS_SRC_ALPHA`). So a master fade is **not** a new pass or a
new blend mode — it is the existing blend weight scaled by a global factor:

```
effective_coverage = coverage * presence        // presence ∈ [0, 1]
```

- `presence = 1` → `coverage * 1` → **bit-identical to today** (the default; every existing caller/test/golden
  unchanged).
- `presence = 0` → blend weight 0 everywhere → the destination shows through **untouched** (no grab artifact,
  no tint).
- `presence = 0.5` → the frosted surface is blended at half weight over the destination — a partial frost.

For the modal case the destination behind the surface rect *is* the dimmed page, and the blur samples that
same dim, so fading `presence` 0→1 dissolves the backdrop from *plain dim* to *frosted dim* — exactly the
fade-in.

## Type & API

- **`Presence(f32)`** newtype in `backdrop-blur-core` `material.rs`, beside `BlurRadius`/`Tint`/
  `CornerRadius`. Doc comment leads with the disambiguation: *surface-global blend weight — distinct from
  `Tint`'s alpha, which is the film mix (blur vs tint color).* `Presence::value(self) -> f32`;
  `Default = Presence(1.0)`; `const FULL = Presence(1.0)`.
- **Constructor scrubs non-finite, then two-sided clamps** — the precedent is `LinearRgba`'s **alpha**
  (a real `[0,1]` clamp, `material.rs:87`), **not** `BlurRadius`/`CornerRadius` (which clamp only the lower
  bound and accept unbounded-high). A naive `v.clamp(0.0, 1.0)` *propagates* `NaN`, the undebuggable-garbage
  case every constructor here prevents (`finite_or_zero`). So: `Presence::new(v) = Self(if v.is_finite() {
  v.clamp(0.0, 1.0) } else { 1.0 })` — non-finite falls back to **1.0** (fully present, the
  behavior-preserving default), not 0.0 (invisible).
- **Re-export `Presence` from BOTH** `backdrop-blur-core/src/lib.rs` (the `material::{…}` group, line 41) and
  `backdrop-blur-egui/src/lib.rs` (the neutral-spine group, line 29), mirroring `Tint`/`CornerRadius` — the
  motivating consumer depends on `-egui`, not core, so without the second re-export it cannot name the type.
- **`BlurRequest.presence: Presence`** (backend-facing) and **`Surface.presence: Presence`** (the egui
  adapter's consumer-facing material). The adapters thread `Surface.presence → BlurRequest.presence`, the
  backends thread it into their (per-backend, differently-shaped) composite params — see §Uniform layout.
- **Backward compatibility = runtime only, NOT source.** `presence` defaults to `1.0`, so behavior and every
  readback golden are unchanged at the default. But `BlurRequest`/`Surface` have **no `Default` derive**
  (only `Scale` does), so there is **no `..Default::default()` shortcut** — every struct literal must add
  `presence: Presence::default()` by hand. The complete, enumerated set the increment must edit (the repo's
  "do all of them" rule):
  - **Production threading (2):** `backdrop-blur-egui/src/own_loop.rs:30`, `…/grab_pass.rs:103` (the
    `BlurRequest { … }` built from a `Surface`).
  - **`Surface` definition + helpers/examples (5):** `…/surface.rs:19` (gains the field),
    `…/own_loop.rs:387` (test helper), `…/tests/own_loop_render.rs:156`, and **three** example binaries —
    `examples/egui-wgpu-panel/src/main.rs:216`, `examples/eframe-glow-panel/src/main.rs:75`,
    `examples/frost-gallery/src/main.rs:86` (the doc previously said "two preview examples" — it is **three**).
  - **`BlurRequest` test literals (7):** `backdrop-blur-core/src/geometry.rs:256,270`;
    `backdrop-blur-glow/src/blur_tests.rs:184,687,814`; `backdrop-blur-wgpu/tests/snapshot.rs:183,327`.

## Shader math (the load-bearing part — preserves the §2d no-halo property)

Both composites fold `presence` into the cover step, **after** the sRGB encode, leaving the §2d argument
intact (the encode-then-cover order is *why* there is no halo; presence does not reorder it):

The two backends reach the **identical color result by different blend configs** (not the same mechanism):
wgpu uses *separate* blend components — color `SrcAlpha, OneMinusSrcAlpha` (straight) and alpha `One,
OneMinusSrcAlpha` (`lib.rs:718–731`); glow uses a *unified* `ONE, ONE_MINUS_SRC_ALPHA` (premultiplied,
`composite.rs:104`). Both yield `out.rgb = a·rgb + (1−a)·dst` for `a = coverage·presence`, because
premultiplication is linear in `a` and commutes with the presence scale.

- **WGSL (straight alpha), `composite.wgsl`:**
  ```
  return vec4<f32>(rgb, coverage * params.opacity);
  ```
  `rgb` (the encoded tint-over-blur edge color) is unchanged; only the straight-alpha weight scales. The
  blend is still a monotonic mix between a *constant* edge color and `dst` — no premultiplied/gamma halo.
  (The uniform field is still named `opacity` in the wgpu-internal `CompositeParams`/`composite.wgsl` — a
  crate-private, positionally-fed uniform, out of scope for this rename; see §Uniform layout.)

- **GLSL (premultiplied), `composite.frag`:**
  ```
  frag = vec4(rgb * coverage * u_opacity, coverage * u_opacity);
  ```
  Premultiplied output stays consistent (`out_rgb` and `out_a` scale together). The encode still happens
  before the coverage/presence multiply (the concave OETF overshoot the §2d comment warns about is avoided
  exactly as before). (`u_opacity` is the glow uniform name — same internal-uniform exception as above.)

**Why presence multiplies `coverage`, not `tint.a`:** fading the surface should not change *what the frost
looks like* (the blur radius, the film color/mix) — only *how present it is*. `tint.a` is the blur-vs-tint
mix; scaling it would wash the tint out, not fade the surface. The master factor belongs on the final
blend weight.

## Uniform layout

The two backends have **differently-shaped** `CompositeParams` (wgpu is a `#[repr(C)] Pod` UBO mirror; glow
is a plain struct of CPU-side values set as individual uniforms). Both chains must be threaded — missing
either silently defaults the fade off on that backend. The internal uniform-path field on both backends
keeps the name `opacity` (crate-private, fed positionally, out of scope for this rename — see the
tri-way vocabulary note below); only the public `BlurRequest`/`Surface` field and the `Presence` type
change.

- **wgpu** (`uniforms.rs`, 64 bytes): replace the tail `_pad: [f32; 2]` with `opacity: f32, _pad: f32` —
  **same 64-byte layout** (`opacity` at offset 56, `_pad` at 60), no realignment. `composite.wgsl`'s struct
  mirrors it: `opacity: f32` then a single `_pad: f32` (was `_pad: vec2<f32>`). `CompositeParams::new`
  (`uniforms.rs:64`) gains an `opacity: f32` arg; call site `lib.rs:356` passes `request.presence.value()`.
  **Amend the layout test** `composite_params_layout_matches_wgsl` (`uniforms.rs:122`) with
  `assert_eq!(offset_of!(CompositeParams, opacity), 56)` — it is the existing guard against a GPU-misread,
  and a new field left unasserted defeats it.
- **glow — the full chain (4 edits, not just the shader):** (a) the glow `CompositeParams` struct
  (`composite.rs:28`) gains `opacity: f32`; (b) `CompositeParams::new` (`composite.rs:47`) gains an
  `opacity: f32` arg, sourced at the call site `blur.rs:153` from `request.presence.value()`; (c)
  `composite::draw` (`composite.rs:108+`) adds `gl.uniform_1_f32(loc("u_opacity").as_ref(), params.opacity)`;
  (d) `composite.frag` declares `uniform float u_opacity;` and folds it into the output (the `frag = …`
  line). The doc-of-record for glow is all four — (a) and (b) and the `blur.rs:153` source are the seams an
  implementer who only reads the shader will miss.

## Verification (the analytic oracle is the real gate)

- **core unit:** `Presence::new` clamps **and scrubs non-finite** — `-1 → 0`, `2 → 1`, `0.3 → 0.3`,
  **`NaN → 1.0`, `+∞ → 1.0`** (the sibling-type discipline: every other constructor has a
  `*_scrubs_non_finite_*` test); `Default == 1.0`, `FULL == 1.0`.
- **wgpu readback** (`tests/snapshot.rs`) and **glow readback** (`blur_tests.rs`), each at `presence ∈ {0.0,
  0.5, 1.0}` over a known destination `D`, with `F` = **the existing presence-1 composite readback**:
  - `presence = 1.0` → **byte-identical to the current golden** (regression guard: the default path is
    untouched).
  - `presence = 0.0` → the target equals `D` **everywhere** (the destination the composite drew over is
    untouched — proves "absent" is truly absent, not a faint grab).
  - `presence = 0.5` → each pixel equals **`lerp(D, F, presence)`** = `lerp(D, F, 0.5)` within tolerance. (NOT
    `lerp(D, F, 0.5*coverage)` — that double-applies coverage, since `F` already contains the per-pixel
    coverage: `F = lerp(D, rgb, coverage)`, and the true output is `lerp(D, rgb, coverage*presence) =
    lerp(D, F, presence)`. The earlier draft's `0.5*coverage` was off by up to 2× at the AA edge.) This is the
    analytic oracle: a real linear blend toward `D`, halo-free at the rounded-rect edge.
- Both tiers run on the existing lavapipe (wgpu) / EGL-surfaceless (glow) harnesses; no new infra.
- **Perf nit (not a gate):** when `presence == 0.0` the composite still runs a full-screen no-op blend. A
  consumer holding 0 across frames can skip the whole grab+blur+composite; the Scrim integration already
  early-outs at `fade == 0`, so this is a documented downstream optimization, not a backend change.

## Scope / non-goals

One scalar, default-1.0, on the existing single-surface composite. Not animation (the consumer drives the
value per frame), not per-corner or gradient presence, not a second blend mode. The own-loop and grab-pass
adapters both gain the `Surface.presence` passthrough; no backend pipeline is added or recompiled (it is a
uniform, not a pipeline key).

## Review outcomes

Two-lens adversarial pass (shader-math/GPU-correctness + API/type/cross-crate). Both confirmed the core
insight (`effective = coverage·presence`, identical on both backends, §2d no-halo preserved) and the
`Presence` newtype design, but caught issues now folded in:

- **Oracle was wrong (blocker, math lens):** the `presence=0.5` check `lerp(D, F, 0.5*coverage)` double-applies
  coverage (`F` already includes it) — off by 2× at AA edges. Corrected to **`lerp(D, F, presence)`**.
- **Glow threading under-specified (blocker, API lens):** the two backends have different `CompositeParams`
  shapes; the draft detailed wgpu but for glow touched only the shader. Now the full 4-edit glow chain
  (struct field `composite.rs:28`, `::new` arg `:47`, source `blur.rs:153`, uniform write `composite.rs:108+`)
  is spelled out.
- **Call sites wrong (blocker, API lens):** "two preview examples unchanged" was false — **three** examples
  build `Surface`, none derive `Default`, so all **12** literal sites need the field. Full list enumerated.
- **Should-fix, folded in:** clamp must scrub non-finite (NaN propagates through `f32::clamp`) → fallback 1.0,
  precedent is `LinearRgba` alpha not `BlurRadius`; re-export `Presence` from both `lib.rs` files; amend the
  `composite_params_layout_matches_wgsl` test; disambiguate `Presence` vs `tint.a` in the doc comment; soften
  the presence=0 "bit-identical" claim (rests on the `src.a=0` short-circuit) + the no-op-draw perf nit.
- **Sound, no change:** newtype altitude (peer of Tint, not a field on it), the name `Presence` (`Coverage` is
  taken by the shader AA term; see the crate-root three-dial vocabulary note for how it reads against
  film opacity), `FULL` + `Default` ergonomics, sRGB encode ordering, the `_pad` reuse
  layout.

**As-built (implemented + verified):** core `Presence` (clamps + scrubs non-finite→1.0, 3 unit tests);
threaded through `BlurRequest`/`Surface` and both backends' composite params; both shaders fold
`coverage·presence`. All 12 struct-literal sites updated; `Presence` re-exported from both `lib.rs`. The
`composite_params_layout_matches_wgsl` and glow `program.rs` shader-text guards were amended. **Empirical
gate met on both backends** via the corrected `lerp(D, F, presence)` oracle: wgpu `image-snapshots`
(`presence_fades_…`, + the presence-1 goldens unchanged) and glow `gl-snapshots`
(`presence_fades_…`, + presence-1 readbacks byte-identical). Default tier (fmt/clippy/test) green. Note: the
glow `gl-snapshots` readback tier must run `--test-threads=1` (parallel GL contexts are flaky in the
harness — pre-existing, documented).
