//! Port of `DrmUsage.cs`: GPU load of Intel cards on Linux (i915 and xe have no `gpu_busy_percent`), added up from
//! the usage counters in every client's DRM fdinfo between two samples, as nvtop and intel_gpu_top do. Only the counters
//! are read, and only from processes this user may look at. A full discovery pass over /proc runs at most every
//! `discovery_seconds`; the samples in between re-read only the fdinfo files that pass found.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// One DRM client (an open file description of a GPU device) as its `/proc/<pid>/fdinfo/<fd>` shows it. `busy` is per
/// engine class: busy nanoseconds for i915 (`drm-engine-<class>`), busy GPU cycles for xe (`drm-cycles-<class>`), in which
/// case `total_cycles` has the cycles that passed for the same class.
#[derive(Debug, Clone, PartialEq)]
pub struct DrmClient {
    pub driver: String,
    pub client_id: String,
    pub pdev: Option<String>,
    pub busy: BTreeMap<String, u64>,
    pub total_cycles: BTreeMap<String, u64>,
    pub capacity: BTreeMap<String, i32>,
}

/// fdinfo files read per discovery pass at most; the pass stops there.
pub const MAX_FDINFO_READS: usize = 4096;
/// Open files looked at per discovery pass at most.
pub const MAX_FDS: usize = 200_000;

/// (pdev, client id): a client is one open file description.
pub type ClientKey = (String, String);

/// See the module doc. One lock around everything, as in C#.
pub struct DrmUsage {
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    discovery_seconds: f64,
    inner: std::sync::Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    meter: Meter,
    root: Option<std::path::PathBuf>,
    // the fdinfo files of the GPU clients the last discovery pass found, with the client each one showed
    known: Option<Vec<(std::path::PathBuf, ClientKey)>>,
    discovered_at: f64,
    discoveries: u32,
}

impl DrmUsage {
    pub fn new() -> DrmUsage {
        let start = std::time::Instant::now();
        DrmUsage::with_clock(Box::new(move || start.elapsed().as_secs_f64()), 30.0)
    }

    pub fn with_clock(
        clock: Box<dyn Fn() -> f64 + Send + Sync>,
        discovery_seconds: f64,
    ) -> DrmUsage {
        DrmUsage {
            clock,
            discovery_seconds,
            inner: Default::default(),
        }
    }

    /// Full discovery passes so far.
    pub fn discoveries(&self) -> u32 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .discoveries
    }

    /// Reads the GPU clients under `proc_root` and returns the load in % of those of `pdevs` that have one. A card has a
    /// value once one of its clients is measured against a baseline from an earlier sample; a card whose clients never
    /// show busy counters has no value at all (missing from the answer, not 0).
    pub fn sample(&self, proc_root: &Path, pdevs: &[String]) -> HashMap<String, f64> {
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = (self.clock)();
        if s.root.as_deref() != Some(proc_root) {
            s.meter.reset();
            s.known = None;
            s.root = Some(proc_root.to_path_buf());
        }
        let (cur, unread) = match s.known.take() {
            Some(mut known) if now - s.discovered_at < self.discovery_seconds => {
                let (cur, unread) = reread(&mut known);
                s.known = Some(known);
                (cur, Some(unread))
            }
            previous => {
                let found = discover(proc_root);
                let mut cur: Vec<DrmClient> = found.iter().map(|f| f.1.clone()).collect();
                let mut known: Vec<(std::path::PathBuf, ClientKey)> = found
                    .iter()
                    .map(|(path, c)| (path.clone(), Meter::key_of(c)))
                    .collect();
                let mut unread = None;
                if let Some(previous) = previous {
                    // a known client the walk didn't get (unreadable for a moment, or past the cap) is tried once more
                    // and kept while it isn't confirmed gone, so that it keeps its baseline
                    let walked: HashSet<&std::path::PathBuf> = known.iter().map(|k| &k.0).collect();
                    let mut missed: Vec<_> = previous
                        .into_iter()
                        .filter(|k| !walked.contains(&k.0))
                        .collect();
                    let (again, u) = reread(&mut missed);
                    cur.extend(again);
                    unread = Some(u);
                    known.extend(missed);
                }
                s.known = Some(known);
                s.discovered_at = now;
                s.discoveries += 1;
                (cur, unread)
            }
        };
        s.meter.update(&cur, now, pdevs, unread.as_ref())
    }

    /// The i915 and xe clients of every process this user can read (one discovery pass).
    pub fn scan(proc_root: &Path) -> Vec<DrmClient> {
        discover(proc_root).into_iter().map(|f| f.1).collect()
    }
}

impl Default for DrmUsage {
    fn default() -> Self {
        DrmUsage::new()
    }
}

