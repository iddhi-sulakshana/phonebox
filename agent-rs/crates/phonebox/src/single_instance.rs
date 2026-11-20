//! One agent per port: the C# agent's named mutex becomes an exclusive lock on a file in the local folder.

use std::path::Path;
use std::time::Duration;

/// Held while the agent runs; dropping it releases the instance.
pub struct InstanceLock {
    _file: std::fs::File,
}

/// Takes the instance named `name` (an `AgentEndpoint::mutex_name`), waiting up to `wait`; None when another holds it.
pub fn acquire(dir: &Path, name: &str, wait: Duration) -> Option<InstanceLock> {
    let file_name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::fs::create_dir_all(dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{file_name}.lock")))
        .ok()?;
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Some(InstanceLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < wait => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_lock_fails_while_first_held() {
        let dir = tempfile::tempdir().unwrap();
        let first =
            acquire(dir.path(), r"Local\PhoneBox.Agent.18765", Duration::ZERO).expect("first");
        assert!(acquire(dir.path(), r"Local\PhoneBox.Agent.18765", Duration::ZERO).is_none());
        assert!(
            acquire(dir.path(), r"Local\PhoneBox.Agent.18766", Duration::ZERO).is_some(),
            "another port is another instance"
        );
        drop(first);
        assert!(acquire(dir.path(), r"Local\PhoneBox.Agent.18765", Duration::ZERO).is_some());
    }

    #[test]
    fn a_restart_waits_for_the_previous_instance() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path(), "x", Duration::ZERO).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
        });
        assert!(acquire(dir.path(), "x", Duration::from_secs(5)).is_some());
        releaser.join().unwrap();
    }
}
