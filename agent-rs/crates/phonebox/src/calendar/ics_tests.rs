//! `IcsTests.cs` (29) and `CalendarOverlapTests.cs` (2). The zones are passed in (UTC unless a test names one), so
//! results never depend on the server's zone.

use super::*;

fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(y, mo, d)
        .unwrap()
        .and_hms_opt(h, mi, s)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn day(y: i32, mo: u32, d: u32) -> i64 {
    ms(y, mo, d, 0, 0, 0)
}

const DAY: i64 = 86_400_000;

/// Monday 9 November 2026, and two weeks on.
fn from() -> i64 {
    day(2026, 11, 9)
}
fn to() -> i64 {
    from() + 14 * DAY
}

fn cal(events: &[&str]) -> String {
    let body: String = events
        .iter()
        .map(|e| {
            format!(
                "BEGIN:VEVENT\r\n{}\r\nEND:VEVENT\r\n",
                e.trim().replace('\n', "\r\n")
            )
        })
        .collect();
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n")
}

fn parse_utc(text: &str, from: i64, to: i64) -> Vec<CalendarEvent> {
    parse(text, from, to, &TimeZone::UTC)
}

fn zone(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap()
}

/// `DateTime.AddMonths` / `AddYears` on a UTC midnight, for the windows the C# tests build.
fn plus_months(ms_value: i64, months: i32) -> i64 {
    let d = chrono::DateTime::from_timestamp_millis(ms_value)
        .unwrap()
        .naive_utc();
    let shifted = if months >= 0 {
        d.checked_add_months(chrono::Months::new(months as u32))
            .unwrap()
    } else {
        d.checked_sub_months(chrono::Months::new((-months) as u32))
            .unwrap()
    };
    shifted.and_utc().timestamp_millis()
}

fn md(ms_value: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms_value)
        .unwrap()
        .format("%m-%d")
        .to_string()
}

fn ymd(ms_value: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms_value)
        .unwrap()
        .format("%Y-%m-%d")
        .to_string()
}

fn day_of_month(ms_value: i64) -> u32 {
    use chrono::Datelike;
    chrono::DateTime::from_timestamp_millis(ms_value)
        .unwrap()
        .day()
}

fn single(events: Vec<CalendarEvent>) -> CalendarEvent {
    assert_eq!(events.len(), 1, "{events:?}");
    events.into_iter().next().unwrap()
}

#[test]
fn single_event_in_utc() {
    let e = single(parse_utc(
        &cal(&[
            "UID:1\nDTSTART:20261110T090000Z\nDTEND:20261110T093000Z\nSUMMARY:Stand-up\\, team\nLOCATION:Room 1",
        ]),
        from(),
        to(),
    ));
    assert_eq!(e.title, "Stand-up, team");
    assert_eq!(e.location.as_deref(), Some("Room 1"));
    assert_eq!(e.start, ms(2026, 11, 10, 9, 0, 0));
    assert_eq!(e.end - e.start, 30 * 60_000);
    assert!(!e.all_day);
    assert_eq!((e.calendar.as_str(), e.color.as_str()), ("", ""));
}

#[test]
fn escaped_backslash_is_not_read_as_a_line_break() {
    // RFC 5545 TEXT: "\\" is one backslash, so "\\new" is a backslash and "new", not a line break and "ew"
    let e = single(parse_utc(
        &cal(&[&format!(
            "UID:1\nDTSTART:20261110T090000Z\n{}\n{}",
            r"SUMMARY:Fix C:\\new\\build\, then\ntest", r"LOCATION:\\\\Nas\\Meeting"
        )]),
        from(),
        to(),
    ));
    assert_eq!(e.title, r"Fix C:\new\build, then test");
    assert_eq!(e.location.as_deref(), Some(r"\\Nas\Meeting"));
}

#[test]
fn folded_lines_are_joined() {
    let e = single(parse_utc(
        &cal(&["UID:1\nDTSTART:20261110T090000Z\nSUMMARY:A very long\n  title"]),
        from(),
        to(),
    ));
    assert_eq!(e.title, "A very long title");
}

