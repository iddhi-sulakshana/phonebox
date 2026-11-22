//! Port of `JobObject.cs`: on Windows, long-running helper programs (nvidia-smi, ffmpeg) join a job that ends them
//! when the agent ends, however it ends (a crash, Task Manager). The agent itself is deliberately not in the job:
//! otherwise the shared adb server, which adb starts on first use, would end with the agent and other adb users would
//! lose it. Elsewhere tracking does nothing: the helpers are stopped on shutdown.

use std::sync::Arc;

/// The agent's job (Windows), made on the first [`JobObject::track`]. Its handle is never closed: the job ends with
/// the process, and with it every program in it. One instance lives in `AppState`.
#[derive(Default)]
pub struct JobObject {
    #[cfg(windows)]
    job: std::sync::OnceLock<Option<isize>>,
}

impl JobObject {
    pub fn new() -> Arc<JobObject> {
        Arc::new(JobObject::default())
    }

    /// Adds a started helper to the job; a program that already ended, or a job that could not be made, is ignored.
    pub fn track(&self, child: &tokio::process::Child) {
        #[cfg(windows)]
        if let Some(process) = child.raw_handle() {
            self.assign(process);
        }
        #[cfg(not(windows))]
        let _ = child;
    }

    /// [`JobObject::track`] for a program started with `std::process`.
    pub fn track_std(&self, child: &std::process::Child) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            self.assign(child.as_raw_handle());
        }
        #[cfg(not(windows))]
        let _ = child;
    }

    #[cfg(windows)]
    fn assign(&self, process: std::os::windows::io::RawHandle) {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
        let Some(job) = *self.job.get_or_init(create) else {
            return;
        };
        // SAFETY: both handles are open for the call (the child's belongs to its `Child`)
        unsafe {
            let _ = AssignProcessToJobObject(HANDLE(job as *mut _), HANDLE(process));
        }
    }
}

/// An unnamed job whose programs end when its last handle closes (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
#[cfg(windows)]
fn create() -> Option<isize> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::core::PCWSTR;
    // SAFETY: the limit information lives through the call; a job that cannot be set up is closed again
    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const std::ffi::c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set.is_err() {
            let _ = CloseHandle(job);
            return None;
        }
        Some(job.0 as isize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick_std() -> std::process::Command {
        if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit 0"]);
            c
        } else {
            std::process::Command::new("true")
        }
    }

    #[tokio::test]
    async fn track_is_harmless_elsewhere() {
        let jobs = JobObject::default();
        let mut child = quick_std().spawn().unwrap();
        jobs.track_std(&child);
        assert!(child.wait().unwrap().success());
        jobs.track_std(&child); // an ended program: ignored
        let mut child = tokio::process::Command::from(quick_std()).spawn().unwrap();
        jobs.track(&child);
        assert!(child.wait().await.unwrap().success());
        jobs.track(&child);
    }

    /// Set in the helper copy of this test that `a_tracked_child_ends_with_its_parent` starts.
    #[cfg(windows)]
    const HELPER: &str = "PHONEBOX_JOB_HELPER";

    // survey b§4.3: the test program starts a copy of itself, which tracks a sleeper; killing the copy ends the sleeper
    #[cfg(windows)]
    #[test]
    fn a_tracked_child_ends_with_its_parent() {
        use std::io::{BufRead, Write};
        use std::process::Stdio;
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        if std::env::var_os(HELPER).is_some() {
            let jobs = JobObject::default();
            let mut sleeper = std::process::Command::new("ping")
                .args(["-n", "60", "127.0.0.1"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            jobs.track_std(&sleeper);
            println!("sleeper {}", sleeper.id());
            std::io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(60));
            // not killed: the test failed already; leave nothing behind
            let _ = sleeper.kill();
            let _ = sleeper.wait();
            return;
        }
        let mut helper = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "job_object::tests::a_tracked_child_ends_with_its_parent",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = std::io::BufReader::new(helper.stdout.take().unwrap());
        let pid: u32 = out
            .lines()
            .map_while(Result::ok)
            // libtest writes "test <name> ... " before the test's own output, on the same line
            .find_map(|l| {
                l.rsplit_once("sleeper ")
                    .and_then(|(_, p)| p.trim().parse().ok())
            })
            .expect("the helper names its sleeper");
        // SAFETY: the sleeper's handle is waited on, then closed
        unsafe {
            let sleeper = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).unwrap();
            helper.kill().unwrap();
            let _ = helper.wait();
            let ended = WaitForSingleObject(sleeper, 5000) == WAIT_OBJECT_0;
            let _ = CloseHandle(sleeper);
            assert!(ended, "the sleeper outlived the program that tracked it");
        }
    }
}
