//! The grab-pass adapter: frosted glass for an `eframe`-on-glow app (and the `cage` kiosk). Unlike
//! the own-loop path, the host owns the GL loop; this adapter rides egui's **paint callback** —
//! egui_glow invokes a closure with the live `glow::Context` mid-frame, after the backdrop is drawn
//! and before the panel's foreground — and inside it grabs the backdrop, blurs it, and composites
//! the frosted surface back into the same framebuffer.
//!
//! This crate stays `#![forbid(unsafe_code)]`: all GL is in `backdrop-blur-glow`. The one thing a
//! safe adapter cannot do — capture `GL_DRAW_FRAMEBUFFER_BINDING` — is the glow crate's safe
//! [`current_draw_framebuffer`] helper.
//!
//! # The load-bearing construction (DESIGN §5)
//!
//! [`callback_region`] turns egui's paint-callback geometry into the **GL bottom-left** region to
//! grab and composite. egui's `ViewportInPixels` already carries `from_bottom_px` (GL-origin), so
//! the mapping is a field read + an `i32 → u32` cast — **no `height − y` flip**. It is a pure
//! function, unit-tested below, so the orientation wiring is proven without a GPU.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use backdrop_blur_core::{BlurError, BlurRequest, GlRegion, RepaintPolicy, Scale};
use backdrop_blur_glow::{FramebufferSize, GlowBlur, current_draw_framebuffer};

use crate::Surface;

/// Drives the grab-pass (eframe-on-glow) frosted-glass path: holds the glow backend behind a mutex
/// (the paint callback is `Fn + Send + Sync`, so it cannot own `&mut`) and enqueues a paint callback
/// per frosted [`Surface`].
///
/// # Lifecycle
///
/// Build once from the host's `glow::Context` (e.g. `eframe::CreationContext::gl`). Call
/// [`destroy`](Self::destroy) from `eframe::App::on_exit` (where `Frame::gl()` is still current) so
/// the backend's GL objects are freed while the context lives — never in `Drop` (DESIGN §11).
pub struct GrabPassRenderer {
    blur: Arc<Mutex<GlowBlur>>,
    /// Set true at paint-callback entry whenever a frost callback actually fires (egui skips a
    /// fully-clipped callback); the host can poll [`took_effect`](Self::took_effect) after the frame
    /// to learn whether the callback fired (the result cannot be returned synchronously from a
    /// callback — DESIGN §7). It signals *the callback ran*, not *the blur composited*: a
    /// clipped-empty or failed frost still sets it.
    ran: Arc<AtomicBool>,
    /// Throttles the best-effort frost-failure warning to once per failure episode: set the first
    /// time a [`frost`](Self::frost) callback errors, cleared on the next success. Under
    /// [`RepaintPolicy::Live`] the callback runs every frame, so without this latch a
    /// persistently-failing context would flood the log (DESIGN §7's "warn once").
    warned: Arc<AtomicBool>,
}

