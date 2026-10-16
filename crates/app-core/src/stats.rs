//! Stats view-model helpers shared by every shell.

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

/// The network half of the latency line: `<1ms`, `23ms`, or what
/// `with_loss` makes of the round trip and the loss, both formatted
/// (`23ms · 1.2% loss`, in the user's language: review 2026-10,
/// P3-54) — loss shown the moment it's nonzero (≥0.05%), because a
/// lossy link is the #1 cause of bad streams.
#[must_use]
pub fn net_str(
    rtt_ms: f64,
    loss_pct: f64,
    with_loss: impl FnOnce(String, String) -> String,
) -> String {
    if rtt_ms < 1.0 {
        "<1ms".to_string()
    } else if loss_pct >= 0.05 {
        with_loss(format!("{rtt_ms:.0}"), format!("{loss_pct:.1}"))
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
        let loss = |rtt: String, loss: String| format!("{rtt}ms · {loss}% perda");
        assert_eq!(net_str(23.0, 0.0, loss), "23ms");
        assert_eq!(net_str(23.0, 1.2, loss), "23ms · 1.2% perda");
        assert_eq!(net_str(0.4, 0.0, loss), "<1ms");
    }
}