#[test]
fn alarm_repeat_duration_does_not_shorten_its_all_day_event() {
    let e = single(parse_utc(
        &cal(&["UID:all-day
DTSTART;VALUE=DATE:20261110
SUMMARY:Day off
BEGIN:VALARM
ACTION:DISPLAY
TRIGGER:-PT15M
REPEAT:2
DURATION:PT15M
DESCRIPTION:Reminder
END:VALARM"]),
        from(),
        to(),
    ));
    assert!(e.all_day);
    assert_eq!(e.end - e.start, DAY);
}

#[test]
fn unrepresentable_duration_does_not_discard_the_calendar() {
    for duration in ["PT2147483648H", "P1000000000W", "P4000000D", "PT\u{0661}H"] {
        let events = parse_utc(
            &cal(&[
                &format!(
                    "UID:bad\nDTSTART;VALUE=DATE:20261110\nDURATION:{duration}\nSUMMARY:Day off"
                ),
                "UID:next\nDTSTART:20261111T090000Z\nDURATION:PT45M\nSUMMARY:Meeting",
            ]),
            from(),
            to(),
        );
        assert_eq!(events.len(), 2, "{duration}");
        assert_eq!(events[0].end - events[0].start, DAY, "{duration}");
        assert_eq!(events[1].end - events[1].start, 45 * 60_000, "{duration}");
    }
}

#[test]
fn all_day_event() {
    let e = single(parse_utc(
        &cal(&["UID:1\nDTSTART;VALUE=DATE:20261112\nDTEND;VALUE=DATE:20261113\nSUMMARY:Holiday"]),
        from(),
        to(),
    ));
    assert!(e.all_day);
    assert_eq!(e.end - e.start, DAY);
}

#[test]
fn time_zone_is_applied() {
    // 10:00 in Tallinn in November (UTC+2) is 08:00 UTC
    let e = single(parse_utc(
        &cal(&[
            "UID:1\nDTSTART;TZID=Europe/Tallinn:20261110T100000\nDTEND;TZID=Europe/Tallinn:20261110T110000\nSUMMARY:Call",
        ]),
        from(),
        to(),
    ));
    assert_eq!(e.start, ms(2026, 11, 10, 8, 0, 0));
}

#[test]
fn weekly_by_day_with_exception_and_moved_occurrence() {
    let events = parse_utc(
        &cal(&[
            "UID:w\nDTSTART:20261001T070000Z\nDTEND:20261001T073000Z\nRRULE:FREQ=WEEKLY;BYDAY=MO,TH\nEXDATE:20261112T070000Z\nSUMMARY:Sync",
            "UID:w\nRECURRENCE-ID:20261116T070000Z\nDTSTART:20261116T120000Z\nDTEND:20261116T123000Z\nSUMMARY:Sync (moved)",
        ]),
        from(),
        to(),
    );
    let mut starts: Vec<(i64, &str)> = events.iter().map(|e| (e.start, e.title.as_str())).collect();
    starts.sort();
    assert_eq!(
        starts,
        [
            (ms(2026, 11, 9, 7, 0, 0), "Sync"),
            // Thursday the 12th is excluded
            (ms(2026, 11, 16, 12, 0, 0), "Sync (moved)"),
            (ms(2026, 11, 19, 7, 0, 0), "Sync"),
        ]
    );
}

#[test]
fn daily_stand_up_from_years_ago_still_shows() {
    let events = parse_utc(
        &cal(&[
            "UID:d\nDTSTART:20150105T080000Z\nDTEND:20150105T081500Z\nRRULE:FREQ=DAILY\nSUMMARY:Daily",
        ]),
        from(),
        to(),
    );
    assert_eq!(events.len(), 14);
    assert_eq!(events[0].start, ms(2026, 11, 9, 8, 0, 0));
}

#[test]
fn count_and_until_end_the_series() {
    let counted = parse_utc(
        &cal(&["UID:c\nDTSTART:20261109T100000Z\nRRULE:FREQ=DAILY;COUNT=3\nSUMMARY:Three"]),
        from(),
        to(),
    );
    assert_eq!(counted.len(), 3);
    let until = parse_utc(
        &cal(&[
            "UID:u\nDTSTART:20261109T100000Z\nRRULE:FREQ=DAILY;UNTIL=20261111T235959Z\nSUMMARY:Until",
        ]),
        from(),
        to(),
    );
    assert_eq!(until.len(), 3);
}

#[test]
fn recurrence_tokens_are_case_insensitive() {
    for (rule, mixed_envelope) in [
        ("FREQ=WEEKLY;BYDAY=MO,WE;COUNT=3", false),
        ("freq=weekly;byday=mo,we;count=3", false),
        ("FrEq=Weekly;ByDaY=Mo,wE;COUNT=3", true),
    ] {
        let mut calendar = cal(&[&format!(
            "UID:case\nDTSTART:20261109T090000Z\nRRULE:{rule}\nSUMMARY:Keep My Case"
        )]);
        if mixed_envelope {
            calendar = calendar
                .replace("BEGIN:VEVENT", "begin:vEvent")
                .replace("END:VEVENT", "End:vevent");
        }
        let result = parse_utc(&calendar, from(), to());
        assert!(result.iter().all(|e| e.title == "Keep My Case"));
        let actual: Vec<String> = result
            .iter()
            .map(|e| day_of_month(e.start).to_string())
            .collect();
        assert_eq!(actual.join(","), "9,11,16", "{rule}");
    }
}

#[test]
fn an_invalid_recurrence_rule_does_not_discard_other_events() {
    for rule in [
        "FREQ=DAILY;COUNT=1",
        "FREQ=DAILY;COUNT=1;COUNT=2",
        "FREQ=WEEKLY;freq=DAILY",
    ] {
        let events = parse_utc(
            &cal(&[
                &format!("UID:one\nDTSTART:20261109T090000Z\nRRULE:{rule}\nSUMMARY:First"),
                "UID:two\nDTSTART:20261110T100000Z\nSUMMARY:Second",
            ]),
            from(),
            to(),
        );
        let titles: Vec<&str> = events.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["First", "Second"], "{rule}");
    }
}

#[test]
fn daily_rules_limit_candidates_to_their_month_days() {
    for (start, filter, expected) in [
        ("20261101", "", "11-01,11-02,11-03"),
        ("20261101", ";BYMONTHDAY=1", "11-01,12-01,01-01"),
        ("20261130", ";BYMONTHDAY=-1", "11-30,12-31,01-31"),
    ] {
        let from = day(2026, 11, 1);
        let events = parse_utc(
            &cal(&[&format!(
                "UID:m\nDTSTART;VALUE=DATE:{start}\nRRULE:FREQ=DAILY;COUNT=3{filter}"
            )]),
            from,
            plus_months(from, 4),
        );
        let actual: Vec<String> = events.iter().map(|e| md(e.start)).collect();
        assert_eq!(actual.join(","), expected, "{start} {filter}");
    }
}

#[test]
fn daily_with_by_day_keeps_only_those_weekdays() {
    use chrono::{Datelike, Weekday};
    // Thunderbird writes "every weekday" as FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR (RFC 5545: BYDAY limits a DAILY rule)
    let weekdays: Vec<i64> = parse_utc(
        &cal(&["UID:d\nDTSTART:20261102T080000Z\nRRULE:FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR\nSUMMARY:Stand-up"]),
        from(),
        to(),
    )
    .iter()
    .map(|e| e.start)
    .collect();
    assert_eq!(weekdays.len(), 10);
    assert!(weekdays.iter().all(|w| {
        let d = chrono::DateTime::from_timestamp_millis(*w)
            .unwrap()
            .weekday();
        d != Weekday::Sat && d != Weekday::Sun
    }));
    assert_eq!(weekdays[0], ms(2026, 11, 9, 8, 0, 0));

    // COUNT counts only the matching days
    let counted: Vec<i64> = parse_utc(
        &cal(&[
            "UID:c\nDTSTART:20261109T080000Z\nRRULE:FREQ=DAILY;BYDAY=MO,WE,FR;COUNT=4\nSUMMARY:Gym",
        ]),
        from(),
        to(),
    )
    .iter()
    .map(|e| e.start)
    .collect();
    assert_eq!(
        counted,
        [
            ms(2026, 11, 9, 8, 0, 0),
            ms(2026, 11, 11, 8, 0, 0),
            ms(2026, 11, 13, 8, 0, 0),
            ms(2026, 11, 16, 8, 0, 0)
        ]
    );

    // INTERVAL still steps in days before the weekday filter: every 2nd day from Mon 9 Nov, weekdays only
    let every_other: Vec<u32> = parse_utc(
        &cal(&["UID:i\nDTSTART:20261109T080000Z\nRRULE:FREQ=DAILY;INTERVAL=2;BYDAY=MO,TU,WE,TH,FR\nSUMMARY:Alt"]),
        from(),
        to(),
    )
    .iter()
    .map(|e| day_of_month(e.start))
    .collect();
    assert_eq!(every_other, [9, 11, 13, 17, 19]);
}

#[test]
fn monthly_rules_combine_weekday_and_month_day() {
    for (first, by_day, by_month_day, dates) in [
        (
            "20240311T090000Z",
            "MO",
            "8,9,10,11,12,13,14",
            "03-11,04-08,05-13",
        ),
        (
            "20240331T090000Z",
            "SU",
            "-7,-6,-5,-4,-3,-2,-1",
            "03-31,04-28,05-26",
        ),
    ] {
        let start = day(2024, 3, 1);
        let events = parse_utc(
            &cal(&[&format!(
                "UID:m\nDTSTART:{first}\nRRULE:FREQ=MONTHLY;BYDAY={by_day};BYMONTHDAY={by_month_day};COUNT=3"
            )]),
            start,
            plus_months(start, 4),
        );
        let actual: Vec<String> = events.iter().map(|e| md(e.start)).collect();
        assert_eq!(actual.join(","), dates, "{first}");
    }
}

#[test]
fn overlapping_monthly_filters_count_distinct_occurrences() {
    for (first, filters, dates) in [
        ("20260101", "BYMONTHDAY=1,-31", "01-01,02-01,03-01"),
        ("20260105", "BYDAY=1MO,MO", "01-05,01-12,01-19"),
    ] {
        let from = day(2026, 1, 1);
        let events = parse_utc(
            &cal(&[&format!(
                "UID:overlap\nDTSTART;VALUE=DATE:{first}\nRRULE:FREQ=MONTHLY;COUNT=3;{filters}"
            )]),
            from,
            plus_months(from, 4),
        );
        let actual: Vec<String> = events.iter().map(|e| md(e.start)).collect();
        assert_eq!(actual.join(","), dates, "{filters}");
    }
}

#[test]
fn yearly_day_filters_without_month_apply_to_the_entire_year() {
    for (first, filters, dates) in [
        ("20250519", "BYDAY=20MO", "2025-05-19,2026-05-18,2027-05-17"),
        ("20251229", "BYDAY=-1MO", "2025-12-29,2026-12-28,2027-12-27"),
        (
            "20260131",
            "BYMONTHDAY=31",
            "2026-01-31,2026-03-31,2026-05-31",
        ),
    ] {
        let from = day(2025, 1, 1);
        let events = parse_utc(
            &cal(&[&format!(
                "UID:year\nDTSTART;VALUE=DATE:{first}\nRRULE:FREQ=YEARLY;COUNT=3;{filters}"
            )]),
            from,
            plus_months(from, 48),
        );
        let actual: Vec<String> = events.iter().map(|e| ymd(e.start)).collect();
        assert_eq!(actual.join(","), dates, "{filters}");
    }
}

#[test]
fn monthly_second_tuesday() {
    let e = single(parse_utc(
        &cal(&["UID:m\nDTSTART:20260113T150000Z\nRRULE:FREQ=MONTHLY;BYDAY=2TU\nSUMMARY:Review"]),
        from(),
        to(),
    ));
    assert_eq!(e.start, ms(2026, 11, 10, 15, 0, 0));
}

#[test]
fn month_filters_limit_non_yearly_rules_before_count() {
    for (frequency, start, expected) in [
        ("DAILY", "20261130", "2026-11-30,2027-11-01"),
        ("WEEKLY", "20261130", "2026-11-30,2027-11-01"),
        ("MONTHLY", "20261101", "2026-11-01,2027-11-01"),
    ] {
        let from = day(2026, 11, 1);
        let events = parse_utc(
            &cal(&[&format!(
                "UID:x\nDTSTART;VALUE=DATE:{start}\nRRULE:FREQ={frequency};BYMONTH=11;COUNT=2"
            )]),
            from,
            plus_months(from, 24),
        );
        let actual: Vec<String> = events.iter().map(|e| ymd(e.start)).collect();
        assert_eq!(actual.join(","), expected, "{frequency}");
    }
}

#[test]
fn yearly_by_month_and_by_day_picks_the_right_day() {
    // Outlook / Google write "yearly on the last Thursday of November" as FREQ=YEARLY;BYMONTH=11;BYDAY=-1TH
    let nov = day(2026, 11, 1);
    let thanksgiving = single(parse_utc(
        &cal(&[
            "UID:t\nDTSTART:20251127T120000Z\nRRULE:FREQ=YEARLY;BYMONTH=11;BYDAY=-1TH\nSUMMARY:Dinner",
        ]),
        nov,
        nov + 30 * DAY,
    ));
    assert_eq!(thanksgiving.start, ms(2026, 11, 26, 12, 0, 0));

    // second Sunday of May, across the year boundary of the series
    let may = day(2026, 5, 1);
    let mothers_day = single(parse_utc(
        &cal(&[
            "UID:m\nDTSTART:20250511T090000Z\nRRULE:FREQ=YEARLY;BYDAY=2SU;BYMONTH=5\nSUMMARY:Mother's Day",
        ]),
        may,
        may + 30 * DAY,
    ));
    assert_eq!(mothers_day.start, ms(2026, 5, 10, 9, 0, 0));

    // several months without BYDAY keep the start's day of the month: 15 March and 15 September
    let jan = day(2026, 1, 1);
    let twice: Vec<i64> = parse_utc(
        &cal(&["UID:s\nDTSTART:20250315T080000Z\nRRULE:FREQ=YEARLY;BYMONTH=3,9\nSUMMARY:Service"]),
        jan,
        plus_months(jan, 12),
    )
    .iter()
    .map(|e| e.start)
    .collect();
    assert_eq!(twice, [ms(2026, 3, 15, 8, 0, 0), ms(2026, 9, 15, 8, 0, 0)]);
}

#[test]
fn biweekly_rule_uses_its_week_start() {
    // RFC 5545 3.8.5.3: "An example where the days generated makes a difference because of WKST"
    for (wkst, august) in [("WKST=MO", [5, 10, 19, 24]), ("WKST=SU", [5, 17, 19, 31])] {
        let events = parse_utc(
            &cal(&[&format!(
                "UID:x\nDTSTART:19970805T130000Z\nRRULE:FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;{wkst}\nSUMMARY:Biweekly"
            )]),
            day(1997, 8, 1),
            day(1997, 10, 1),
        );
        let days: Vec<u32> = events.iter().map(|e| day_of_month(e.start)).collect();
        assert_eq!(days, august, "{wkst}");
    }
}

#[test]
fn by_set_pos_picks_from_each_months_set() {
    // RFC 5545 3.8.5.3 examples, and Outlook's "last weekday of every month"
    for (start, rrule, dates) in [
        (
            "19970904T130000Z",
            "FREQ=MONTHLY;COUNT=3;BYDAY=TU,WE,TH;BYSETPOS=3",
            "09-04,10-07,11-06",
        ),
        (
            "19970929T130000Z",
            "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-2",
            "09-29,10-30,11-27,12-30",
        ),
        (
            "19970930T130000Z",
            "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
            "09-30,10-31,11-28,12-31",
        ),
    ] {
        let events = parse_utc(
            &cal(&[&format!("UID:x\nDTSTART:{start}\nRRULE:{rrule}\nSUMMARY:S")]),
            day(1997, 9, 1),
            day(1998, 1, 1),
        );
        let actual: Vec<String> = events.iter().map(|e| md(e.start)).collect();
        assert_eq!(actual.join(","), dates, "{rrule}");
    }
}

#[test]
fn leap_day_birthday_only_happens_in_leap_years() {
    // RFC 5545: invalid dates (29 February in 2026) are ignored, not moved to the 28th
    let birthday =
        cal(&["UID:b\nDTSTART;VALUE=DATE:20240229\nRRULE:FREQ=YEARLY\nSUMMARY:Birthday"]);
    assert!(parse_utc(&birthday, day(2026, 1, 1), day(2027, 1, 1)).is_empty());
    assert_eq!(
        single(parse_utc(&birthday, day(2028, 1, 1), day(2029, 1, 1))).start,
        day(2028, 2, 29)
    );
    // COUNT counts the real occurrences
    let counted: Vec<String> = parse_utc(
        &cal(&["UID:c\nDTSTART:20240229T090000Z\nRRULE:FREQ=YEARLY;COUNT=3\nSUMMARY:Leap"]),
        day(2024, 1, 1),
        day(2040, 1, 1),
    )
    .iter()
    .map(|e| ymd(e.start)[..4].to_string())
    .collect();
    assert_eq!(counted, ["2024", "2028", "2032"]);
}

#[test]
fn cancelled_events_are_skipped() {
    assert!(
        parse_utc(
            &cal(&["UID:x\nDTSTART:20261110T090000Z\nSTATUS:CANCELLED\nSUMMARY:Gone"]),
            from(),
            to()
        )
        .is_empty()
    );
}

#[test]
fn ambiguous_fall_back_times_use_the_first_occurrence() {
    let from = day(2026, 11, 1);
    let events = parse_utc(
        &cal(&[
            "UID:single\nDTSTART;TZID=America/New_York:20261101T013000\nSUMMARY:Single",
            "UID:weekly\nDTSTART;TZID=America/New_York:20261025T013000\nRRULE:FREQ=WEEKLY;COUNT=2\nSUMMARY:Weekly",
        ]),
        from,
        from + DAY,
    );
    assert_eq!(events.len(), 2);
    assert!(
        events.iter().all(|e| e.start == ms(2026, 11, 1, 5, 30, 0)),
        "{events:?}"
    );
}

#[test]
fn wall_clock_time_survives_daylight_saving_change() {
    // weekly at 09:00 Berlin time across the end of DST (25 Oct 2026): 07:00 UTC before, 08:00 UTC after
    let from = day(2026, 10, 19);
    let events = parse_utc(
        &cal(&[
            "UID:b\nDTSTART;TZID=Europe/Berlin:20261019T090000\nRRULE:FREQ=WEEKLY\nSUMMARY:Weekly",
        ]),
        from,
        from + 10 * DAY,
    );
    assert_eq!(events[0].start, ms(2026, 10, 19, 7, 0, 0));
    assert_eq!(events[1].start, ms(2026, 10, 26, 8, 0, 0));
}

#[test]
fn local_time_series_keeps_its_wall_clock_across_daylight_saving() {
    // floating times and TZIDs the computer does not know are the computer's own time: 09:00 in winter and in summer
    let berlin = zone("Europe/Berlin");
    let winter = day(2026, 3, 9);
    let summer = day(2026, 7, 6);
    for start in [
        "DTSTART:20260302T090000",
        "DTSTART;TZID=Customized Time Zone:20260302T090000",
    ] {
        let ics = cal(&[&format!(
            "UID:f\n{start}\nRRULE:FREQ=WEEKLY\nSUMMARY:Standup"
        )]);
        assert_eq!(
            single(parse(&ics, winter, winter + 7 * DAY, &berlin)).start,
            ms(2026, 3, 9, 8, 0, 0)
        ); // 09:00 CET
        assert_eq!(
            single(parse(&ics, summer, summer + 7 * DAY, &berlin)).start,
            ms(2026, 7, 6, 7, 0, 0)
        ); // 09:00 CEST
    }
    // a UTC series still repeats in UTC
    let utc = single(parse(
        &cal(&["UID:u\nDTSTART:20260302T080000Z\nRRULE:FREQ=WEEKLY\nSUMMARY:Sync"]),
        summer,
        summer + 7 * DAY,
        &berlin,
    ));
    assert_eq!(utc.start, ms(2026, 7, 6, 8, 0, 0));
}

#[test]
fn a_huge_interval_cannot_take_down_the_feed() {
    let events = parse_utc(
        &cal(&[
            "UID:1\nDTSTART:20261110T090000Z\nSUMMARY:Daily\nRRULE:FREQ=DAILY;INTERVAL=2000000000",
            "UID:2\nDTSTART:20261110T090000Z\nSUMMARY:Monthly\nRRULE:FREQ=MONTHLY;INTERVAL=2000000",
            "UID:3\nDTSTART:20261110T090000Z\nSUMMARY:Yearly\nRRULE:FREQ=YEARLY;INTERVAL=20000",
            "UID:4\nDTSTART:20261111T100000Z\nSUMMARY:Review",
        ]),
        from(),
        to(),
    );
    assert!(events.iter().any(|e| e.title == "Review"));
    // each still yields its DTSTART
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.title.as_str(), "Daily" | "Monthly" | "Yearly"))
            .count(),
        3
    );
}