impl GrabPassRenderer {
    /// Build the backend against the host's shared GL context (probe its capabilities, compile the
    /// programs). Returns [`BlurError::UnsupportedContext`] if the context is too old for the
    /// backend (raised inside [`GlowBlur::new`]).
    ///
    /// [`BlurError::UnsupportedContext`]: backdrop_blur_core::BlurError::UnsupportedContext
    pub fn new(gl: &Arc<glow::Context>) -> Result<Self, BlurError> {
        let blur = GlowBlur::new(gl)?;
        Ok(Self {
            blur: Arc::new(Mutex::new(blur)),
            ran: Arc::new(AtomicBool::new(false)),
            warned: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Free the backend's GL objects. Call from `eframe::App::on_exit` while the context is current
    /// (DESIGN §11). Idempotent. Recovers a poisoned lock so the GL objects are still freed after a
    /// prior frost panic — otherwise they would leak (DESIGN §6/§12: the poisoned path is recovered,
    /// never silently dropped).
    pub fn destroy(&self, gl: &glow::Context) {
        let mut guard = match self.blur.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.blur.clear_poison();
                poisoned.into_inner()
            }
        };
        guard.destroy(gl);
    }

    /// Whether a frost paint callback has fired since the last call (reads **and resets** the flag).
    /// egui skips a callback whose rect is fully clipped/offscreen, so a frosted surface is not
    /// guaranteed to paint; this lets the host observe it after the frame.
    pub fn took_effect(&self) -> bool {
        self.ran.swap(false, Ordering::Relaxed)
    }

    /// Enqueue a frosted [`Surface`] for this frame: drive its repaint policy and add a paint
    /// callback that, mid-frame, grabs the backdrop behind `surface.rect`, blurs it, and composites
    /// the frosted surface back. The crate owns only the background; the surface's foreground is the
    /// host's (painted in its own later pass), and a frosted surface adds nothing to the AccessKit
    /// tree.
    pub fn frost(&self, ui: &egui::Ui, surface: Surface) {
        // The adapter drives liveness — a stale backdrop cannot be silently forgotten (DESIGN §4.6).
        match surface.repaint {
            RepaintPolicy::Live => ui.ctx().request_repaint(),
            RepaintPolicy::Bounded(after) => ui.ctx().request_repaint_after(after),
            RepaintPolicy::Static => {}
        }

        let blur = Arc::clone(&self.blur);
        let ran = Arc::clone(&self.ran);
        let warned = Arc::clone(&self.warned);
        let callback = egui_glow::CallbackFn::new(move |info, painter| {
            // Flag at callback ENTRY (DESIGN §7): record that the callback fired this frame, before
            // any early-out, so `took_effect` is honest even when the region clips to nothing.
            ran.store(true, Ordering::Relaxed);
            let gl = painter.gl();

            let Some((region, framebuffer_size)) = callback_region(&info) else {
                return; // the panel clipped to nothing — a no-op
            };
            // A poisoned lock means a prior frost panicked mid-blur. GlowBlur is a bag of GL handles
            // with no invariant a panic can corrupt, so clear the poison and recover the guard rather
            // than leave frost permanently dead and silent (DESIGN §6/§12: log_err + flat fallback,
            // never silently skipped). clear_poison makes the warning self-limiting — once per panic,
            // not once per frame under Live.
            let mut guard = match blur.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    log::warn!(
                        "backdrop-blur grab-pass mutex was poisoned by a prior panic; recovered"
                    );
                    blur.clear_poison();
                    poisoned.into_inner()
                }
            };

            // Material off the surface, geometry from the GL-origin region; both source and target
            // are the same on-screen rect (v1: the backdrop directly behind the glass).
            let request = BlurRequest {
                source_region: region.into_region(),
                target_rect: region.into_region(),
                strength: surface.strength,
                tint: surface.tint,
                corner_radius: surface.corner_radius,
                opacity: surface.opacity,
            };
            let target = current_draw_framebuffer(gl);
            match guard.frost_region(gl, target, region, framebuffer_size, &request) {
                // Recovered (or never failed) — re-arm the once-per-episode failure warning below.
                Ok(()) => warned.store(false, Ordering::Relaxed),
                // A paint callback cannot propagate an error; the frost is best-effort. Warn ONCE per
                // failure episode (DESIGN §7): under RepaintPolicy::Live this runs every frame, so an
                // unthrottled warn floods a kiosk log. The format! runs only on the first bad frame.
                Err(e) => {
                    if !warned.swap(true, Ordering::Relaxed) {
                        log::warn!("backdrop-blur grab-pass frost failed: {e}");
                    }
                }
            }
        });

        ui.painter().add(egui::PaintCallback {
            rect: surface.rect,
            callback: Arc::new(callback),
        });
    }
}

