"""The headless phone of survey-c 4.3: what a phone on USB and its panel ask of the agent, on one timeline for every
agent. Standard library only; run with python3 -I.

    python3 -I tools/resources/client.py --agent cs=PORT --agent rs=PORT \
        --phases warmup=120,S0=600,S1=600,S2=720 --phase-file run/phase --log run/schedule.log

Scenarios (survey-c 4.3):
  warmup, S0  nothing is sent.
  S1  the phone is connected and its panel shows page 1: one /api/stream held open and read (with the app's
      X-PhoneBox-App/-Device/-Model headers), the panel's boot requests (/api/config, /api/push, /api/approvals,
      /api/state?ev=0, quotas and calendar if a widget needs them, the shortcut keys' pictures) and its global loops at
      main.js's intervals (/api/approvals 5 s, /api/quotas 30 s while a widget needs quotas, /api/calendar 120 s while
      one needs the calendar, /api/config 60 s, the processes list 2 s while the shown page has a processes widget),
      plus the shell's /api/pair every 60 s. The loaders of the widgets on the shown page and its neighbours run as
      the panel runs them (main.js activeSet: tick updates reach pages curPage-1..curPage+1).
  S2  S1 plus page cycling: every enabled page from the agent's own /api/config is shown for --page-seconds in turn.
      Following S1 it continues the same panel (no second boot, the stream stays open).
The panel's POST /api/phone/battery is never sent unless --battery is given (with charge switching off in the
config). Weather goes from the phone to open-meteo directly, so the agent never sees it. The phase file holds the
current phase for sample.py; the log gets each phase's plan (pages and the number of requests) and every request
made (seconds into the phase, phase, agent, page, method, path, status, bytes, ms).
"""
from __future__ import annotations

import argparse
import http.client
import json
import os
import signal
import sys
import threading
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

PHASES = ("warmup", "S0", "S1", "S2")
ACTIVE = ("S1", "S2")
BATTERY_REPORT = "/api/phone/battery?level=80&plugged=true&temp=30&source=ac&usb=connected,configured,adb"

# what a widget type asks for (android/app/src/main/assets/www): the every() loaders while the widget is active,
# as (path, seconds)
EVERY = {
    "batteries": ("/api/batteries", 20),   # widgets/pc.js:74
    "obs": ("/api/obs", 2),                # pc.js:127
    "discord": ("/api/discord", 2),        # pc.js:218
    "docker": ("/api/docker", 4),          # pc.js:384
    "apps": ("/api/apps", 60),             # pc.js:467
    "feeds": ("/api/feeds", 60),           # widgets/web.js:101
    "growatt": ("/api/growatt", 3),        # widgets/growatt.js (every 3000)
    "tcl": ("/api/tcl", 5),                # widgets/tcl.js (every 5000)
}
HA_SECONDS = 4                             # pc.js:320, /api/ha/states?ids=
COMMAND_SECONDS = 5                        # widgets/basic.js:1290 (2 s while running)
PHOTO_LIST_SECONDS = 600                   # widgets/photos.js:104
# integration widgets ask on every tick, guarded only by their own request in flight (widgets/controls.js:653)
PER_TICK = {**{t: "/api/controls/obs" for t in ("obs-mixer", "obs-scenes", "obs-sources", "obs-recording",
                                                "obs-replay", "obs-transition", "obs-camera")},
            "media-queue": "/api/controls/spotify/state", "media-favorites": "/api/controls/spotify/state"}
# deck widgets share one /api/controls/state per tick (deckState's 650 ms cache, controls.js:11)
DECK = {"audio-mixer", "app-volume", "device-volume", "audio-balance", "audio-meter", "microphone", "audio-inputs",
        "audio-outputs", "push-to-talk", "audio-profiles", "audio-ducking", "media-sources", "media-remote",
        "media-seek", "media-modes", "media-sleep", "brightness", "power-plans", "pc-listen", "power-controls",
        "app-dock", "hotkeys", "macro-pad", "window-layouts", "project-launcher", "snippets", "presentation",
        "display-profiles", "focus-profile", "meeting-controls", "discord-controls", "meeting-profile", "task-runner",
        "dev-servers", "services", "workflow-runner", "script-pad", "soundboard", "radio", "playlists",
        "audio-routing", "external-mixer", "midi-pad", "osc-pad"}
