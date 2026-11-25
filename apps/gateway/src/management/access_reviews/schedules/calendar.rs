//! When a scheduled access review is due (#481): a fixed cadence (monthly,
//! quarterly or yearly) and a start day, in UTC. A period starts at 00:00 UTC
//! on the start day of each month in the cycle, or on the month's last day when
//! the start day is past its end (a start day of 31 is the 30th in April and
//! the 28th or 29th in February). Quarterly and yearly cycles are anchored on a
//! start month. There is no free-form interval or cron.
use gateway_datetime_compat::{CalendarDate, UtcInstant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cadence {
    Monthly,
    Quarterly,
    Yearly,
}

impl Cadence {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "monthly" => Some(Self::Monthly),
            "quarterly" => Some(Self::Quarterly),
            "yearly" => Some(Self::Yearly),
            _ => None,
        }
    }
}

/// A schedule's cadence, start day (1 to 31) and, for quarterly and yearly
/// cycles, start month (1 to 12; monthly cycles have none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Calendar {
    cadence: Cadence,
    start_day: u8,
    start_month: Option<u8>,
}

impl Calendar {
    /// `None` unless the combination is valid.
    pub(crate) fn new(cadence: Cadence, start_day: u8, start_month: Option<u8>) -> Option<Self> {
        let month_ok = match (cadence, start_month) {
            (Cadence::Monthly, None) => true,
            (Cadence::Quarterly | Cadence::Yearly, Some(m)) => (1..=12).contains(&m),
            _ => false,
        };
        ((1..=31).contains(&start_day) && month_ok).then_some(Self {
            cadence,
            start_day,
            start_month,
        })
    }

    /// Whether `month` (1 to 12) starts a period.
    fn in_cycle(self, month: u8) -> bool {
        match (self.cadence, self.start_month) {
            (Cadence::Monthly, _) => true,
            (Cadence::Quarterly, Some(start)) => (month + 12 - start).is_multiple_of(3),
            (Cadence::Yearly, Some(start)) => month == start,
            _ => false,
        }
    }

    /// The start of the period in `year`/`month` (which must be in the cycle).
    fn boundary(self, year: i32, month: u8) -> Option<UtcInstant> {
        let last = (28..=31u8)
            .rev()
            .find(|d| CalendarDate::from_ymd_opt(year, month.into(), (*d).into()).is_some())?;
        CalendarDate::from_ymd_opt(year, month.into(), self.start_day.min(last).into())?
            .midnight()
            .ok()
    }

    /// The months from `now`'s month going back (`step` -1) or forward (+1).
    fn months(now: UtcInstant, step: i32) -> impl Iterator<Item = (i32, u8)> {
        let start = now.year() * 12 + now.month() as i32 - 1;
        (0..=13).map(move |i| {
            let index = start + step * i;
            (index.div_euclid(12), (index.rem_euclid(12) + 1) as u8)
        })
    }

    /// The latest period start at or before `now`: the one a late run is for.
    pub(crate) fn latest_at_or_before(self, now: UtcInstant) -> Option<UtcInstant> {
        Self::months(now, -1)
            .filter(|(_, m)| self.in_cycle(*m))
            .filter_map(|(y, m)| self.boundary(y, m))
            .find(|b| *b <= now)
    }

