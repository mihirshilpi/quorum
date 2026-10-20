//! Tiny civil-date helpers for the calendar UI (the core does time zones; the UI only steps
//! through `YYYY-MM-DD` dates and `HH:MM` times). Pure and tested natively.

/// A calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ymd {
    /// Year.
    pub y: i32,
    /// Month 1–12.
    pub m: u32,
    /// Day 1–31.
    pub d: u32,
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Days in month `m` of year `y` (0 for an invalid month).
pub fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

impl Ymd {
    /// A valid date, or `None`.
    pub fn new(y: i32, m: u32, d: u32) -> Option<Self> {
        ((-9999..=9999).contains(&y) && d >= 1 && d <= days_in_month(y, m)).then_some(Self { y, m, d })
    }

    /// Parses `YYYY-MM-DD` (a longer string's first ten characters, e.g. a date-time).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().get(..10)?;
        let mut it = s.split('-');
        let y = it.next()?.parse().ok()?;
        let m = it.next()?.parse().ok()?;
        let d = it.next()?.parse().ok()?;
        Self::new(y, m, d)
    }

    /// Days since 1970-01-01 (Howard Hinnant's algorithm).
    pub fn to_days(self) -> i64 {
        let y = i64::from(self.y) - i64::from(self.m <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.m);
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + i64::from(self.d) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// The date `days` after 1970-01-01.
    pub fn from_days(days: i64) -> Self {
        let z = days.clamp(-4_000_000, 4_000_000) + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
        let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
        let y = i32::try_from(yoe + era * 400 + i64::from(m <= 2)).unwrap_or(1970);
        Self { y, m, d }
    }

    /// `self + n` days.
    pub fn add_days(self, n: i64) -> Self {
        Self::from_days(self.to_days().saturating_add(n))
    }

    /// Days from `self` to `other`.
    pub fn days_until(self, other: Self) -> i64 {
        other.to_days() - self.to_days()
    }

    /// Weekday, `0` = Monday.
    pub fn weekday(self) -> u32 {
        u32::try_from((self.to_days() + 3).rem_euclid(7)).unwrap_or(0)
    }

    /// `YYYY-MM-DD`.
    pub fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.y, self.m, self.d)
    }

    /// `October`.
    pub fn month_name(self) -> &'static str {
        MONTHS.get(usize::try_from(self.m.saturating_sub(1)).unwrap_or(0)).copied().unwrap_or("")
    }

    /// `Mon`.
    pub fn weekday_name(self) -> &'static str {
        WEEKDAYS.get(usize::try_from(self.weekday()).unwrap_or(0)).copied().unwrap_or("")
    }

    /// `Fri, Oct 17`.
    pub fn short(self) -> String {
        format!("{}, {} {}", self.weekday_name(), self.month_name().get(..3).unwrap_or(""), self.d)
    }
}

/// Parses `HH:MM` to minutes after midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.get(..2)?.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// `09:05` (24:00 and later wrap into the next day's clock).
pub fn hhmm(minutes: u32) -> String {
    let m = minutes % 1440;
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// The hour label of the time grid (`09:00`).
pub fn hour_label(h: u32) -> String {
    format!("{h:02}:00")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_days() {
        let d = Ymd::new(2026, 10, 17).unwrap();
        assert_eq!(Ymd::from_days(d.to_days()), d);
        assert_eq!(Ymd::new(1970, 1, 1).unwrap().to_days(), 0);
        assert_eq!(d.weekday(), 5, "Saturday");
        assert_eq!(d.add_days(15), Ymd::new(2026, 11, 1).unwrap());
        assert_eq!(Ymd::new(2028, 2, 28).unwrap().add_days(1), Ymd::new(2028, 2, 29).unwrap());
        assert_eq!(d.iso(), "2026-10-17");
        assert_eq!(d.short(), "Sat, Oct 17");
        assert_eq!(Ymd::parse("2026-10-17T09:00"), Some(d));
        for bad in ["", "2026-02-30", "x", "2026-13-01", "20261017"] {
            assert_eq!(Ymd::parse(bad), None, "{bad}");
        }
        for n in [-1_000_000, -1, 0, 1, 59, 1_000_000] {
            assert_eq!(Ymd::from_days(n).to_days(), n);
        }
        let _ = Ymd::from_days(i64::MAX);
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_hhmm("09:30"), Some(570));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("9"), None);
        assert_eq!(hhmm(1440), "00:00");
        assert_eq!(hhmm(615), "10:15");
    }
}