/// The clients behind the fdinfo files a discovery pass found. A file that is gone or no longer an i915 / xe client is
/// dropped from `known`; one that can't be read this time stays, and its client goes to the unread set so that it keeps
/// its baseline instead of looking idle.
fn reread(
    known: &mut Vec<(std::path::PathBuf, ClientKey)>,
) -> (Vec<DrmClient>, HashSet<ClientKey>) {
    let mut list = Vec::with_capacity(known.len());
    let mut keep = Vec::with_capacity(known.len());
    let mut unread = HashSet::new();
    for (path, client) in known.drain(..) {
        match read_client(&path) {
            (Some(c), _) => {
                keep.push((path, Meter::key_of(&c)));
                list.push(c);
            }
            (None, false) => {
                unread.insert(client.clone());
                keep.push((path, client));
            }
            (None, true) => {}
        }
    }
    *known = keep;
    (list, unread)
}

/// The i915 / xe client in one fdinfo file; `gone` when the file no longer exists or holds no such client.
fn read_client(path: &Path) -> (Option<DrmClient>, bool) {
    match std::fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            match text
                .contains("drm-driver:")
                .then(|| parse_fdinfo(&text))
                .flatten()
            {
                Some(c) if (c.driver == "i915" || c.driver == "xe") && c.pdev.is_some() => {
                    (Some(c), false)
                }
                _ => (None, true),
            }
        }
        Err(e) => {
            let gone = matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            );
            (None, gone)
        }
    }
}

/// The discovery pass: pids in order, bounded work, so a capped pass sees the same processes each time; anything
/// unreadable (another user's process, one that just exited) is skipped.
fn discover(proc_root: &Path) -> Vec<(std::path::PathBuf, DrmClient)> {
    let mut list = Vec::new();
    let Ok(dirs) = std::fs::read_dir(proc_root) else {
        return list;
    };
    let mut pids: Vec<u64> = dirs
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))?
                .parse()
                .ok()
        })
        .collect();
    pids.sort_unstable();
    let (mut reads, mut fds) = (0usize, 0usize);
    for pid in pids {
        let proc = proc_root.join(pid.to_string());
        let Ok(entries) = std::fs::read_dir(proc.join("fdinfo")) else {
            continue;
        };
        let mut files: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        for file in files {
            fds += 1;
            if fds > MAX_FDS || reads >= MAX_FDINFO_READS {
                return list;
            }
            let Some(name) = file.file_name() else {
                continue;
            };
            if !maybe_gpu(&proc.join("fd").join(name)) {
                continue;
            }
            reads += 1;
            if let (Some(c), _) = read_client(&file) {
                list.push((file, c));
            }
        }
    }
    list
}

/// An fd whose link points elsewhere than /dev/dri (a socket, a pipe, a regular file) is not a GPU client; when the link
/// can't be read, the fdinfo decides.
fn maybe_gpu(fd_link: &Path) -> bool {
    match std::fs::read_link(fd_link) {
        Ok(target) => target.to_string_lossy().starts_with("/dev/dri/"),
        Err(_) => true,
    }
}

/// The load arithmetic, without any I/O. Per client and engine class it keeps the highest busy counter accepted so far
/// with its reference (the read time for i915, the total cycles for xe) and the sample it came from. Only a baseline
/// from the sample just before counts; a counter that reads lower is ignored until it catches up. Per engine class the
/// shares are summed over the clients and divided by the engine count; a card's load is its busiest class.
/// (pdev, client id, engine class).
type ClassKey = (String, String, String);
/// The accepted busy counter, its reference (total cycles for xe, read time for i915) and the sample it came from.
type Baseline = (u64, u64, f64, i64);

#[derive(Default)]
pub struct Meter {
    base: HashMap<ClassKey, Baseline>,
    // cards with a valid measurement so far: from then on a card with no clients left is idle (0)
    measured: HashSet<String>,
    sample: i64,
}

impl Meter {
    /// A client is one open file description: several fds can share it and show the same counters.
    pub fn key_of(c: &DrmClient) -> ClientKey {
        (
            c.pdev.clone().unwrap_or_default().to_lowercase(),
            c.client_id.clone(),
        )
    }

    pub fn reset(&mut self) {
        self.base.clear();
        self.measured.clear();
    }

