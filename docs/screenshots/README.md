# Screenshots

Visual proof of the blur, captured from the real render paths (no mockups).

## Backdrop (wgpu own-loop)

Rendered headlessly through `OwnLoopRenderer` on a software (lavapipe) device by
`examples/frost-gallery` — deterministic, no display required.

| File | What it shows |
| --- | --- |
| `backdrop-gallery-contact.png` | Contact sheet of the strength ramp: bare backdrop → dark glass at 4 / 12 / 20 / 40 / 64 px (straddling the Gaussian → dual-Kawase threshold) → light frost at 32 px. |
| `gallery-contact.png` | Earlier contact sheet of the same gallery. |
| `winit-live.png` | A frame grabbed from the live `examples/egui-wgpu-panel` winit window. |

Regenerate the individual full-res frames with:

```sh
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json WGPU_BACKEND=vulkan \
  cargo run --manifest-path examples/frost-gallery/Cargo.toml
```

## Glass (egui-on-glow grab-pass)

The `backdrop-blur-egui` `GrabPassRenderer` integrated into `abdu-egui-ui`'s
overlay layer. This path needs a live `glow::Context`, so these were captured by
running the eframe examples on a display and grabbing the window — there is no
headless snapshot of the grab-pass blur. All three read "frosted glass: ON" (the
real lens, not the opaque fallback): the colour grid blurs through a pure clear
glass surface that adds no colour film of its own.

| File | What it shows |
| --- | --- |
| `glass-dialog-dark.png` | A frosted Dialog card over a busy colour grid, dark theme. |
| `glass-dialog-light.png` | The same card, light theme — the text legibility halo flips luminance. |
| `glass-popover.png` | A `.frost(true)` Popover anchored to a trigger. |

These come from `abdu-egui-ui`'s `dialog_frost_glow` and `popover_frost_glow`
examples (`--features "preview frosted-backdrop"`), captured by eye on a machine
with a display + GL 3.3 / GLES 3.0. They are a per-display visual proof, not a
deterministic regression artifact.
