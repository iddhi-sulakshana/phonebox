//! `Ics`: a small iCalendar reader: VEVENTs with time zones and the common recurrence rules.
//!
//! Times are .NET `DateTime` ticks (100 ns since 0001-01-01) up to 9999-12-31T23:59:59.9999999, so a rule that steps
//! past that limit ends its own event where C#'s `ArgumentOutOfRangeException` did, and the rest of the feed stays.

use std::collections::{HashMap, HashSet};

use jiff::tz::{AmbiguousOffset, TimeZone};

use super::CalendarEvent;

const TICKS_PER_SECOND: i64 = 10_000_000;
const TICKS_PER_HOUR: i64 = 3600 * TICKS_PER_SECOND;
const TICKS_PER_DAY: i64 = 24 * TICKS_PER_HOUR;
/// `DateTime.MaxValue.Ticks`.
const MAX_TICKS: i64 = 3_155_378_975_999_999_999;
/// 1970-01-01 in ticks.
const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;

/// A `DateTime` (wall clock or UTC, by context) in ticks, `0..=MAX_TICKS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Dt(i64);

/// `ArgumentOutOfRangeException`: a date past the calendar's ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutOfRange;

/// One content line: `NAME;PARAM=VALUE:value`.
struct Prop {
    name: String,
    params: HashMap<String, String>,
    value: String,
}

/// The events of `text` with an occurrence overlapping `[from_ms, to_ms)`; `local` is the zone of floating times and
/// of TZIDs this computer does not know (the computer's own zone).
pub fn parse(text: &str, from_ms: i64, to_ms: i64, local: &TimeZone) -> Vec<CalendarEvent> {
    let from = dt_of_ms(from_ms);
    let to = dt_of_ms(to_ms);
    let mut events: Vec<Vec<Prop>> = Vec::new();
    let mut cur: Option<Vec<Prop>> = None;
    let mut nested = 0usize;
    for line in unfold(text) {
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            cur = Some(Vec::new());
            nested = 0;
            continue;
        }
        if line.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(done) = cur.take() {
                events.push(done);
            }
            continue;
        }
        let Some(props) = cur.as_mut() else {
            continue;
        };
        // a VALARM can have its own DURATION and other event-like properties
        if starts_with_ignore_case(&line, "BEGIN:") {
            nested += 1;
            continue;
        }
        if starts_with_ignore_case(&line, "END:") {
            nested = nested.saturating_sub(1);
            continue;
        }
        if nested > 0 {
            continue;
        }
        if let Some(p) = parse_line(&line) {
            props.push(p);
        }
    }

    // moved or cancelled occurrences of recurring events: UID + original start
    let mut overrides: HashSet<(String, i64)> = HashSet::new();
    for e in &events {
        if let (Some(rid), Some(uid)) = (first(e, "RECURRENCE-ID"), first(e, "UID"))
            && let Some((r, _)) = to_utc(&rid.value, &rid.params, local)
        {
            overrides.insert((uid.value.clone(), r.0));
        }
    }

    let mut result = Vec::new();
    for e in &events {
        if first(e, "STATUS").is_some_and(|p| p.value.eq_ignore_ascii_case("CANCELLED")) {
            continue;
        }
        let Some(dt_start) = first(e, "DTSTART") else {
            continue;
        };
        let Some((start, all_day)) = to_utc(&dt_start.value, &dt_start.params, local) else {
            continue;
        };
        let remaining = MAX_TICKS - start.0;
        let end = first(e, "DTEND")
            .and_then(|p| to_utc(&p.value, &p.params, local))
            .map(|(en, _)| en)
            .or_else(|| {
                let d = try_duration(&first(e, "DURATION")?.value)?;
                (d >= -start.0 && d <= remaining).then(|| Dt(start.0 + d))
            })
            .unwrap_or(if all_day {
                Dt(start.0 + TICKS_PER_DAY.min(remaining))
            } else {
                start
            });
        let length = end.0 - start.0;
        let title = unescape(first(e, "SUMMARY").map_or("(no title)", |p| p.value.as_str()));
        let location = first(e, "LOCATION")
            .filter(|p| !p.value.is_empty())
            .map(|p| unescape(&p.value));
        let uid = first(e, "UID").map_or("", |p| p.value.as_str());
        let rrule = first(e, "RRULE").map(|p| p.value.as_str());
        let is_override = e.iter().any(|p| p.name == "RECURRENCE-ID");

        let mut starts = vec![start];
        if let Some(rrule) = rrule
            && !is_override
        {
            let exdates: HashSet<i64> = e
                .iter()
                .filter(|p| p.name == "EXDATE")
                .flat_map(|p| {
                    p.value
                        .split(',')
                        .filter_map(|v| to_utc(v, &p.params, local))
                        .map(|(x, _)| x.0)
                })
                .collect();
            // a local start (floating, or a TZID this computer does not know) keeps its wall-clock time in the
            // computer's zone, like to_utc reads it; only a UTC start ("...Z") repeats in UTC
            let zone = if dt_start.value.trim().ends_with('Z') {
                None
            } else {
                Some(
                    dt_start
                        .params
                        .get("TZID")
                        .and_then(|tz| find_zone(tz))
                        .unwrap_or_else(|| local.clone()),
                )
            };
            // older occurrences may still overlap the window: fast-forward from their earliest possible start
            let earliest = Dt(from.0 - from.0.min(length.max(0)));
            let mut expanded = Vec::new();
            // a rule that steps past the year 9999 ends that event's repeats there (C#'s ArgumentOutOfRangeException)
            let _ = expand(
                start,
                rrule,
                zone.as_ref(),
                all_day,
                earliest,
                to,
                local,
                &mut expanded,
            );
            starts = expanded
                .into_iter()
                .filter(|s| !exdates.contains(&s.0) && !overrides.contains(&(uid.to_string(), s.0)))
                .collect();
        }
        for s in starts {
            // the event's length past the year 9999 ends its repeats there; the occurrences before it stay
            let Ok(f) = s.add_ticks(length) else {
                break;
            };
            if f <= from && !(length == 0 && s >= from) {
                continue;
            }
            if s >= to {
                break;
            }
            result.push(CalendarEvent {
                title: title.clone(),
                start: ms_of(s),
                end: ms_of(f),
                all_day,
                location: location.clone(),
                calendar: String::new(),
                color: String::new(),
            });
        }
    }
    result
}

