//! `CalendarMonitor.cs`: events from iCalendar (ICS) feeds for the Calendar widget.

pub mod ics;
mod windows_zones;

/// One occurrence. Start and end are UTC ms; an all-day event uses UTC midnight of its date (the phone shifts it to
/// its own day). `calendar` and `color` come from the feed's settings.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    pub title: String,
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub location: Option<String>,
    pub calendar: String,
    pub color: String,
}