#[test]
fn a_rule_that_runs_into_the_year9999_only_stops_its_own_event() {
    // within the interval bounds, but a weekday or a time of day near the end of the calendar still steps past
    // DateTime.MaxValue: that event stops repeating, the rest of the feed stays
    let events = parse_utc(
        &cal(&[
            "UID:1\nDTSTART:20261110T090000Z\nSUMMARY:Weekly\nRRULE:FREQ=WEEKLY;INTERVAL=416018;BYDAY=TU,SU",
            "UID:2\nDTSTART:20261110T230000Z\nDURATION:PT2H\nSUMMARY:Daily\nRRULE:FREQ=DAILY;INTERVAL=2912410",
            "UID:3\nDTSTART:20261111T100000Z\nSUMMARY:Review",
        ]),
        from(),
        to(),
    );
    assert!(events.iter().any(|e| e.title == "Review"));
    assert!(events.iter().any(|e| e.title == "Weekly"));
    assert!(events.iter().any(|e| e.title == "Daily"));
}

#[test]
fn recurrence_fast_forward_retains_every_overlapping_occurrence() {
    for (days, expected) in [
        (0, "10"),
        (1, "10"),
        (2, "9,10"),
        (3, "8,9,10"),
        (4, "7,8,9,10"),
        (10, "1,2,3,4,5,6,7,8,9,10"),
    ] {
        let from = day(2026, 1, 10);
        let feed = format!(
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:long\nDTSTART:20260101T000000Z\nDURATION:P{days}D\nRRULE:FREQ=DAILY\nSUMMARY:Long\nEND:VEVENT\nEND:VCALENDAR\n"
        );
        let actual: Vec<String> = parse_utc(&feed, from, from + DAY)
            .iter()
            .map(|e| day_of_month(e.start).to_string())
            .collect();
        assert_eq!(actual.join(","), expected, "{days}");
    }
}