SPECTRUM_SECONDS = 2                       # widgets/spectrum.js:42 while the stream is live
NEEDS_QUOTAS = {"clock", "quotas", "quotaring"}
NEEDS_CALENDAR = {"agenda", "nextup"}
NEEDS_PROCS = {"processes", "topprocs"}    # both fill process icons (main.js:918)
MAX_ICONS = 200                            # main.js:892


@dataclass(frozen=True)
class Event:
    at: int          # seconds into the phase
    method: str
    path: str
    page: str = ""   # the page whose widget asks; empty for the panel's and the shell's own requests
    kind: str = "get"  # get | art (the cover, when it changed) | procs-icons | photo-list | photo
    panel: bool = True  # fetch() from the panel carries X-PhoneBox-Panel; image loads do not
    key: str = ""    # the widget, for kinds that keep state per widget


@dataclass
class Schedule:
    stream: bool
    pages: list
    events: list


def parse_phases(text: str) -> list:
    out = []
    for item in text.split(","):
        name, _, seconds = item.strip().partition("=")
        if name not in PHASES:
            raise ValueError(f"unknown phase {name!r} (known: {', '.join(PHASES)})")
        out.append((name, int(seconds)))
    return out


def _config(config: dict) -> dict:
    return config["config"] if isinstance(config.get("config"), dict) else config


def shown_pages(config: dict) -> list:
    """The pages the panel shows: enabled, with a widget list (main.js `shown`)."""
    return [p for p in _config(config).get("pages") or []
            if isinstance(p, dict) and p.get("enabled") is not False and isinstance(p.get("widgets"), list)]


def _quote(text) -> str:
    return urllib.parse.quote(str(text), safe="")


def _ha_ids(o: dict) -> list:
    items = o.get("items") if isinstance(o.get("items"), list) else []
    ids = [str(i.get("id")) for i in items if isinstance(i, dict) and i.get("id")]
    if not ids:
        ids = [x for x in str(o.get("entities") or "").replace(";", ",").replace(" ", ",").split(",") if "." in x]
    return ids