    /// Takes the clients read at `now` (seconds) and returns the load in % of those of `pdevs` that have a value.
    /// Clients in `unread`, and clients without busy counters this time, keep their baselines; any other missing client
    /// has gone, and its baselines with it.
    pub fn update(
        &mut self,
        clients: &[DrmClient],
        now: f64,
        pdevs: &[String],
        unread: Option<&HashSet<ClientKey>>,
    ) -> HashMap<String, f64> {
        self.sample += 1;
        let sample = self.sample;
        let mut shares: HashMap<(String, String), f64> = HashMap::new();
        let mut counted: HashSet<ClientKey> = HashSet::new();
        let mut unknown: HashSet<ClientKey> = unread.cloned().unwrap_or_default();
        let mut present: HashSet<String> = HashSet::new();
        let mut measured_now: HashSet<String> = HashSet::new();
        let mut seen: HashSet<ClientKey> = HashSet::new();
        // two descriptors of one client count once; the one with counters wins
        let mut ordered: Vec<&DrmClient> = clients.iter().collect();
        ordered.sort_by_key(|c| c.busy.is_empty()); // stable
        for c in ordered {
            if c.pdev.is_none() || !seen.insert(Meter::key_of(c)) {
                continue;
            }
            let (pdev, id) = Meter::key_of(c);
            present.insert(pdev.clone());
            if c.busy.is_empty() {
                unknown.insert((pdev, id));
                continue;
            }
            counted.insert((pdev.clone(), id.clone()));
            for (cls, &busy) in &c.busy {
                let total_cycles = c.total_cycles.get(cls).copied();
                let xe = total_cycles.is_some();
                let total = total_cycles.unwrap_or(0);
                let key = (pdev.clone(), id.clone(), cls.clone());
                let Some(&b) = self.base.get(&key) else {
                    self.base.insert(key, (busy, total, now, sample)); // first sight: a baseline
                    continue;
                };
                if busy < b.0 {
                    continue; // keep the higher value until the counter catches up
                }
                if b.3 != sample - 1 {
                    self.base.insert(key, (busy, total, now, sample)); // missed a sample: another interval
                    continue;
                }
                // xe: busy cycles over the GPU cycles that passed for this client, independent of the wall clock
                let passed = if xe { total > b.1 } else { now > b.2 };
                if !passed {
                    self.base.insert(key, (b.0, b.1, b.2, sample)); // no time passed: same baseline
                    continue;
                }
                let share = if xe {
                    (busy - b.0) as f64 / (total - b.1) as f64
                } else {
                    (busy - b.0) as f64 / ((now - b.2) * 1e9)
                };
                self.base.insert(key, (busy, total, now, sample));
                let engines = c.capacity.get(cls).copied().filter(|n| *n > 1).unwrap_or(1);
                *shares.entry((pdev.clone(), cls.clone())).or_default() +=
                    share / f64::from(engines);
                self.measured.insert(pdev.clone());
                measured_now.insert(pdev.clone());
            }
        }
        self.base.retain(|k, _| {
            let client = (k.0.clone(), k.1.clone());
            counted.contains(&client) || unknown.contains(&client)
        });

        let mut load = HashMap::new();
        for card in pdevs {
            let pdev = card.to_lowercase();
            let idle = self.measured.contains(&pdev)
                && !present.contains(&pdev)
                && !unknown.iter().any(|k| k.0 == pdev);
            if !measured_now.contains(&pdev) && !idle {
                continue;
            }
            let max = shares
                .iter()
                .filter(|(k, _)| k.0 == pdev)
                .map(|(_, v)| *v)
                .fold(0.0, f64::max);
            load.insert(
                card.clone(),
                crate::system_monitor::round_dotnet((max * 100.0).clamp(0.0, 100.0), 1),
            );
        }
        load
    }
}

/// The DRM part of one fdinfo file, or None when it isn't a DRM client. Memory and other lines are ignored.
pub fn parse_fdinfo(text: &str) -> Option<DrmClient> {
    let (mut driver, mut client, mut pdev) = (None, None, None);
    let mut engine_ns: BTreeMap<String, u64> = BTreeMap::new();
    let mut cycles: BTreeMap<String, u64> = BTreeMap::new();
    let mut total: BTreeMap<String, u64> = BTreeMap::new();
    let mut capacity: BTreeMap<String, i32> = BTreeMap::new();
    for line in text.split('\n') {
        let Some(colon) = line.find(':').filter(|c| *c > 0) else {
            continue;
        };
        let key = line[..colon].trim();
        let value = line[colon + 1..].trim();
        if !key.starts_with("drm-") || value.is_empty() {
            continue;
        }
        match key {
            "drm-driver" => driver = Some(value.to_string()),
            "drm-client-id" => client = Some(value.to_string()),
            "drm-pdev" => pdev = Some(value.to_lowercase()),
            // the longer prefixes first: "drm-engine-capacity-video" is not an engine called "capacity-video"
            _ => {
                if let Some(cls) = class(key, "drm-engine-capacity-") {
                    if let Some(n) = digits(value).and_then(|d| d.parse::<i32>().ok()) {
                        capacity.insert(cls.into(), n);
                    }
                } else if let Some(cls) = class(key, "drm-engine-") {
                    if let Some(ns) = counter(value, Some("ns")) {
                        engine_ns.insert(cls.into(), ns);
                    }
                } else if let Some(cls) = class(key, "drm-total-cycles-") {
                    if let Some(t) = counter(value, None) {
                        total.insert(cls.into(), t);
                    }
                } else if let Some(cls) = class(key, "drm-cycles-")
                    && let Some(cy) = counter(value, None)
                {
                    cycles.insert(cls.into(), cy);
                }
            }
        }
    }
    let (driver, client_id) = (driver?, client?);
    // cycles need their total to mean anything; a class with both kinds of counter is read as cycles
    let mut busy = BTreeMap::new();
    let mut totals = BTreeMap::new();
    for (cls, cy) in cycles {
        if let Some(t) = total.get(&cls) {
            busy.insert(cls.clone(), cy);
            totals.insert(cls, *t);
        }
    }
    for (cls, ns) in engine_ns {
        busy.entry(cls).or_insert(ns);
    }
    Some(DrmClient {
        driver,
        client_id,
        pdev,
        busy,
        total_cycles: totals,
        capacity,
    })
}

