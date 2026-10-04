//! Clock recovery: map a sender's OMT timestamps onto a local clock without
//! drift, and resample audio so it follows the corrected timeline.
//!
//! A sender stamps frames with its own clock. A receiver presenting them
//! against its own clock (a GStreamer pipeline clock, an audio device) sees
//! the two run at slightly different rates - tens of ppm is normal, which is
//! over a second across an 8-hour show. Mapping timestamps with a fixed offset
//! then lets buffers drift steadily early or late until they are dropped or
//! pile up.
//!
//! [`ClockRecovery`] is a phase-locked loop on arrival times. Network delay
//! only ever *adds* to an arrival, so each one-second window's *minimum*
//! delay is a clean, jitter-free measurement of where the sender's clock is
//! relative to ours. A second-order loop steers the mapping's rate until that
//! minimum stays at its starting value: the recovered rate is the drift, and
//! the mapping no longer walks away. Video and audio share one instance, so
//! they stay in sync with each other while both follow the local clock.
//!
//! [`DriftResampler`] keeps audio continuous through that correction: it
//! stretches or squeezes each buffer by the (ppm-scale) ratio between the
//! corrected timeline and the sender's sample count, so audio is never
//! clipped or gapped to catch up.

use crate::protocol::TICKS_PER_SECOND;

/// Measurement window: one second of sender time.
const WINDOW_TICKS: i64 = TICKS_PER_SECOND;
/// Loop gains, per one-second window: a phase error is worked off over
/// roughly 30 s, and the integrator (the learned drift) is critically damped.
const KP: f64 = 1.0 / 30.0;
const KI: f64 = KP * KP / 4.0;
/// Real clocks differ by tens of ppm; anything past this is a step, not drift.
const MAX_DRIFT: f64 = 1e-3;
/// A timestamp jump this large (either way) restarts the loop: the sender
/// restarted or its clock was reset.
const RESET_JUMP_NS: f64 = 2e9;

/// Maps sender timestamps (OMT ticks) to local nanoseconds, correcting drift.
#[derive(Debug, Clone, Default)]
pub struct ClockRecovery {
    state: Option<Loop>,
}

#[derive(Debug, Clone)]
struct Loop {
    anchor_ts: i64,
    anchor_ns: f64,
    /// Local ns per sender ns. 1 + drift.
    rate: f64,
    integrator: f64,
    window_start: i64,
    window_min: f64,
    window_sum: f64,
    window_n: u32,
    /// The minimum delay of the first window; the loop holds it there.
    target: Option<f64>,
    last_phase: f64,
    last_mean_delay: f64,
}

