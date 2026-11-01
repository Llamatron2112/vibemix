//! Continuous trim-learn controller.
//!
//! It keeps the post-trim peak under a target while boosting only up to the
//! headroom measured by a slow peak hold. The audio thread only accumulates the
//! peak; this logic runs on the control thread.

use std::time::Duration;

use vibemix_core::params::{linear_to_db, TRIM_MAX_DB, TRIM_MIN_DB};

use crate::params::{TRIM_LEARN_MAX, TRIM_LEARN_MIN, TRIM_LEARN_NONE};

/// How often the control thread consumes the peak hold.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Peak target. A little below full scale so rounding cannot trip a clip.
pub(super) const TARGET_DB: f32 = -0.1;
/// Below this the input is treated as silence; learn never boosts into noise.
const SIGNAL_FLOOR_DB: f32 = -60.0;
/// Do not boost while the held peak is within this band of the target.
const HYSTERESIS_DB: f32 = 0.25;
/// How long the signal must stay below the target before boosting.
const BOOST_HOLD: Duration = Duration::from_millis(200);
/// Peak-hold release rate: loud material keeps the boost limit conservative.
const RELEASE_DB_PER_SEC: f32 = 12.0;
const EPS: f32 = 0.005;

#[derive(Debug, Clone, Copy)]
pub(super) struct Decision {
    /// New trim in dB, when it changed.
    pub trim_db: Option<f32>,
    /// `TRIM_LEARN_NONE`, `TRIM_LEARN_MIN` or `TRIM_LEARN_MAX`.
    pub limit: u32,
}

/// Per-strip controller state.
#[derive(Debug, Clone)]
pub(super) struct TrimLearn {
    held_peak_db: f32,
    below_for: Duration,
}

impl Default for TrimLearn {
    fn default() -> Self {
        Self {
            held_peak_db: SIGNAL_FLOOR_DB,
            below_for: Duration::ZERO,
        }
    }
}