fn first<'a>(e: &'a [Prop], name: &str) -> Option<&'a Prop> {
    e.iter().find(|p| p.name == name)
}

fn starts_with_ignore_case(line: &str, prefix: &str) -> bool {
    line.len() >= prefix.len()
        && line.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn dt_of_ms(ms: i64) -> Dt {
    Dt(ms
        .saturating_mul(10_000)
        .saturating_add(UNIX_EPOCH_TICKS)
        .clamp(0, MAX_TICKS))
}

/// `DateTimeOffset.ToUnixTimeMilliseconds`.
fn ms_of(utc: Dt) -> i64 {
    utc.0 / 10_000 - UNIX_EPOCH_TICKS / 10_000
}

impl Dt {
    fn from_ymd(y: i64, m: u32, d: u32) -> Option<Dt> {
        if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
            return None;
        }
        Some(Dt(days_from_civil(y, m, d) * TICKS_PER_DAY))
    }

    fn days(self) -> i64 {
        self.0 / TICKS_PER_DAY
    }

    fn date(self) -> Dt {
        Dt(self.days() * TICKS_PER_DAY)
    }

    fn time_of_day(self) -> i64 {
        self.0 % TICKS_PER_DAY
    }

    fn ymd(self) -> (i64, u32, u32) {
        civil_from_days(self.days())
    }

    fn year(self) -> i64 {
        self.ymd().0
    }

    fn month(self) -> u32 {
        self.ymd().1
    }

    fn day(self) -> u32 {
        self.ymd().2
    }

    /// `DayOfWeek`: Sunday = 0 (0001-01-01 was a Monday).
    fn day_of_week(self) -> u32 {
        ((self.days() + 1) % 7) as u32
    }

    fn add_ticks(self, t: i64) -> Result<Dt, OutOfRange> {
        let v = self.0.checked_add(t).ok_or(OutOfRange)?;
        if (0..=MAX_TICKS).contains(&v) {
            Ok(Dt(v))
        } else {
            Err(OutOfRange)
        }
    }

    fn add_days(self, n: i64) -> Result<Dt, OutOfRange> {
        self.add_ticks(n.checked_mul(TICKS_PER_DAY).ok_or(OutOfRange)?)
    }

    /// `AddMonths`: the day is clamped to the target month's length.
    fn add_months(self, months: i64) -> Result<Dt, OutOfRange> {
        if !(-120_000..=120_000).contains(&months) {
            return Err(OutOfRange);
        }
        let (y, m, d) = self.ymd();
        let total = y * 12 + i64::from(m) - 1 + months;
        let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
        if !(1..=9999).contains(&ny) {
            return Err(OutOfRange);
        }
        let nd = d.min(days_in_month(ny, nm));
        Ok(Dt(
            days_from_civil(ny, nm, nd) * TICKS_PER_DAY + self.time_of_day()
        ))
    }

    /// `AddYears`: 29 February becomes the 28th in a common year.
    fn add_years(self, years: i64) -> Result<Dt, OutOfRange> {
        if !(-10_000..=10_000).contains(&years) {
            return Err(OutOfRange);
        }
        let (y, m, d) = self.ymd();
        let ny = y + years;
        if !(1..=9999).contains(&ny) {
            return Err(OutOfRange);
        }
        let nd = if m == 2 && d == 29 && !is_leap(ny) {
            28
        } else {
            d
        };
        Ok(Dt(
            days_from_civil(ny, m, nd) * TICKS_PER_DAY + self.time_of_day()
        ))
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 0001-01-01 (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + 719_162
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days - 719_162 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn civil(l: Dt) -> Option<jiff::civil::DateTime> {
    let (y, m, d) = l.ymd();
    let tod = l.time_of_day();
    let secs = tod / TICKS_PER_SECOND;
    jiff::civil::DateTime::new(
        y as i16,
        m as i8,
        d as i8,
        (secs / 3600) as i8,
        (secs / 60 % 60) as i8,
        (secs % 60) as i8,
        ((tod % TICKS_PER_SECOND) * 100) as i32,
    )
    .ok()
}

fn timestamp(utc: Dt) -> Option<jiff::Timestamp> {
    let rel = utc.0 - UNIX_EPOCH_TICKS;
    jiff::Timestamp::new(
        rel.div_euclid(TICKS_PER_SECOND),
        (rel.rem_euclid(TICKS_PER_SECOND) * 100) as i32,
    )
    .ok()
}

/// `TimeZoneInfo.ConvertTimeFromUtc` (clamped to the calendar's ends, as .NET does).
fn utc_to_wall(utc: Dt, zone: &TimeZone) -> Dt {
    let Some(ts) = timestamp(utc) else {
        return utc;
    };
    let offset = i64::from(zone.to_offset(ts).seconds());
    Dt((utc.0 + offset * TICKS_PER_SECOND).clamp(0, MAX_TICKS))
}

/// `SafeToUtc`: a wall-clock time skipped by a DST change moves an hour later; one that occurs twice is the earlier
/// instant (iCalendar's rule). Clamped to the calendar's ends.
fn safe_to_utc(local: Dt, zone: &TimeZone) -> Dt {
    let offset_of = |l: Dt| civil(l).map(|c| zone.to_ambiguous_timestamp(c).offset());
    let mut l = local;
    let mut found = offset_of(l);
    if matches!(found, Some(AmbiguousOffset::Gap { .. }))
        && let Ok(later) = l.add_ticks(TICKS_PER_HOUR)
    {
        l = later;
        found = offset_of(l);
    }
    let offset = match found {
        Some(AmbiguousOffset::Unambiguous { offset }) => offset,
        Some(AmbiguousOffset::Fold { before, after }) => {
            if before.seconds() >= after.seconds() {
                before
            } else {
                after
            }
        }
        // still in a gap after an hour (gaps longer than an hour; C# threw): the offset after it
        Some(AmbiguousOffset::Gap { after, .. }) => after,
        None => return local,
    };
    Dt((l.0 - i64::from(offset.seconds()) * TICKS_PER_SECOND).clamp(0, MAX_TICKS))
}

/// `FindZone`: an IANA name (any case), else its last two `/` segments ("/mozilla.org/20050126_1/Europe/Tallinn");
/// a Windows id only on Windows (C# on Linux and macOS runs with `InvariantGlobalization` and does not know them).
fn find_zone(id: &str) -> Option<TimeZone> {
    let id = id.trim_matches('"');
    let lookup = |name: &str| {
        if cfg!(windows)
            && let Some(iana) = super::windows_zones::iana(name)
        {
            return TimeZone::get(iana).ok();
        }
        TimeZone::get(name).ok()
    };
    lookup(id).or_else(|| {
        let parts: Vec<&str> = id.split('/').collect();
        lookup(&parts[parts.len().saturating_sub(2)..].join("/"))
    })
}

/// A DTSTART-style value in UTC and whether it is a date (all-day: midnight, without a zone).
fn to_utc(value: &str, params: &HashMap<String, String>, local: &TimeZone) -> Option<(Dt, bool)> {
    let v = value.trim();
    let all_day = v.encode_utf16().count() == 8 || params.get("VALUE").is_some_and(|x| x == "DATE");
    if all_day {
        let head: String = v.chars().take(8).collect();
        return parse_date(&head).map(|d| (d, true));
    }
    let t = parse_date_time(v.trim_end_matches('Z'))?;
    if v.ends_with('Z') {
        return Some((t, false));
    }
    let zone = params.get("TZID").and_then(|tz| find_zone(tz));
    Some((safe_to_utc(t, zone.as_ref().unwrap_or(local)), false))
}

fn digits(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// `yyyyMMdd`.
fn parse_date(s: &str) -> Option<Dt> {
    if s.len() != 8 || !s.is_ascii() {
        return None;
    }
    Dt::from_ymd(
        digits(&s[..4])?,
        digits(&s[4..6])? as u32,
        digits(&s[6..])? as u32,
    )
}

/// `yyyyMMdd'T'HHmmss`.
fn parse_date_time(s: &str) -> Option<Dt> {
    if s.len() != 15 || !s.is_ascii() || s.as_bytes()[8] != b'T' {
        return None;
    }
    let date = parse_date(&s[..8])?;
    let (h, m, sec) = (digits(&s[9..11])?, digits(&s[11..13])?, digits(&s[13..])?);
    if h > 23 || m > 59 || sec > 59 {
        return None;
    }
    date.add_ticks((h * 3600 + m * 60 + sec) * TICKS_PER_SECOND)
        .ok()
}

/// `int.TryParse`: white space around (tab to carriage return, and space) and one leading sign.
fn dotnet_int(s: &str) -> Option<i32> {
    s.trim_matches(|c| matches!(c, '\t'..='\r' | ' '))
        .parse()
        .ok()
}

/// `P1DT2H30M`, `PT45M`, `P1W` (ASCII digits; `P` and `PT` alone are zero) in ticks; None when it does not fit a
/// `TimeSpan`.
fn try_duration(v: &str) -> Option<i64> {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^([+-])?P(?:([0-9]+)W)?(?:([0-9]+)D)?(?:T(?:([0-9]+)H)?(?:([0-9]+)M)?(?:([0-9]+)S)?)?\z")
            .expect("duration")
    });
    let m = PATTERN.captures(v)?;
    let group = |i: usize| -> Option<i64> {
        m.get(i)
            .map_or(Some(0), |g| g.as_str().parse::<i32>().ok().map(i64::from))
    };
    let (weeks, days, hours, minutes, seconds) =
        (group(2)?, group(3)?, group(4)?, group(5)?, group(6)?);
    let days = i32::try_from(weeks * 7 + days).ok()?; // checked(W * 7 + D)
    let millis = (i64::from(days) * 86_400 + hours * 3600 + minutes * 60 + seconds) * 1000;
    // TimeSpan's range in milliseconds
    if millis.abs() > i64::MAX / 10_000 {
        return None;
    }
    let ticks = millis * 10_000;
    Some(if m.get(1).is_some_and(|s| s.as_str() == "-") {
        -ticks
    } else {
        ticks
    })
}

/// Content lines: a line starting with a space or a tab continues the one before (that one character dropped).
fn unfold(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for raw in text.replace("\r\n", "\n").split('\n') {
        if raw.starts_with([' ', '\t']) {
            cur.push_str(&raw[1..]);
            continue;
        }
        if !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        cur.push_str(raw);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// `NAME;PARAM=VALUE;PARAM="VA:LUE":value`: the first colon outside quotes ends the name part.
fn parse_line(line: &str) -> Option<Prop> {
    let mut quoted = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == ':' && !quoted {
            colon = Some(i);
            break;
        }
    }
    let colon = colon.filter(|&c| c > 0)?;
    let mut head = line[..colon].split(';');
    let name = head.next()?.to_uppercase();
    let mut params = HashMap::new();
    for part in head {
        if let Some((k, v)) = part.split_once('=') {
            params.insert(k.to_uppercase(), v.trim_matches('"').to_string());
        }
    }
    Some(Prop {
        name,
        params,
        value: line[colon + 1..].to_string(),
    })
}

/// TEXT escapes (`\\` `\;` `\,` `\n` `\N`), read left to right so `\\n` stays a backslash and an "n"; a line break
/// becomes a space; any other `\x` keeps both characters.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('\\'),
            Some('n' | 'N') => out.push(' '),
            Some(next @ ('\\' | ',' | ';')) => out.push(next),
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
        }
    }
    out.trim().to_string()
}