def build_schedule(config: dict, scenario: str, duration: int, page_seconds: int = 30, boot: bool = True,
                   battery: bool = False) -> Schedule:
    """The requests of one phase, second by second, from the agent's /api/config answer."""
    if scenario not in ACTIVE:
        return Schedule(False, [], [])
    cfg = _config(config)
    pages = shown_pages(cfg)
    n = len(pages)
    types = {w.get("type") for p in pages for w in p["widgets"] if isinstance(w, dict)}
    events = []

    def add(at, path, page="", kind="get", panel=True, method="GET", key=""):
        events.append(Event(at, method, path, page, kind, panel, key))

    quotas, calendar = bool(types & NEEDS_QUOTAS), bool(types & NEEDS_CALENDAR)
    if boot:  # main.js setOnline and poll(): the stream is not attached yet
        for path in ("/api/config", "/api/push", "/api/approvals", "/api/state?ev=0"):
            add(0, path)
        if quotas:
            add(0, "/api/quotas")
        if calendar:
            add(0, "/api/calendar")
        if "shortcuts" in types:  # main.js:711, a picture for each key that has one
            for sc in cfg.get("shortcuts") or []:
                if isinstance(sc, dict) and sc.get("id") and sc.get("iconRev") is not None:
                    add(0, f"/api/shortcuts/icon?id={_quote(sc['id'])}", panel=False)
    timeline, due = [], {}
    spectrum_due = 0
    for t in range(duration):
        cur = (t // page_seconds) % n if scenario == "S2" and n else 0
        if n and (t == 0 or (scenario == "S2" and t % page_seconds == 0)):
            timeline.append((t, pages[cur].get("id", str(cur))))
        # the shell's pairing refresh on USB (AgentLink.kt maybePair) and the panel's setInterval loops
        if t % 60 == 0 and (t > 0 or boot):
            add(t, "/api/pair", panel=False)
        if t > 0 and t % 5 == 0:
            add(t, "/api/approvals")
        if t > 0 and t % 30 == 0 and quotas:
            add(t, "/api/quotas")
        if t > 0 and t % 120 == 0 and calendar:
            add(t, "/api/calendar")
        if t > 0 and t % 60 == 0:
            add(t, "/api/config")
        if battery and (t == 5 or (t > 0 and t % 30 == 0)):
            add(t, BATTERY_REPORT, method="POST")
        if not n:
            continue
        if t > 0 and t % 2 == 0 and any(w.get("type") in NEEDS_PROCS for w in pages[cur]["widgets"]):
            add(t, "/api/procs?top=150", pages[cur].get("id", ""), kind="procs-icons")
        deck = spectrum = False
        for i in (cur - 1, cur, cur + 1):
            if not 0 <= i < n:
                continue
            page = pages[i]
            pid = page.get("id", str(i))
            for j, w in enumerate(page["widgets"]):
                if not isinstance(w, dict):
                    continue
                kind, o = w.get("type"), w.get("options") if isinstance(w.get("options"), dict) else {}
                key = f"{pid}/{w.get('id', j)}"
                loads = []
                if kind in EVERY:
                    loads.append(EVERY[kind])
                elif kind == "ha" and _ha_ids(o):
                    loads.append((f"/api/ha/states?ids={_quote(','.join(_ha_ids(o)))}", HA_SECONDS))
                elif kind == "command":
                    loads.append((f"/api/widget/{_quote(pid)}/{_quote(w.get('id', j))}", COMMAND_SECONDS))
                elif kind == "photos":
                    interval = max(5, min(3600, int(o.get("interval") or 30)))
                    for path, seconds, k in (("/api/photos", PHOTO_LIST_SECONDS, "photo-list"),
                                             ("/api/photos/", interval, "photo")):
                        if t >= due.get((key, k), 0):
                            add(t, path, pid, kind=k, key=key)
                            due[(key, k)] = t + seconds
                elif kind in PER_TICK:
                    add(t, PER_TICK[kind], pid, key=key)
                elif kind in DECK:
                    deck = True
                elif kind == "spectrum":
                    spectrum = True
                elif kind == "media":  # widgets/basic.js:284, the cover when state.media.artId changes
                    add(t, "/api/art", pid, kind="art", panel=False, key=key)
                for path, seconds in loads:  # every(): due again `seconds` after the last load, kept across pages
                    if t >= due.get((key, path), 0):
                        add(t, path, pid, key=key)
                        due[(key, path)] = t + seconds
        if deck:
            add(t, "/api/controls/state")
        if spectrum and t >= spectrum_due:
            add(t, "/api/spectrum")
            spectrum_due = t + SPECTRUM_SECONDS
    events.sort(key=lambda e: e.at)
    return Schedule(True, timeline, events)


# ---------------------------------------------------------------- running it


class Log:
    def __init__(self, path):
        self.f = open(path, "a", encoding="utf-8") if path else sys.stdout
        self.lock = threading.Lock()

    def line(self, text):
        with self.lock:
            self.f.write(text + "\n")
            self.f.flush()


class Agent:
    def __init__(self, label, port, args, log):
        self.label, self.port, self.log = label, port, log
        self.headers = {"Host": f"127.0.0.1:{port}", "X-PhoneBox-App": args.app_version,
                        "X-PhoneBox-Device": args.device, "X-PhoneBox-Model": args.model}
        self.pool = ThreadPoolExecutor(max_workers=8, thread_name_prefix=f"client-{label}")
        self.lock = threading.Lock()
        self.art_id, self.art_seen, self.icons, self.photo_ids, self.photo_pos = None, {}, set(), [], {}
        self.phase, self.t0 = "", time.monotonic()
        self.config = None
        self._stream_thread = self._stream_conn = None
        self._stream_stop = threading.Event()

    def request(self, method, path, panel=True, page="", timeout=15.0):
        start = time.monotonic()
        status, body = "ERR", b""
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        try:
            conn.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
            for k, v in self.headers.items():
                conn.putheader(k, v)
            if panel:
                conn.putheader("X-PhoneBox-Panel", "1")
            if method == "POST":
                conn.putheader("Content-Length", "0")
            conn.endheaders()
            r = conn.getresponse()
            body = r.read()
            status = r.status
        except (OSError, http.client.HTTPException) as e:
            status = f"ERR:{type(e).__name__}"
        finally:
            conn.close()
        ms = (time.monotonic() - start) * 1000
        self.log.line(f"{start - self.t0:9.3f}\t{self.phase}\t{self.label}\t{page or '-'}\t{method}\t{path}\t"
                      f"{status}\t{len(body)}\t{ms:.0f}")
        return status, body

    def fetch_config(self):
        status, body = self.request("GET", "/api/config")
        if status != 200:
            raise RuntimeError(f"{self.label}: /api/config answered {status}")
        self.config = json.loads(body)
        return self.config

    def run_event(self, e):
        if e.kind == "art":
            with self.lock:
                art = self.art_id
                if not art or self.art_seen.get(e.key) == art:
                    return
                self.art_seen[e.key] = art
            self.request("GET", f"/api/art?id={_quote(art)}", panel=False, page=e.page)
            return
        if e.kind == "photo":
            with self.lock:
                if not self.photo_ids:
                    return
                pos = self.photo_pos.get(e.key, 0)
                self.photo_pos[e.key] = pos + 1
                photo = self.photo_ids[pos % len(self.photo_ids)]
            self.request("GET", f"/api/photos/{_quote(photo)}", page=e.page)
            return
        status, body = self.request(e.method, e.path, e.panel, e.page)
        if status != 200:
            return
        if e.kind == "procs-icons":  # each new process name's icon once (fillIcon's cache)
            try:
                groups = json.loads(body).get("groups") or []
            except ValueError:
                return
            with self.lock:
                new = [g["name"] for g in groups if isinstance(g, dict) and g.get("name")
                       and g.get("hasIcon") is not False and g["name"] not in self.icons]
                new = new[:max(0, MAX_ICONS - len(self.icons))]
                self.icons.update(new)
            for name in new:
                self.pool.submit(self.request, "GET", f"/api/procs/icon?name={_quote(name)}", False, e.page)
        elif e.kind == "photo-list":
            try:
                ids = json.loads(body).get("ids") or []
            except ValueError:
                return
            with self.lock:
                self.photo_ids = [str(i) for i in ids]

    def run_events(self, events, t0, stop):
        for e in events:
            left = t0 + e.at - time.monotonic()
            if left > 0 and stop.wait(left):
                return
            if stop.is_set():
                return
            self.pool.submit(self.run_event, e)

    # the stream: held open and read, reconnected after a second when it drops, as the shell does on USB
    def open_stream(self):
        if self._stream_thread:
            return
        self._stream_stop.clear()
        self._stream_thread = threading.Thread(target=self._stream_loop, name=f"stream-{self.label}", daemon=True)
        self._stream_thread.start()

    def close_stream(self):
        if not self._stream_thread:
            return
        self._stream_stop.set()
        conn = self._stream_conn
        if conn is not None and conn.sock is not None:
            try:
                conn.sock.shutdown(2)
            except OSError:
                pass
        self._stream_thread.join(15)
        self._stream_thread = None

    def _stream_loop(self):
        while not self._stream_stop.is_set():
            start = time.monotonic()
            conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=15)
            self._stream_conn = conn
            status = "ERR"
            try:
                conn.putrequest("GET", "/api/stream", skip_host=True, skip_accept_encoding=True)
                for k, v in self.headers.items():
                    conn.putheader(k, v)
                conn.endheaders()
                r = conn.getresponse()
                status = r.status
                self.log.line(f"{start - self.t0:9.3f}\t{self.phase}\t{self.label}\t-\tGET\t/api/stream\t{status}\t"
                              f"open\t{(time.monotonic() - start) * 1000:.0f}")
                if status == 200:
                    self._read_events(r)
            except (OSError, http.client.HTTPException, ValueError) as e:
                if not self._stream_stop.is_set():
                    self.log.line(f"{time.monotonic() - self.t0:9.3f}\t{self.phase}\t{self.label}\t-\tGET\t"
                                  f"/api/stream\tERR:{type(e).__name__}\tclosed\t0")
            finally:
                conn.close()
                self._stream_conn = None
            self._stream_stop.wait(1)

    def _read_events(self, r):
        event, data = "", []
        while not self._stream_stop.is_set():
            line = r.readline()
            if not line:
                raise ConnectionError("the stream ended")
            line = line.decode("utf-8", "replace").rstrip("\r\n")
            if line.startswith("event:"):
                event = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].lstrip())
            elif not line:
                if event == "state" and data:
                    try:
                        media = json.loads("\n".join(data)).get("media") or {}
                    except ValueError:
                        media = {}
                    with self.lock:
                        self.art_id = media.get("artId") if isinstance(media, dict) else None
                event, data = "", []


