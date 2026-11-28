//! history — the two vitals rings and the fixed-step windows read from them.
//!
//! Coarse: 240 x 15 s, fed by every sample. Fine: 150 x 2 s, fed only while a `Full` tier is live.

use std::collections::VecDeque;

use super::model::{HistoryRange, HistorySeries, HistoryView, VitalsSample};

/// Width of a coarse slot (matches the idle sampling interval).
pub(crate) const COARSE_STEP_MS: u64 = 15_000;
/// Coarse slots kept: one hour.
pub(crate) const COARSE_SLOTS: usize = 240;
/// Width of a fine slot (matches the `Full` floor).
pub(crate) const FINE_STEP_MS: u64 = 2_000;
/// Fine slots kept: five minutes.
pub(crate) const FINE_SLOTS: usize = 150;

/// Series kept, in [`HistorySeries`] field order.
const SERIES: usize = 7;

/// The history values of one sample; `None` where the sample has no figure.
fn values_of(sample: &VitalsSample) -> [Option<f32>; SERIES] {
    let net = sample.network.as_ref();
    let bps = |v: Option<u64>| v.map(|v| v as f32);
    [
        Some(sample.cpu_percent),
        sample.cpu_system_percent,
        Some(sample.memory.used_fraction()),
        bps(net.and_then(|n| n.rx_bps)),
        bps(net.and_then(|n| n.tx_bps)),
        bps(sample.disk_read_bps),
        bps(sample.disk_write_bps),
    ]
}

// ─── Rings ────────────────────────────────────────────────────────────────────

/// A running mean.
#[derive(Debug, Clone, Copy, Default)]
struct Acc {
    sum: f64,
    count: u32,
}

impl Acc {
    fn add(&mut self, value: f64, count: u32) {
        self.sum += value;
        self.count += count;
    }

    fn mean(self) -> Option<f32> {
        (self.count > 0).then(|| (self.sum / f64::from(self.count)) as f32)
    }
}

/// One slot: the sums of every sample whose time falls in `[index * step, (index + 1) * step)`.
#[derive(Debug, Clone)]
struct Slot {
    index: u64,
    acc: [Acc; SERIES],
}

/// Slots aligned to wall-clock multiples of `step_ms`, newest last, at most `capacity` of them.
#[derive(Debug)]
struct Ring {
    step_ms: u64,
    capacity: usize,
    slots: VecDeque<Slot>,
}

impl Ring {
    fn new(step_ms: u64, capacity: usize) -> Self {
        Self {
            step_ms,
            capacity,
            slots: VecDeque::with_capacity(capacity),
        }
    }

    fn push(&mut self, unix_ms: u64, values: &[Option<f32>; SERIES]) {
        let index = unix_ms / self.step_ms;
        // The wall clock stepped backwards: the old slots no longer line up with "now".
        if self.slots.back().is_some_and(|back| back.index > index) {
            self.slots.clear();
        }
        if self.slots.back().is_none_or(|back| back.index != index) {
            self.slots.push_back(Slot {
                index,
                acc: [Acc::default(); SERIES],
            });
        }
        let slot = self.slots.back_mut().expect("a slot was just ensured");
        for (acc, value) in slot.acc.iter_mut().zip(values) {
            if let Some(value) = value.filter(|v| v.is_finite()) {
                acc.add(f64::from(value), 1);
            }
        }
        while self
            .slots
            .front()
            .is_some_and(|front| front.index + self.capacity as u64 <= index)
        {
            self.slots.pop_front();
        }
    }