/// A day of the week with its ordinal: `MO`, `+1MO`, `-1TH` (Sunday = 0).
fn parse_day(s: &str) -> Option<(i32, u32)> {
    let s = s.trim();
    let chars: Vec<char> = s.chars().collect();
    if s.encode_utf16().count() < 2 || chars.len() < 2 {
        return None;
    }
    let code: String = chars[chars.len() - 2..]
        .iter()
        .collect::<String>()
        .to_uppercase();
    let prefix: String = chars[..chars.len() - 2].iter().collect();
    let nth = if s.encode_utf16().count() > 2 {
        dotnet_int(&prefix).unwrap_or(0)
    } else {
        0
    };
    let day = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"]
        .iter()
        .position(|d| *d == code)? as u32;
    Some((nth, day))
}

/// Occurrence starts (UTC) of a recurring event, pushed in order; wall-clock times stay put across DST changes.
/// Err: a date past the year 9999 ended the series there.
#[allow(clippy::too_many_arguments)]
fn expand(
    first_utc: Dt,
    rrule: &str,
    zone: Option<&TimeZone>,
    all_day: bool,
    from_utc: Dt,
    to_utc_bound: Dt,
    local_zone: &TimeZone,
    out: &mut Vec<Dt>,
) -> Result<(), OutOfRange> {
    let mut r: HashMap<String, String> = HashMap::new();
    for part in rrule.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            if r.contains_key(&k.to_uppercase()) {
                // a repeated rule part makes the recurrence ambiguous: keep DTSTART and the rest of the calendar
                out.push(first_utc);
                return Ok(());
            }
            r.insert(k.to_uppercase(), v.to_string());
        }
    }
    let get = |k: &str| r.get(k).map(String::as_str);
    let freq = get("FREQ").unwrap_or("").to_uppercase();
    let interval = get("INTERVAL")
        .and_then(dotnet_int)
        .filter(|v| *v > 0)
        .unwrap_or(1);
    let count = get("COUNT").and_then(dotnet_int).unwrap_or(i32::MAX);
    let until = get("UNTIL")
        .and_then(|u| to_utc(u, &HashMap::new(), local_zone))
        .map(|(u, _)| u);
    let by_day: Vec<(i32, u32)> = get("BYDAY")
        .map(|b| b.split(',').filter_map(parse_day).collect())
        .unwrap_or_default();
    let ints = |k: &str| -> Vec<i32> {
        get(k)
            .map(|v| {
                v.split(',')
                    .map(|x| dotnet_int(x).unwrap_or(0))
                    .filter(|n| *n != 0)
                    .collect()
            })
            .unwrap_or_default()
    };
    let by_month_day = ints("BYMONTHDAY");
    let mut by_month: Vec<u32> = Vec::new();
    for m in ints("BYMONTH") {
        if (1..=12).contains(&m) && !by_month.contains(&(m as u32)) {
            by_month.push(m as u32);
        }
    }
    let wkst = get("WKST")
        .and_then(parse_day)
        .filter(|(nth, _)| *nth == 0)
        .map_or(1, |(_, d)| d);
    let by_set_pos = ints("BYSETPOS");
    let year_day_filters = (!by_day.is_empty() || !by_month_day.is_empty())
        && !r.contains_key("BYWEEKNO")
        && !r.contains_key("BYYEARDAY");

    // work in the event's local wall-clock time
    let wall = |utc: Dt| match zone {
        Some(z) if !all_day => utc_to_wall(utc, z),
        _ => utc,
    };
    let to_utc_time = |l: Dt| match zone {
        Some(z) if !all_day => safe_to_utc(l, z),
        _ => l,
    };
    let local = wall(first_utc);

    // without COUNT, skip the periods long before the window (a daily stand-up that started years ago)
    let mut skip: i64 = 0;
    if count == i32::MAX && from_utc > first_utc {
        let days = (from_utc.0 - first_utc.0) as f64 / TICKS_PER_DAY as f64;
        let period_days = match freq.as_str() {
            "DAILY" => 1.0,
            "WEEKLY" => 7.0,
            "MONTHLY" => 31.0,
            "YEARLY" => 366.0,
            _ => f64::MAX,
        };
        skip = i64::from(0.max((days / (period_days * f64::from(interval))) as i32 - 2));
    }
    // a huge but valid INTERVAL must not overflow the period arithmetic or the date adds below
    let max_days = (Dt(MAX_TICKS).date().0 - local.date().0) / TICKS_PER_DAY;
    let max_months = (9999 - local.year()) * 12 + 12 - i64::from(local.month());
    let max_filter_years = 9999.min(to_utc_bound.year() + 1) - local.year();
    let mut emitted: i64 = 0;
    let fast = matches!(freq.as_str(), "DAILY" | "WEEKLY" | "MONTHLY");
    for period in skip..skip + 3000 {
        if emitted >= i64::from(count) {
            break;
        }
        let n = period * i64::from(interval);
        let mut candidates: Vec<Dt> = match freq.as_str() {
            // BYDAY limits a daily rule to those weekdays (Thunderbird's "every weekday")
            "DAILY" if n <= max_days => {
                let day = local.add_days(n)?;
                if by_day.is_empty() || by_day.iter().any(|b| b.1 == day.day_of_week()) {
                    vec![day]
                } else {
                    vec![]
                }
            }
            "DAILY" => vec![],
            "WEEKLY" if n <= max_days / 7 => week_days(local, n, &by_day, wkst)?,
            "WEEKLY" => vec![],
            "MONTHLY" if n <= max_months => month_days(local, n, &by_day, &by_month_day)?,
            "MONTHLY" => vec![],
            // BYMONTH (with BYDAY / BYMONTHDAY) picks days within those months: "second Sunday of May"
            "YEARLY" if !by_month.is_empty() => {
                let mut v = Vec::new();
                for m in &by_month {
                    let mo = n * 12 + i64::from(*m) - i64::from(local.month());
                    if mo <= max_months {
                        v.extend(month_days(local, mo, &by_day, &by_month_day)?);
                    }
                }
                v
            }
            "YEARLY" if year_day_filters => {
                if n <= max_filter_years {
                    year_days(local, n, &by_day, &by_month_day)?
                } else {
                    vec![]
                }
            }
            // AddYears turns 29 February into the 28th; RFC 5545 skips the invalid date (leap-day birthdays)
            "YEARLY" => {
                if n <= 9999 - local.year() {
                    let year = local.add_years(n)?;
                    if year.day() == local.day() {
                        vec![year]
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                }
            }
            _ => {
                if period == 0 {
                    vec![local]
                } else {
                    vec![]
                }
            }
        };
        // YEARLY uses BYMONTH to generate candidates; faster rules are limited by it
        if !by_month.is_empty() && fast {
            candidates.retain(|d| by_month.contains(&d.month()));
        }
        if !by_month_day.is_empty() && freq == "DAILY" {
            candidates.retain(|d| matches_month_day(*d, &by_month_day));
        }
        candidates.sort();
        candidates.dedup();
        // BYSETPOS picks from each period's set: "the last weekday of the month" is BYDAY=MO,…,FR;BYSETPOS=-1
        if !by_set_pos.is_empty() {
            let all = candidates;
            let len = all.len() as i64;
            let mut picked: Vec<i64> = by_set_pos
                .iter()
                .map(|p| {
                    if *p > 0 {
                        i64::from(*p) - 1
                    } else {
                        len + i64::from(*p)
                    }
                })
                .filter(|i| (0..len).contains(i))
                .collect();
            picked.sort();
            picked.dedup();
            candidates = picked.into_iter().map(|i| all[i as usize]).collect();
        }
        let any = !candidates.is_empty();
        for l in candidates {
            if l < local {
                continue;
            }
            let utc = to_utc_time(l);
            if until.is_some_and(|u| utc > u) || utc >= to_utc_bound {
                return Ok(());
            }
            emitted += 1;
            if emitted > i64::from(count) {
                return Ok(());
            }
            out.push(utc);
        }
        if !any && !fast && freq != "YEARLY" {
            return Ok(());
        }
    }
    Ok(())
}

