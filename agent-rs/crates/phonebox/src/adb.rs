//! Port of `AdbReverse.cs`: while a phone is connected over USB, keeps an "adb reverse tcp:8765 tcp:<port>" rule in
//! place so that 127.0.0.1:8765 on the phone reaches this agent. A newly plugged-in phone is woken and shows the panel.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::config::store::ConfigStore;

pub const PANEL_ACTIVITY: &str = "net.oxog.phonebox.deskpanel/.MainActivity";
const HOME_ALIAS: &str = "net.oxog.phonebox.deskpanel/.HomeAlias";
const PACKAGE: &str = "net.oxog.phonebox.deskpanel";
const RULE: &str = "tcp:8765";
const CATEGORY: &str = "AdbReverse";
/// Pause between two rounds.
const ROUND_PAUSE: Duration = Duration::from_secs(4);

const INSTALL_HINT: &str = if cfg!(windows) {
    "run 'winget install Google.PlatformTools'"
} else if cfg!(target_os = "macos") {
    "run 'brew install android-platform-tools'"
} else {
    "run 'sudo apt install adb' (or your distribution's package)"
};

/// What an adb command printed on standard output, and its exit code.
#[derive(Debug, Clone, PartialEq)]
pub struct AdbOutput {
    pub code: i32,
    pub output: String,
}

pub type AdbFuture<'a> = Pin<Box<dyn Future<Output = Result<AdbOutput, String>> + Send + 'a>>;

/// Runs adb: the real program ([`ProcessRunner`]) or a fake in tests. An Err means adb did not run to its end (could
/// not start, or timed out).
pub trait AdbRunner: Send + Sync {
    /// Where adb is ([`find_adb`] for the real one).
    fn locate(&self, configured: Option<&str>) -> Option<PathBuf>;
    /// `args` as the C# agent writes them ("-s SERIAL reverse --list"); split on spaces.
    fn run<'a>(&'a self, adb: &'a Path, args: &'a str) -> AdbFuture<'a>;
}

/// The real adb, at most `timeout` per command (`ToolProcess.RunAsync`).
pub struct ProcessRunner {
    pub timeout: Duration,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        ProcessRunner {
            timeout: Duration::from_secs(8),
        }
    }
}

impl AdbRunner for ProcessRunner {
    fn locate(&self, configured: Option<&str>) -> Option<PathBuf> {
        find_adb(configured)
    }

    fn run<'a>(&'a self, adb: &'a Path, args: &'a str) -> AdbFuture<'a> {
        Box::pin(async move {
            let mut cmd = tokio::process::Command::new(adb);
            cmd.args(args.split_whitespace())
                .current_dir(std::env::temp_dir()) // AppPaths.NeutralCwd: never hold a folder the user may delete
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true); // a command that timed out is killed when its future is dropped
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            let child = cmd
                .spawn()
                .map_err(|e| format!("Could not start {}: {e}", adb.display()))?;
            match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
                Ok(Ok(out)) => Ok(AdbOutput {
                    code: out.status.code().unwrap_or(-1),
                    output: String::from_utf8_lossy(&out.stdout).into_owned(),
                }),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => {
                    let name = adb
                        .file_stem()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    Err(format!("{name} timed out"))
                }
            }
        })
    }
}

/// Why a round ended early.
#[derive(Debug, Clone, PartialEq)]
pub enum AdbError {
    /// adb ran and exited with this code.
    Exit(i32),
    /// adb did not run to its end.
    Failed(String),
}

impl std::fmt::Display for AdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdbError::Exit(code) => write!(f, "ADB exited with code {code}"),
            AdbError::Failed(message) => f.write_str(message),
        }
    }
}

/// What the USB side knows right now (for /api/info and the settings page).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsbStatus {
    pub connected: bool,
    pub all_devices: Vec<String>,
    pub missing_app: Vec<String>,
    pub usb_problem: Option<String>,
}

#[derive(Default)]
struct Seen {
    /// Phones in use (after the deviceSerial filter).
    devices: Vec<String>,
    /// All attached phones, before the filter.
    all: Vec<String>,
    /// Connected phones without the PhoneBox app.
    missing_app: Vec<String>,
}

#[derive(Default)]
struct Round {
    home_set: HashSet<String>,
    awake_set: HashSet<String>,
    /// Phones on Wi-Fi adb that refused a reverse rule; not asked again until they reconnect.
    no_rule: HashSet<String>,
    last_state: String,
    last_problem: Option<String>,
}

