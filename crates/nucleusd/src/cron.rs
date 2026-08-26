//! Minimal 5-field crontab engine: `min hour dom month dow`
//! Supports `*`, lists (`1,5,9`), ranges (`1-5`), steps (`*/15`, `5-30/10`).
//! Day-of-week: 0-6 where 0 = Sunday. All times are daemon-local.

use anyhow::{anyhow, Result};
use chrono::{Datelike, Timelike};

#[derive(Debug, Clone)]
pub struct Cron {
    minute: Vec<u32>,
    hour: Vec<u32>,
    dom: Vec<u32>,
    month: Vec<u32>,
    dow: Vec<u32>,
    /// Vixie-cron rule: when both dom and dow are restricted they are OR'd.
    dom_unrestricted: bool,
    dow_unrestricted: bool,
}

fn parse_field(field: &str, min: u32, max: u32) -> Result<Vec<u32>> {
    let mut out = std::collections::BTreeSet::new();
    for part in field.split(',') {
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s.parse().map_err(|_| anyhow!("bad step '{s}'"))?;
                if step == 0 {
                    return Err(anyhow!("step cannot be 0"));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let a: u32 = a.parse().map_err(|_| anyhow!("bad range start '{a}'"))?;
            let b: u32 = b.parse().map_err(|_| anyhow!("bad range end '{b}'"))?;
            (a, b)
        } else {
            let v: u32 = range_part
                .parse()
                .map_err(|_| anyhow!("bad value '{range_part}'"))?;
            (v, v)
        };
        if lo < min || hi > max || lo > hi {
            return Err(anyhow!("value out of range {min}-{max}: '{range_part}'"));
        }
        let mut v = lo;
        while v <= hi {
            out.insert(v);
            v += step;
        }
    }
    Ok(out.into_iter().collect())
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(anyhow!("cron needs exactly 5 fields: 'min hour dom month dow'"));
        }
        Ok(Self {
            minute: parse_field(fields[0], 0, 59)?,
            hour: parse_field(fields[1], 0, 23)?,
            dom: parse_field(fields[2], 1, 31)?,
            month: parse_field(fields[3], 1, 12)?,
            dow: parse_field(fields[4], 0, 6)?,
            dom_unrestricted: fields[2].trim() == "*",
            dow_unrestricted: fields[4].trim() == "*",
        })
    }

    pub fn matches(&self, t: &chrono::DateTime<chrono::Local>) -> bool {
        let day_match = if self.dom_unrestricted || self.dow_unrestricted {
            self.dom.contains(&t.day())
                && self.dow.contains(&(t.weekday().num_days_from_sunday()))
        } else {
            // both restricted: either may match (Vixie cron)
            self.dom.contains(&t.day())
                || self.dow.contains(&(t.weekday().num_days_from_sunday()))
        };
        self.minute.contains(&t.minute())
            && self.hour.contains(&t.hour())
            && self.month.contains(&t.month())
            && day_match
    }

    /// First matching minute strictly after `from`, scanning up to ~2 years.
    pub fn next_after(&self, from: &chrono::DateTime<chrono::Local>) -> Option<chrono::DateTime<chrono::Local>> {
        let mut t = from
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or_else(chrono::Local::now)
            + chrono::Duration::minutes(1);
        for _ in 0..(366 * 24 * 60) {
            if self.matches(&t) {
                return Some(t);
            }
            t += chrono::Duration::minutes(1);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_and_matches_basic() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        let t = chrono::Local.with_ymd_and_hms(2026, 8, 23, 12, 30, 20).unwrap();
        assert!(c.matches(&t));
        let t2 = chrono::Local.with_ymd_and_hms(2026, 8, 23, 12, 31, 0).unwrap();
        assert!(!c.matches(&t2));
    }

    #[test]
    fn lists_ranges_steps_dow() {
        let c = Cron::parse("0 4 1,15 * 1-5").unwrap(); // 04:00 on 1st/15th, Mon-Fri
        // 2026-08-03 is a Monday
        let mon = chrono::Local.with_ymd_and_hms(2026, 8, 3, 4, 0, 0).unwrap();
        assert!(c.matches(&mon));
        let sat = chrono::Local.with_ymd_and_hms(2026, 8, 1, 4, 0, 0).unwrap();
        assert!(c.matches(&sat)); // dom match
        let tue_wrong_hour = chrono::Local.with_ymd_and_hms(2026, 8, 4, 5, 0, 0).unwrap();
        assert!(!c.matches(&tue_wrong_hour));
    }

    #[test]
    fn step_range() {
        let c = Cron::parse("10-40/10 12 * * *").unwrap();
        for m in [10, 20, 30, 40] {
            let t = chrono::Local.with_ymd_and_hms(2026, 8, 23, 12, m, 0).unwrap();
            assert!(c.matches(&t), "minute {m} should match");
        }
        let t = chrono::Local.with_ymd_and_hms(2026, 8, 23, 12, 25, 0).unwrap();
        assert!(!c.matches(&t));
    }

    #[test]
    fn rejects_bad_exprs() {
        assert!(Cron::parse("* * * *").is_err());
        assert!(Cron::parse("61 * * * *").is_err());
        assert!(Cron::parse("* * * * 7").is_err());
        assert!(Cron::parse("*/0 * * * *").is_err());
    }

    #[test]
    fn next_after_finds_next_minute_match() {
        let c = Cron::parse("30 12 * * *").unwrap();
        let from = chrono::Local.with_ymd_and_hms(2026, 8, 23, 12, 31, 0).unwrap();
        let next = c.next_after(&from).unwrap();
        assert_eq!(
            next,
            chrono::Local.with_ymd_and_hms(2026, 8, 24, 12, 30, 0).unwrap()
        );
    }
}