/// The days of week `weeks` after `first`'s (weeks start on `wkst`), at `first`'s time of day.
fn week_days(
    first: Dt,
    weeks: i64,
    by_day: &[(i32, u32)],
    wkst: u32,
) -> Result<Vec<Dt>, OutOfRange> {
    let back = i64::from((first.day_of_week() + 7 - wkst) % 7);
    let week_start = first.date().add_days(-back)?.add_days(weeks * 7)?;
    if by_day.is_empty() {
        return Ok(vec![first.add_days(weeks * 7)?]);
    }
    by_day
        .iter()
        .map(|(_, day)| {
            week_start
                .add_days(i64::from((day + 7 - wkst) % 7))?
                .add_ticks(first.time_of_day())
        })
        .collect()
}

/// The days of the month `months` after `first`'s that BYDAY / BYMONTHDAY pick (else `first`'s day of the month).
fn month_days(
    first: Dt,
    months: i64,
    by_day: &[(i32, u32)],
    by_month_day: &[i32],
) -> Result<Vec<Dt>, OutOfRange> {
    let m = Dt::from_ymd(first.year(), first.month(), 1)
        .ok_or(OutOfRange)?
        .add_months(months)?;
    let days = i64::from(days_in_month(m.year(), m.month()));
    let month_day = |d: i32| {
        if d > 0 {
            i64::from(d)
        } else {
            days + i64::from(d) + 1
        }
    };
    let tod = first.time_of_day();
    let mut out = Vec::new();
    if !by_day.is_empty() {
        for (nth, day) in by_day {
            let mut all = Vec::new();
            for d in 1..=days {
                let date = m.add_days(d - 1)?;
                if date.day_of_week() == *day {
                    all.push(date);
                }
            }
            let count = all.len() as i32;
            let matching: Vec<Dt> = match *nth {
                0 => all,
                n if n > 0 && n <= count => vec![all[(n - 1) as usize]],
                n if n < 0 && n >= -count => vec![all[(count + n) as usize]],
                _ => vec![],
            };
            for d in matching {
                if by_month_day.is_empty()
                    || by_month_day
                        .iter()
                        .any(|md| month_day(*md) == i64::from(d.day()))
                {
                    out.push(d.add_ticks(tod)?);
                }
            }
        }
        return Ok(out);
    }
    let own = [first.day() as i32];
    for md in if by_month_day.is_empty() {
        &own[..]
    } else {
        by_month_day
    } {
        let d = month_day(*md);
        if (1..=days).contains(&d) {
            out.push(m.add_days(d - 1)?.add_ticks(tod)?);
        }
    }
    Ok(out)
}

