//! Port of `ConfigStore.Normalize` and its helpers: defaults, templates, the pre-0.6.2 page migration, grid limits and
//! unique ids.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::*;
use crate::catalog::{self, COLS, ROWS};
use crate::icon_text;

/// Pages a fresh install (or an empty page list) starts with.
pub const DEFAULT_PAGES: [&str; 6] = ["home", "ai", "work", "system", "procs", "deck"];
const DEFAULT_PAGE_ICON: &str = "\u{25ab}\u{fe0f}";
const WEEK_ORDER: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Fills in defaults: builds pages from templates, migrates old pages, generates unique ids. `stale_ids` are the
/// shortcut ids of the settings being replaced, so a new shortcut never reuses a just-deleted one's id.
pub fn normalize(mut c: AgentConfig, stale_ids: Option<&HashSet<String>>) -> AgentConfig {
    c.weather.unit = normalize_weather_unit(Some(&c.weather.unit));
    c.weather.model = normalize_weather_model(Some(&c.weather.model));
    let mut seen = HashSet::new();
    c.codex_bar.providers.retain(|p| {
        matches!(p.as_str(), "claude" | "codex" | "antigravity") && seen.insert(p.clone())
    });
    c.codex_bar.page_size = c.codex_bar.page_size.clamp(0, 6);
    c.codex_bar.rotate_interval = c.codex_bar.rotate_interval.clamp(2, 300);

    let mut ids = HashSet::new();
    c.controls
        .actions
        .retain(|a| !is_blank(&a.id) && ids.insert(a.id.clone()));
    c.controls.actions.truncate(512);
    let mut ids = HashSet::new();
    c.controls
        .profiles
        .retain(|p| !is_blank(&p.id) && ids.insert(p.id.clone()));
    c.controls.profiles.truncate(128);
    for profile in &mut c.controls.profiles {
        profile.levels.truncate(512);
        profile.actions.retain(|a| !a.is_empty());
        profile.actions.truncate(64);
    }

    let d = &mut c.display;
    if let Some(a) = d.appearance.as_mut() {
        a.normalize(false);
    }
    d.hour12 = normalize_hour12(Some(&d.hour12));
    d.idle_return_sec = d.idle_return_sec.clamp(0, 3600);
    d.idle_dim_min = d.idle_dim_min.clamp(0, 1440);
    // a language the panel does not have falls back to following the phone / browser
    let lang = d.language.trim().to_lowercase();
    d.language = if LANGUAGES.contains(&lang.as_str()) {
        lang
    } else {
        "auto".to_string()
    };
    d.switch_countdown_sec = d.switch_countdown_sec.clamp(0, 30);
    d.touch_grace_sec = d.touch_grace_sec.clamp(0, 600);
    d.min_dwell_sec = d.min_dwell_sec.clamp(0, 600);
    d.hold_max_min = d.hold_max_min.clamp(0, 1440);

    let host = c
        .ping
        .host
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let bad = host.chars().count() > 253 || host.chars().any(|ch| ch <= ' ' || ch > '~');
    c.ping.host = Some(if bad {
        DEFAULT_PING_HOST.to_string()
    } else {
        host
    });
    c.claude.timeout_sec = c.claude.timeout_sec.clamp(10, 3600);
    // this fork updates from its own releases, never from upstream's C# agent
    if is_blank(&c.updates.repo) || c.updates.repo == UPSTREAM_REPO {
        c.updates.repo = FORK_REPO.to_string();
    }
    c.lan.port = c.lan.port.clamp(1024, 65535);
    if c.lan.token.as_deref().is_none_or(is_blank) {
        c.lan.token = Some(new_token());
    }
    if c.lan.webhook_token.as_deref().is_none_or(is_blank) {
        c.lan.webhook_token = Some(new_token());
    }
    c.night.brightness = c.night.brightness.clamp(0.01, 1.0);

    let (pages, original_ids) = normalize_pages(std::mem::take(&mut c.pages));
    c.pages = pages;
    // an exact match first; a reference with stray spaces around it still finds the page it names
    let page_id = |id: &str| -> String {
        original_ids
            .get(id)
            .or_else(|| original_ids.get(id.trim()))
            .cloned()
            .unwrap_or_else(|| id.trim().to_string())
    };
    c.display.home_page = match c.display.home_page.as_deref() {
        Some(h) if !is_blank(h) => Some(page_id(h).trim().to_string()),
        _ => None,
    };

    normalize_shortcuts(&mut c.shortcuts, stale_ids);

    let mut ids = HashSet::new();
    c.phones
        .retain(|p| !is_blank(&p.id) && ids.insert(p.id.clone()));
    c.phones.truncate(20);
    for ph in &mut c.phones {
        ph.name = ph.name.trim().to_string();
        if let Some(pages) = ph.pages.take() {
            let mut kept: Vec<String> = Vec::new();
            for id in pages
                .iter()
                .filter(|id| !is_blank(id))
                .map(|id| page_id(id))
            {
                if !kept.contains(&id) {
                    kept.push(id);
                }
            }
            ph.pages = if kept.is_empty() { None } else { Some(kept) };
        }
    }
    let mut seen = HashSet::new();
    let adapters = std::mem::take(&mut c.ping.adapters);
    c.ping.adapters = adapters
        .into_iter()
        .filter(|a| !is_blank(a))
        .map(|a| a.trim().to_string())
        .filter(|a| seen.insert(a.to_lowercase()))
        .take(16)
        .collect();
    c.discord.client_id = trimmed_or_none(c.discord.client_id.take());
    c.discord.client_secret = trimmed_or_none(c.discord.client_secret.take());
    if is_blank(&c.obs.url) {
        c.obs.url = "ws://127.0.0.1:4455".to_string();
    }
    c.home_assistant.url =
        trimmed_or_none(c.home_assistant.url.take()).map(|u| u.trim_end_matches('/').to_string());
    c.home_assistant.token = trimmed_or_none(c.home_assistant.token.take());

    c.console.profiles.truncate(200); // more than 30 is refused on save
    let mut console_ids = HashSet::new();
    for p in &mut c.console.profiles {
        p.label = cap(p.label.trim(), 40);
        p.apps = p.apps.trim().to_string(); // too long is refused on save, not cut short
        let base = slug(if is_blank(&p.id) { &p.label } else { &p.id });
        p.id = unique_id(
            if base.is_empty() { "app".into() } else { base },
            &mut console_ids,
            |_| false,
        );
        p.keys.retain(|k| !is_blank(&k.keys));
        p.keys.truncate(100);
        for k in &mut p.keys {
            k.keys = k.keys.trim().to_string();
            let label = k.label.trim();
            // a key without a label shows its keys on the phone ("ctrl+z")
            k.label = cap(if label.is_empty() { &k.keys } else { label }, 24);
        }
        p.dial_left = trimmed_or_none(p.dial_left.take());
        p.dial_right = trimmed_or_none(p.dial_right.take());
        p.dial_press = trimmed_or_none(p.dial_press.take());
    }
    c.calendars.truncate(20);
    for cal in &mut c.calendars {
        cal.name = cal.name.trim().to_string();
        cal.url = cal.url.trim().to_string();
    }
    c.feeds.truncate(30);
    c.photos.folder = cap(c.photos.folder.trim(), 1024);
    for f in &mut c.feeds {
        f.name = cap(f.name.trim(), 60);
        f.url = f.url.trim().to_string();
    }
    c.cameras.ffmpeg = cap(c.cameras.ffmpeg.trim(), 1024);
    // a row left empty in the editor is dropped; ids are random and kept, so widgets keep pointing at the same camera
    c.cameras.sources.retain(|s| !is_blank(&s.url));
    c.cameras.sources.truncate(100);
    let mut camera_ids = HashSet::new();
    for cam in &mut c.cameras.sources {
        cam.name = cap(cam.name.trim(), 60);
        cam.url = cap(cam.url.trim(), 2048);
        cam.transport = if cam.transport.trim().eq_ignore_ascii_case("udp") {
            "udp"
        } else {
            "tcp"
        }
        .to_string();
        cam.fps = if (1..=CAMERA_MAX_FPS).contains(&cam.fps) {
            cam.fps
        } else {
            CAMERA_DEFAULT_FPS
        };
        let mut id = cam.id.trim().to_lowercase();
        if !valid_camera_id(&id) || !camera_ids.insert(id.clone()) {
            id = random_hex(12);
            camera_ids.insert(id.clone());
        }
        cam.id = id;
    }

    c.charge.min = c.charge.min.clamp(5, 95);
    c.charge.max = c.charge.max.clamp(c.charge.min + 5, 100);
    c.charge.method = if c.charge.method == "shortcuts" {
        "shortcuts"
    } else {
        "ha"
    }
    .to_string();
    c.charge.entity = trimmed_or_none(c.charge.entity.take());

    c.rules.retain(|r| {
        RULE_TRIGGERS.contains(&r.when.as_str()) && RULE_ACTIONS.contains(&r.then.as_str())
    });
    c.rules.truncate(50);
    let mut rule_ids = HashSet::new();
    for r in &mut c.rules {
        r.name = r.name.trim().to_string();
        let source = if is_blank(&r.id) {
            if r.name.is_empty() {
                r.when.clone()
            } else {
                r.name.clone()
            }
        } else {
            r.id.clone()
        };
        let base = slug(&source);
        r.id = unique_id(
            if base.is_empty() { "rule".into() } else { base },
            &mut rule_ids,
            |_| false,
        );
        let max = match r.when.as_str() {
            "idle" => 1440.0,
            "ping-high" => 10000.0,
            _ => 100.0,
        };
        r.value = r.value.map(|v| v.clamp(0.0, max));
        r.level = Some(match r.level.as_deref() {
            Some(l @ ("ok" | "warn" | "error")) => l.to_string(),
            _ => "info".to_string(),
        });
        r.priority = r.priority.clamp(0, 9);
        r.for_sec = r.for_sec.clamp(0, 3600);
        r.cooldown_min = r.cooldown_min.clamp(0, 1440);
        r.countdown_sec = r.countdown_sec.map(|cd| cd.clamp(0, 30));
        r.days = normalize_days(r.days.as_deref());
        r.during = trimmed_or_none(r.during.take());
        if !matches!(
            r.op.as_deref(),
            Some(">" | ">=" | "<" | "<=" | "=" | "!=" | "stale")
        ) {
            r.op = None;
        }
        // seconds without an update: 1 s to a day, a minute when the box was left empty or holds text ("Infinity" too)
        if r.op.as_deref() == Some("stale") {
            let seconds = match r.arg.as_deref().and_then(parse_invariant_double) {
                Some(s) if s.is_finite() && s > 0.0 => s.round_ties_even().clamp(1.0, 86400.0),
                _ => 60.0,
            };
            r.arg = Some(crate::config::write::format_double(seconds));
        }
        r.shortcut = trimmed_or_none(r.shortcut.take());
        if let Some(page) = r.page.take() {
            r.page = Some(page_id(&page));
        }
    }
    c
}

