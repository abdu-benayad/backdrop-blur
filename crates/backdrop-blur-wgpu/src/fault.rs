//! The deferred-fault vocabulary for the web (wasm32 WebGPU-dispatch) own-loop path.
//!
//! On the browser's WebGPU dispatch an error scope's `pop()` resolves as a promise, so a
//! frame-path creation cannot read its fault synchronously the way native does. The web path
//! instead *parks* each scope's outcome as a [`PendingFault`] in a [`FaultLog`] shared between
//! the backend and the spawned pop-awaiting tasks, and folds drained faults into one
//! latest-wins [`FaultReport`] the host takes once per frame. This module is the pure half of
//! that machinery — no GPU types beyond [`BlurError`], compiled and unit-tested on every
//! target; the wasm32 collector that feeds it lands with the web frame path.

use backdrop_blur_core::BlurError;

/// The host-facing *kind* of resource a deferred fault names. Diagnostic only: the recovery
/// contract never branches on which slot faulted (design D2) — every report means "do not
/// trust the presented frost; re-request a repaint and retry unfrosted or shed surfaces".
/// Kinds, not cache keys: the backend's internal keying stays out of the public API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultSlot {
    /// A Gaussian ping-pong scratch texture chain.
    Scratch,
    /// A dual-Kawase mip-pyramid chain.
    Pyramid,
    /// The per-target-format composite render pipeline.
    CompositePipeline,
    /// A per-frame uniform buffer.
    UniformBuffer,
    /// A per-frame bind group.
    BindGroup,
    /// The adapter-owned offscreen intermediate texture.
    Intermediate,
}

/// The latest-wins fault state the host reads once per frame. `error` and `slot` are the most
/// recent reportable fault (always [`BlurError::DeviceOutOfMemory`] — the web path has no
/// device-fatal creation arm); `occurrences` counts every reportable fault folded in since the
/// host last took a report, saturating, so a pressure burst is visible as more than "one".
#[derive(Debug)]
pub struct FaultReport {
    /// The most recent reportable fault, carrying the flattened backend message as its source.
    pub error: BlurError,
    /// Which kind of resource the most recent fault named.
    pub slot: FaultSlot,
    /// How many reportable faults were folded into this report (saturating).
    pub occurrences: u32,
}

/// One parked error-scope outcome, recorded by a spawned pop-awaiting task. `message` is the
/// `describe()`-flattened error text — the live `wgpu::Error` is not `Send + Sync` on wasm, so
/// it is flattened at capture (K1) and re-boxed into [`BlurError`] only when folded into the
/// host report.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "constructed only by the wasm32 deferred collector (lands with the web frame path); unit-tested natively"
    )
)]
pub(crate) struct PendingFault {
    /// The host-facing kind of the resource whose scope caught the fault.
    pub(crate) slot_kind: FaultSlot,
    /// The backend generation active when the faulted resource was created; the drain compares
    /// it against the named cache entry's stamp to tell a live fault from a stale one.
    pub(crate) generation: u64,
    /// The flattened backend error text.
    pub(crate) message: String,
}

/// The shared collector: parked scope outcomes plus the folded host report. Spawned tasks
/// `record` into `pending`; the backend drains, decides, and folds; the host takes the report.
#[derive(Default)]
pub(crate) struct FaultLog {
    pending: Vec<PendingFault>,
    report: Option<FaultReport>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "called only by the wasm32 deferred collector (lands with the web frame path); unit-tested natively"
    )
)]
impl FaultLog {
    /// Parks one scope outcome for the next drain.
    pub(crate) fn record(&mut self, fault: PendingFault) {
        self.pending.push(fault);
    }

    /// Removes and returns every pending fault matching `pred`; non-matching records stay
    /// parked for a later drain (the adapter and the backend drain disjoint slot kinds).
    pub(crate) fn drain_where(
        &mut self,
        pred: impl FnMut(&PendingFault) -> bool,
    ) -> Vec<PendingFault> {
        let (drained, kept) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(pred);
        self.pending = kept;
        drained
    }