pub struct AdbReverse {
    cfg: Arc<ConfigStore>,
    port: u16,
    runner: Arc<dyn AdbRunner>,
    seen: Mutex<Seen>,
    round: Mutex<Round>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl AdbReverse {
    /// Not running yet: [`AdbReverse::start`] starts the loop.
    pub fn new(cfg: Arc<ConfigStore>, port: u16, runner: Arc<dyn AdbRunner>) -> Arc<AdbReverse> {
        Arc::new(AdbReverse {
            cfg,
            port,
            runner,
            seen: Default::default(),
            round: Default::default(),
        })
    }

    /// The real adb, running.
    pub fn spawn(cfg: Arc<ConfigStore>, port: u16) -> Arc<AdbReverse> {
        Self::spawn_with(Arc::new(ProcessRunner::default()), cfg, port)
    }

    pub fn spawn_with(
        runner: Arc<dyn AdbRunner>,
        cfg: Arc<ConfigStore>,
        port: u16,
    ) -> Arc<AdbReverse> {
        let adb = Self::new(cfg, port, runner);
        adb.start();
        adb
    }

    /// A round now, then every 4 s (in the tokio runtime; the loop ends with it).
    pub fn start(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = this.ensure().await {
                    this.report(&format!("adb error: {e}"));
                }
                tokio::time::sleep(ROUND_PAUSE).await;
            }
        });
    }

    pub fn connected(&self) -> bool {
        !lock(&self.seen).devices.is_empty()
    }

    /// All attached devices, before the deviceSerial filter (for the settings page).
    pub fn all_devices(&self) -> Vec<String> {
        lock(&self.seen).all.clone()
    }

    /// Connected phones that do not have the PhoneBox app yet (installed from Settings).
    /// `OpenPanelAsync` (Settings › Show on phone): wakes every phone in use and starts the panel; nothing without adb.
    pub async fn open_panel(&self) -> Result<(), AdbError> {
        let configured = self.cfg.get().adb_path.clone();
        let Some(adb) = self.runner.locate(configured.as_deref()) else {
            return Ok(());
        };
        let devices = lock(&self.seen).devices.clone();
        for d in devices {
            self.adb(
                &adb,
                &format!("-s {d} shell input keyevent KEYCODE_WAKEUP"),
                false,
            )
            .await?;
            self.adb(
                &adb,
                &format!("-s {d} shell am start -n {PANEL_ACTIVITY}"),
                false,
            )
            .await?;
        }
        Ok(())
    }

    pub fn missing_app(&self) -> Vec<String> {
        lock(&self.seen).missing_app.clone()
    }

    /// Why the phone is missing from adb although it reports in over Wi-Fi (see `usb_doctor`), or None.
    pub fn usb_problem(&self) -> Option<String> {
        crate::usb_doctor::diagnose(
            crate::usb_doctor::last_report().as_ref(),
            self.connected(),
            crate::server::now_ms(),
        )
    }

    pub fn status(&self) -> UsbStatus {
        UsbStatus {
            connected: self.connected(),
            all_devices: self.all_devices(),
            missing_app: self.missing_app(),
            usb_problem: self.usb_problem(),
        }
    }

    fn target_rule(&self) -> String {
        format!("tcp:{}", self.port)
    }

    async fn adb(&self, adb: &Path, args: &str, any_exit: bool) -> Result<String, AdbError> {
        let out = self.runner.run(adb, args).await.map_err(AdbError::Failed)?;
        if out.code != 0 && !any_exit {
            return Err(AdbError::Exit(out.code));
        }
        Ok(out.output)
    }

    /// One round (`Ensure`).
    async fn ensure(&self) -> Result<(), AdbError> {
        let cfg = self.cfg.get();
        let Some(adb) = self.runner.locate(cfg.adb_path.as_deref()) else {
            self.set_devices(Vec::new());
            self.report(&format!(
                "adb not found: {INSTALL_HINT} or set adbPath in the settings"
            ));
            return Ok(());
        };
        let listed = match self.adb(&adb, "devices", false).await {
            Ok(listed) => listed,
            Err(e) => {
                // adb that cannot list its devices keeps no phone connected (the C# agent kept the last list)
                self.set_devices(Vec::new());
                let mut seen = lock(&self.seen);
                seen.all.clear();
                seen.missing_app.clear();
                return Err(e);
            }
        };
        let all: Vec<String> = listed
            .split('\n')
            .skip(1)
            .filter_map(
                |line| match line.trim().split('\t').collect::<Vec<_>>()[..] {
                    [serial, "device"] => Some(serial.to_string()),
                    _ => None,
                },
            )
            .collect();
        let serial = cfg.device_serial.as_deref().unwrap_or("");
        let devices: Vec<String> = all
            .iter()
            .filter(|s| serial.is_empty() || *s == serial)
            .cloned()
            .collect();
        lock(&self.seen).all = all;
        self.set_devices(devices.clone());
        let problem = self.usb_problem();
        {
            let mut round = lock(&self.round);
            if problem != round.last_problem {
                round.last_problem = problem.clone();
                match &problem {
                    Some(p) => crate::log::warn(CATEGORY, &format!("USB: {p}")),
                    None if !devices.is_empty() => {
                        crate::log::info(CATEGORY, "USB: the phone is back on USB")
                    }
                    None => {}
                }
            }
        }
        lock(&self.seen).missing_app.retain(|d| devices.contains(d));
        if devices.is_empty() {
            self.report("no phone connected, waiting...");
            return Ok(());
        }

        // A phone that did not answer "reverse --list" sits out the rest of this round (home screen, keep awake): one
        // more failure there would end the round before the phones after it. The next round tries it again.
        let mut unreachable = HashSet::new();
        for d in &devices {
            if lock(&self.round).no_rule.contains(d) {
                continue;
            }
            let list = match self
                .adb(&adb, &format!("-s {d} reverse --list"), false)
                .await
            {
                Ok(list) => list,
                Err(e) => {
                    crate::log::warn(
                        CATEGORY,
                        &format!("{d}: could not list adb reverse rules ({e})"),
                    );
                    unreachable.insert(d.clone());
                    continue;
                }
            };
            let ruled = self.has_own_rule(&list);
            if ruled && !self.missing_app().contains(d) {
                continue;
            }
            // is the app there at all? (the agent may have been installed on its own, with winget or Scoop)
            let installed = self
                .adb(&adb, &format!("-s {d} shell pm path {PACKAGE}"), true)
                .await?
                .contains("package:");
            {
                let mut seen = lock(&self.seen);
                seen.missing_app.retain(|x| x != d);
                if !installed {
                    seen.missing_app.push(d.clone());
                }
            }
            if !installed {
                self.report(&format!(
                    "{d}: the PhoneBox app is not installed (Settings \u{203a} General \u{203a} Install the phone app)"
                ));
                continue;
            }
            if ruled {
                continue;
            }
            let set = self
                .adb(
                    &adb,
                    &format!("-s {d} reverse {RULE} {}", self.target_rule()),
                    false,
                )
                .await;
            // Android 8 refuses a rule over Wi-Fi adb ("more than one device/emulator") while its USB transport is still
            // registered. The panel reaches the agent over its own Wi-Fi link, so go on without the rule.
            if let Err(AdbError::Exit(_)) = &set
                && over_network(d)
            {
                lock(&self.round).no_rule.insert(d.clone());
                crate::log::info(
                    CATEGORY,
                    &format!(
                        "{d}: the phone refused an adb reverse rule over Wi-Fi; going on without it"
                    ),
                );
                continue;
            }
            set?;
            // Settings > Display > open on connect is off (a phone also used for app testing): the phone stays as it
            // is, and the panel opens when the user starts it
            if !cfg.display.open_on_connect {
                crate::log::info(CATEGORY, &format!("{d}: adb reverse set up"));
                continue;
            }
            // display.followComputerLock (macOS) keeps a phone plugged in while the Mac is locked dark; it needs the
            // Mac's lock state, which comes with the session lock monitor (plan 6). Until then: wake and open.
            self.adb(
                &adb,
                &format!("-s {d} shell input keyevent KEYCODE_WAKEUP"),
                false,
            )
            .await?;
            self.adb(
                &adb,
                &format!("-s {d} shell am start -n {PANEL_ACTIVITY}"),
                false,
            )
            .await?;
            crate::log::info(CATEGORY, &format!("{d}: adb reverse set up, panel opened"));
        }
        // Settings > Display > home screen: the app offers itself as a home app; make it the default one (no root).
        if cfg.display.home_screen {
            for d in &devices {
                if unreachable.contains(d) || lock(&self.round).home_set.contains(d) {
                    continue;
                }
                // Android answers a refusal with exit code 1; that must not end the round for the other phones
                let r = self
                    .adb(
                        &adb,
                        &format!("-s {d} shell cmd package set-home-activity {HOME_ALIAS}"),
                        true,
                    )
                    .await?;
                if !r.contains("Success") {
                    continue; // the app has not enabled its home entry yet; try again shortly
                }
                lock(&self.round).home_set.insert(d.clone());
                crate::log::info(CATEGORY, &format!("{d}: PhoneBox is now the home app"));
            }
        } else {
            lock(&self.round).home_set.clear();
        }
        // Settings > Display > keep awake: Android's screen saver (Daydream) starts on the dock or charger and covers
        // the panel; turn it off and keep the screen on while plugged in (plain adb settings, no root).
        if cfg.display.keep_awake {
            for d in &devices {
                if unreachable.contains(d)
                    || self.missing_app().contains(d)
                    || lock(&self.round).awake_set.contains(d)
                {
                    continue;
                }
                for setting in [
                    "secure screensaver_enabled 0",
                    "secure screensaver_activate_on_dock 0",
                    "secure screensaver_activate_on_sleep 0",
                    "global stay_on_while_plugged_in 7",
                ] {
                    self.adb(&adb, &format!("-s {d} shell settings put {setting}"), false)
                        .await?;
                }
                lock(&self.round).awake_set.insert(d.clone());
                crate::log::info(
                    CATEGORY,
                    &format!("{d}: screen saver off, stays awake while plugged in"),
                );
            }
        } else {
            lock(&self.round).awake_set.clear();
        }
        self.report(&format!("phone connected: {}", devices.join(", ")));
        Ok(())
    }

    /// True when "reverse --list" has this agent's rule: phone tcp:8765 to this agent's port. A rule left by an agent
    /// on another port (--port) points the phone at a dead port; lines read "<transport> <phone> <computer>".
    fn has_own_rule(&self, list: &str) -> bool {
        let target = self.target_rule();
        list.split('\n').any(|line| {
            let p: Vec<&str> = line
                .split([' ', '\t', '\r'])
                .filter(|w| !w.is_empty())
                .collect();
            p.len() >= 2 && p[p.len() - 2] == RULE && p[p.len() - 1] == target
        })
    }

    fn set_devices(&self, devices: Vec<String>) {
        {
            let mut round = lock(&self.round);
            round.awake_set.retain(|d| devices.contains(d));
            round.no_rule.retain(|d| devices.contains(d));
        }
        lock(&self.seen).devices = devices;
    }

    /// Logs the loop's state when it changed.
    fn report(&self, state: &str) {
        let mut round = lock(&self.round);
        if round.last_state != state {
            round.last_state = state.to_string();
            crate::log::info(CATEGORY, state);
        }
    }
}