fn normalize_shortcuts(shortcuts: &mut [Shortcut], stale_ids: Option<&HashSet<String>>) {
    let mut used: HashSet<String> = HashSet::new();
    // Pass 1: reserve every explicitly-provided id first, so a blank-id (brand-new) shortcut earlier in the list never
    // grabs an id that a later one explicitly keeps.
    let mut reserved: HashMap<usize, String> = HashMap::new();
    for (i, s) in shortcuts
        .iter()
        .enumerate()
        .filter(|(_, s)| !is_blank(&s.id))
    {
        let base = slug(&s.id);
        let base = if base.is_empty() {
            "shortcut".to_string()
        } else {
            base
        };
        reserved.insert(i, unique_id(base, &mut used, |_| false));
    }
    for (i, s) in shortcuts.iter_mut().enumerate() {
        s.label = s.label.trim().to_string();
        s.icon = icon_text::repair(Some(&s.icon));
        if let Some(kept) = reserved.remove(&i) {
            s.id = kept;
        } else {
            let base = slug(&s.label);
            let base = if base.is_empty() {
                "shortcut".to_string()
            } else {
                base
            };
            s.id = unique_id(base, &mut used, |candidate| {
                stale_ids.is_some_and(|stale| stale.contains(candidate))
            });
        }
        s.args = s.args.take().filter(|a| !is_blank(a));
        s.cwd = s.cwd.take().filter(|a| !is_blank(a));
        s.steps = s.steps.take().map(|steps| {
            steps
                .iter()
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .take(50)
                .collect()
        });
        if s.steps.as_ref().is_some_and(Vec::is_empty) {
            s.steps = None;
        }
        s.state_key = s.state_key.take().filter(|a| !is_blank(a));
        s.off_cmd = s.off_cmd.take().filter(|a| !is_blank(a));
        s.off_args = s.off_args.take().filter(|a| !is_blank(a));
    }
}

