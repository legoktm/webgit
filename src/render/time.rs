//! How a timestamp is shown: a coarse relative age inside two weeks, the
//! commit's own date beyond it, and the full `YYYY-MM-DD HH:MM:SS ±HH:MM` on
//! the pages that show one timestamp rather than a column of them.

fn age(dt: &jiff::Zoned) -> u64 {
    let now_ms = js_sys::Date::now();
    let then_ms = dt.timestamp().as_millisecond() as f64;
    ((now_ms - then_ms) / 1000.0).max(0.0) as u64
}

/// A commit/ref timestamp that keeps both representations: the elapsed seconds
/// (for sorting by recency and choosing a format) and the calendar date in the
/// commit's own timezone. It sorts by recency and serializes — at render time —
/// to a coarse relative age within the last two weeks, or that absolute
/// `YYYY-MM-DD` date beyond that.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Age {
    pub(super) secs: u64,
    pub(super) when: jiff::civil::Date,
}

impl Age {
    pub(super) fn new(when: &jiff::Zoned) -> Self {
        Self {
            secs: age(when),
            when: when.date(),
        }
    }

    /// Elapsed seconds, the sort key (smaller is more recent).
    pub(crate) fn secs(&self) -> u64 {
        self.secs
    }

    /// The rendered age: a coarse relative bucket within two weeks, else an
    /// absolute date. Used by every view that renders a row's age.
    pub(crate) fn display(&self) -> String {
        format_age(self.secs, self.when)
    }
}

/// The display rule, split out as a pure function so the bucket boundaries can
/// be tested without depending on the wall clock. A [`jiff::civil::Date`]
/// displays as ISO 8601 `YYYY-MM-DD`, which is the format we want verbatim.
fn format_age(secs: u64, date: jiff::civil::Date) -> String {
    match secs {
        s if s < 90 => plural(s, "second"),
        s if s < 90 * 60 => plural(s / 60, "minute"),
        s if s < 36 * 3600 => plural(s / 3600, "hour"),
        s if s < 14 * 86400 => plural(s / 86400, "day"),
        _ => date.to_string(),
    }
}

/// A timestamp rendered as `YYYY-MM-DD HH:MM:SS ±HH:MM` in its own timezone,
/// for the commit and tag metadata tables. Assembled by hand rather than via
/// `strftime` — the pieces each `Display` in exactly the shape we need, except
/// the offset, which jiff prints as `+01` where we want `+01:00`.
pub(crate) fn format_datetime(dt: &jiff::Zoned) -> String {
    let total = dt.offset().seconds();
    let sign = if total < 0 { '-' } else { '+' };
    let (hours, minutes) = (total.abs() / 3600, (total.abs() % 3600) / 60);
    format!("{} {} {sign}{hours:02}:{minutes:02}", dt.date(), dt.time())
}

/// `<n> <unit>`, with the unit pluralised unless `n` is exactly 1.
fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_date() -> jiff::civil::Date {
        jiff::civil::date(2001, 2, 3)
    }

    #[test]
    fn test_format_age_relative_buckets() {
        let date = fixed_date();
        assert_eq!(format_age(0, date), "0 seconds");
        assert_eq!(format_age(1, date), "1 second");
        assert_eq!(format_age(89, date), "89 seconds");
        assert_eq!(format_age(90, date), "1 minute");
        assert_eq!(format_age(89 * 60, date), "89 minutes");
        assert_eq!(format_age(90 * 60, date), "1 hour");
        assert_eq!(format_age(35 * 3600, date), "35 hours");
        assert_eq!(format_age(36 * 3600, date), "1 day");
        assert_eq!(format_age(13 * 86400, date), "13 days");
    }

    #[test]
    fn test_format_age_two_weeks_and_older_is_date() {
        let date = fixed_date();
        // From exactly two weeks on, show the commit's own date instead.
        assert_eq!(format_age(14 * 86400, date), "2001-02-03");
        assert_eq!(format_age(86400 * 400, date), "2001-02-03");
    }

    #[test]
    fn test_format_datetime() {
        fn at(secs: i64, offset_secs: i32) -> jiff::Zoned {
            jiff::Timestamp::from_second(secs)
                .unwrap()
                .to_zoned(jiff::tz::TimeZone::fixed(
                    jiff::tz::Offset::from_seconds(offset_secs).unwrap(),
                ))
        }
        // A whole-hour offset still gets its `:00` minutes, and the wall clock
        // is the one in that offset, not UTC.
        assert_eq!(
            format_datetime(&at(1774735018, 0)),
            "2026-03-28 21:56:58 +00:00"
        );
        assert_eq!(
            format_datetime(&at(1774735018, 3600)),
            "2026-03-28 22:56:58 +01:00"
        );
        assert_eq!(
            format_datetime(&at(1774735018, 19800)),
            "2026-03-29 03:26:58 +05:30"
        );
        assert_eq!(
            format_datetime(&at(1774735018, -28800)),
            "2026-03-28 13:56:58 -08:00"
        );
    }

    #[test]
    fn age_sorts_by_recency_regardless_of_display() {
        let date = fixed_date();
        // A mix of relative-rendered and date-rendered ages; sorting must order
        // them by elapsed seconds (most recent first), not by the display text.
        let mut ages = [
            Age {
                secs: 86400 * 400,
                when: date,
            },
            Age {
                secs: 60,
                when: date,
            },
            Age {
                secs: 3600,
                when: date,
            },
        ];
        ages.sort_by_key(Age::secs);
        assert_eq!(
            ages.map(|a| a.secs()),
            [60, 3600, 86400 * 400],
            "expected ascending recency order"
        );
    }
}