    /// Folds one reportable fault into the host report: latest-wins on `error`/`slot`, with a
    /// saturating occurrence count carried across folds until the host takes the report.
    pub(crate) fn fold_report(&mut self, slot: FaultSlot, message: String) {
        let occurrences = self
            .report
            .as_ref()
            .map_or(0u32, |report| report.occurrences)
            .saturating_add(1);
        self.report = Some(FaultReport {
            error: BlurError::DeviceOutOfMemory {
                source: message.into(),
            },
            slot,
            occurrences,
        });
    }

    /// Hands the folded report to the host and clears it; `None` means no reportable fault
    /// since the last take.
    pub(crate) fn take_report(&mut self) -> Option<FaultReport> {
        self.report.take()
    }
}

/// The log handle shared between the backend and its spawned pop-awaiting tasks. `Rc<RefCell>`
/// because the wasm runtime this path supports is single-threaded (the design excludes
/// atomics-wasm); native code only touches it in unit tests.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used only by the wasm32 deferred collector (lands with the web frame path)"
    )
)]
pub(crate) type SharedFaultLog = std::rc::Rc<std::cell::RefCell<FaultLog>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(slot_kind: FaultSlot, generation: u64, message: &str) -> PendingFault {
        PendingFault {
            slot_kind,
            generation,
            message: message.to_owned(),
        }
    }

    #[test]
    fn fold_report_is_latest_wins() {
        let mut log = FaultLog::default();
        log.fold_report(FaultSlot::Scratch, "first out of memory".to_owned());
        log.fold_report(
            FaultSlot::CompositePipeline,
            "second out of memory".to_owned(),
        );

        let report = log.take_report().expect("a report was folded");
        assert_eq!(report.slot, FaultSlot::CompositePipeline);
        assert_eq!(report.occurrences, 2);
        let source = std::error::Error::source(&report.error).expect("the flattened source");
        assert_eq!(source.to_string(), "second out of memory");
    }

    #[test]
    fn fold_report_saturates_the_occurrence_count() {
        let mut log = FaultLog::default();
        log.fold_report(FaultSlot::Scratch, "oom".to_owned());
        // Force the counter to the ceiling, then fold once more: it must not wrap.
        if let Some(report) = log.report.as_mut() {
            report.occurrences = u32::MAX;
        }
        log.fold_report(FaultSlot::Scratch, "oom again".to_owned());
        let report = log.take_report().expect("a report was folded");
        assert_eq!(report.occurrences, u32::MAX);
    }

    #[test]
    fn drain_where_partitions_and_keeps_the_rest() {
        let mut log = FaultLog::default();
        log.record(pending(FaultSlot::Scratch, 3, "scratch oom"));
        log.record(pending(FaultSlot::Intermediate, 4, "intermediate oom"));
        log.record(pending(FaultSlot::Pyramid, 5, "pyramid oom"));

        let drained = log.drain_where(|fault| fault.slot_kind != FaultSlot::Intermediate);
        assert_eq!(drained.len(), 2);
        assert!(
            drained
                .iter()
                .all(|f| f.slot_kind != FaultSlot::Intermediate)
        );

        let rest = log.drain_where(|_| true);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].slot_kind, FaultSlot::Intermediate);
        assert_eq!(rest[0].generation, 4);
        assert_eq!(rest[0].message, "intermediate oom");
    }

    #[test]
    fn shared_log_folds_through_any_handle() {
        let log = SharedFaultLog::default();
        let collector_handle = std::rc::Rc::clone(&log);
        collector_handle
            .borrow_mut()
            .fold_report(FaultSlot::BindGroup, "oom".to_owned());
        assert!(log.borrow_mut().take_report().is_some());
    }

    #[test]
    fn take_report_clears() {
        let mut log = FaultLog::default();
        log.fold_report(FaultSlot::UniformBuffer, "oom".to_owned());
        assert!(log.take_report().is_some());
        assert!(log.take_report().is_none());
    }
}