/// Expands templates, migrates the old fixed pages, keeps widgets on the grid and makes ids unique. Also returns each
/// page's original id → its id now, so references to pages follow.
fn normalize_pages(input: Vec<PageConfig>) -> (Vec<PageConfig>, HashMap<String, String>) {
    let mut original_ids: HashMap<String, String> = HashMap::new();
    let src: Vec<PageConfig> = if input.is_empty() {
        DEFAULT_PAGES
            .iter()
            .map(|t| PageConfig {
                template: Some(t.to_string()),
                ..PageConfig::default()
            })
            .collect()
    } else {
        input
    };
    let mut page_ids = HashSet::new();
    let reserved_pages = reserve_layout_ids(
        src.iter()
            .map(|p| (p.id.clone(), "page".to_string()))
            .collect(),
        &mut page_ids,
    );
    let mut pages = Vec::with_capacity(src.len());
    for (index, mut p) in src.into_iter().enumerate() {
        if let Some(a) = p.appearance.as_mut() {
            a.normalize(false);
        }
        let tpl = catalog::template(p.template.as_deref().or(p.r#type.as_deref()));
        if p.widgets.as_ref().is_none_or(Vec::is_empty)
            && let Some(t) = tpl
        {
            p.widgets = Some(t.widgets.clone());
            if p.r#type.is_some() {
                migrate_legacy(&mut p);
            }
        }
        if is_blank(&p.title) {
            p.title = tpl
                .map(|t| t.title.clone())
                .unwrap_or_else(|| "Page".into());
        }
        if is_blank(&p.icon) {
            p.icon = tpl
                .map(|t| t.icon.clone())
                .unwrap_or_else(|| DEFAULT_PAGE_ICON.into());
        }
        p.icon = icon_text::repair(Some(&p.icon));
        p.title = p.title.trim().to_string();
        p.idle_sec = p.idle_sec.map(|s| s.clamp(0, 86400));
        let original = std::mem::take(&mut p.id);
        p.id = match reserved_pages.get(&index) {
            Some(id) => id.clone(),
            None => unique_id(
                layout_id(tpl.map(|t| t.id.as_str()).unwrap_or(&p.title), "page"),
                &mut page_ids,
                |_| false,
            ),
        };
        if !is_blank(&original) {
            original_ids.entry(original).or_insert_with(|| p.id.clone());
        }
        p.template = None;
        p.r#type = None;
        p.options = None;

        let supported: Vec<WidgetConfig> = p
            .widgets
            .take()
            .unwrap_or_default()
            .into_iter()
            .filter(|w| catalog::is_widget(&w.r#type))
            .collect();
        let mut widget_ids = HashSet::new();
        let reserved_widgets = reserve_layout_ids(
            supported
                .iter()
                .map(|w| (w.id.clone(), w.r#type.clone()))
                .collect(),
            &mut widget_ids,
        );
        let mut widgets = Vec::with_capacity(supported.len());
        for (wi, mut w) in supported.into_iter().enumerate() {
            if let Some(a) = w.appearance.as_mut() {
                a.normalize(true);
            }
            let (min_w, min_h) = catalog::min_size(&w.r#type);
            w.w = w.w.clamp(min_w.min(COLS), COLS);
            w.h = w.h.clamp(min_h.min(ROWS), ROWS);
            w.x = w.x.clamp(0, COLS - w.w);
            w.y = w.y.clamp(0, ROWS - w.h);
            w.id = match reserved_widgets.get(&wi) {
                Some(id) => id.clone(),
                None => unique_id(layout_id(&w.r#type, &w.r#type), &mut widget_ids, |_| false),
            };
            widgets.push(w);
        }
        p.widgets = Some(widgets);
        pages.push(p);
    }
    if !pages.is_empty() && pages.iter().all(|p| !p.enabled) {
        pages[0].enabled = true;
    }
    (pages, original_ids)
}

/// `ReserveLayoutIds`: explicit ids (index → (original id, fallback)) are reserved before any blank one is assigned; ids
/// that are already canonical go first, colliding ones get a suffix afterwards.
fn reserve_layout_ids(
    items: Vec<(String, String)>,
    used: &mut HashSet<String>,
) -> HashMap<usize, String> {
    let mut explicit: Vec<(usize, String, bool)> = items
        .iter()
        .enumerate()
        .filter(|(_, (original, _))| !is_blank(original))
        .map(|(i, (original, fallback))| {
            let id = layout_id(original, fallback);
            let canonical = &id == original;
            (i, id, canonical)
        })
        .collect();
    explicit.sort_by_key(|(_, _, canonical)| !*canonical); // stable: canonical ones first, order kept
    let mut reserved = HashMap::new();
    let mut collisions = Vec::new();
    for (i, id, _) in explicit {
        if used.insert(id.clone()) {
            reserved.insert(i, id);
        } else {
            collisions.push((i, id));
        }
    }
    for (i, id) in collisions {
        reserved.insert(i, unique_id(id, used, |_| false));
    }
    reserved
}

fn layout_id(text: &str, fallback: &str) -> String {
    let id = slug(text);
    if id.is_empty() {
        fallback.to_string()
    } else {
        id
    }
}

/// `id`, or `id-2`, `id-3`… : the first one not yet used (and not rejected), which is then marked used.
fn unique_id(id: String, used: &mut HashSet<String>, rejected: impl Fn(&str) -> bool) -> String {
    let mut unique = id.clone();
    let mut i = 2;
    while rejected(&unique) || !used.insert(unique.clone()) {
        unique = format!("{id}-{i}");
        i += 1;
    }
    unique
}

/// Carries the options of a pre-0.6.2 fixed page over to the widgets of its template.
fn migrate_legacy(p: &mut PageConfig) {
    let options = p.options.clone().unwrap_or_default();
    let on = |key: &str| options.get(key).is_none_or(|v| *v != Value::Bool(false));
    let ws = p.widgets.as_mut().expect("template widgets");
    let find = |ws: &Vec<WidgetConfig>, ty: &str| ws.iter().position(|w| w.r#type == ty);
    match p.r#type.as_deref() {
        Some("home") => {
            let weather = on("weather");
            if let Some(i) = find(ws, "clock") {
                ws[i].set("weather", Value::Bool(weather));
            }
            let tiles: Vec<String> = match options.get("tiles") {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
                _ => ["cpu", "gpu", "ram", "net"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            };
            ws.retain(|w| w.r#type != "stat");
            if !on("media") {
                ws.retain(|w| w.r#type != "media");
                if let Some(i) = find(ws, "clock") {
                    ws[i].w = 12;
                }
            }
            for w in ws.iter_mut() {
                w.h = if tiles.is_empty() { 6 } else { 4 };
            }
            let n = tiles.len() as i32;
            for (i, metric) in tiles.iter().enumerate() {
                let i = i as i32;
                let x = 12 * i / n;
                let mut tw = WidgetConfig {
                    r#type: "stat".into(),
                    x,
                    y: 4,
                    h: 2,
                    w: 12 * (i + 1) / n - x,
                    ..WidgetConfig::default()
                };
                tw.set("metric", Value::String(metric.clone()));
                ws.push(tw);
            }
        }
        Some("ai") => {
            let providers: Vec<Value> = ["claude", "codex"]
                .iter()
                .filter(|x| on(x))
                .map(|x| Value::String(x.to_string()))
                .collect();
            if let Some(i) = find(ws, "quotas") {
                ws[i].set("providers", Value::Array(providers));
            }
        }
        Some("work") => {
            if !on("sessions") {
                ws.retain(|w| w.r#type != "sessions");
                if let Some(i) = find(ws, "github") {
                    ws[i].x = 0;
                    ws[i].w = 12;
                }
            } else if !on("github") {
                ws.retain(|w| w.r#type != "github");
                if let Some(i) = find(ws, "sessions") {
                    ws[i].w = 12;
                }
            }
        }
        Some("procs") => {
            let allow = on("allowKill");
            if let Some(i) = find(ws, "processes") {
                ws[i].set("allowKill", Value::Bool(allow));
            }
        }
        Some("deck") => {
            // JsonElement.TryGetInt32: an integer written without a fraction, within the int range
            let columns = options
                .get("columns")
                .and_then(Value::as_i64)
                .filter(|n| i32::try_from(*n).is_ok());
            if let (Some(n), Some(i)) = (columns, find(ws, "shortcuts")) {
                ws[i].set("columns", Value::from(n));
            }
        }
        _ => {}
    }
}

/// `RuleEngine.NormalizeDays`: "weekdays" / "weekends" expanded, in week order; every day or none means null.
pub fn normalize_days(days: Option<&str>) -> Option<String> {
    let days = days.filter(|d| !is_blank(d))?;
    let lower = days.to_lowercase();
    let mut set: HashSet<&str> = lower
        .split([',', ' ', ';'])
        .filter(|s| !s.is_empty())
        .collect();
    if set.remove("weekdays") {
        set.extend(["mon", "tue", "wed", "thu", "fri"]);
    }
    if set.remove("weekends") {
        set.extend(["sat", "sun"]);
    }
    let list: Vec<&str> = WEEK_ORDER
        .iter()
        .copied()
        .filter(|d| set.contains(d))
        .collect();
    if list.is_empty() || list.len() == 7 {
        None
    } else {
        Some(list.join(","))
    }
}

/// A Wi-Fi or webhook key: 32 random bytes, base64url without padding.
pub fn new_token() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("random bytes");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_hex(len: usize) -> String {
    let mut bytes = vec![0u8; len.div_ceil(2)];
    getrandom::fill(&mut bytes).expect("random bytes");
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()[..len].to_string()
}

/// `^[a-z0-9]{6,32}$`.
fn valid_camera_id(id: &str) -> bool {
    (6..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// `ConfigStore.Slug`: lower case (the Turkish letters mapped by hand), runs of anything but a-z and 0-9 become one
/// dash, no dashes at the ends.
pub fn slug(s: &str) -> String {
    // İ first: Rust lower-cases it to "i̇" (two chars) where .NET's ToLowerInvariant keeps it for the mapping below
    let lowered = s.replace('İ', "i").to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut dash = false;
    for ch in lowered.chars() {
        let ch = match ch {
            'ı' => 'i',
            'ğ' => 'g',
            'ü' => 'u',
            'ş' => 's',
            'ö' => 'o',
            'ç' => 'c',
            other => other,
        };
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            out.push(ch);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// `double.TryParse(s, NumberStyles.Float, CultureInfo.InvariantCulture)` for the values a rule's box may hold.
pub(crate) fn parse_invariant_double(s: &str) -> Option<f64> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "infinity" | "+infinity" | "∞" => Some(f64::INFINITY),
        "-infinity" | "-∞" => Some(f64::NEG_INFINITY),
        "nan" => Some(f64::NAN),
        _ => t
            .parse::<f64>()
            .ok()
            .filter(|_| !t.contains(['_', 'i', 'I', 'n', 'N'])),
    }
}

pub(crate) fn is_blank(s: &str) -> bool {
    s.trim().is_empty()
}

fn trimmed_or_none(s: Option<String>) -> Option<String> {
    s.filter(|v| !is_blank(v)).map(|v| v.trim().to_string())
}

/// `s[..max]` in UTF-16 units, as the C# code cuts strings.
fn cap(s: &str, max: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for ch in s.chars() {
        units += ch.len_utf16();
        if units > max {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_the_csharp_rules() {
        assert_eq!(slug("Desk / Live"), "desk-live");
        assert_eq!(slug("  --Hello__World--  "), "hello-world");
        assert_eq!(slug("İş Akışı"), "is-akisi");
        assert_eq!(slug("Çağrı Ölçü"), "cagri-olcu");
        assert_eq!(slug("✨"), "");
    }

    #[test]
    fn days_follow_the_week_and_collapse_to_null() {
        assert_eq!(
            normalize_days(Some("fri, Mon;wed")).as_deref(),
            Some("mon,wed,fri")
        );
        assert_eq!(normalize_days(Some("weekdays,weekends")), None);
        assert_eq!(normalize_days(Some("weekends")).as_deref(), Some("sat,sun"));
        assert_eq!(normalize_days(Some("  ")), None);
    }

    #[test]
    fn stale_arguments_become_whole_seconds() {
        let r = |arg: &str| {
            let c = normalize(
                crate::config::read::parse(&format!(
                    r#"{{"rules":[{{"when":"push","then":"notify","op":"stale","arg":"{arg}"}}]}}"#
                ))
                .unwrap(),
                None,
            );
            c.rules[0].arg.clone().unwrap()
        };
        assert_eq!(r("90.4"), "90");
        assert_eq!(r("2.5"), "2"); // Math.Round rounds half to even
        assert_eq!(r("abc"), "60");
        assert_eq!(r("Infinity"), "60");
        assert_eq!(r("999999"), "86400");
    }
}