    /// The first period start strictly after `now`: the next run. After a
    /// late run it is the next period boundary, never `now` plus the cadence.
    pub(crate) fn first_after(self, now: UtcInstant) -> Option<UtcInstant> {
        Self::months(now, 1)
            .filter(|(_, m)| self.in_cycle(*m))
            .filter_map(|(y, m)| self.boundary(y, m))
            .find(|b| *b > now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_datetime_compat::http::parse_rfc3339;

    fn at(text: &str) -> UtcInstant {
        parse_rfc3339(text).unwrap()
    }
    fn monthly(day: u8) -> Calendar {
        Calendar::new(Cadence::Monthly, day, None).unwrap()
    }

    #[test]
    fn only_valid_combinations_build_a_calendar() {
        assert!(Calendar::new(Cadence::Monthly, 1, None).is_some());
        assert!(Calendar::new(Cadence::Monthly, 31, None).is_some());
        assert!(Calendar::new(Cadence::Monthly, 0, None).is_none());
        assert!(Calendar::new(Cadence::Monthly, 32, None).is_none());
        assert!(Calendar::new(Cadence::Monthly, 5, Some(1)).is_none());
        assert!(Calendar::new(Cadence::Quarterly, 5, None).is_none());
        assert!(Calendar::new(Cadence::Quarterly, 5, Some(13)).is_none());
        assert!(Calendar::new(Cadence::Yearly, 5, Some(0)).is_none());
        assert!(Calendar::new(Cadence::Yearly, 5, Some(12)).is_some());
        assert_eq!(Cadence::parse("monthly"), Some(Cadence::Monthly));
        assert_eq!(Cadence::parse("quarterly"), Some(Cadence::Quarterly));
        assert_eq!(Cadence::parse("yearly"), Some(Cadence::Yearly));
        assert_eq!(Cadence::parse("weekly"), None);
    }

    #[test]
    fn a_start_day_past_a_months_end_is_that_months_last_day() {
        let c = monthly(31);
        assert_eq!(
            c.first_after(at("2026-01-31T00:00:00Z")),
            Some(at("2026-02-28T00:00:00Z"))
        );
        assert_eq!(
            c.first_after(at("2026-02-28T00:00:00Z")),
            Some(at("2026-03-31T00:00:00Z"))
        );
        assert_eq!(
            c.first_after(at("2026-03-31T00:00:00Z")),
            Some(at("2026-04-30T00:00:00Z"))
        );
        let c = monthly(30);
        assert_eq!(
            c.first_after(at("2026-02-01T00:00:00Z")),
            Some(at("2026-02-28T00:00:00Z"))
        );
        let c = monthly(29);
        assert_eq!(
            c.first_after(at("2027-02-01T00:00:00Z")),
            Some(at("2027-02-28T00:00:00Z"))
        );
        // The clamped day does not stick: March is the 29th again.
        assert_eq!(
            c.first_after(at("2027-02-28T00:00:00Z")),
            Some(at("2027-03-29T00:00:00Z"))
        );
    }

    #[test]
    fn leap_years_have_a_29th_of_february() {
        for day in [29, 30, 31] {
            assert_eq!(
                monthly(day).first_after(at("2028-02-01T00:00:00Z")),
                Some(at("2028-02-29T00:00:00Z")),
                "start day {day}"
            );
        }
        let c = Calendar::new(Cadence::Yearly, 29, Some(2)).unwrap();
        assert_eq!(
            c.first_after(at("2027-03-01T00:00:00Z")),
            Some(at("2028-02-29T00:00:00Z"))
        );
        assert_eq!(
            c.first_after(at("2028-02-29T00:00:00Z")),
            Some(at("2029-02-28T00:00:00Z"))
        );
        // 2100 is not a leap year.
        assert_eq!(
            c.first_after(at("2099-03-01T00:00:00Z")),
            Some(at("2100-02-28T00:00:00Z"))
        );
    }

    #[test]
    fn a_schedule_created_after_its_start_day_first_runs_next_period() {
        // Created on the 20th with a start day of the 5th: next month's 5th.
        assert_eq!(
            monthly(5).first_after(at("2026-11-20T09:30:00Z")),
            Some(at("2026-12-05T00:00:00Z"))
        );
        // Created on its start day after midnight: also the next period.
        assert_eq!(
            monthly(5).first_after(at("2026-11-05T00:00:01Z")),
            Some(at("2026-12-05T00:00:00Z"))
        );
        // Created before the start day: this month.
        assert_eq!(
            monthly(5).first_after(at("2026-11-04T23:59:59Z")),
            Some(at("2026-11-05T00:00:00Z"))
        );
        // Quarterly from January (Jan, Apr, Jul, Oct): created in May.
        let q = Calendar::new(Cadence::Quarterly, 1, Some(1)).unwrap();
        assert_eq!(
            q.first_after(at("2026-05-10T00:00:00Z")),
            Some(at("2026-07-01T00:00:00Z"))
        );
        // Quarterly from February (Feb, May, Aug, Nov), across a year end.
        let q = Calendar::new(Cadence::Quarterly, 15, Some(2)).unwrap();
        assert_eq!(
            q.first_after(at("2026-11-16T00:00:00Z")),
            Some(at("2027-02-15T00:00:00Z"))
        );
        // Yearly in March, created after it this year.
        let y = Calendar::new(Cadence::Yearly, 1, Some(3)).unwrap();
        assert_eq!(
            y.first_after(at("2026-03-02T00:00:00Z")),
            Some(at("2027-03-01T00:00:00Z"))
        );
    }

    #[test]
    fn after_downtime_one_late_run_for_the_latest_period_then_the_next_boundary() {
        // Down from 2026-01-04 to 2026-04-10 with a monthly schedule on the 5th:
        // one run, for April's period, and the next one is May's, not now + 1 month.
        let c = monthly(5);
        let back = at("2026-04-10T13:00:00Z");
        assert_eq!(
            c.latest_at_or_before(back),
            Some(at("2026-04-05T00:00:00Z"))
        );
        assert_eq!(c.first_after(back), Some(at("2026-05-05T00:00:00Z")));
        // A yearly schedule down for more than a year: still one run.
        let y = Calendar::new(Cadence::Yearly, 31, Some(12)).unwrap();
        let back = at("2028-01-15T00:00:00Z");
        assert_eq!(
            y.latest_at_or_before(back),
            Some(at("2027-12-31T00:00:00Z"))
        );
        assert_eq!(y.first_after(back), Some(at("2028-12-31T00:00:00Z")));
        // At a boundary exactly, that boundary is the current period.
        assert_eq!(
            c.latest_at_or_before(at("2026-06-05T00:00:00Z")),
            Some(at("2026-06-05T00:00:00Z"))
        );
    }

    #[test]
    fn periods_are_computed_in_utc_at_midnight() {
        let c = Calendar::new(Cadence::Quarterly, 31, Some(3)).unwrap();
        // Mar, Jun, Sep, Dec on the 31st, clamped: Jun 30, Sep 30.
        let mut now = at("2026-01-01T00:00:00Z");
        let mut seen = Vec::new();
        for _ in 0..5 {
            now = c.first_after(now).unwrap();
            seen.push(now);
        }
        assert_eq!(
            seen,
            [
                "2026-03-31",
                "2026-06-30",
                "2026-09-30",
                "2026-12-31",
                "2027-03-31"
            ]
            .map(|d| at(&format!("{d}T00:00:00Z")))
        );
    }
}
