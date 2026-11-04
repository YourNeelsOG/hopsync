//! Lane auto-tune (Phase 1 step 14): add SSH connections while each one still raises throughput.
//!
//! Pure decisions, fed one throughput sample per interval, so the rules are unit tested apart from
//! the network. The rate climbs for seconds after a start or a new lane while TCP and SSH windows
//! open, so a lane count is judged only at its plateau: two samples in a row that no longer rise by
//! `GAIN`. Each plateau is compared with the previous one; a lane that adds less than `GAIN` stops
//! the tuning for the rest of the transfer, because the link, the disk or a server-side cap is
//! then the limit and more connections only load the server.

/// The smallest rise that counts: between samples (still ramping) and between plateaus (the last
/// lane helped).
pub const GAIN: f64 = 1.10;

#[derive(Debug)]
pub struct Tuner {
    /// Plateau of the previous lane count, in bytes per second.
    baseline: Option<f64>,
    /// Last sample at the current lane count.
    prev: Option<f64>,
    active: usize,
    max: usize,
    settled: bool,
}

impl Tuner {
    /// Starts at `active` lanes and never goes past `max`.
    pub fn new(active: usize, max: usize) -> Self {
        Self {
            baseline: None,
            prev: None,
            active,
            max,
            settled: false,
        }
    }

    pub fn active(&self) -> usize {
        self.active
    }

    /// No more lanes will be added.
    pub fn settled(&self) -> bool {
        self.settled || self.active >= self.max
    }

    /// Takes the throughput measured since the last call (the caller skips the interval in which
    /// a new lane connects). Returns true when one more lane should be added.
    pub fn observe(&mut self, rate: f64) -> bool {
        // Nothing moved (a tree walk, a slow listing): no evidence either way.
        if self.settled() || rate <= 0.0 {
            return false;
        }
        match self.prev.replace(rate) {
            // First sample at this lane count, or still ramping up.
            None => return false,
            Some(prev) if rate > prev * GAIN => return false,
            Some(_) => {}
        }
        if self.baseline.is_some_and(|b| rate < b * GAIN) {
            self.settled = true;
            return false;
        }
        self.baseline = Some(rate);
        self.prev = None;
        self.active += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `rate(active lanes)` for `n` intervals; returns how many lanes were added.
    fn run(t: &mut Tuner, n: usize, rate: impl Fn(usize) -> f64) -> usize {
        (0..n).filter(|_| t.observe(rate(t.active()))).count()
    }

    #[test]
    fn adds_lanes_while_each_one_helps_then_stops() {
        let mut t = Tuner::new(4, 16);
        // 10 MB/s per lane up to a 60 MB/s cap.
        let added = run(&mut t, 30, |n| (n as f64 * 10e6).min(60e6));
        // 4 -> 5 -> 6 helped; the 7th lane brought nothing, so it stays at 7 and stops.
        assert_eq!((added, t.active()), (3, 7));
        assert!(t.settled());
        assert!(!t.observe(1e9), "settled tuning never adds again");
    }

    #[test]
    fn waits_for_ramp_up_before_judging() {
        // TCP and SSH windows open over the first seconds: the rate climbs with no lane added.
        let mut t = Tuner::new(4, 16);
        for rate in [8e6, 20e6, 35e6, 40e6] {
            assert!(!t.observe(rate), "still rising at {rate}");
        }
        assert!(t.observe(41e6), "plateau reached: try one more lane");
        // The extra lane ramps too; its plateau is judged against 41 MB/s, not the early samples.
        assert!(!t.observe(30e6));
        assert!(!t.observe(42e6));
        assert!(!t.observe(43e6));
        assert!(t.settled(), "42-43 MB/s is under a 10 % gain");
        assert_eq!(t.active(), 5);
    }

    #[test]
    fn a_throttle_settles_instead_of_adding() {
        let mut t = Tuner::new(4, 16);
        assert!(!t.observe(40e6));
        assert!(t.observe(40e6));
        // The provider throttles: lower with more lanes.
        assert!(!t.observe(16e6));
        assert!(!t.observe(16e6));
        assert!(t.settled());
    }

    #[test]
    fn never_exceeds_the_ceiling() {
        let mut t = Tuner::new(2, 3);
        assert!(!t.observe(1e6));
        assert!(t.observe(1e6));
        assert!(!t.observe(1e9));
        assert_eq!(t.active(), 3);
        assert!(Tuner::new(4, 4).settled());
    }

    #[test]
    fn idle_intervals_are_not_evidence() {
        let mut t = Tuner::new(1, 8);
        assert!(!t.observe(0.0));
        assert!(!t.observe(5e6));
        assert!(!t.observe(0.0));
        assert!(t.observe(5e6));
        assert_eq!(t.active(), 2);
    }

    #[test]
    fn a_small_gain_settles() {
        let mut t = Tuner::new(1, 8);
        assert_eq!(run(&mut t, 2, |_| 100e6), 1);
        assert_eq!(run(&mut t, 2, |_| 105e6), 0);
        assert!(t.settled(), "5 % is below the gain threshold");
        assert_eq!(t.active(), 2);
    }
}
