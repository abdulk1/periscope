//! Presentation helpers.
//!
//! [`age`] is re-exported rather than defined here: the cluster layer prints
//! durations too, for a CRD's own `date` printer column, so the one
//! implementation lives in [`periscope_bridge::format`] where both can reach it.

use std::time::Duration;

pub use periscope_bridge::format::age;

/// Formats a round-trip time for the status chrome.
pub fn millis(duration: Duration) -> String {
    format!("{:.1}ms", duration.as_secs_f64() * 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_are_rendered_to_a_tenth_of_a_millisecond() {
        assert_eq!(millis(Duration::from_micros(1_234)), "1.2ms");
    }
}
