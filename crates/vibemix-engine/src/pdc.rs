//! Automatic delay compensation for sends and buses.
//!
//! Manual send delays alone cannot keep parallel paths aligned: a send into a
//! bus with latency-inducing inserts comes back late next to a dry send. This
//! module computes an extra delay per send so every path that reaches the same
//! destination has the same latency, and reports how much each destination is
//! delayed for JACK port-latency reporting and the recorder.
//!
//! The plan is recomputed on the audio thread, so it only writes into
//! preallocated vectors and never allocates.

/// Preallocated compensation plan, indexed by strip and bus.
pub struct PdcPlan {
    /// Extra delay (frames) added to each send on top of its manual delay.
    send_delay: Vec<Vec<u32>>,
    /// Latency (frames) at each bus input, before that bus's own inserts.
    bus_input: Vec<u32>,
    /// Delay (frames) applied to launcher playback before the master bus, so
    /// it stays aligned with compensated sends.
    master_input: u32,
    /// Per-bus local alignment target used while a plan is computed.
    local: Vec<f32>,
    /// True when a path could not be fully aligned because it exceeded the
    /// maximum delay the send lines can hold.
    limited: bool,
}

impl PdcPlan {
    pub fn new(strips: usize, buses: usize) -> Self {
        Self {
            send_delay: (0..strips).map(|_| vec![0; buses]).collect(),
            bus_input: vec![0; buses],
            master_input: 0,
            local: vec![0.0; buses],
            limited: false,
        }
    }

    /// Recomputes the plan from the current insert latencies and send setup.
    ///
    /// `send_manual_frames` is the user's alignment delay converted to frames;
    /// it is part of each path, so a manual delay shifts the whole destination
    /// rather than being overwritten. `enabled` turns automatic compensation
    /// off while still computing the real latency used for JACK reporting and
    /// the recorder. `max_frames` is the longest delay a send line can hold
    /// (the manual-delay capacity); longer compensated paths are clamped and
    /// `limited` is set.
    #[allow(clippy::too_many_arguments)]
    pub fn update<G, P, M>(
        &mut self,
        strip_pre: &[u32],
        strip_post: &[u32],
        bus_chain: &[u32],
        bus_active: &[bool],
        bus_mix_master: &[bool],
        send_gain: G,
        send_pre_fader: P,
        send_manual_frames: M,
        enabled: bool,
        max_frames: u32,
    ) where
        G: Fn(usize, usize) -> f32,
        P: Fn(usize, usize) -> bool,
        M: Fn(usize, usize) -> f32,
    {
        let strips = self.send_delay.len();
        let buses = self.bus_input.len();
        let max = max_frames as f32;
        let mut limited = false;

        for row in &mut self.send_delay {
            row.fill(0);
        }
        self.bus_input.fill(0);

        let reaches_master =
            |bus: usize| bus == 0 || bus_mix_master.get(bus).copied().unwrap_or(false);
        let active = |bus: usize| bus_active.get(bus).copied().unwrap_or(false);
        let tap = |strip: usize, bus: usize| -> f32 {
            let pre = strip_pre.get(strip).copied().unwrap_or(0) as f32;
            if send_pre_fader(strip, bus) {
                pre
            } else {
                pre + strip_post.get(strip).copied().unwrap_or(0) as f32
            }
        };

        // First pass: the slowest path through each destination. For buses
        // that reach master the target is global (including the inserts of
        // intermediate buses); for hardware-only buses it is local, so a large
        // master path cannot needlessly delay a separate monitor output. The
        // local maximum is computed for every bus too: with compensation off it
        // is the real latency each bus output carries.
        let mut master_target = 0.0f32;
        self.local.fill(0.0);
        for strip in 0..strips {
            for bus in 0..buses {
                if !active(bus) || send_gain(strip, bus) <= 0.0 {
                    continue;
                }
                let requested = send_manual_frames(strip, bus);
                if enabled && requested > max {
                    limited = true;
                }
                let manual = requested.clamp(0.0, max);
                let tap = tap(strip, bus);
                self.local[bus] = self.local[bus].max(tap + manual);
                if reaches_master(bus) {
                    let upstream = if bus != 0 {
                        bus_chain.get(bus).copied().unwrap_or(0) as f32
                    } else {
                        0.0
                    };
                    master_target = master_target.max(tap + manual + upstream);
                }
            }
        }
        let compensation_target = if enabled {
            if master_target > max {
                limited = true;
            }
            master_target.min(max)
        } else {
            master_target
        };
        if enabled && self.local.iter().any(|value| *value > max) {
            limited = true;
        }

        // Second pass: give each send enough delay to reach the target. The
        // target is the latency at the destination input, so subtract the
        // source tap and any intermediate bus inserts (the destination's own
        // inserts, and the master's, happen after all sends are summed).
        if enabled {
            for strip in 0..strips {
                for bus in 0..buses {
                    if !active(bus) || send_gain(strip, bus) <= 0.0 {
                        continue;
                    }
                    let manual = send_manual_frames(strip, bus).clamp(0.0, max);
                    let tap = tap(strip, bus);
                    let target = if reaches_master(bus) {
                        compensation_target
                    } else {
                        self.local[bus]
                    };
                    let upstream = if reaches_master(bus) && bus != 0 {
                        bus_chain.get(bus).copied().unwrap_or(0) as f32
                    } else {
                        0.0
                    };
                    let needed = target - tap - upstream;
                    if needed < 0.0 {
                        // The path is already slower than the target (its
                        // inserts run before its own output); it cannot be
                        // pulled back.
                        limited = true;
                    }
                    let comp = (needed.max(0.0) - manual).max(0.0);
                    let comp = comp.min((max - manual).max(0.0));
                    if comp >= (max - manual).max(0.0) && needed > manual + comp {
                        limited = true;
                    }
                    self.send_delay[strip][bus] = comp.ceil() as u32;
                }
            }
        }

        self.master_input = if enabled {
            compensation_target.ceil() as u32
        } else {
            0
        };
        for bus in 0..buses {
            let value = if !active(bus) {
                0.0
            } else if !enabled {
                // No compensation: report the real (slowest) path. Bus 0 is
                // the master output, so it includes intermediate bus inserts.
                if bus == 0 {
                    master_target
                } else {
                    self.local[bus]
                }
            } else if reaches_master(bus) {
                // Sends are delayed to the master target before crossing their
                // own inserts, so a bus with more insert latency needs less
                // delay at its input. The bus output (input + inserts) then
                // lands on the same master target as every other path.
                let upstream = if bus != 0 {
                    bus_chain.get(bus).copied().unwrap_or(0) as f32
                } else {
                    0.0
                };
                (compensation_target - upstream).max(0.0)
            } else {
                self.local[bus].min(max)
            };
            self.bus_input[bus] = value.ceil() as u32;
        }
        self.limited = limited;
    }

