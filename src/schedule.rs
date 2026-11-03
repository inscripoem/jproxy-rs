//! Java-style cron schedules.
//!
//! Only the fixed expressions from `application-prod.yml` are needed, so this
//! module implements the exact next-run calculation instead of a general cron
//! parser. Everything is evaluated in process-local time, like Spring's
//! `@Scheduled(cron = ...)`.
//!
//! A cron expression does not fire at process start. The rename and sync loops
//! therefore sleep until the next matching wall-clock instant. Downloader
//! login is the exception: Java also runs it once at startup from a
//! `CommandLineRunner`, then at every `0 */30 * * * ?` boundary.

use chrono::{
    DateTime, Days, Duration as ChronoDuration, Local, LocalResult, NaiveDate, TimeZone, Timelike,
};

/// Sonarr title sync: `0 0 0/1 * * ?` (hourly at :00).
pub const SONARR_TITLE: CronSchedule = CronSchedule::HourlyAtMinute(0);
/// Radarr title sync: `0 30 0/1 * * ?` (hourly at :30).
pub const RADARR_TITLE: CronSchedule = CronSchedule::HourlyAtMinute(30);
/// Sonarr rule sync: `0 15 0 * * ?` (daily 00:15).
pub const SONARR_RULE: CronSchedule = CronSchedule::DailyAt {
    hour: 0,
    minute: 15,
};
/// Radarr rule sync: `0 45 1 * * ?` (daily 01:45).
pub const RADARR_RULE: CronSchedule = CronSchedule::DailyAt {
    hour: 1,
    minute: 45,
};
/// Sonarr rename: `0/30 * * * * ?` (seconds 0 and 30 of every minute).
pub const SONARR_RENAME: CronSchedule = CronSchedule::EverySeconds(30);
/// Radarr rename: `0 0/1 * * * ?` (second 0 of every minute).
pub const RADARR_RENAME: CronSchedule = CronSchedule::EveryMinutes(1);
/// Downloader login: `0 */30 * * * ?` (second 0 of minutes 0 and 30).
pub const DOWNLOADER_LOGIN: CronSchedule = CronSchedule::EveryMinutes(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronSchedule {
    /// Second 0 of `minute` in every hour.
    HourlyAtMinute(u32),
    /// A fixed local time every day.
    DailyAt { hour: u32, minute: u32 },
    /// Wall-clock seconds divisible by the step (Spring `0/30`).
    EverySeconds(u32),
    /// Whole minutes divisible by the step, at second 0 (Spring `0 */n`).
    EveryMinutes(u32),
}

impl CronSchedule {
    /// First matching instant strictly after `after`.
    pub fn next_run(self, after: DateTime<Local>) -> DateTime<Local> {
        match self {
            Self::HourlyAtMinute(minute) => {
                let minute = minute.min(59);
                for day_offset in 0..=2 {
                    let Some(date) = after.date_naive().checked_add_days(Days::new(day_offset))
                    else {
                        continue;
                    };
                    for hour in 0..24 {
                        if let Some(candidate) = local_datetime(date, hour, minute, 0)
                            && candidate > after
                        {
                            return candidate;
                        }
                    }
                }
                after + ChronoDuration::hours(1)
            }
            Self::DailyAt { hour, minute } => {
                let hour = hour.min(23);
                let minute = minute.min(59);
                for day_offset in 0..=2 {
                    let Some(date) = after.date_naive().checked_add_days(Days::new(day_offset))
                    else {
                        continue;
                    };
                    if let Some(candidate) = local_datetime(date, hour, minute, 0)
                        && candidate > after
                    {
                        return candidate;
                    }
                }
                after + ChronoDuration::days(1)
            }
            Self::EverySeconds(step) => {
                let step = step.max(1);
                let mut candidate = after.with_nanosecond(0).unwrap_or(after);
                if candidate <= after {
                    candidate += ChronoDuration::seconds(1);
                }
                while !candidate.second().is_multiple_of(step) {
                    candidate += ChronoDuration::seconds(1);
                }
                candidate
            }
            Self::EveryMinutes(step) => {
                let step = step.max(1);
                let mut candidate = after
                    .with_second(0)
                    .and_then(|value| value.with_nanosecond(0))
                    .unwrap_or(after);
                if candidate <= after {
                    candidate += ChronoDuration::minutes(1);
                }
                while !candidate.minute().is_multiple_of(step) {
                    candidate += ChronoDuration::minutes(1);
                }
                candidate
            }
        }
    }
}

/// Sleep until the next matching local time, then return. A clock jump that
/// skips a boundary simply waits for the following one, as cron would.
pub async fn sleep_until_next(schedule: CronSchedule) {
    let now = Local::now();
    let delay = schedule
        .next_run(now)
        .signed_duration_since(now)
        .to_std()
        .unwrap_or_default();
    tokio::time::sleep(delay).await;
}

fn local_datetime(date: NaiveDate, hour: u32, minute: u32, second: u32) -> Option<DateTime<Local>> {
    let naive = date.and_hms_opt(hour, minute, second)?;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => Some(value),
        // A repeated wall-clock time (DST fall-back) fires once, at the first
        // occurrence. A skipped time (spring-forward) is skipped for the day.
        LocalResult::Ambiguous(first, _) => Some(first),
        LocalResult::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, second)
            .single()
            .expect("unambiguous local time")
    }

    #[test]
    fn hourly_schedules_hit_minutes_zero_and_thirty() {
        let after = at(2024, 1, 15, 10, 42, 7);
        assert_eq!(SONARR_TITLE.next_run(after), at(2024, 1, 15, 11, 0, 0));
        assert_eq!(RADARR_TITLE.next_run(after), at(2024, 1, 15, 11, 30, 0));

        let after = at(2024, 1, 15, 23, 10, 0);
        assert_eq!(SONARR_TITLE.next_run(after), at(2024, 1, 16, 0, 0, 0));
        let after = at(2024, 1, 15, 23, 40, 0);
        assert_eq!(RADARR_TITLE.next_run(after), at(2024, 1, 16, 0, 30, 0));
    }

    #[test]
    fn exactly_on_boundary_returns_the_following_boundary() {
        let after = at(2024, 1, 15, 11, 0, 0);
        assert_eq!(SONARR_TITLE.next_run(after), at(2024, 1, 15, 12, 0, 0));
        let after = at(2024, 1, 15, 11, 30, 0);
        assert_eq!(RADARR_TITLE.next_run(after), at(2024, 1, 15, 12, 30, 0));
    }

    #[test]
    fn daily_schedules_use_java_local_times() {
        let after = at(2024, 1, 15, 0, 0, 0);
        assert_eq!(SONARR_RULE.next_run(after), at(2024, 1, 15, 0, 15, 0));
        assert_eq!(RADARR_RULE.next_run(after), at(2024, 1, 15, 1, 45, 0));

        let after = at(2024, 1, 15, 2, 0, 0);
        assert_eq!(SONARR_RULE.next_run(after), at(2024, 1, 16, 0, 15, 0));
        assert_eq!(RADARR_RULE.next_run(after), at(2024, 1, 16, 1, 45, 0));
    }

    #[test]
    fn thirty_second_schedule_matches_spring_0_slash_30() {
        assert_eq!(
            SONARR_RENAME.next_run(at(2024, 1, 15, 10, 0, 0)),
            at(2024, 1, 15, 10, 0, 30)
        );
        assert_eq!(
            SONARR_RENAME.next_run(at(2024, 1, 15, 10, 0, 29)),
            at(2024, 1, 15, 10, 0, 30)
        );
        assert_eq!(
            SONARR_RENAME.next_run(at(2024, 1, 15, 10, 0, 30)),
            at(2024, 1, 15, 10, 1, 0)
        );
        // Sub-second instants still advance to the next whole boundary.
        let after = at(2024, 1, 15, 10, 0, 29) + ChronoDuration::milliseconds(500);
        assert_eq!(SONARR_RENAME.next_run(after), at(2024, 1, 15, 10, 0, 30));
    }

    #[test]
    fn per_minute_and_half_hour_schedules_align_to_wall_clock() {
        assert_eq!(
            RADARR_RENAME.next_run(at(2024, 1, 15, 10, 0, 0)),
            at(2024, 1, 15, 10, 1, 0)
        );
        assert_eq!(
            RADARR_RENAME.next_run(at(2024, 1, 15, 10, 0, 59)),
            at(2024, 1, 15, 10, 1, 0)
        );
        assert_eq!(
            DOWNLOADER_LOGIN.next_run(at(2024, 1, 15, 10, 0, 0)),
            at(2024, 1, 15, 10, 30, 0)
        );
        assert_eq!(
            DOWNLOADER_LOGIN.next_run(at(2024, 1, 15, 10, 29, 59)),
            at(2024, 1, 15, 10, 30, 0)
        );
        assert_eq!(
            DOWNLOADER_LOGIN.next_run(at(2024, 1, 15, 10, 30, 0)),
            at(2024, 1, 15, 11, 0, 0)
        );
    }

    #[test]
    fn next_run_is_strictly_after_and_keeps_zero_nanoseconds() {
        let after = at(2024, 1, 15, 10, 59, 59) + ChronoDuration::nanoseconds(999_999_999);
        let next = SONARR_TITLE.next_run(after);
        assert!(next > after);
        assert_eq!(next.nanosecond(), 0);
        assert_eq!(next, at(2024, 1, 15, 11, 0, 0));
    }
}