#[test]
fn a_long_all_day_occurrence_starting_before_the_window_is_kept() {
    let from = day(2026, 1, 10);
    let feed = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:all\nDTSTART;VALUE=DATE:20260101\nDTEND;VALUE=DATE:20260105\nRRULE:FREQ=DAILY\nSUMMARY:All day\nEND:VEVENT\nEND:VCALENDAR\n";
    let events = parse_utc(feed, from, from + DAY);
    let days: Vec<u32> = events.iter().map(|e| day_of_month(e.start)).collect();
    assert_eq!(days, [7, 8, 9, 10]);
    assert!(events.iter().all(|e| e.all_day));
}

#[test]
fn zones_are_found_by_iana_name_a_mozilla_prefix_or_quotes_and_windows_ids_only_on_windows() {
    let tallinn = |tzid: &str| {
        let events = parse_utc(
            &cal(&[&format!(
                "UID:1\nDTSTART;TZID={tzid}:20261110T100000\nSUMMARY:Call"
            )]),
            from(),
            to(),
        );
        single(events).start
    };
    let eight = ms(2026, 11, 10, 8, 0, 0);
    assert_eq!(tallinn("/mozilla.org/20050126_1/Europe/Tallinn"), eight);
    assert_eq!(tallinn("\"Europe/Tallinn\""), eight);
    assert_eq!(tallinn("europe/tallinn"), eight);
    // C# on Linux and macOS (InvariantGlobalization) does not know Windows ids: the floating zone (UTC here) applies
    let berlin = if cfg!(windows) {
        ms(2026, 11, 10, 9, 0, 0)
    } else {
        ms(2026, 11, 10, 10, 0, 0)
    };
    assert_eq!(tallinn("W. Europe Standard Time"), berlin);
    assert_eq!(
        super::super::windows_zones::iana("w. europe standard time"),
        Some("Europe/Berlin")
    );
    assert_eq!(super::super::windows_zones::WINDOWS_ZONES.len(), 138);
}