    /// Extra delay for one send, in frames.
    pub fn send_delay(&self, strip: usize, bus: usize) -> u32 {
        self.send_delay
            .get(strip)
            .and_then(|row| row.get(bus))
            .copied()
            .unwrap_or(0)
    }

    /// Latency at a bus input, before its own insert chain, in frames.
    pub fn bus_input(&self, bus: usize) -> u32 {
        self.bus_input.get(bus).copied().unwrap_or(0)
    }

    /// Delay applied to launcher playback before the master bus, in frames.
    pub fn master_input(&self) -> u32 {
        self.master_input
    }

    /// Whether any path exceeded the delay-line capacity.
    pub fn limited(&self) -> bool {
        self.limited
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(
        strip_pre: [u32; 2],
        strip_post: [u32; 2],
        bus_chain: [u32; 3],
        bus_mix_master: [bool; 3],
        sends: &[(usize, usize, bool, f32, u32)],
        max: u32,
    ) -> PdcPlan {
        let mut plan = PdcPlan::new(2, 3);
        plan.update(
            &strip_pre,
            &strip_post,
            &bus_chain,
            &[true, true, true],
            &bus_mix_master,
            |strip, bus| {
                sends
                    .iter()
                    .find(|send| send.0 == strip && send.1 == bus)
                    .map(|send| send.3)
                    .unwrap_or(0.0)
            },
            |strip, bus| {
                sends
                    .iter()
                    .find(|send| send.0 == strip && send.1 == bus)
                    .map(|send| send.2)
                    .unwrap_or(false)
            },
            |strip, bus| {
                sends
                    .iter()
                    .find(|send| send.0 == strip && send.1 == bus)
                    .map(|send| send.4 as f32)
                    .unwrap_or(0.0)
            },
            true,
            max,
        );
        plan
    }

    #[test]
    fn dry_path_is_delayed_to_match_a_latent_bus() {
        // Strip 0's post-fader tap has 100 frames, bus 1 has 50 frames. The
        // dry send to master is 150 frames early and must be delayed.
        let plan = plan(
            [0, 0],
            [100, 0],
            [0, 50, 0],
            [true, true, false],
            &[(0, 0, false, 1.0, 0), (0, 1, false, 1.0, 0)],
            48_000,
        );
        assert_eq!(plan.master_input(), 150);
        // The dry send's own tap is 100 frames, so it needs 50 more to match
        // the 100 + 50-frame wet path.
        assert_eq!(plan.send_delay(0, 0), 50);
        assert_eq!(plan.send_delay(0, 1), 0);
        assert_eq!(plan.bus_input(0), 150);
        // Bus 1's 50-frame insert is already part of the 150-frame master
        // target, so its input only needs the remaining 100.
        assert_eq!(plan.bus_input(1), 100);
        assert_eq!(plan.bus_input(1) + 50, plan.master_input());
        assert!(!plan.limited());
    }

    #[test]
    fn pre_fader_send_uses_only_the_pre_chain() {
        let plan = plan(
            [30, 0],
            [0, 100],
            [0, 0, 0],
            [true, false, false],
            &[(0, 0, true, 1.0, 0)],
            48_000,
        );
        // The only path has a 30-frame tap, so it defines the target itself.
        assert_eq!(plan.master_input(), 30);
        assert_eq!(plan.send_delay(0, 0), 0);
    }

    #[test]
    fn manual_delay_is_part_of_the_path() {
        // Strip 1 is 60 frames earlier than strip 0's manual 60-frame delay.
        let plan = plan(
            [0, 0],
            [0, 0],
            [0, 0, 0],
            [true, false, false],
            &[(0, 0, false, 1.0, 60), (1, 0, false, 1.0, 0)],
            48_000,
        );
        assert_eq!(plan.master_input(), 60);
        assert_eq!(plan.send_delay(0, 0), 0);
        assert_eq!(plan.send_delay(1, 0), 60);
    }

    #[test]
    fn disabled_plan_keeps_real_latency_but_adds_no_delay() {
        let mut plan = PdcPlan::new(2, 3);
        plan.update(
            &[0, 0],
            &[100, 0],
            &[0, 50, 0],
            &[true, true, true],
            &[true, true, false],
            |strip, bus| {
                if strip == 0 && (bus == 0 || bus == 1) {
                    1.0
                } else {
                    0.0
                }
            },
            |_, _| false,
            |_, _| 0.0,
            false,
            48_000,
        );
        assert_eq!(plan.master_input(), 0);
        assert_eq!(plan.send_delay(0, 0), 0);
        assert_eq!(plan.send_delay(0, 1), 0);
        // JACK/recorder still see the actual paths.
        assert_eq!(plan.bus_input(0), 150);
        assert_eq!(plan.bus_input(1), 100);
        assert_eq!(plan.bus_input(1) + 50, 150);
        assert!(!plan.limited());
    }

    #[test]
    fn hardware_bus_is_aligned_locally_not_to_master() {
        // Bus 2 does not mix to master. Its sends align among themselves; the
        // global master path (1000 frames) must not delay it.
        let plan = plan(
            [0, 0],
            [0, 0],
            [0, 0, 0],
            [true, false, false],
            &[
                (0, 0, false, 1.0, 0),
                (1, 0, false, 1.0, 1_000),
                (0, 2, false, 1.0, 10),
                (1, 2, false, 1.0, 0),
            ],
            48_000,
        );
        assert_eq!(plan.master_input(), 1_000);
        assert_eq!(plan.bus_input(2), 10);
        assert_eq!(plan.send_delay(0, 2), 0);
        assert_eq!(plan.send_delay(1, 2), 10);
    }

    #[test]
    fn paths_beyond_the_send_line_capacity_are_reported() {
        let plan = plan(
            [0, 0],
            [0, 0],
            [0, 0, 0],
            [true, false, false],
            &[(0, 0, false, 1.0, 2_000)],
            1_000,
        );
        assert_eq!(plan.master_input(), 1_000);
        assert!(plan.limited());
        // The manual delay alone already exceeds the line; no extra delay is
        // added, and the send line clamps the manual value itself.
        assert_eq!(plan.send_delay(0, 0), 0);
    }

    #[test]
    fn inactive_buses_are_ignored() {
        let mut plan = PdcPlan::new(1, 2);
        plan.update(
            &[0],
            &[0],
            &[0, 0],
            &[true, false],
            &[true, true],
            |_, _| 1.0,
            |_, _| false,
            |_, _| 0.0,
            true,
            48_000,
        );
        assert_eq!(plan.master_input(), 0);
        assert_eq!(plan.bus_input(1), 0);
    }

    #[test]
    fn zero_gain_sends_do_not_define_latency() {
        let plan = plan(
            [0, 0],
            [0, 500],
            [0, 0, 0],
            [true, false, false],
            &[(0, 0, false, 1.0, 0), (0, 1, false, 1.0, 0)],
            48_000,
        );
        // The bus-1 send is silent, so only the dry path counts.
        assert_eq!(plan.master_input(), 0);
    }
}