def write_phase(path, name):
    if not path:
        return
    tmp = f"{path}.tmp"
    Path(tmp).write_text(name + "\n")
    os.replace(tmp, path)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--agent", action="append", required=True, metavar="LABEL=PORT", help="an agent (repeat)")
    ap.add_argument("--phases", default="warmup=120,S0=600,S1=600,S2=720", type=parse_phases)
    ap.add_argument("--page-seconds", type=int, default=30)
    ap.add_argument("--phase-file", help="written with the current phase, for sample.py")
    ap.add_argument("--log", help="the schedule log (default: stdout)")
    ap.add_argument("--battery", action="store_true",
                    help="also POST /api/phone/battery every 30 s (only with charge switching off in the config)")
    ap.add_argument("--app-version", default="1.0.0-alpha.1")
    ap.add_argument("--device", default="phonebox-resource")
    ap.add_argument("--model", default="PhoneBoxResource")
    args = ap.parse_args()
    log = Log(args.log)
    agents = []
    for item in args.agent:
        label, _, port = item.partition("=")
        agents.append(Agent(label, int(port), args, log))
    stop = threading.Event()

    def on_term(*_):
        raise SystemExit(143)

    signal.signal(signal.SIGTERM, on_term)
    log.line(f"# client: agents {', '.join(f'{a.label}:{a.port}' for a in agents)}; phases "
             f"{', '.join(f'{n} {s} s' for n, s in args.phases)}; page {args.page_seconds} s; battery reports "
             f"{'ON' if args.battery else 'off (never POSTs /api/phone/battery)'}")
    try:
        previous = ""
        for name, seconds in args.phases:
            write_phase(args.phase_file, name)
            t0 = time.monotonic()
            for a in agents:
                a.phase, a.t0 = name, t0
            plans = []
            if name in ACTIVE:
                boot = previous not in ACTIVE
                for a in agents:
                    fetched = a.config is None
                    if fetched:
                        a.fetch_config()  # this is the first boot's own /api/config
                    s = build_schedule(a.config, name, seconds, args.page_seconds, boot, args.battery)
                    events = list(s.events)
                    first = next((e for e in events if e.at == 0 and e.path == "/api/config"), None)
                    if fetched and first:
                        events.remove(first)
                    plans.append((a, events))
                    log.line(f"# {time.strftime('%Y-%m-%dT%H:%M:%S')} {name} {seconds} s {a.label}: "
                             f"{len(events)} requests planned (cover and photo checks included); pages "
                             + " ".join(f"{pid}@{at}" for at, pid in s.pages))
                    if s.stream:
                        a.open_stream()
                ids = {tuple(p.get("id") for p in shown_pages(a.config)) for a in agents}
                if len(ids) > 1:
                    log.line(f"# WARNING: the agents show different pages: {sorted(ids)}")
            else:
                for a in agents:
                    a.close_stream()
                log.line(f"# {time.strftime('%Y-%m-%dT%H:%M:%S')} {name} {seconds} s: no requests")
            threads = [threading.Thread(target=a.run_events, args=(ev, t0, stop), daemon=True) for a, ev in plans]
            for t in threads:
                t.start()
            while not stop.wait(max(0.0, min(1.0, t0 + seconds - time.monotonic()))):
                if time.monotonic() >= t0 + seconds:
                    break
            for t in threads:
                t.join(5)
            previous = name
    except KeyboardInterrupt:
        return 130
    finally:
        stop.set()
        for a in agents:
            a.close_stream()
            a.pool.shutdown(wait=True, cancel_futures=True)
        write_phase(args.phase_file, "end")
        log.line(f"# {time.strftime('%Y-%m-%dT%H:%M:%S')} client done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