/// True for a phone on adb over Wi-Fi: "host:port" (adb connect) or an mDNS name
/// ("adb-<serial>-<id>._adb-tls-connect._tcp", Android 11+ wireless debugging). USB serials and emulators
/// ("emulator-5554") have neither.
fn over_network(serial: &str) -> bool {
    serial.contains(':') || serial.ends_with("._tcp")
}

/// The adb to use: the configured path when it exists, else the first one on PATH or in the usual places.
pub fn find_adb(configured: Option<&str>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default();
    find_adb_in(configured, &path, &crate::app_paths::home(), &local)
}

/// [`find_adb`] with the environment passed in: `path` is the PATH variable, `home` the user's folder, `local` the
/// Windows LocalAppData folder.
pub fn find_adb_in(
    configured: Option<&str>,
    path: &str,
    home: &Path,
    local: &Path,
) -> Option<PathBuf> {
    if let Some(c) = configured
        .filter(|c| !c.is_empty())
        .filter(|c| Path::new(c).is_file())
    {
        return Some(PathBuf::from(c));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for dir in path.split(';').filter(|d| !d.is_empty()) {
            candidates.push(Path::new(dir.trim().trim_matches('"')).join("adb.exe")); // PATH entries may be quoted
        }
        candidates.push(
            local
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join("adb.exe"),
        );
        if let Ok(entries) =
            std::fs::read_dir(local.join("Microsoft").join("WinGet").join("Packages"))
        {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("Google.PlatformTools")
                })
                .map(|e| e.path())
                .collect();
            dirs.sort();
            candidates.extend(
                dirs.into_iter()
                    .map(|d| d.join("platform-tools").join("adb.exe")),
            );
        }
    } else {
        candidates.extend(
            path.split(':')
                .filter(|d| !d.is_empty())
                .map(|d| Path::new(d).join("adb")),
        );
        candidates.extend([
            home.join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join("adb"),
            home.join("Library")
                .join("Android")
                .join("sdk")
                .join("platform-tools")
                .join("adb"),
            PathBuf::from("/opt/homebrew/bin/adb"),
            PathBuf::from("/usr/local/bin/adb"),
            PathBuf::from("/usr/bin/adb"),
            PathBuf::from("/usr/lib/android-sdk/platform-tools/adb"),
        ]);
    }
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::config::store::ConfigStore;

    // not-ported: AdbReverseTests.CommandLogCanBeReadWhileTheFakeAdbWriterIsOpen: tests the C# fake adb's call log file (Windows file sharing); the Rust fake keeps its calls in memory

    const AWAKE: &str = "shell settings put global stay_on_while_plugged_in 7";

    /// A fake adb (never the real adb or a real phone): `devices` attached, "reverse --list" prints `reverse_list`,
    /// the app is installed, "cmd package set-home-activity" fails on `home_refused` the way Android refuses it (an
    /// error line, exit code 1), "reverse --list" fails on `reverse_refused`, setting a reverse rule fails on
    /// `rule_refused` (what Android 8 answers over Wi-Fi adb while its USB transport is still registered), and
    /// `broken` fails its "reverse --list" and every "settings" command.
    #[derive(Default)]
    struct Fake {
        devices: Vec<String>,
        reverse_list: String,
        not_installed: bool,
        home_refused: Option<String>,
        reverse_refused: Option<String>,
        rule_refused: Option<String>,
        broken: Option<String>,
        fail_all: Option<String>,
        calls: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct FakeAdb(Arc<Mutex<Fake>>);

    impl FakeAdb {
        fn with(f: impl FnOnce(&mut Fake)) -> FakeAdb {
            let fake = FakeAdb::default();
            fake.set(f);
            fake
        }
        fn set(&self, f: impl FnOnce(&mut Fake)) {
            f(&mut self.0.lock().unwrap());
        }
        fn devices(&self, devices: &[&str]) {
            self.set(|f| f.devices = devices.iter().map(|d| d.to_string()).collect());
        }
        fn fail_all(&self, message: &str) {
            self.set(|f| f.fail_all = Some(message.to_string()));
        }
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().calls.clone()
        }
        fn clear_calls(&self) {
            self.set(|f| f.calls.clear());
        }
        fn answer(&self, args: &str) -> Result<AdbOutput, String> {
            let mut f = self.0.lock().unwrap();
            f.calls.push(args.to_string());
            if let Some(message) = &f.fail_all {
                return Err(message.clone());
            }
            let p: Vec<&str> = args.split(' ').collect();
            let arg = |i: usize| p.get(i).copied().unwrap_or("");
            let is = |who: &Option<String>| who.as_deref() == Some(arg(1));
            let ok = |output: &str| {
                Ok(AdbOutput {
                    code: 0,
                    output: output.to_string(),
                })
            };
            let fail = |output: &str| {
                Ok(AdbOutput {
                    code: 1,
                    output: output.to_string(),
                })
            };
            if arg(0) == "devices" {
                let list: String = f.devices.iter().map(|d| format!("{d}\tdevice\n")).collect();
                return ok(&format!("List of devices attached\n{list}"));
            }
            if arg(2) == "reverse" && arg(3) == "--list" {
                return if is(&f.reverse_refused) || is(&f.broken) {
                    fail("")
                } else {
                    ok(&f.reverse_list.clone())
                };
            }
            if arg(2) == "reverse" && is(&f.rule_refused) {
                return fail("adb: error: more than one device/emulator\n");
            }
            if arg(3) == "pm" {
                return if f.not_installed {
                    fail("")
                } else {
                    ok("package:/data/app/base.apk\n")
                };
            }
            if arg(3) == "settings" && is(&f.broken) {
                return fail("");
            }
            if arg(5) == "set-home-activity" {
                return if is(&f.home_refused) {
                    fail("Error: Failed to set default home.\n")
                } else {
                    ok("Success\n")
                };
            }
            ok("")
        }
    }

    impl AdbRunner for FakeAdb {
        fn locate(&self, _configured: Option<&str>) -> Option<PathBuf> {
            Some(PathBuf::from("fake-adb"))
        }
        fn run<'a>(&'a self, _adb: &'a Path, args: &'a str) -> AdbFuture<'a> {
            let answer = self.answer(args);
            Box::pin(async move { answer })
        }
    }

    fn test_cfg(json: &str) -> (tempfile::TempDir, Arc<ConfigStore>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), json).unwrap();
        let store = Arc::new(ConfigStore::new(dir.path().join("config.json")));
        (dir, store)
    }

    fn count(calls: &[String], call: &str) -> usize {
        calls.iter().filter(|c| *c == call).count()
    }

    #[tokio::test(start_paused = true)]
    async fn survives_missing_adb_and_unplug() {
        let fake = FakeAdb::default();
        fake.devices(&[]); // nothing plugged in
        let (_d, cfg) = test_cfg("{}");
        let adb = AdbReverse::spawn_with(Arc::new(fake.clone()), cfg, 8765);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!adb.connected());
        fake.devices(&["8UE5T17A26006928"]); // plugged in
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(adb.connected());
        assert!(
            fake.calls()
                .iter()
                .any(|c| c == "-s 8UE5T17A26006928 reverse tcp:8765 tcp:8765")
        );
        fake.fail_all("adb: not found"); // adb gone / server killed
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!adb.connected());
        assert!(adb.all_devices().is_empty());
        fake.set(|f| f.fail_all = None); // back: the next round finds the phone again
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(adb.connected());
    }

    #[tokio::test]
    async fn unplugging_all_phones_removes_missing_app_install_targets() {
        let fake = FakeAdb::with(|f| {
            f.devices = vec!["FAKE1".into()];
            f.not_installed = true;
        });
        let (_d, cfg) = test_cfg(r#"{"display":{"homeScreen":false,"keepAwake":false}}"#);
        let adb = AdbReverse::new(cfg, 8765, Arc::new(fake.clone()));
        adb.ensure().await.unwrap();
        assert_eq!(adb.missing_app(), ["FAKE1"]);
        fake.devices(&[]);
        adb.ensure().await.unwrap();
        assert!(adb.missing_app().is_empty());
        assert!(!adb.connected());
        adb.ensure().await.unwrap();
        assert!(adb.missing_app().is_empty());
    }

    #[tokio::test]
    async fn show_on_phone_wakes_every_phone_in_use_and_starts_the_panel() {
        let fake = FakeAdb::with(|f| f.devices = vec!["FAKE1".into(), "FAKE2".into()]);
        let (_d, cfg) =
            test_cfg(r#"{"display":{"homeScreen":false,"keepAwake":false,"openOnConnect":false}}"#);
        let adb = AdbReverse::new(cfg, 8765, Arc::new(fake.clone()));
        adb.ensure().await.unwrap();
        fake.clear_calls();
        adb.open_panel().await.unwrap();
        assert_eq!(
            fake.calls(),
            [
                "-s FAKE1 shell input keyevent KEYCODE_WAKEUP",
                "-s FAKE1 shell am start -n net.oxog.phonebox.deskpanel/.MainActivity",
                "-s FAKE2 shell input keyevent KEYCODE_WAKEUP",
                "-s FAKE2 shell am start -n net.oxog.phonebox.deskpanel/.MainActivity",
            ]
        );
        fake.fail_all("adb: device offline");
        assert!(adb.open_panel().await.is_err());
    }

    #[tokio::test]
    async fn keep_awake_is_reapplied_only_after_disconnect() {
        for (disconnect, second_phone) in [(false, false), (true, false), (true, true)] {
            let devices: Vec<String> = if second_phone {
                vec!["FAKE1".into(), "FAKE2".into()]
            } else {
                vec!["FAKE1".into()]
            };
            let fake = FakeAdb::with(|f| {
                f.devices = devices.clone();
                f.reverse_list = "UsbFfs tcp:8765 tcp:8765".into();
            });
            let (_d, cfg) = test_cfg(r#"{"display":{"homeScreen":false,"keepAwake":true}}"#);
            let adb = AdbReverse::new(cfg, 8765, Arc::new(fake.clone()));
            adb.ensure().await.unwrap();
            assert_eq!(count(&fake.calls(), &format!("-s FAKE1 {AWAKE}")), 1);
            if disconnect {
                fake.devices(if second_phone { &["FAKE2"] } else { &[] });
                adb.ensure().await.unwrap();
                assert_eq!(adb.connected(), second_phone);
                fake.set(|f| f.devices = devices.clone());
            }
            adb.ensure().await.unwrap();
            assert!(adb.connected());
            let calls = fake.calls();
            let case = format!("disconnect {disconnect}, second phone {second_phone}");
            assert_eq!(
                count(&calls, &format!("-s FAKE1 {AWAKE}")),
                if disconnect { 2 } else { 1 },
                "{case}"
            );
            assert_eq!(
                count(&calls, &format!("-s FAKE2 {AWAKE}")),
                usize::from(second_phone),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn failed_reverse_query_does_not_block_next_phone() {
        let fake = FakeAdb::with(|f| f.devices = vec!["FAKE1".into(), "FAKE2".into()]);
        let (_d, cfg) = test_cfg(
            r#"{"display":{"homeScreen":false,"keepAwake":false,"followComputerLock":false}}"#,
        );
        let adb = AdbReverse::new(cfg, 8765, Arc::new(fake.clone()));
        let first = "-s FAKE1 reverse tcp:8765 tcp:8765";
        let second = "-s FAKE2 reverse tcp:8765 tcp:8765";
        adb.ensure().await.unwrap();
        assert!(fake.calls().iter().any(|c| c == first));
        assert!(fake.calls().iter().any(|c| c == second));
        fake.clear_calls();
        fake.set(|f| f.reverse_refused = Some("FAKE1".into()));
        // a fresh agent: neither phone is set up yet
        let (_d2, cfg2) = test_cfg(
            r#"{"display":{"homeScreen":false,"keepAwake":false,"followComputerLock":false}}"#,
        );
        let adb = AdbReverse::new(cfg2, 8765, Arc::new(fake.clone()));
        let result = adb.ensure().await;
        let calls = fake.calls();
        assert!(result.is_ok(), "the round failed: {result:?}");
        assert!(
            calls.iter().any(|c| c == second),
            "the next phone was not set up: {calls:?}"
        );
        assert!(!calls.iter().any(|c| c == first));
    }

    /// Android 8 refuses a reverse rule over Wi-Fi adb ("more than one device/emulator") while its USB transport is
    /// still registered. The panel has its own Wi-Fi link, so that phone goes on without a rule: once logged, not
    /// retried until it reconnects, and the round carries on (the C# agent ended the round with "adb error" every 4 s).
    #[tokio::test]
    async fn a_wifi_phone_that_refuses_a_reverse_rule_does_not_end_the_round() {
        let wifi = "192.168.1.5:5555";
        let usb = "8UE5T17A26006928";
        let fake = FakeAdb::with(|f| {
            f.devices = vec![wifi.into(), usb.into()];
            f.rule_refused = Some(wifi.into());
        });
        let (_d, cfg) = test_cfg(r#"{"display":{"homeScreen":false,"keepAwake":true}}"#);
        let adb = AdbReverse::new(cfg, 8765, Arc::new(fake.clone()));
        let rule = |d: &str| format!("-s {d} reverse tcp:8765 tcp:8765");

        let result = adb.ensure().await;
        let calls = fake.calls();
        assert!(result.is_ok(), "the round failed: {result:?}");
        assert_eq!(count(&calls, &rule(usb)), 1, "{calls:?}");
        assert_eq!(count(&calls, &format!("-s {usb} {AWAKE}")), 1);
        assert_eq!(count(&calls, &format!("-s {wifi} {AWAKE}")), 1);
        assert_eq!(
            lock(&adb.round).last_state,
            format!("phone connected: {wifi}, {usb}")
        );

        adb.ensure().await.unwrap(); // not asked again while it stays connected
        assert_eq!(count(&fake.calls(), &rule(wifi)), 1);

        fake.devices(&[usb]); // the Wi-Fi link drops, then comes back: one more try
        adb.ensure().await.unwrap();
        fake.devices(&[wifi, usb]);
        adb.ensure().await.unwrap();
        assert_eq!(count(&fake.calls(), &rule(wifi)), 2);
    }

    #[test]
    fn wifi_adb_serials_are_told_apart_from_usb_ones() {
        for serial in [
            "192.168.1.5:5555",
            "[fe80::1]:5555",
            "adb-8UE5T17A26006928-AbCdEf._adb-tls-connect._tcp",
        ] {
            assert!(over_network(serial), "{serial}");
        }
        for serial in ["8UE5T17A26006928", "emulator-5554", "R58M123ABC"] {
            assert!(!over_network(serial), "{serial}");
        }
    }

    #[tokio::test]
    async fn refused_reverse_queries_preserve_the_other_phones() {
        for (devices, refused, working) in [
            (&["FAKE1"][..], "FAKE1", None),
            (&["FAKE2", "FAKE1"][..], "FAKE1", Some("FAKE2")),
        ] {
            let fake = FakeAdb::with(|f| {
                f.devices = devices.iter().map(|d| d.to_string()).collect();
                f.reverse_refused = Some(refused.into());
            });
            let (_d, cfg) = test_cfg(
                r#"{"display":{"homeScreen":false,"keepAwake":false,"followComputerLock":false}}"#,
            );
            let adb = AdbReverse::new(cfg, 8765, Arc::new(fake.clone()));
            adb.ensure().await.unwrap();
            let calls = fake.calls();
            assert!(
                !calls
                    .iter()
                    .any(|c| *c == format!("-s {refused} reverse tcp:8765 tcp:8765"))
            );
            if let Some(w) = working {
                assert!(
                    calls
                        .iter()
                        .any(|c| *c == format!("-s {w} reverse tcp:8765 tcp:8765"))
                );
            }
        }
    }

    /// Runs the loop until `done` holds or three rounds ("adb devices" calls) went by.
    async fn run_until(
        fake: &FakeAdb,
        settings: &str,
        port: u16,
        done: impl Fn(&[String]) -> bool,
    ) -> Vec<String> {
        let (_d, cfg) = test_cfg(settings);
        let _adb = AdbReverse::spawn_with(Arc::new(fake.clone()), cfg, port);
        for _ in 0..300 {
            let calls = fake.calls();
            if done(&calls) || count(&calls, "devices") >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        fake.calls()
    }

    /// Android refuses "set-home-activity" (exit 1) while the app's home entry is still off or the app is missing.
    /// That must not end the round: the other phone still becomes PhoneBox's home and keep-awake reaches both phones.
    #[tokio::test(start_paused = true)]
    async fn a_refused_home_screen_on_one_phone_does_not_hold_back_the_others_or_keep_awake() {
        let fake = FakeAdb::with(|f| {
            f.devices = vec!["FAKE1".into(), "FAKE2".into()];
            f.reverse_list = "UsbFfs tcp:8765 tcp:8765".into();
            f.home_refused = Some("FAKE1".into());
        });
        let awake1 = format!("-s FAKE1 {AWAKE}");
        let awake2 = format!("-s FAKE2 {AWAKE}");
        let calls = run_until(
            &fake,
            r#"{"display":{"homeScreen":true,"keepAwake":true,"followComputerLock":false}}"#,
            8765,
            |c| c.contains(&awake1) && c.contains(&awake2),
        )
        .await;
        assert!(calls.iter().any(|c| c == "-s FAKE2 shell cmd package set-home-activity net.oxog.phonebox.deskpanel/.HomeAlias"));
        assert!(calls.contains(&awake1));
        assert!(calls.contains(&awake2));
    }

    /// A rule left by an agent on another port (--port) points the phone at a port nobody listens on; the agent must
    /// point it back at itself instead of taking any "tcp:8765" in the list for its own rule.
    #[tokio::test(start_paused = true)]
    async fn a_reverse_rule_to_another_port_is_pointed_at_this_agent() {
        for (existing, port) in [
            ("UsbFfs tcp:8765 tcp:18765", 8765), // a development agent ran before
            ("UsbFfs tcp:8765 tcp:8765", 18765), // the development agent itself, after the installed one
            ("UsbFfs tcp:9000 tcp:8765", 8765),  // some other rule onto the computer's 8765
        ] {
            let fake = FakeAdb::with(|f| {
                f.devices = vec!["FAKE1".into()];
                f.reverse_list = existing.into();
            });
            let want = format!("-s FAKE1 reverse tcp:8765 tcp:{port}");
            let calls = run_until(
                &fake,
                r#"{"deviceSerial":"FAKE1","display":{"homeScreen":false,"keepAwake":false}}"#,
                port,
                |c| c.contains(&want),
            )
            .await;
            assert!(
                calls.contains(&want),
                "{existing} on port {port}: {calls:?}"
            );
        }
    }

    /// The agent's own rule is left alone: no re-binding and no panel opening every round.
    #[tokio::test(start_paused = true)]
    async fn the_agents_own_rule_is_left_alone() {
        let fake = FakeAdb::with(|f| {
            f.devices = vec!["FAKE1".into()];
            f.reverse_list = "UsbFfs tcp:8765 tcp:8765".into();
        });
        let calls = run_until(
            &fake,
            r#"{"deviceSerial":"FAKE1","display":{"homeScreen":false,"keepAwake":false}}"#,
            8765,
            |_| false,
        )
        .await;
        assert!(count(&calls, "devices") >= 2, "the loop did not run twice");
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("-s FAKE1 reverse tcp:") || c.contains("am start"))
        );
    }

    /// A phone that stops answering adb mid-round sits out the rest of that round, so the phones after it still get
    /// keep-awake, and the next round tries it again.
    #[tokio::test]
    async fn a_phone_that_failed_its_reverse_query_sits_out_the_rest_of_the_round() {
        let fake = FakeAdb::with(|f| {
            f.devices = vec!["FAKE1".into(), "FAKE2".into()];
            f.broken = Some("FAKE1".into());
        });
        let (_d, cfg) = test_cfg(
            r#"{"display":{"homeScreen":false,"keepAwake":true,"followComputerLock":false}}"#,
        );
        let adb = AdbReverse::new(cfg, 8765, Arc::new(fake.clone()));
        let result = adb.ensure().await;
        let calls = fake.calls();
        assert!(result.is_ok(), "the round failed: {result:?}");
        assert!(calls.contains(&format!("-s FAKE2 {AWAKE}")));
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("-s FAKE1 shell settings"))
        );
        fake.clear_calls();
        fake.set(|f| f.broken = None); // FAKE1 answers again: the next round sets it up
        adb.ensure().await.unwrap();
        let calls = fake.calls();
        assert!(
            calls
                .iter()
                .any(|c| c == "-s FAKE1 reverse tcp:8765 tcp:8765")
        );
        assert!(calls.contains(&format!("-s FAKE1 {AWAKE}")));
    }

    /// A newly plugged-in phone is woken and shows the panel, unless Settings > Display > open on connect is off (a
    /// phone that is also used for app testing): then only the reverse rule is set and the phone is left as it is.
    #[tokio::test(start_paused = true)]
    async fn a_new_phone_opens_the_panel_only_when_open_on_connect_is_on() {
        for open_on_connect in [true, false] {
            let fake = FakeAdb::with(|f| f.devices = vec!["FAKE1".into()]);
            let settings = format!(
                r#"{{"deviceSerial":"FAKE1","display":{{"homeScreen":false,"keepAwake":false,"followComputerLock":false,"openOnConnect":{open_on_connect}}}}}"#
            );
            let calls = run_until(&fake, &settings, 8765, |c| count(c, "devices") >= 2).await;
            assert!(
                calls
                    .iter()
                    .any(|c| c == "-s FAKE1 reverse tcp:8765 tcp:8765")
            );
            let opened = "-s FAKE1 shell am start -n net.oxog.phonebox.deskpanel/.MainActivity";
            assert_eq!(
                calls.iter().any(|c| c == opened),
                open_on_connect,
                "openOnConnect {open_on_connect}"
            );
            let woken = "-s FAKE1 shell input keyevent KEYCODE_WAKEUP";
            assert_eq!(
                calls.iter().any(|c| c == woken),
                open_on_connect,
                "openOnConnect {open_on_connect}"
            );
        }
    }

    #[test]
    fn open_on_connect_is_on_by_default() {
        let (_d, cfg) = test_cfg("{}");
        assert!(cfg.get().display.open_on_connect);
    }

    #[tokio::test]
    async fn a_new_phone_is_woken_and_shows_the_panel() {
        let fake = FakeAdb::with(|f| f.devices = vec!["FAKE1".into()]);
        let (_d, cfg) = test_cfg(
            r#"{"display":{"homeScreen":false,"keepAwake":false,"followComputerLock":false}}"#,
        );
        let adb = AdbReverse::new(cfg, 18765, Arc::new(fake.clone()));
        adb.ensure().await.unwrap();
        let calls = fake.calls();
        let at = |call: &str| {
            calls
                .iter()
                .position(|c| c == call)
                .unwrap_or_else(|| panic!("{call} missing: {calls:?}"))
        };
        let reverse = at("-s FAKE1 reverse tcp:8765 tcp:18765");
        let wake = at("-s FAKE1 shell input keyevent KEYCODE_WAKEUP");
        let open = at("-s FAKE1 shell am start -n net.oxog.phonebox.deskpanel/.MainActivity");
        assert!(reverse < wake && wake < open);
    }

    #[tokio::test]
    async fn a_failing_command_ends_the_round_with_its_exit_code() {
        // like the C# agent: only "reverse --list" is allowed to fail; a refused "reverse" ends the round (except on
        // Wi-Fi adb: a_wifi_phone_that_refuses_a_reverse_rule_does_not_end_the_round)
        let fake = FakeAdb::with(|f| f.devices = vec!["FAKE1".into()]);
        let (_d, cfg) = test_cfg("{}");
        let adb = AdbReverse::new(cfg, 8765, Arc::new(Refuses(fake, "reverse tcp:8765")));
        let error = adb.ensure().await.unwrap_err();
        assert_eq!(error.to_string(), "ADB exited with code 1");
    }

    /// Wraps a fake: every call containing `.1` exits with code 1.
    struct Refuses(FakeAdb, &'static str);

    impl AdbRunner for Refuses {
        fn locate(&self, configured: Option<&str>) -> Option<PathBuf> {
            self.0.locate(configured)
        }
        fn run<'a>(&'a self, adb: &'a Path, args: &'a str) -> AdbFuture<'a> {
            if args.contains(self.1) {
                return Box::pin(async {
                    Ok(AdbOutput {
                        code: 1,
                        output: String::new(),
                    })
                });
            }
            self.0.run(adb, args)
        }
    }

    #[cfg(unix)]
    #[test]
    fn find_adb_takes_the_configured_one_then_path_then_the_sdk() {
        let dir = tempfile::tempdir().unwrap();
        let touch = |p: &Path| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        };
        let configured = dir.path().join("mine").join("adb");
        let on_path = dir.path().join("bin").join("adb");
        let home = dir.path().join("home");
        let sdk = home
            .join("Android")
            .join("Sdk")
            .join("platform-tools")
            .join("adb");
        touch(&configured);
        touch(&on_path);
        touch(&sdk);
        let path_var = format!("/nonexistent:{}", on_path.parent().unwrap().display());
        let find =
            |c: Option<&str>, path: &str| find_adb_in(c, path, &home, Path::new("/nonexistent"));
        assert_eq!(
            find(Some(configured.to_str().unwrap()), &path_var),
            Some(configured.clone())
        );
        assert_eq!(
            find(Some("/nonexistent/adb"), &path_var),
            Some(on_path.clone())
        );
        assert_eq!(find(Some(""), &path_var), Some(on_path));
        assert_eq!(find(None, ""), Some(sdk));
    }

    #[cfg(windows)]
    #[test]
    fn find_adb_takes_the_configured_one_then_path_then_the_sdk() {
        let dir = tempfile::tempdir().unwrap();
        let touch = |p: &Path| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        };
        let on_path = dir.path().join("bin").join("adb.exe");
        let local = dir.path().join("local");
        let winget = local
            .join("Microsoft")
            .join("WinGet")
            .join("Packages")
            .join("Google.PlatformTools_x")
            .join("platform-tools")
            .join("adb.exe");
        touch(&on_path);
        touch(&winget);
        // PATH entries may be quoted
        let path_var = format!(
            "C:\\nonexistent; \"{}\" ",
            on_path.parent().unwrap().display()
        );
        let find = |path: &str| find_adb_in(None, path, Path::new("C:\\nonexistent"), &local);
        assert_eq!(find(&path_var), Some(on_path));
        assert_eq!(find(""), Some(winget));
    }

    /// CoreAuditTests C3: Android's "pm path" exits 1 for a package that is not installed, and adb shell passes that
    /// on (a real script on Unix, as in C#; the in-memory fake answering exit 1 on Windows).
    #[tokio::test]
    async fn c3_a_phone_without_the_app_is_offered_the_install() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let runner: Arc<dyn AdbRunner> = {
            use std::os::unix::fs::PermissionsExt;
            let script = dir.path().join("adb-real");
            std::fs::write(
                &script,
                "#!/bin/sh\nif [ \"$1\" = devices ]; then printf 'List of devices attached\\nFAKE123\\tdevice\\n'; exit 0; fi\n\
                 if [ \"$4\" = pm ]; then exit 1; fi\nexit 0\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            Arc::new(ScriptAdb(script, ProcessRunner::default()))
        };
        #[cfg(not(unix))]
        let runner: Arc<dyn AdbRunner> = Arc::new(FakeAdb::with(|f| {
            f.devices = vec!["FAKE123".into()];
            f.not_installed = true;
        }));
        let (_d, cfg) = test_cfg(r#"{"display":{"homeScreen":false,"keepAwake":false}}"#);
        let adb = AdbReverse::new(cfg, 8765, runner);
        for _ in 0..300 {
            let _ = adb.ensure().await;
            if !adb.missing_app().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        drop(dir);
        assert_eq!(adb.missing_app(), ["FAKE123"]);
    }

    /// The real process runner on a script standing in for adb.
    #[cfg(unix)]
    struct ScriptAdb(PathBuf, ProcessRunner);

    #[cfg(unix)]
    impl AdbRunner for ScriptAdb {
        fn locate(&self, _configured: Option<&str>) -> Option<PathBuf> {
            Some(self.0.clone())
        }
        fn run<'a>(&'a self, adb: &'a Path, args: &'a str) -> AdbFuture<'a> {
            self.1.run(adb, args)
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_process_runner_reports_the_exit_code_and_output_and_times_out() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("adb");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = slow ]; then sleep 5; fi\necho \"args: $*\"\necho oops >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runner = ProcessRunner {
            timeout: Duration::from_millis(500),
        };
        let out = runner
            .run(&script, "-s FAKE1 reverse --list")
            .await
            .unwrap();
        assert_eq!(out.code, 3);
        assert_eq!(out.output, "args: -s FAKE1 reverse --list\n"); // standard output only
        let started = std::time::Instant::now();
        let slow = runner.run(&script, "slow").await.unwrap_err();
        assert_eq!(slow, "adb timed out");
        assert!(started.elapsed() < Duration::from_secs(3));
        let missing = runner
            .run(&dir.path().join("nope"), "devices")
            .await
            .unwrap_err();
        assert!(missing.starts_with("Could not start"), "{missing}");
    }
}