#[test]
fn a_time_in_a_spring_forward_gap_moves_an_hour_later() {
    // 02:30 on 8 March 2026 does not exist in New York: C# reads it as 03:30 EDT
    let e = single(parse_utc(
        &cal(&["UID:g\nDTSTART;TZID=America/New_York:20260308T023000\nSUMMARY:Gap"]),
        day(2026, 3, 8),
        day(2026, 3, 9),
    ));
    assert_eq!(e.start, ms(2026, 3, 8, 7, 30, 0));
}

#[test]
fn content_line_edges_match_the_csharp_reader() {
    let events = parse_utc(
        &cal(&[
            // a quoted parameter may hold a colon; LOCATION may unescape to ""; an unknown escape keeps both characters
            "UID:1\nLOCATION: \nDTSTART;X-NOTE=\"a:b\":20261110T090000Z\nSUMMARY:Tab\\tkeeps\\",
            // no SUMMARY; a negative duration; a zero-length event exactly at the window start is kept
            "UID:2\nDTSTART:20261110T100000Z\nDURATION:-PT30M",
            "UID:3\nDTSTART:20261109T000000Z",
            // COUNT=0 means no occurrence at all; an unparsable COUNT means unbounded
            "UID:4\nDTSTART:20261110T090000Z\nRRULE:FREQ=DAILY;COUNT=0\nSUMMARY:None",
            "UID:5\nDTSTART:20261121T090000Z\nRRULE:FREQ=DAILY;COUNT=x\nSUMMARY:Many",
        ]),
        from(),
        to(),
    );
    assert_eq!(events[0].title, "Tab\\tkeeps\\");
    assert_eq!(events[0].location.as_deref(), Some(""));
    assert_eq!(events[1].title, "(no title)");
    assert_eq!(events[1].end - events[1].start, -30 * 60_000);
    assert_eq!(events[2].start, from());
    assert!(!events.iter().any(|e| e.title == "None"));
    assert_eq!(events.iter().filter(|e| e.title == "Many").count(), 2); // 21 and 22 November
}
