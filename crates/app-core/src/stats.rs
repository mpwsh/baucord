//! Stats view-model helpers shared by every shell.

/// Live stream statistics for the sharer-side stats block.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamStats {
    pub quality: String,
    pub avg_frame_kb: f32,
    pub encode_ms: f32,
    pub viewers: u32,
    /// 100 = full rate; below 100 the sender is shedding fps for lossy viewers.
    pub fps_scale_pct: u32,
    /// Worst viewer loss in percent (from `StreamHealth`).
    pub loss_pct: f32,
}

impl StreamStats {
    /// The block turns warning-colored when adaptation kicked in — the
    /// sharer should notice without reading numbers.
    #[must_use]
    pub fn degraded(&self) -> bool {
        self.fps_scale_pct < 100
    }
}

/// RTT → status color bucket (green / amber / red), same thresholds as
/// the FLTK sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyGrade {
    Good,
    Fair,
    Bad,
}

#[must_use]
pub fn latency_grade(rtt_ms: f64) -> LatencyGrade {
    if rtt_ms < 80.0 {
        LatencyGrade::Good
    } else if rtt_ms < 150.0 {
        LatencyGrade::Fair
    } else {
        LatencyGrade::Bad
    }
}

/// The network half of the latency line: `<1ms`, `23ms`, or
/// `23ms · 1.2% loss` — loss shown the moment it's nonzero (≥0.05%),
/// because a lossy link is the #1 cause of bad streams.
#[must_use]
pub fn net_str(rtt_ms: f64, loss_pct: f64) -> String {
    if rtt_ms < 1.0 {
        "<1ms".to_string()
    } else if loss_pct >= 0.05 {
        format!("{rtt_ms:.0}ms · {loss_pct:.1}% loss")
    } else {
        format!("{rtt_ms:.0}ms")
    }
}

/// Viewer-toolbar smoothing: ring of the last N encoded frame sizes.
#[derive(Default)]
pub struct FrameRing {
    ring: [u64; 10],
    idx: usize,
}

impl FrameRing {
    pub fn push(&mut self, bytes: u64) {
        self.ring[self.idx] = bytes;
        self.idx = (self.idx + 1) % self.ring.len();
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn avg_kb(&self) -> f32 {
        self.ring.iter().sum::<u64>() as f32 / self.ring.len() as f32 / 1024.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_grades_match_thresholds() {
        assert_eq!(latency_grade(20.0), LatencyGrade::Good);
        assert_eq!(latency_grade(100.0), LatencyGrade::Fair);
        assert_eq!(latency_grade(200.0), LatencyGrade::Bad);
    }

    #[test]
    fn net_str_shows_loss_from_five_hundredths() {
        assert_eq!(net_str(23.0, 0.0), "23ms");
        assert_eq!(net_str(23.0, 1.2), "23ms · 1.2% loss");
        assert_eq!(net_str(0.4, 0.0), "<1ms");
    }
}