impl ClockRecovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that the frame stamped `ts` arrived at local time `arrival_ns`
    /// and returns the local time to present it at. The first call fixes the
    /// mapping's origin at that arrival.
    pub fn map(&mut self, ts: i64, arrival_ns: i64) -> i64 {
        let arrival = arrival_ns as f64;
        if let Some(l) = &self.state {
            let mapped = l.anchor_ns + (ts - l.anchor_ts) as f64 * 100.0 * l.rate;
            if (arrival - mapped).abs() > RESET_JUMP_NS {
                self.state = None;
            }
        }
        let l = self.state.get_or_insert(Loop {
            anchor_ts: ts,
            anchor_ns: arrival,
            rate: 1.0,
            integrator: 0.0,
            window_start: ts,
            window_min: f64::MAX,
            window_sum: 0.0,
            window_n: 0,
            target: None,
            last_phase: 0.0,
            last_mean_delay: 0.0,
        });
        let mapped = l.anchor_ns + (ts - l.anchor_ts) as f64 * 100.0 * l.rate;
        let delay = arrival - mapped;
        l.window_min = l.window_min.min(delay);
        l.window_sum += delay;
        l.window_n += 1;

        if ts - l.window_start >= WINDOW_TICKS {
            let target = *l.target.get_or_insert(l.window_min);
            // Positive: frames arrive later than the mapping predicts, i.e.
            // the local clock is running faster than the sender's, so local
            // time per sender tick must grow.
            let phase_s = (l.window_min - target) / 1e9;
            l.integrator = (l.integrator + KI * phase_s).clamp(-MAX_DRIFT, MAX_DRIFT);
            let new_rate =
                1.0 + (l.integrator + KP * phase_s).clamp(-2.0 * MAX_DRIFT, 2.0 * MAX_DRIFT);
            // Re-anchor at this frame so the mapping stays continuous.
            l.anchor_ns = mapped;
            l.anchor_ts = ts;
            l.rate = new_rate;
            l.last_phase = l.window_min - target;
            l.last_mean_delay = l.window_sum / l.window_n.max(1) as f64 - target;
            l.window_start = ts;
            l.window_min = f64::MAX;
            l.window_sum = 0.0;
            l.window_n = 0;
        }
        mapped.round() as i64
    }

    /// The local time for `ts` under the current mapping, without recording
    /// an arrival. `None` before the first [`map`](Self::map).
    pub fn predict(&self, ts: i64) -> Option<i64> {
        self.state
            .as_ref()
            .map(|l| (l.anchor_ns + (ts - l.anchor_ts) as f64 * 100.0 * l.rate).round() as i64)
    }

    /// Measured drift of the local clock against the sender's, in ppm
    /// (positive: local runs fast). Settles within a few minutes; it is the
    /// loop's integrator, so network jitter cannot push it around.
    pub fn drift_ppm(&self) -> f64 {
        self.state.as_ref().map_or(0.0, |l| l.integrator * 1e6)
    }

    /// The rate correction being applied right now, in ppm: the settled
    /// drift plus the loop's current phase correction. Follows a change in
    /// seconds, where [`drift_ppm`](Self::drift_ppm) takes minutes.
    pub fn correction_ppm(&self) -> f64 {
        self.state.as_ref().map_or(0.0, |l| (l.rate - 1.0) * 1e6)
    }

    /// How far the minimum arrival delay currently sits from where the loop
    /// holds it, in ms. Near zero once locked.
    pub fn phase_error_ms(&self) -> f64 {
        self.state.as_ref().map_or(0.0, |l| l.last_phase / 1e6)
    }

    /// Mean arrival delay over the last window, beyond the minimum: the
    /// network jitter a presenting pipeline must absorb, in ms.
    pub fn jitter_ms(&self) -> f64 {
        self.state.as_ref().map_or(0.0, |l| l.last_mean_delay / 1e6)
    }

    pub fn reset(&mut self) {
        self.state = None;
    }
}

/// Audio through drift correction: turns sender buffers (planar f32, the
/// sender's sample clock) into interleaved buffers whose sample counts match
/// the corrected local timeline exactly, with continuous timestamps.
#[derive(Debug, Clone, Default)]
pub struct DriftResampler {
    stream: Option<Stream>,
}

#[derive(Debug, Clone)]
struct Stream {
    rate: i32,
    channels: usize,
    origin_ns: i64,
    produced: u64,
    next_ts: i64,
    /// Last input sample per channel, for interpolating across buffers.
    last: Vec<f32>,
}

/// One resampled buffer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResampledAudio {
    /// Local presentation time of the first sample, ns.
    pub pts_ns: i64,
    /// Interleaved samples, `frames * channels` long.
    pub interleaved: Vec<f32>,
    pub frames: usize,
    /// True when the stream (re)started here - a gap, a format change or the
    /// first buffer - so the consumer should treat it as a discontinuity.
    pub discont: bool,
}