/// Turn egui's paint-callback geometry into the **GL bottom-left** region to grab + composite, plus
/// the full framebuffer size (the composite viewport). Pure — no GL — so the orientation wiring is
/// unit-tested. The region is `viewport ∩ clip_rect ∩ framebuffer`; `None` when that is empty (a
/// no-op). egui's `from_bottom_px` is already GL-origin, so this never computes a `height − y` flip.
fn callback_region(info: &egui::PaintCallbackInfo) -> Option<(GlRegion, FramebufferSize)> {
    let ppp = info.pixels_per_point;
    let to_gl = |v: &egui::epaint::ViewportInPixels| {
        GlRegion::from_bottom_px(
            [v.left_px.max(0) as u32, v.from_bottom_px.max(0) as u32],
            [v.width_px.max(0) as u32, v.height_px.max(0) as u32],
            Scale::new(ppp),
        )
    };
    let viewport = to_gl(&info.viewport_in_pixels());
    let clip = to_gl(&info.clip_rect_in_pixels());
    let region = viewport.intersect(&clip)?.clip_to(info.screen_size_px)?;
    Some((region, FramebufferSize(info.screen_size_px)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(
        viewport: egui::Rect,
        clip: egui::Rect,
        screen: [u32; 2],
        ppp: f32,
    ) -> egui::PaintCallbackInfo {
        egui::PaintCallbackInfo {
            viewport,
            clip_rect: clip,
            pixels_per_point: ppp,
            screen_size_px: screen,
        }
    }

    fn rect(min: (f32, f32), size: (f32, f32)) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(min.0, min.1), egui::vec2(size.0, size.1))
    }

    #[test]
    fn callback_region_maps_egui_top_to_gl_high_y_no_flip() {
        // A 800x600 screen; a panel flush with egui's TOP (y 0..50). In GL bottom-left, the top of
        // the screen is HIGH y: from_bottom = 600 - 50 - 0 = 550. This is the whole orientation
        // contract (DESIGN §5) — proven through egui's own from_bottom_px, with no manual flip.
        let panel = rect((0.0, 0.0), (100.0, 50.0));
        let (region, fb) =
            callback_region(&info(panel, panel, [800, 600], 1.0)).expect("non-empty");
        assert_eq!(region.origin_bl(), [0, 550]);
        assert_eq!(region.size(), [100, 50]);
        assert_eq!(fb, FramebufferSize([800, 600]));
    }

    #[test]
    fn callback_region_maps_egui_bottom_to_gl_low_y() {
        // A panel flush with egui's BOTTOM (y 550..600) → GL from_bottom = 600 - 50 - 550 = 0.
        let panel = rect((0.0, 550.0), (100.0, 50.0));
        let (region, _) = callback_region(&info(panel, panel, [800, 600], 1.0)).expect("non-empty");
        assert_eq!(region.origin_bl(), [0, 0]);
        assert_eq!(region.size(), [100, 50]);
    }

    #[test]
    fn callback_region_scales_by_pixels_per_point() {
        // ppp = 2.0: a 100x50-pt panel at (10,10) → 200x100 px at left=20; from_bottom = 1200-100-20.
        let panel = rect((10.0, 10.0), (100.0, 50.0));
        let (region, fb) =
            callback_region(&info(panel, panel, [1600, 1200], 2.0)).expect("non-empty");
        assert_eq!(region.size(), [200, 100]);
        assert_eq!(region.origin_bl(), [20, 1200 - 100 - 20]);
        assert_eq!(fb, FramebufferSize([1600, 1200]));
    }

    #[test]
    fn callback_region_is_none_when_clip_is_disjoint() {
        // The clip rect shares no area with the viewport → the panel paints nothing.
        let viewport = rect((0.0, 0.0), (100.0, 100.0));
        let clip = rect((400.0, 400.0), (50.0, 50.0));
        assert!(callback_region(&info(viewport, clip, [800, 600], 1.0)).is_none());
    }
}
