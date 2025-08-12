use std::str::FromStr;

use chrono::{DateTime, Duration, TimeZone};

/// Whether a playbook should run now or later
#[derive(PartialEq, Eq, Debug)]
pub enum Timing<Tz: TimeZone> {
    /// The playbook should run _now_ due to some reason. The inner DateTime represents the targeted time
    /// and is to be expected in the recent past.
    Now(DateTime<Tz>),

    /// The playbook will be delayed until some time in the future
    Delayed(DateTime<Tz>),
}

pub fn evaluate_schedule<Tz: TimeZone>(
    schedule: Option<&str>,
    now: DateTime<Tz>,
    window: Duration,
) -> Timing<Tz> {
    if schedule.is_none() {
        return Timing::Now(now);
    }

    let schedule = schedule.unwrap();
    let next_run = forecast_next_run(schedule, now.clone(), Some(window));

    let offset_now = now - window;
    let diff = next_run.clone() - offset_now;

    if diff <= window {
        return Timing::Now(next_run);
    }

    Timing::Delayed(next_run)
}

pub fn forecast_next_run<Tz: TimeZone>(
    cron: &str,
    now: DateTime<Tz>,
    window: Option<Duration>,
) -> DateTime<Tz> {
    let offset_now = now - window.unwrap_or(Duration::zero());
    let schedule = cron::Schedule::from_str(format!("0 {cron}").as_str()).unwrap();
    schedule.after(&offset_now).next().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: &str) -> DateTime<chrono::Utc> {
        value.parse::<DateTime<chrono::Utc>>().unwrap()
    }

    #[test]
    fn test_delayed_triggers() {
        // Given
        let schedule = Some("0 0 20 * * *");
        let window = Duration::seconds(60);

        // When
        let too_early = evaluate_schedule(schedule, parse("2025-08-12T19:59:00Z"), window);
        let on_time = evaluate_schedule(schedule, parse("2025-08-12T20:00:00Z"), window);
        let latest = evaluate_schedule(schedule, parse("2025-08-12T20:00:59Z"), window);
        let too_late = evaluate_schedule(schedule, parse("2025-08-12T20:01:00Z"), window);

        // Then
        assert_eq!(Timing::Delayed(parse("2025-08-12T20:00:00Z")), too_early);
        assert_eq!(Timing::Now(parse("2025-08-12T20:00:00Z")), on_time);
        assert_eq!(Timing::Now(parse("2025-08-12T20:00:00Z")), latest);
        assert_eq!(Timing::Delayed(parse("2025-08-13T20:00:00Z")), too_late);
    }
}
