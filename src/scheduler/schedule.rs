//! Schedule specs for tasks, all in the machine's local time:
//! - `every 30m` / `every 2h` / `every 1d` (at least a minute)
//! - `at 2026-10-06 09:00` (once)
//! - a 5-field cron expression: `minute hour day-of-month month day-of-week`

use anyhow::{Result, bail};
use chrono::{Datelike, Duration, Local, NaiveDateTime, TimeZone, Timelike};

pub enum Schedule {
    Every(i64),
    At(i64),
    Cron(Cron),
}

pub struct Cron {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    any_day: bool,
    any_weekday: bool,
}

impl Schedule {
    pub fn parse(spec: &str) -> Result<Schedule> {
        let spec = spec.trim();
        if let Some(rest) = spec.strip_prefix("every ") {
            let rest = rest.trim();
            let (num, unit) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len()));
            let n: i64 = num.parse().map_err(|_| anyhow::anyhow!("bad interval `{rest}`, expected e.g. `every 30m`"))?;
            let secs = match unit.trim() {
                "m" | "min" => n * 60,
                "h" => n * 3600,
                "d" => n * 86400,
                _ => bail!("bad interval unit in `{rest}`, use m, h or d"),
            };
            if secs < 60 {
                bail!("interval must be at least 1 minute");
            }
            return Ok(Schedule::Every(secs));
        }
        if let Some(rest) = spec.strip_prefix("at ") {
            let naive = NaiveDateTime::parse_from_str(rest.trim(), "%Y-%m-%d %H:%M")
                .map_err(|_| anyhow::anyhow!("bad time `{rest}`, expected `at YYYY-MM-DD HH:MM` (local time)"))?;
            let ts = Local
                .from_local_datetime(&naive)
                .earliest()
                .ok_or_else(|| anyhow::anyhow!("`{rest}` does not exist in the local time zone"))?
                .timestamp();
            return Ok(Schedule::At(ts));
        }
        Ok(Schedule::Cron(Cron::parse(spec)?))
    }

    /// First run strictly after `from` (unix seconds); `None` if it never runs again.
    pub fn next_after(&self, from: i64) -> Option<i64> {
        match self {
            Schedule::Every(secs) => Some(from + secs),
            Schedule::At(ts) => (*ts > from).then_some(*ts),
            Schedule::Cron(c) => c.next_after(from),
        }
    }
}

fn parse_field(field: &str, min: u32, max: u32) -> Result<u64> {
    let mut bits = 0u64;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0).ok_or_else(|| anyhow::anyhow!("bad step in `{part}`"))?),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse()?, b.parse()?)
        } else {
            let v: u32 = range.parse().map_err(|_| anyhow::anyhow!("bad value `{range}`"))?;
            (v, if step > 1 { max } else { v })
        };
        if lo < min || hi > max || lo > hi {
            bail!("`{part}` is outside {min}-{max}");
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok(bits)
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron> {
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            bail!("expected `every <n>m|h|d`, `at YYYY-MM-DD HH:MM` or 5 cron fields (min hour day month weekday), got `{expr}`");
        }
        let mut weekdays = parse_field(f[4], 0, 7)?;
        if weekdays & (1 << 7) != 0 {
            weekdays |= 1; // 7 is Sunday too
        }
        Ok(Cron {
            minutes: parse_field(f[0], 0, 59)?,
            hours: parse_field(f[1], 0, 23)?,
            days: parse_field(f[2], 1, 31)?,
            months: parse_field(f[3], 1, 12)?,
            weekdays: weekdays & 0x7f,
            any_day: f[2] == "*",
            any_weekday: f[4] == "*",
        })
    }

    fn day_matches(&self, d: NaiveDateTime) -> bool {
        let dom = self.days & (1 << d.day()) != 0;
        let dow = self.weekdays & (1 << d.weekday().num_days_from_sunday()) != 0;
        // Like classic cron: when both are restricted, either one may match.
        match (self.any_day, self.any_weekday) {
            (false, false) => dom || dow,
            (false, true) => dom,
            (true, false) => dow,
            (true, true) => true,
        }
    }

    pub fn next_after(&self, from: i64) -> Option<i64> {
        let start = Local.timestamp_opt(from, 0).single()?.naive_local();
        let mut t = start.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        for _ in 0..200_000 {
            if self.months & (1 << t.month()) == 0 {
                let (y, m) = if t.month() == 12 { (t.year() + 1, 1) } else { (t.year(), t.month() + 1) };
                t = chrono::NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)?;
            } else if !self.day_matches(t) {
                t = (t.date() + Duration::days(1)).and_hms_opt(0, 0, 0)?;
            } else if self.hours & (1 << t.hour()) == 0 {
                t = t.with_minute(0)? + Duration::hours(1);
            } else if self.minutes & (1 << t.minute()) == 0 {
                t += Duration::minutes(1);
            } else {
                // A time skipped by a DST jump simply never fires that day.
                if let Some(local) = Local.from_local_datetime(&t).earliest() {
                    return Some(local.timestamp());
                }
                t += Duration::minutes(1);
            }
        }
        None
    }
}

/// Human-readable local time of a unix timestamp.
pub fn fmt_time(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(s: &str) -> i64 {
        let n = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        Local.from_local_datetime(&n).earliest().unwrap().timestamp()
    }

    #[test]
    fn every_and_at() {
        assert_eq!(Schedule::parse("every 30m").unwrap().next_after(1000), Some(2800));
        assert_eq!(Schedule::parse("every 2h").unwrap().next_after(0), Some(7200));
        assert!(Schedule::parse("every 10s").is_err());
        assert!(Schedule::parse("every soon").is_err());
        let at = Schedule::parse("at 2030-01-02 03:04").unwrap();
        assert_eq!(at.next_after(0), Some(local("2030-01-02 03:04")));
        assert_eq!(at.next_after(local("2030-01-02 03:04")), None);
    }

    #[test]
    fn cron_daily_and_weekly() {
        let s = Schedule::parse("0 9 * * *").unwrap();
        assert_eq!(s.next_after(local("2026-10-05 08:00")), Some(local("2026-10-05 09:00")));
        assert_eq!(s.next_after(local("2026-10-05 09:00")), Some(local("2026-10-06 09:00")));
        // 2026-10-05 is a Monday; next Friday is the 9th
        let s = Schedule::parse("30 18 * * 5").unwrap();
        assert_eq!(s.next_after(local("2026-10-05 12:00")), Some(local("2026-10-09 18:30")));
        let s = Schedule::parse("*/15 * * * *").unwrap();
        assert_eq!(s.next_after(local("2026-10-05 12:01")), Some(local("2026-10-05 12:15")));
        let s = Schedule::parse("0 0 1 1 *").unwrap();
        assert_eq!(s.next_after(local("2026-10-05 12:00")), Some(local("2027-01-01 00:00")));
        let s = Schedule::parse("0 8 * * 1-5").unwrap();
        assert_eq!(s.next_after(local("2026-10-09 09:00")), Some(local("2026-10-12 08:00")));
    }

    #[test]
    fn rejects_bad_cron() {
        assert!(Schedule::parse("* * *").is_err());
        assert!(Schedule::parse("61 * * * *").is_err());
        assert!(Schedule::parse("* 24 * * *").is_err());
    }
}
