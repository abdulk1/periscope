//! Durations, formatted the way `kubectl` prints them.
//!
//! The age column matches `kubectl`'s `HumanDuration` exactly — `5m30s`, `3h`,
//! `12d`, `2y40d`. The target user reads these strings all day; inventing a
//! different scheme would be a papercut on every single row.
//!
//! It lives in the bridge rather than the view because two layers print ages
//! now: the AGE column is rendered by the UI from a row's creation timestamp,
//! and a CRD's own `date` printer column is rendered by the cluster layer as it
//! projects a row. One implementation, so the same duration cannot come out two
//! different ways in two columns of the same table.

use std::time::Duration;

/// Formats a duration the way `kubectl` prints an object's age.
pub fn age(duration: Duration) -> String {
    let seconds = duration.as_secs();

    if seconds < 120 {
        return format!("{seconds}s");
    }

    let minutes = seconds / 60;
    if minutes < 10 {
        return match seconds % 60 {
            0 => format!("{minutes}m"),
            rest => format!("{minutes}m{rest}s"),
        };
    }
    if minutes < 180 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    if hours < 8 {
        return match minutes % 60 {
            0 => format!("{hours}h"),
            rest => format!("{hours}h{rest}m"),
        };
    }
    if hours < 48 {
        return format!("{hours}h");
    }

    let days = hours / 24;
    if hours < 24 * 8 {
        return match hours % 24 {
            0 => format!("{days}d"),
            rest => format!("{days}d{rest}h"),
        };
    }
    if days < 365 * 2 {
        return format!("{days}d");
    }

    let years = days / 365;
    if years < 8 {
        return match days % 365 {
            0 => format!("{years}y"),
            rest => format!("{years}y{rest}d"),
        };
    }

    format!("{years}y")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(seconds: u64) -> String {
        age(Duration::from_secs(seconds))
    }

    #[test]
    fn seconds_up_to_two_minutes() {
        assert_eq!(secs(0), "0s");
        assert_eq!(secs(45), "45s");
        assert_eq!(secs(119), "119s");
    }

    #[test]
    fn minutes_carry_seconds_only_below_ten_minutes() {
        assert_eq!(secs(120), "2m");
        assert_eq!(secs(150), "2m30s");
        assert_eq!(secs(600), "10m");
        assert_eq!(secs(630), "10m");
    }

    #[test]
    fn hours_carry_minutes_only_below_eight_hours() {
        assert_eq!(secs(3 * 3600), "3h");
        assert_eq!(secs(3 * 3600 + 25 * 60), "3h25m");
        assert_eq!(secs(9 * 3600 + 25 * 60), "9h");
    }

    #[test]
    fn days_carry_hours_only_in_the_first_week() {
        assert_eq!(secs(50 * 3600), "2d2h");
        assert_eq!(secs(48 * 3600), "2d");
        assert_eq!(secs(20 * 24 * 3600), "20d");
    }

    #[test]
    fn years_appear_after_two() {
        assert_eq!(secs(400 * 24 * 3600), "400d");
        assert_eq!(secs(800 * 24 * 3600), "2y70d");
        assert_eq!(secs(3 * 365 * 24 * 3600), "3y");
        assert_eq!(secs(9 * 365 * 24 * 3600), "9y");
    }
}