impl DriftResampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resamples one sender buffer stamped `ts` (OMT ticks), arriving at
    /// `arrival_ns`, using (and feeding) the shared `clock`.
    pub fn process(
        &mut self,
        clock: &mut ClockRecovery,
        planar: &[f32],
        rate: i32,
        channels: usize,
        ts: i64,
        arrival_ns: i64,
    ) -> ResampledAudio {
        let channels = channels.max(1);
        let spc = planar.len() / channels;
        let start_ns = clock.map(ts, arrival_ns);
        if spc == 0 || rate <= 0 {
            return ResampledAudio {
                pts_ns: start_ns,
                ..Default::default()
            };
        }
        let end_ts = ts + (spc as i64 * TICKS_PER_SECOND) / rate as i64;

        let continuous = self.stream.as_ref().is_some_and(|s| {
            s.rate == rate
                && s.channels == channels
                && (ts - s.next_ts).abs() < TICKS_PER_SECOND / 10
        });
        let discont = !continuous;
        if discont {
            self.stream = Some(Stream {
                rate,
                channels,
                origin_ns: start_ns,
                produced: 0,
                next_ts: ts,
                last: (0..channels).map(|c| planar[c * spc]).collect(),
            });
        }
        let s = self.stream.as_mut().unwrap();
        let end_ns = clock.predict(end_ts).unwrap_or(start_ns);
        let target_total = (((end_ns - s.origin_ns) as f64) * rate as f64 / 1e9)
            .round()
            .max(0.0) as u64;
        let mut frames = target_total.saturating_sub(s.produced) as usize;
        // Drift is ppm-scale; anything else means the mapping stepped.
        // Pass the buffer through unchanged rather than distort it.
        if frames.abs_diff(spc) > spc / 50 + 2 {
            frames = spc;
        }

        let mut out = vec![0f32; frames * channels];
        if frames > 0 {
            // Inputs y_0 = previous buffer's last sample, y_1..=y_spc = this
            // buffer; output k samples position (k + 1) * step, which lands
            // exactly on y_spc for the last output when frames == spc.
            let step = spc as f64 / frames as f64;
            for c in 0..channels {
                let plane = &planar[c * spc..(c + 1) * spc];
                let y = |i: usize| if i == 0 { s.last[c] } else { plane[i - 1] };
                for k in 0..frames {
                    let pos = (k + 1) as f64 * step;
                    let i = (pos.floor() as usize).min(spc);
                    let frac = (pos - i as f64) as f32;
                    let a = y(i);
                    let b = if i < spc { y(i + 1) } else { a };
                    out[k * channels + c] = a + (b - a) * frac;
                }
                s.last[c] = plane[spc - 1];
            }
        }
        let pts_ns = s.origin_ns + (s.produced as f64 * 1e9 / rate as f64).round() as i64;
        s.produced += frames as u64;
        s.next_ts = end_ts;
        ResampledAudio {
            pts_ns,
            interleaved: out,
            frames,
            discont,
        }
    }

    pub fn reset(&mut self) {
        self.stream = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random jitter in [0, max_ms) ms.
    fn jitter(seed: &mut u64, max_ms: f64) -> f64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*seed >> 33) as f64 / (1u64 << 31) as f64) * max_ms * 1e6
    }

    /// Eight simulated hours of 30 fps video from a sender whose clock is
    /// `ppm` slow against ours, through 0-8 ms of network jitter.
    fn run(ppm: f64) -> (ClockRecovery, f64, f64) {
        let mut clock = ClockRecovery::new();
        let mut seed = 7;
        let frame_ticks = TICKS_PER_SECOND / 30;
        let base_delay_ns = 3e6;
        let mut worst_late_after_lock = 0f64;
        let mut last_offset = 0f64;
        let frames = 8 * 3600 * 30;
        for n in 0..frames {
            let ts = 1_000_000_000_000 + n as i64 * frame_ticks;
            // Our clock reads sender time * (1 + ppm).
            let true_ns = (n as f64 * frame_ticks as f64 * 100.0) * (1.0 + ppm * 1e-6);
            let arrival = true_ns + base_delay_ns + jitter(&mut seed, 8.0);
            let mapped = clock.map(ts, arrival as i64) as f64;
            // How far presentation time sits from the jitter-free arrival.
            let offset = (true_ns + base_delay_ns) - mapped;
            if n > 10 * 60 * 30 {
                worst_late_after_lock = worst_late_after_lock.max(offset.abs());
            }
            last_offset = offset;
        }
        (clock, worst_late_after_lock, last_offset)
    }

    #[test]
    fn locks_to_drift_and_holds_for_eight_hours() {
        for ppm in [0.0, 50.0, -100.0, 300.0] {
            let (clock, worst, last) = run(ppm);
            assert!(
                (clock.drift_ppm() - ppm).abs() < 2.0,
                "{ppm} ppm measured as {:.2}",
                clock.drift_ppm()
            );
            // After lock the mapping never wanders more than a few ms from
            // the clean arrival line - nowhere near a frame (33 ms), let alone
            // the 50 ppm x 8 h = 1.44 s an uncorrected mapping would drift.
            assert!(worst < 5e6, "{ppm} ppm: worst offset {:.2} ms", worst / 1e6);
            assert!(
                last.abs() < 5e6,
                "{ppm} ppm: final offset {:.2} ms",
                last / 1e6
            );
        }
    }

    #[test]
    fn a_timestamp_jump_restarts_the_loop() {
        let mut clock = ClockRecovery::new();
        assert_eq!(clock.map(0, 1_000), 1_000);
        clock.map(TICKS_PER_SECOND, 1_000_001_000);
        // Sender restarted: timestamps go back to near zero, arrivals carry on.
        let t = clock.map(5, 5_000_000_000);
        assert_eq!(t, 5_000_000_000);
    }

    /// Audio and video from one sender, through one clock, for eight
    /// simulated hours with drift: the audio sample count must track the
    /// video timeline to within a frame (A/V sync), and audio timestamps must
    /// be continuous.
    #[test]
    fn audio_stays_with_video_for_eight_hours() {
        let ppm = 80.0;
        let mut clock = ClockRecovery::new();
        let mut resampler = DriftResampler::new();
        let mut seed = 11;
        let rate = 48_000;
        let spc = 1600; // 30 buffers per second
        let buf_ticks = (spc as i64 * TICKS_PER_SECOND) / rate as i64;
        let planar: Vec<f32> = (0..spc * 2).map(|i| (i % 7) as f32 / 7.0).collect();
        let mut expected_pts: Option<i64> = None;
        let mut worst_av = 0f64;
        for n in 0..(8 * 3600 * 30) {
            let ts = n as i64 * buf_ticks;
            let true_ns = (ts as f64 * 100.0) * (1.0 + ppm * 1e-6);
            let arrival = (true_ns + 2e6 + jitter(&mut seed, 6.0)) as i64;
            // Video frame with the same timestamp, through the same clock.
            let video_pts = clock.map(ts, arrival);
            let out = resampler.process(&mut clock, &planar, rate, 2, ts, arrival);
            if let Some(exp) = expected_pts {
                assert!(!out.discont, "unexpected discontinuity at buffer {n}");
                assert!(
                    (out.pts_ns - exp).abs() <= 1,
                    "audio timestamps not continuous at {n}"
                );
            }
            expected_pts =
                Some(out.pts_ns + (out.frames as f64 * 1e9 / rate as f64).round() as i64);
            // A/V: where the audio for this timestamp starts vs the video.
            worst_av = worst_av.max((out.pts_ns - video_pts).abs() as f64);
        }
        let frame_ns = 1e9 / 30.0;
        assert!(worst_av < frame_ns, "A/V drifted {:.2} ms", worst_av / 1e6);
        assert!((clock.drift_ppm() - ppm).abs() < 2.0);
    }

    #[test]
    fn resampling_at_unity_reproduces_the_input() {
        let mut clock = ClockRecovery::new();
        let mut r = DriftResampler::new();
        let planar: Vec<f32> = (0..960).map(|i| i as f32).collect(); // 2 ch x 480
        let first = r.process(&mut clock, &planar, 48_000, 2, 0, 0);
        assert_eq!(first.frames, 480);
        // Interleaved, unchanged.
        assert_eq!(&first.interleaved[..4], &[0.0, 480.0, 1.0, 481.0]);
        assert!(first.discont);
    }
}