/// The days of the year `years` after `first`'s that BYDAY (with ordinals counted in the year) / BYMONTHDAY pick.
fn year_days(
    first: Dt,
    years: i64,
    by_day: &[(i32, u32)],
    by_month_day: &[i32],
) -> Result<Vec<Dt>, OutOfRange> {
    let start = Dt::from_ymd(first.year() + years, 1, 1)
        .ok_or(OutOfRange)?
        .add_ticks(first.time_of_day())?;
    let days: i64 = if is_leap(start.year()) { 366 } else { 365 };
    let mut out = Vec::new();
    for offset in 0..days {
        let date = start.add_days(offset)?;
        if !by_month_day.is_empty() && !matches_month_day(date, by_month_day) {
            continue;
        }
        let from_start = (offset / 7 + 1) as i32;
        let from_end = -((days - 1 - offset) / 7 + 1) as i32;
        if by_day.is_empty()
            || by_day.iter().any(|(nth, day)| {
                *day == date.day_of_week() && (*nth == 0 || *nth == from_start || *nth == from_end)
            })
        {
            out.push(date);
        }
    }
    Ok(out)
}

fn matches_month_day(date: Dt, days: &[i32]) -> bool {
    let d = date.day() as i32;
    let dim = days_in_month(date.year(), date.month()) as i32;
    days.contains(&d) || days.contains(&(d - dim - 1))
}

#[cfg(test)]
#[path = "ics_tests.rs"]
mod tests;
