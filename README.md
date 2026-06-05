# backdrop-blur

**Real backdrop blur — frosted glass / vibrancy — as a reusable, toolkit-agnostic GPU
capability for Rust GUIs.**

A frosted-glass surface is one whose background is the blurred, tinted copy of the content
behind it (macOS vibrancy, Windows Acrylic, CSS `backdrop-filter`). It is a first-class
material that **no Rust GUI toolkit ships today** — `window-vibrancy` does whole-window *OS*
vibrancy, Vello does *shape* blur; neither is in-app, arbitrary-surface backdrop blur. This
crate fills that gap once, so each toolkit needs only a thin adapter instead of re-deriving
the render-to-texture + multi-tap convolution every time.

> **Status: pre-release, v1 in progress.** The API is not yet stable and nothing is published
> to crates.io. See [`docs/IMPL.md`](docs/IMPL.md) for the build sequence.

## What it is (and is not)

- **Is:** one material — backdrop blur + tint + rounded-rect mask — as a GPU pass with a
  backend-agnostic seam, plus thin per-toolkit adapters. Built for *surfaces* (tooltip,
  dialog, drawer, popover): a handful visible at once, each paying one grab + blur.
- **Is not:** a general effects/filter graph, OS/compositor vibrancy, or a renderer. It
  composites *into* a target the host owns; it never owns the frame.

## v1 scope, stated plainly

v1 is the disciplined minimal slice on the safe backend:

```
backdrop-blur-core  +  backdrop-blur-wgpu  +  backdrop-blur-egui (own-loop)  +  examples/egui-wgpu-panel
```

- **100% safe Rust.** The only `unsafe` crate (`backdrop-blur-glow`, for the grab-pass path)
  is **deferred**.
- The egui path is the **own-loop** path — apps driving `egui-winit` + `egui-wgpu` directly,
  using `egui-wgpu::Renderer`'s caller-chosen attachment (no fork). Mainstream `eframe`-on-glow
  reach arrives later with the deferred glow backend.
- v1 supports a **single frosted surface over a once-rendered backdrop**, non-overlapping.
  Ordered, stacked glass is named future work.

## Workspace layout

| Crate | Role | Status |
|---|---|---|
| `backdrop-blur-core` | seam trait + material/geometry/error/liveness vocabulary; no GPU dep; `#![forbid(unsafe_code)]` | v1 |
| `backdrop-blur-wgpu` | wgpu backend (WGSL), safe | v1 |
| `backdrop-blur-egui` | egui adapter — own-loop (→ wgpu) | v1 |
| `backdrop-blur-glow` | glow backend (GLES) — the only `unsafe` crate | deferred |
| `backdrop-blur` | optional thin facade (re-exports) | deferred |

Backends and adapters are separate crates on purpose: distinct public resource types
(`wgpu::TextureView` vs a glow texture) plus Cargo's additive-feature rule mean a consumer
compiles exactly the backends and toolkits it names — a wgpu user never builds glow's
`unsafe`.

## Verification

Two tiers (see [`docs/IMPL.md`](docs/IMPL.md) §8):

```bash
# Default tier — runs on any machine, GPU or not:
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test  --workspace
cargo build --workspace

# Gated GPU tier — lavapipe software Vulkan only (added with backdrop-blur-wgpu):
cargo test -p backdrop-blur-wgpu --features image-snapshots -- --test-threads=1
```

## Documentation

The full design trail travels with the crate:

- [`docs/DESIGN.md`](docs/DESIGN.md) — the *what & why*; §4 is the load-bearing type design.
- [`docs/IMPL.md`](docs/IMPL.md) — the *how & in what order*; each sub-step is a compiling, testable increment.
- [`docs/STRUCTURE.md`](docs/STRUCTURE.md) — the workspace topology and its ecosystem precedents.
- [`docs/RESEARCH.md`](docs/RESEARCH.md) — feasibility and the blur algorithm (dual-Kawase tap offsets).

## License

Dual-licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