    /// Add every slot selected by `takes(slot_start_ms)` into `out`, series by series.
    fn gather(&self, out: &mut [Acc; SERIES], takes: impl Fn(u64) -> bool) {
        for slot in &self.slots {
            if !takes(slot.index * self.step_ms) {
                continue;
            }
            for (acc, slot_acc) in out.iter_mut().zip(&slot.acc) {
                acc.add(slot_acc.sum, slot_acc.count);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

// ─── History ──────────────────────────────────────────────────────────────────

/// Both rings. Dropped and rebuilt empty when monitoring is disabled.
#[derive(Debug)]
pub(crate) struct History {
    coarse: Ring,
    fine: Ring,
}

impl Default for History {
    fn default() -> Self {
        Self {
            coarse: Ring::new(COARSE_STEP_MS, COARSE_SLOTS),
            fine: Ring::new(FINE_STEP_MS, FINE_SLOTS),
        }
    }
}

impl History {
    /// Record one sample: always into the coarse ring, into the fine ring when `fine`.
    pub(crate) fn record(&mut self, sample: &VitalsSample, fine: bool) {
        let values = values_of(sample);
        self.coarse.push(sample.unix_ms, &values);
        if fine {
            self.fine.push(sample.unix_ms, &values);
        }
    }

    /// Whether the fine ring holds anything.
    #[cfg(test)]
    pub(crate) fn has_fine_data(&self) -> bool {
        !self.fine.is_empty()
    }

    /// The `range` window whose newest slot holds `now_unix_ms`.
    ///
    /// Each slot is the mean of the samples in it. Ranges finer than the coarse step read the
    /// fine ring first (a 2 s fine slot counts toward the output slot holding its start, exact
    /// for 1M and within one fine slot for 5M); a series with no fine data in a slot is filled
    /// from the coarse slots overlapping it, which holds the coarse value across its sub-slots.
    /// Coarser ranges read the coarse ring alone, whose slots they align with. No data: NaN.
    pub(crate) fn view(&self, range: HistoryRange, now_unix_ms: u64, revision: u64) -> HistoryView {
        let step = range.step_ms();
        let n = range.slots();
        let end = (now_unix_ms / step + 1) * step;
        let start = end.saturating_sub(step * n as u64);
        let use_fine = step < COARSE_STEP_MS && !self.fine.is_empty();

        let mut columns: [Vec<f32>; SERIES] = std::array::from_fn(|_| Vec::with_capacity(n));
        for i in 0..n as u64 {
            let (a, b) = (start + i * step, start + (i + 1) * step);
            let mut fine = [Acc::default(); SERIES];
            if use_fine {
                self.fine.gather(&mut fine, |s| (a..b).contains(&s));
            }
            let mut coarse = [Acc::default(); SERIES];
            self.coarse
                .gather(&mut coarse, |s| s < b && s + COARSE_STEP_MS > a);
            for (column, (fine, coarse)) in columns.iter_mut().zip(fine.iter().zip(&coarse)) {
                column.push(fine.mean().or(coarse.mean()).unwrap_or(f32::NAN));
            }
        }

        let [
            cpu_total,
            cpu_system,
            memory_used_fraction,
            net_rx_bps,
            net_tx_bps,
            disk_read_bps,
            disk_write_bps,
        ] = columns;
        HistoryView {
            range,
            step_ms: step,
            end_unix_ms: end,
            revision,
            series: HistorySeries {
                cpu_total,
                cpu_system,
                memory_used_fraction,
                net_rx_bps,
                net_tx_bps,
                disk_read_bps,
                disk_write_bps,
            },
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::model::{Memory, NetInterface};

    /// A wall-clock instant aligned to every step (a multiple of 60 s).
    const BASE: u64 = 1_800_000_000_000;

    fn sample(unix_ms: u64, cpu: f32) -> VitalsSample {
        VitalsSample {
            unix_ms,
            cpu_percent: cpu,
            memory: Memory {
                total: 100,
                used: 25,
                cache: 25,
                free: 50,
                available: 75,
            },
            ..VitalsSample::default()
        }
    }

    fn assert_nan(values: &[f32]) {
        assert!(values.iter().all(|v| v.is_nan()), "{values:?}");
    }

    #[test]
    fn an_empty_history_is_all_nan_and_aligned() {
        let view = History::default().view(HistoryRange::FiveMinutes, BASE + 12_345, 0);
        assert_eq!(view.step_ms, 5_000);
        assert_eq!(view.end_unix_ms, BASE + 15_000);
        assert_eq!(view.series.cpu_total.len(), 60);
        assert_nan(&view.series.cpu_total);
        assert_nan(&view.series.disk_write_bps);
    }

    #[test]
    fn the_fine_ring_fills_only_when_asked() {
        let mut history = History::default();
        history.record(&sample(BASE, 10.0), false);
        assert!(!history.has_fine_data());
        history.record(&sample(BASE + 2_000, 10.0), true);
        assert!(history.has_fine_data());
    }

    #[test]
    fn one_minute_after_twenty_seconds_of_fine_data_has_ten_real_slots_then_nan() {
        let mut history = History::default();
        for k in 0..10u64 {
            history.record(&sample(BASE + k * 2_000, 10.0 + k as f32), true);
        }
        let view = history.view(HistoryRange::OneMinute, BASE + 18_500, 10);
        assert_eq!(view.end_unix_ms, BASE + 20_000);
        assert_eq!(view.revision, 10);
        let cpu = &view.series.cpu_total;
        assert_eq!(cpu.len(), 30);
        assert_nan(&cpu[..20]);
        let real: Vec<f32> = (0..10).map(|k| 10.0 + k as f32).collect();
        assert_eq!(&cpu[20..], &real[..]);
        assert!(
            view.series.memory_used_fraction[20..]
                .iter()
                .all(|&m| m == 0.25)
        );
        assert_nan(&view.series.cpu_system);
    }

    #[test]
    fn slots_without_fine_data_step_hold_the_coarse_value() {
        let mut history = History::default();
        // Idle sampling before the Full subscriber arrived: coarse only.
        history.record(&sample(BASE - 30_000, 50.0), false);
        history.record(&sample(BASE - 15_000, 70.0), false);
        for k in 0..10u64 {
            history.record(&sample(BASE + k * 2_000, 10.0 + k as f32), true);
        }
        let view = history.view(HistoryRange::OneMinute, BASE + 18_500, 12);
        let cpu = &view.series.cpu_total;
        // Slot k covers [BASE - 40 s + 2k s, +2 s).
        assert_nan(&cpu[..5]);
        assert!(cpu[5..12].iter().all(|&v| v == 50.0), "{cpu:?}");
        assert_eq!(
            cpu[12], 60.0,
            "a slot straddling two coarse slots averages them"
        );
        assert!(cpu[13..20].iter().all(|&v| v == 70.0), "{cpu:?}");
        assert_eq!(cpu[20], 10.0);
        assert_eq!(cpu[29], 19.0);
    }

    #[test]
    fn coarse_ranges_are_the_mean_of_the_samples_in_each_slot() {
        let mut history = History::default();
        history.record(&sample(BASE, 10.0), false);
        history.record(&sample(BASE + 5_000, 20.0), false);
        history.record(&sample(BASE + 10_000, 30.0), false);
        history.record(&sample(BASE + 15_000, 90.0), true);
        let view = history.view(HistoryRange::FifteenMinutes, BASE + 15_000, 4);
        let cpu = &view.series.cpu_total;
        assert_eq!(cpu[59], 90.0);
        assert_eq!(cpu[58], 20.0);
        assert_nan(&cpu[..58]);

        let hour = history.view(HistoryRange::OneHour, BASE + 15_000, 4);
        assert_eq!(hour.series.cpu_total[59], 37.5);
    }

    #[test]
    fn missing_series_stay_nan_while_present_ones_fill() {
        let mut history = History::default();
        let mut s = sample(BASE, 40.0);
        s.cpu_system_percent = Some(12.0);
        s.network = Some(NetInterface {
            name: "eth0".into(),
            address: None,
            rx_bps: None,
            tx_bps: Some(1_000),
            rx_total: 0,
            tx_total: 0,
        });
        history.record(&s, true);
        let view = history.view(HistoryRange::OneMinute, BASE, 1);
        let last = view.series.cpu_total.len() - 1;
        assert_eq!(view.series.cpu_system[last], 12.0);
        assert_eq!(view.series.net_tx_bps[last], 1_000.0);
        assert!(view.series.net_rx_bps[last].is_nan());
        assert!(view.series.disk_read_bps[last].is_nan());
    }

    #[test]
    fn slot_alignment_is_stable_across_calls_in_one_slot() {
        let mut history = History::default();
        for k in 0..5u64 {
            history.record(&sample(BASE + k * 2_000, k as f32), true);
        }
        for range in [
            HistoryRange::OneMinute,
            HistoryRange::FiveMinutes,
            HistoryRange::FifteenMinutes,
            HistoryRange::OneHour,
        ] {
            let now = BASE + 8_100;
            let first = history.view(range, now, 5);
            let second = history.view(range, now + 600, 5);
            assert_eq!(first.end_unix_ms % first.step_ms, 0);
            assert_eq!(first.end_unix_ms, second.end_unix_ms, "{range:?}");
            let bits = |v: &HistoryView| {
                v.series
                    .cpu_total
                    .iter()
                    .map(|f| f.to_bits())
                    .collect::<Vec<_>>()
            };
            assert_eq!(bits(&first), bits(&second), "{range:?}");
        }
    }

    #[test]
    fn rings_drop_slots_older_than_their_capacity() {
        let mut ring = Ring::new(FINE_STEP_MS, FINE_SLOTS);
        let one = [Some(1.0); SERIES];
        for k in 0..(FINE_SLOTS as u64 + 10) {
            ring.push(BASE + k * FINE_STEP_MS, &one);
        }
        assert_eq!(ring.slots.len(), FINE_SLOTS);
        // A gap longer than the ring empties all but the new slot.
        ring.push(BASE + 10_000 * FINE_STEP_MS, &one);
        assert_eq!(ring.slots.len(), 1);
    }

    #[test]
    fn a_clock_stepping_back_restarts_the_ring() {
        let mut ring = Ring::new(COARSE_STEP_MS, COARSE_SLOTS);
        let one = [Some(1.0); SERIES];
        ring.push(BASE + 60_000, &one);
        ring.push(BASE + 75_000, &one);
        ring.push(BASE, &one);
        assert_eq!(ring.slots.len(), 1);
        assert_eq!(ring.slots[0].index, BASE / COARSE_STEP_MS);
    }
}