fn class<'a>(key: &'a str, prefix: &str) -> Option<&'a str> {
    (key.len() > prefix.len() && key.starts_with(prefix)).then(|| &key[prefix.len()..])
}

/// `NumberStyles.None`: digits only.
fn digits(s: &str) -> Option<&str> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then_some(s)
}

/// "25662044495 ns" → 25662044495; a unit other than `unit` (or any unit, when None) is not a counter.
fn counter(value: &str, unit: Option<&str>) -> Option<u64> {
    let (number, rest) = match value.find([' ', '\t']) {
        Some(i) => (&value[..i], value[i..].trim()),
        None => (value, ""),
    };
    if !rest.is_empty() && Some(rest) != unit {
        return None;
    }
    digits(number)?.parse().ok()
}

/// The fdinfo texts the tests use (shared with the gpus tests).
#[cfg(test)]
pub(crate) mod fixtures {
    pub const IGPU: &str = "0000:00:02.0";

    pub fn i915(client: &str, render: u64, video: u64, pdev: &str) -> String {
        format!(
            "pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t685\ndrm-driver:\ti915\ndrm-client-id:\t{client}\n\
             drm-pdev:\t{pdev}\ndrm-total-system0:\t12288 KiB\ndrm-shared-system0:\t0\ndrm-active-system0:\t0\n\
             drm-resident-system0:\t12288 KiB\ndrm-purgeable-system0:\t0\ndrm-memory-system0:\t12288 KiB\n\
             drm-engine-render:\t{render} ns\ndrm-engine-copy:\t0 ns\ndrm-engine-video:\t{video} ns\n\
             drm-engine-capacity-video:\t2\ndrm-engine-video-enhance:\t0 ns"
        )
    }

    pub fn xe(client: &str, rcs: u64, total: u64, vcs: u64, pdev: &str) -> String {
        format!(
            "pos:\t0\nflags:\t0100002\nmnt_id:\t26\nino:\t1061\ndrm-driver:\txe\ndrm-client-id:\t{client}\n\
             drm-pdev:\t{pdev}\ndrm-total-system:\t0\ndrm-shared-system:\t0\ndrm-active-system:\t0\n\
             drm-resident-system:\t0\ndrm-purgeable-system:\t0\ndrm-total-gtt:\t192 KiB\ndrm-shared-gtt:\t0\n\
             drm-active-gtt:\t0\ndrm-resident-gtt:\t192 KiB\ndrm-cycles-rcs:\t{rcs}\ndrm-total-cycles-rcs:\t{total}\n\
             drm-cycles-bcs:\t0\ndrm-total-cycles-bcs:\t{total}\ndrm-cycles-vcs:\t{vcs}\ndrm-total-cycles-vcs:\t{total}\n\
             drm-engine-capacity-vcs:\t2\ndrm-cycles-vecs:\t0\ndrm-total-cycles-vecs:\t{total}\ndrm-cycles-ccs:\t0\n\
             drm-total-cycles-ccs:\t{total}\ndrm-engine-capacity-ccs:\t4"
        )
    }