impl TrimLearn {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// `peak` is the maximum post-trim linear peak since the previous call and
    /// `current_gain` the trim currently applied. `dt` is the elapsed control
    /// interval.
    pub fn update(&mut self, peak: f32, current_gain: f32, dt: Duration) -> Decision {
        let dt = dt.min(Duration::from_secs(1));
        let current_db = linear_to_db(current_gain);
        let peak_db = if peak.is_finite() && peak > 0.0 {
            linear_to_db(peak)
        } else {
            SIGNAL_FLOOR_DB
        };

        // Slow release, then fold in the newest peak.
        let release = RELEASE_DB_PER_SEC * dt.as_secs_f32();
        self.held_peak_db = (self.held_peak_db - release).max(SIGNAL_FLOOR_DB);
        self.held_peak_db = self.held_peak_db.max(peak_db);

        if peak_db > TARGET_DB {
            let next = (current_db + (TARGET_DB - peak_db)).clamp(TRIM_MIN_DB, TRIM_MAX_DB);
            self.below_for = Duration::ZERO;
            let limit = if next <= TRIM_MIN_DB + 0.01 && peak_db > TARGET_DB + 0.05 {
                TRIM_LEARN_MIN
            } else {
                TRIM_LEARN_NONE
            };
            return Decision {
                trim_db: (next < current_db - EPS).then_some(next),
                limit,
            };
        }

        if peak_db <= SIGNAL_FLOOR_DB + 0.5 {
            // Silence: do not boost as gaps between notes would run the trim up.
            self.below_for = Duration::ZERO;
            return Decision {
                trim_db: None,
                limit: TRIM_LEARN_NONE,
            };
        }

        if self.held_peak_db < TARGET_DB - HYSTERESIS_DB {
            self.below_for += dt;
            if self.below_for >= BOOST_HOLD {
                let headroom = TARGET_DB - self.held_peak_db;
                let allowed = (current_db + headroom).min(TRIM_MAX_DB);
                // Proportional approach: fast when far away, gentle near the
                // target, and always capped by the detected headroom.
                let step = (headroom * 0.5).clamp(0.25, 6.0);
                let next = (current_db + step)
                    .min(allowed)
                    .clamp(TRIM_MIN_DB, TRIM_MAX_DB);
                let limit = if next >= TRIM_MAX_DB - 0.01
                    && self.held_peak_db < TARGET_DB - HYSTERESIS_DB
                {
                    TRIM_LEARN_MAX
                } else {
                    TRIM_LEARN_NONE
                };
                return Decision {
                    trim_db: (next > current_db + EPS).then_some(next),
                    limit,
                };
            }
        } else {
            self.below_for = Duration::ZERO;
        }

        Decision {
            trim_db: None,
            limit: TRIM_LEARN_NONE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibemix_core::params::db_to_linear;

    fn update(controller: &mut TrimLearn, peak_db: f32, current_db: f32, millis: u64) -> Decision {
        controller.update(
            db_to_linear(peak_db),
            db_to_linear(current_db),
            Duration::from_millis(millis),
        )
    }

    #[test]
    fn hot_peak_cuts_to_target() {
        let mut controller = TrimLearn::default();
        let decision = update(&mut controller, 6.0, 0.0, 50);
        let next = decision.trim_db.expect("cut");
        assert!((next - (TARGET_DB - 6.0)).abs() < 0.01, "{next}");
        assert_eq!(decision.limit, TRIM_LEARN_NONE);
    }

    #[test]
    fn silence_never_boosts() {
        let mut controller = TrimLearn::default();
        for _ in 0..50 {
            let decision = update(&mut controller, -120.0, 0.0, 100);
            assert!(decision.trim_db.is_none());
        }
    }

    #[test]
    fn boost_is_capped_by_held_headroom() {
        let mut controller = TrimLearn::default();
        // Raw source peak before the trim. The measured post-trim peak follows
        // the trim as it rises.
        let raw_db = -20.0;
        let mut current_db = 0.0;
        let mut last = 0.0;
        for _ in 0..100 {
            let measured = raw_db + current_db;
            let decision = update(&mut controller, measured, current_db, 500);
            if let Some(next) = decision.trim_db {
                assert!(next >= current_db);
                current_db = next;
                last = next;
            }
        }
        // The peak was measured at -20 dB, so the trim can never exceed the
        // headroom that would put it at the target.
        assert!(last <= TARGET_DB - raw_db + 1.0e-3, "{last}");
    }

    #[test]
    fn quiet_signal_waits_for_the_hold() {
        let mut controller = TrimLearn::default();
        for _ in 0..3 {
            assert!(update(&mut controller, -20.0, 0.0, 60).trim_db.is_none());
        }
        assert!(update(&mut controller, -20.0, 0.0, 60).trim_db.is_some());
    }

    #[test]
    fn boost_uses_half_the_headroom() {
        let mut controller = TrimLearn::default();
        let next = update(&mut controller, -20.0, 0.0, 1000)
            .trim_db
            .expect("boost");
        // 19.9 dB of headroom would be a 9.95 dB step, capped at 6 dB.
        assert!((next - 6.0).abs() < 1.0e-3, "{next}");
    }

    #[test]
    fn minimum_limit_is_reported() {
        let mut controller = TrimLearn::default();
        let decision = update(&mut controller, 10.0, -23.0, 50);
        assert_eq!(decision.limit, TRIM_LEARN_MIN);
        assert!(
            (decision.trim_db.expect("cut") - TRIM_MIN_DB).abs() < 1.0e-3,
            "{decision:?}"
        );
    }

    #[test]
    fn maximum_limit_is_reported() {
        let mut controller = TrimLearn::default();
        let decision = update(&mut controller, -50.0, TRIM_MAX_DB, 500);
        assert_eq!(decision.limit, TRIM_LEARN_MAX);
        assert!(decision.trim_db.is_none());
    }

    #[test]
    fn within_hysteresis_does_not_boost() {
        let mut controller = TrimLearn::default();
        let mut decision = update(&mut controller, TARGET_DB - 0.2, 0.0, 1000);
        assert!(decision.trim_db.is_none());
        assert_eq!(decision.limit, TRIM_LEARN_NONE);
        decision = update(&mut controller, TARGET_DB - 1.0, 0.0, 1000);
        assert!(decision.trim_db.is_some());
    }
}