    pub fn write(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::fixtures::{IGPU, write};
    use super::*;

    fn i915(client: &str, render: u64) -> DrmClient {
        p(&super::fixtures::i915(client, render, 0, IGPU))
    }
    fn i915v(client: &str, render: u64, video: u64) -> DrmClient {
        p(&super::fixtures::i915(client, render, video, IGPU))
    }
    fn i915on(client: &str, render: u64, pdev: &str) -> DrmClient {
        p(&super::fixtures::i915(client, render, 0, pdev))
    }
    fn xe(client: &str, rcs: u64, total: u64) -> DrmClient {
        p(&super::fixtures::xe(client, rcs, total, 0, IGPU))
    }
    fn xev(client: &str, rcs: u64, total: u64, vcs: u64) -> DrmClient {
        p(&super::fixtures::xe(client, rcs, total, vcs, IGPU))
    }
    fn p(text: &str) -> DrmClient {
        parse_fdinfo(text).expect("not a DRM client")
    }
    fn cards(pdev: &str) -> Vec<String> {
        vec![pdev.to_string()]
    }

    /// The load after two samples, `seconds` apart.
    fn load(prev: &[DrmClient], cur: &[DrmClient], seconds: f64, pdev: &str) -> Option<f64> {
        let mut meter = Meter::default();
        meter.update(prev, 0.0, &cards(pdev), None);
        meter
            .update(cur, seconds, &cards(pdev), None)
            .get(pdev)
            .copied()
    }
    fn load1(prev: &[DrmClient], cur: &[DrmClient]) -> Option<f64> {
        load(prev, cur, 1.0, IGPU)
    }
    fn value(load: HashMap<String, f64>) -> Option<f64> {
        load.get(IGPU).copied()
    }
    fn up(meter: &mut Meter, clients: &[DrmClient], now: f64) -> Option<f64> {
        value(meter.update(clients, now, &cards(IGPU), None))
    }

    #[test]
    fn an_i915_fdinfo_is_parsed_without_its_memory_lines() {
        let c = p(&super::fixtures::i915("7", 25_662_044_495, 1200, IGPU));
        assert_eq!(c.driver, "i915");
        assert_eq!(c.client_id, "7");
        assert_eq!(c.pdev.as_deref(), Some(IGPU));
        assert_eq!(
            c.busy.keys().collect::<Vec<_>>(),
            ["copy", "render", "video", "video-enhance"]
        );
        assert_eq!(c.busy["render"], 25_662_044_495);
        assert_eq!(c.busy["video"], 1200);
        assert!(c.total_cycles.is_empty()); // i915 counts nanoseconds
        assert_eq!(c.capacity.len(), 1); // "capacity-video" is not an engine of its own
        assert_eq!(c.capacity["video"], 2);
    }

    #[test]
    fn an_xe_fdinfo_is_parsed_into_cycles_and_totals() {
        let c = xev("19", 28_257_900, 7_655_183_225, 500);
        assert_eq!(c.driver, "xe");
        assert_eq!(c.client_id, "19");
        assert_eq!(c.pdev.as_deref(), Some(IGPU));
        assert_eq!(
            c.busy.keys().collect::<Vec<_>>(),
            ["bcs", "ccs", "rcs", "vcs", "vecs"]
        );
        assert_eq!(c.busy["rcs"], 28_257_900);
        assert_eq!(c.busy["vcs"], 500);
        assert_eq!(c.total_cycles["rcs"], 7_655_183_225); // and not "drm-total-system" or "drm-total-gtt"
        assert_eq!(c.total_cycles.len(), 5);
        assert_eq!(c.capacity["vcs"], 2);
        assert_eq!(c.capacity["ccs"], 4);
    }

    #[test]
    fn other_fdinfo_is_not_a_client() {
        for text in [
            "pos:\t0\nflags:\t02\nmnt_id:\t15\nino:\t1033\n", // a regular file
            "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\n",   // no client id
            "",
        ] {
            assert_eq!(parse_fdinfo(text), None, "{text:?}");
        }
    }

    #[test]
    fn i915_load_is_busy_nanoseconds_over_the_interval() {
        // render: 0.5 s busy in 1 s = 50 %; video: 0.8 s busy on 2 engines = 40 %; the busiest class wins
        let prev = [i915v("7", 1_000_000_000, 100_000_000)];
        let cur = [i915v("7", 1_500_000_000, 900_000_000)];
        assert_eq!(load1(&prev, &cur), Some(50.0));
        assert_eq!(load(&prev, &cur, 2.0, IGPU), Some(25.0));
    }

    #[test]
    fn xe_load_is_busy_cycles_over_total_cycles() {
        // rcs: 300 of 1200 cycles = 25 %; vcs: 1080 of 1200 on 2 engines = 45 %; the wall clock doesn't matter
        let prev = [xev("19", 1000, 50_000, 20)];
        let cur = [xev("19", 1300, 51_200, 1100)];
        assert_eq!(load(&prev, &cur, 2.0, IGPU), Some(45.0));
        assert_eq!(load(&prev, &cur, 7.0, IGPU), Some(45.0));
    }

    #[test]
    fn clients_add_up_and_two_fds_of_one_client_count_once() {
        let prev = [xe("19", 0, 1000), xe("19", 0, 1000), xe("20", 0, 1000)];
        let cur = [
            xe("19", 300, 2000),
            xe("19", 300, 2000),
            xe("20", 200, 2000),
        ];
        assert_eq!(load1(&prev, &cur), Some(50.0)); // 30 % + 20 %, not 30 % + 30 % + 20 %
    }

    #[test]
    fn only_clients_in_both_samples_and_on_this_card_count() {
        let other = "0000:03:00.0";
        let prev = [i915("1", 0), i915("2", 0), i915on("3", 0, other)];
        let cur = [
            i915("1", 200_000_000),
            i915("4", 900_000_000),
            i915on("3", 900_000_000, other),
        ];
        // client 2 is gone, client 4 is new (no baseline yet), client 3 is on another card
        assert_eq!(load1(&prev, &cur), Some(20.0));
        assert_eq!(load(&prev, &cur, 1.0, other), Some(90.0));
    }

    #[test]
    fn a_counter_that_went_backwards_is_skipped() {
        // a reused client id: render went down, so only its video counter (on 2 engines) counts this time
        assert_eq!(
            load1(
                &[i915v("1", 5_000_000_000, 0)],
                &[i915v("1", 1_000_000_000, 600_000_000)]
            ),
            Some(30.0)
        );
        // xe: the total going backwards too, so nothing was measured at all
        assert_eq!(load1(&[xe("1", 500, 9000)], &[xe("1", 600, 8000)]), None);
    }

    #[test]
    fn a_dip_is_ignored_until_the_counter_catches_up() {
        // i915, a second apart: 1000 → 800 → 1000 → 1200 (in 1e6 ns); the way back up from 800 is not load
        let mut meter = Meter::default();
        assert_eq!(up(&mut meter, &[i915("1", 1_000_000_000)], 0.0), None);
        assert_eq!(up(&mut meter, &[i915("1", 800_000_000)], 1.0), Some(0.0));
        assert_eq!(up(&mut meter, &[i915("1", 1_000_000_000)], 2.0), Some(0.0));
        assert_eq!(up(&mut meter, &[i915("1", 1_200_000_000)], 3.0), Some(20.0));

        // xe: the same, measured against the total cycles of the highest reading
        let mut meter = Meter::default();
        up(&mut meter, &[xe("1", 1000, 10_000)], 0.0);
        assert_eq!(up(&mut meter, &[xe("1", 800, 11_000)], 1.0), Some(0.0));
        assert_eq!(up(&mut meter, &[xe("1", 1000, 12_000)], 2.0), Some(0.0));
        assert_eq!(up(&mut meter, &[xe("1", 1200, 13_000)], 3.0), Some(20.0));
    }

    #[test]
    fn only_shares_over_the_same_interval_are_added_up() {
        // A is busy from 0 to 2 s and idle after, but its 2 s read fails; B is 20 % busy from 2 to 4 s.
        // Measured from 0 s, A would add 50 % to B's 20 %; it missed a sample, so it takes a new baseline instead.
        let mut meter = Meter::default();
        up(&mut meter, &[i915("A", 0)], 0.0);
        up(&mut meter, &[i915("A", 0)], 1.0);
        let unread: HashSet<ClientKey> = [(IGPU.to_string(), "A".to_string())].into();
        meter.update(&[i915("B", 0)], 2.0, &cards(IGPU), Some(&unread));
        assert_eq!(
            up(
                &mut meter,
                &[i915("A", 2_000_000_000), i915("B", 400_000_000)],
                4.0
            ),
            Some(20.0)
        );
        assert_eq!(
            up(
                &mut meter,
                &[i915("A", 2_000_000_000), i915("B", 400_000_000)],
                6.0
            ),
            Some(0.0)
        );
    }

    #[test]
    fn a_card_is_measured_only_when_time_passed() {
        let mut meter = Meter::default();
        up(&mut meter, &[i915("1", 0)], 5.0);
        assert_eq!(up(&mut meter, &[i915("1", 0)], 5.0), None); // two reads at the same clock value
        assert_eq!(up(&mut meter, &[i915("1", 0)], 6.0), Some(0.0)); // an idle second: a real 0

        let mut meter = Meter::default();
        up(&mut meter, &[xe("1", 100, 1000)], 0.0);
        assert_eq!(up(&mut meter, &[xe("1", 100, 1000)], 2.0), None); // the GPU clock didn't move
        assert_eq!(up(&mut meter, &[xe("1", 100, 2000)], 4.0), Some(0.0));
    }

    #[test]
    fn an_xe_client_without_counters_for_a_moment_is_unknown_not_idle() {
        // xe prints the client without drm-cycles when it can't wake the GPU
        let none = p(
            "drm-driver:\txe\ndrm-client-id:\t1\ndrm-pdev:\t0000:00:02.0\ndrm-total-gtt:\t192 KiB\n",
        );
        let mut meter = Meter::default();
        up(&mut meter, &[xe("1", 0, 1000)], 0.0);
        assert_eq!(up(&mut meter, &[xe("1", 300, 2200)], 2.0), Some(25.0));
        assert_eq!(up(&mut meter, std::slice::from_ref(&none), 4.0), None); // the card's only client: no value, not 0
        assert_eq!(up(&mut meter, &[xe("1", 900, 3400)], 6.0), None); // back, but it missed a sample: a new baseline
        assert_eq!(up(&mut meter, &[xe("1", 1500, 4600)], 8.0), Some(50.0));

        // next to a client that was measured, the card shows that one
        let mut meter = Meter::default();
        up(&mut meter, &[xe("1", 0, 1000), xe("2", 0, 1000)], 0.0);
        assert_eq!(
            up(&mut meter, &[none.clone(), xe("2", 100, 2000)], 2.0),
            Some(10.0)
        );

        // two descriptors of one client, the first without counters: the one with counters is used, once
        let mut meter = Meter::default();
        up(&mut meter, &[none.clone(), xe("1", 0, 1000)], 0.0);
        assert_eq!(
            up(
                &mut meter,
                &[none, xe("1", 300, 2200), xe("1", 300, 2200)],
                2.0
            ),
            Some(25.0)
        );
    }

    #[test]
    fn a_card_has_a_value_only_once_a_client_was_measured_and_stays_at_zero_when_its_clients_exit()
    {
        let mut meter = Meter::default();
        assert_eq!(up(&mut meter, &[], 0.0), None); // nothing on the card yet
        assert_eq!(up(&mut meter, &[xe("1", 5000, 10_000)], 2.0), None); // a busy client appears: its baseline only
        assert_eq!(up(&mut meter, &[xe("1", 5300, 11_200)], 4.0), Some(25.0));
        assert_eq!(up(&mut meter, &[], 6.0), Some(0.0)); // its last client exited
    }

    #[test]
    fn the_load_stays_within_a_hundred_and_a_second_read_at_the_same_time_keeps_the_baseline() {
        assert_eq!(
            load1(&[i915("1", 0)], &[i915("1", 3_000_000_000)]),
            Some(100.0)
        );
        let mut meter = Meter::default();
        up(&mut meter, &[i915("1", 0)], 0.0);
        up(&mut meter, &[i915("1", 250_000_000)], 0.0); // no time passed: nothing measured, the baseline stays
        assert_eq!(up(&mut meter, &[i915("1", 500_000_000)], 1.0), Some(50.0));
    }

    // ---------------------------------------------------------------- the whole path, on a fake /proc

    /// A DrmUsage whose clock the test sets.
    fn usage(discovery_seconds: f64) -> (DrmUsage, Arc<Mutex<f64>>) {
        let now = Arc::new(Mutex::new(0.0));
        let clock = now.clone();
        (
            DrmUsage::with_clock(Box::new(move || *clock.lock().unwrap()), discovery_seconds),
            now,
        )
    }
    fn set(now: &Arc<Mutex<f64>>, t: f64) {
        *now.lock().unwrap() = t;
    }
    fn fixture_i915(client: &str, render: u64) -> String {
        super::fixtures::i915(client, render, 0, IGPU)
    }

    #[test]
    fn the_first_sample_has_no_load_and_an_idle_card_then_shows_zero() {
        let root = tempfile::tempdir().unwrap();
        let proc = root.path();
        let (usage, now) = usage(30.0);
        set(&now, 10.0);
        write(
            &proc.join("300/fdinfo/5"),
            &fixture_i915("1", 4_000_000_000),
        );
        assert!(usage.sample(proc, &cards(IGPU)).is_empty()); // first sample: a baseline, no value
        set(&now, 12.0);
        write(
            &proc.join("300/fdinfo/5"),
            &fixture_i915("1", 4_100_000_000),
        );
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 5.0);
        set(&now, 14.0);
        std::fs::remove_dir_all(proc.join("300")).unwrap(); // the only client exited
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 0.0);
        assert!(
            DrmUsage::new()
                .sample(&proc.join("missing"), &cards(IGPU))
                .is_empty()
        );
    }

    #[test]
    fn a_client_that_cant_be_read_for_a_moment_keeps_its_baseline() {
        let root = tempfile::tempdir().unwrap();
        let proc = root.path();
        let fdinfo = proc.join("300/fdinfo/5");
        let (usage, now) = usage(30.0);
        write(&fdinfo, &fixture_i915("1", 0));
        usage.sample(proc, &cards(IGPU));
        set(&now, 2.0);
        write(&fdinfo, &fixture_i915("1", 200_000_000));
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 10.0);

        // the file can't be read for one sample (a directory in its place: access denied, not "not found")
        std::fs::remove_file(&fdinfo).unwrap();
        std::fs::create_dir(&fdinfo).unwrap();
        set(&now, 4.0);
        assert!(usage.sample(proc, &cards(IGPU)).is_empty()); // its load is unknown this time, not 0

        // readable again and still known without a new pass; it missed a sample, so it takes a new baseline first
        std::fs::remove_dir(&fdinfo).unwrap();
        write(&fdinfo, &fixture_i915("1", 1_400_000_000));
        set(&now, 6.0);
        assert!(usage.sample(proc, &cards(IGPU)).is_empty());
        write(&fdinfo, &fixture_i915("1", 1_800_000_000));
        set(&now, 8.0);
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 20.0);
        assert_eq!(usage.discoveries(), 1);
    }

    #[test]
    fn a_client_unreadable_during_a_discovery_pass_is_kept_and_retried() {
        let root = tempfile::tempdir().unwrap();
        let proc = root.path();
        let fdinfo = proc.join("300/fdinfo/5");
        let (usage, now) = usage(30.0);
        write(&fdinfo, &fixture_i915("1", 0));
        usage.sample(proc, &cards(IGPU));
        set(&now, 2.0);
        write(&fdinfo, &fixture_i915("1", 200_000_000));
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 10.0);

        // unreadable while the 30 s pass walks /proc: the pass doesn't find it, but it isn't forgotten
        std::fs::remove_file(&fdinfo).unwrap();
        std::fs::create_dir(&fdinfo).unwrap();
        set(&now, 30.0);
        assert!(usage.sample(proc, &cards(IGPU)).is_empty()); // unknown, not 0
        assert_eq!(usage.discoveries(), 2);

        std::fs::remove_dir(&fdinfo).unwrap();
        write(&fdinfo, &fixture_i915("1", 1_000_000_000));
        set(&now, 32.0);
        assert!(usage.sample(proc, &cards(IGPU)).is_empty()); // re-read by the 2 s poll: a new baseline
        write(&fdinfo, &fixture_i915("1", 1_600_000_000));
        set(&now, 34.0);
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 30.0);
        assert_eq!(usage.discoveries(), 2);
    }

    #[test]
    fn between_discovery_passes_only_the_known_clients_are_read() {
        let root = tempfile::tempdir().unwrap();
        let proc = root.path();
        let f = |pid: &str| proc.join(pid).join("fdinfo/5");
        let (usage, now) = usage(30.0);
        write(&f("300"), &fixture_i915("1", 0));
        assert!(usage.sample(proc, &cards(IGPU)).is_empty());
        assert_eq!(usage.discoveries(), 1);

        write(&f("400"), &fixture_i915("2", 0)); // a new client: not seen until the next pass
        set(&now, 2.0);
        write(&f("300"), &fixture_i915("1", 200_000_000));
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 10.0);

        set(&now, 4.0);
        write(&f("300"), &fixture_i915("1", 400_000_000));
        write(&f("400"), &fixture_i915("2", 1_000_000_000)); // would add 50 % had pid 400 been walked at 2 s
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 10.0);
        assert_eq!(usage.discoveries(), 1);

        set(&now, 30.0); // the next pass finds pid 400 and takes its baseline
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 0.0);
        assert_eq!(usage.discoveries(), 2);

        set(&now, 32.0);
        write(&f("400"), &fixture_i915("2", 1_600_000_000));
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 30.0);
        assert_eq!(usage.discoveries(), 2);
    }

    #[test]
    fn a_client_whose_fdinfo_is_gone_is_dropped_without_a_full_pass() {
        let root = tempfile::tempdir().unwrap();
        let proc = root.path();
        let f = |pid: &str| proc.join(pid).join("fdinfo/5");
        let (usage, now) = usage(30.0);
        write(&f("300"), &fixture_i915("1", 0));
        write(&f("301"), &fixture_i915("2", 0));
        assert!(usage.sample(proc, &cards(IGPU)).is_empty());

        set(&now, 2.0);
        write(&f("300"), &fixture_i915("1", 200_000_000));
        write(&f("301"), &fixture_i915("2", 200_000_000));
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 20.0);

        std::fs::remove_dir_all(proc.join("301")).unwrap(); // the process exited
        set(&now, 4.0);
        write(&f("300"), &fixture_i915("1", 400_000_000));
        assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 10.0);

        // a new process under the same path is not read again before the next pass
        write(&f("301"), &fixture_i915("2", 0));
        for i in 1..=2u64 {
            set(&now, 4.0 + 2.0 * i as f64);
            write(&f("300"), &fixture_i915("1", 400_000_000 + 200_000_000 * i));
            write(&f("301"), &fixture_i915("2", 1_000_000_000 * i));
            assert_eq!(usage.sample(proc, &cards(IGPU))[IGPU], 10.0);
        }
        assert_eq!(usage.discoveries(), 1);
    }

    #[test]
    fn a_kernel_without_busy_counters_leaves_the_load_empty_not_zero() {
        let root = tempfile::tempdir().unwrap();
        let proc = root.path();
        // early xe: client, card and memory lines, but no drm-cycles; i915 before 5.19: no drm-* lines at all
        write(
            &proc.join("300/fdinfo/5"),
            "pos:\t0\nflags:\t0100002\nmnt_id:\t26\nino:\t1061\ndrm-driver:\txe\n\
             drm-client-id:\t3\ndrm-pdev:\t0000:00:02.0\ndrm-total-system:\t0\ndrm-resident-gtt:\t192 KiB\n",
        );
        write(
            &proc.join("301/fdinfo/4"),
            "pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t685\n",
        );
        let (usage, now) = usage(30.0);
        for t in [10.0, 12.0, 14.0] {
            set(&now, t);
            assert!(usage.sample(proc, &cards(IGPU)).is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn fds_that_arent_gpu_devices_are_not_read() {
        let root = tempfile::tempdir().unwrap();
        let pid = root.path().join("500");
        write(&pid.join("fdinfo/3"), &fixture_i915("1", 0));
        write(&pid.join("fdinfo/4"), &fixture_i915("2", 0));
        std::fs::create_dir_all(pid.join("fd")).unwrap();
        std::os::unix::fs::symlink("/dev/dri/renderD128", pid.join("fd/3")).unwrap();
        std::os::unix::fs::symlink("/run/user/1000/socket", pid.join("fd/4")).unwrap();
        let found = DrmUsage::scan(root.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].client_id, "1");
    }
}
