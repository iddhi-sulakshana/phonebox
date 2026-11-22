//! Port of `Unix/AppleSmc.cs`: CPU and GPU temperature on a Mac with Apple silicon, from the System Management
//! Controller (AppleSMC) without root. The key rules follow macmon (MIT, github.com/vladkens/macmon): the average of
//! the "Tp", "Te" and "Ts" float keys (not "Ts*P") is the CPU, the average of the "Tg" keys the GPU.

// "Ts*P" keys don't follow the CPU load (macmon), so they are left out of the CPU average.
pub fn is_cpu_key(key: &str) -> bool {
    let b = key.as_bytes();
    b.len() == 4
        && (key.starts_with("Tp")
            || key.starts_with("Te")
            || (key.starts_with("Ts") && b[3] != b'P'))
}

pub fn is_gpu_key(key: &str) -> bool {
    key.len() == 4 && key.starts_with("Tg")
}

/// Average of the readings a die can give: a sensor that is off reads 0 or a few degrees.
pub fn average(values: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    let valid: Vec<f64> = values
        .into_iter()
        .flatten()
        .filter(|v| v.is_finite() && *v > 10.0 && *v <= 150.0)
        .collect();
    if valid.is_empty() {
        return None;
    }
    Some(crate::system_monitor::round_dotnet(
        valid.iter().sum::<f64>() / valid.len() as f64,
        1,
    ))
}

/// The key's four ASCII characters as a big-endian number.
pub fn four_cc(key: &str) -> u32 {
    let mut b = [0u8; 4];
    for (d, s) in b.iter_mut().zip(key.bytes()) {
        *d = s;
    }
    u32::from_be_bytes(b)
}

pub fn key_name(key: u32) -> String {
    key.to_be_bytes().iter().map(|b| *b as char).collect()
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use mac::AppleSmc;

/// The SMC connection: only a Mac with Apple silicon has one; elsewhere `open` says so.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub struct AppleSmc;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
impl AppleSmc {
    pub fn open() -> Option<AppleSmc> {
        None
    }

    pub fn read(&self) -> Result<(Option<f64>, Option<f64>), String> {
        Err("no SMC".into())
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod mac {
    use std::ffi::{CStr, c_char, c_void};

    use super::{average, four_cc, is_cpu_key, is_gpu_key, key_name};

    const STRUCT_SIZE: usize = 80; // SMCKeyData_t
    const HANDLE_YPC_EVENT: u32 = 2; // kSMCHandleYPCEvent
    const CMD_READ_KEY: u8 = 5;
    const CMD_READ_INDEX: u8 = 8;
    const CMD_READ_KEY_INFO: u8 = 9;
    const FLOAT_TYPE: u32 = 0x666C_7420; // "flt "
    const MAX_KEYS: u32 = 4096; // a sane bound for "#KEY": the M4 has ~1350

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
        fn IOServiceOpen(service: u32, owning_task: u32, kind: u32, connect: *mut u32) -> i32;
        fn IOServiceClose(connect: u32) -> i32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IOConnectCallStructMethod(
            connection: u32,
            selector: u32,
            input: *const u8,
            input_size: usize,
            output: *mut u8,
            output_size: *mut usize,
        ) -> i32;
    }

    unsafe extern "C" {
        static mach_task_self_: u32;
    }

    /// The open SMC connection and the temperature keys found when it was opened.
    pub struct AppleSmc {
        conn: u32,
        cpu_keys: Vec<u32>,
        gpu_keys: Vec<u32>,
    }

    impl AppleSmc {
        /// Opens the SMC and finds the temperature keys, or None where there is no SMC or no such key.
        pub fn open() -> Option<AppleSmc> {
            const NAME: &CStr = c"AppleSMC";
            // SAFETY: IOServiceMatching copies the name into a new dictionary that IOServiceGetMatchingService consumes.
            let service =
                unsafe { IOServiceGetMatchingService(0, IOServiceMatching(NAME.as_ptr())) };
            if service == 0 {
                return None;
            }
            let mut conn = 0u32;
            // SAFETY: `service` is a valid io_service_t; mach_task_self_ is this task's port; `conn` is an out-pointer.
            let opened =
                unsafe { IOServiceOpen(service, mach_task_self_, 0, &mut conn) } == 0 && conn != 0;
            // SAFETY: the service object was returned to us and is released once.
            unsafe { IOObjectRelease(service) };
            if !opened {
                return None;
            }
            let mut smc = AppleSmc {
                conn,
                cpu_keys: Vec::new(),
                gpu_keys: Vec::new(),
            };
            smc.find_keys().then_some(smc)
        }

        /// The CPU and GPU averages in °C, rounded to a tenth; Err when no key answers at all: every one of them did
        /// when the SMC was opened, so the connection is gone and must be opened again.
        pub fn read(&self) -> Result<(Option<f64>, Option<f64>), String> {
            let cpu: Vec<Option<f64>> = self.cpu_keys.iter().map(|k| self.read_float(*k)).collect();
            let gpu: Vec<Option<f64>> = self.gpu_keys.iter().map(|k| self.read_float(*k)).collect();
            if cpu.iter().chain(&gpu).all(Option::is_none) {
                return Err("the SMC answered no temperature key".into());
            }
            Ok((average(cpu), average(gpu)))
        }

        fn find_keys(&mut self) -> bool {
            let count = match self.read_raw(four_cc("#KEY"), None) {
                Some(d) if d.len() == 4 => u32::from_be_bytes([d[0], d[1], d[2], d[3]]),
                _ => 0,
            };
            for i in 0..count.min(MAX_KEYS) {
                let mut input = [0u8; STRUCT_SIZE];
                input[42] = CMD_READ_INDEX;
                input[44..48].copy_from_slice(&i.to_le_bytes());
                let Some(output) = self.call(&input) else {
                    continue;
                };
                let key = u32::from_le_bytes([output[0], output[1], output[2], output[3]]);
                let name = key_name(key);
                let cpu = is_cpu_key(&name);
                if !cpu && !is_gpu_key(&name) {
                    continue;
                }
                if self.read_float(key).is_none() {
                    continue; // not a float, or unreadable: never part of an average
                }
                if cpu {
                    self.cpu_keys.push(key)
                } else {
                    self.gpu_keys.push(key)
                }
            }
            !self.cpu_keys.is_empty() || !self.gpu_keys.is_empty()
        }

        fn read_float(&self, key: u32) -> Option<f64> {
            let d = self
                .read_raw(key, Some(FLOAT_TYPE))
                .filter(|d| d.len() == 4)?;
            Some(f64::from(f32::from_le_bytes([d[0], d[1], d[2], d[3]])))
        }

        /// One key's bytes; with `kind`, only a key of that data type.
        fn read_raw(&self, key: u32, kind: Option<u32>) -> Option<Vec<u8>> {
            let mut input = [0u8; STRUCT_SIZE];
            input[0..4].copy_from_slice(&key.to_le_bytes());
            input[42] = CMD_READ_KEY_INFO;
            let info = self.call(&input).filter(|o| o[40] == 0)?;
            let size = u32::from_le_bytes([info[28], info[29], info[30], info[31]]);
            let data_type = u32::from_le_bytes([info[32], info[33], info[34], info[35]]);
            if size == 0 || size > 32 || kind.is_some_and(|t| data_type != t) {
                return None;
            }
            input[42] = CMD_READ_KEY;
            input[28..32].copy_from_slice(&size.to_le_bytes());
            let output = self.call(&input).filter(|o| o[40] == 0)?;
            Some(output[48..48 + size as usize].to_vec())
        }

        fn call(&self, input: &[u8; STRUCT_SIZE]) -> Option<[u8; STRUCT_SIZE]> {
            let mut output = [0u8; STRUCT_SIZE];
            let mut out_size = STRUCT_SIZE;
            // SAFETY: both buffers are STRUCT_SIZE bytes, as the sizes passed say; `conn` is open while self lives.
            let ok = self.conn != 0
                && unsafe {
                    IOConnectCallStructMethod(
                        self.conn,
                        HANDLE_YPC_EVENT,
                        input.as_ptr(),
                        STRUCT_SIZE,
                        output.as_mut_ptr(),
                        &mut out_size,
                    )
                } == 0;
            ok.then_some(output)
        }
    }

    impl Drop for AppleSmc {
        fn drop(&mut self) {
            if self.conn != 0 {
                // SAFETY: the connection was opened by IOServiceOpen and is closed once.
                unsafe { IOServiceClose(self.conn) };
            }
            self.conn = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_keys() {
        for (key, expected) in [
            ("Tp00", true),
            ("Tp0W", true),
            ("Te05", true),
            ("Ts0S", true),
            ("Ts1P", false), // "Ts*P" doesn't follow the CPU load
            ("Tg0C", false),
            ("TPD0", false),
            ("Tp0", false),
            ("tp00", false),
        ] {
            assert_eq!(is_cpu_key(key), expected, "{key}");
        }
    }

    #[test]
    fn gpu_keys() {
        for (key, expected) in [
            ("Tg0C", true),
            ("Tg1z", true),
            ("Tp00", false),
            ("TG0C", false),
            ("Tg0", false),
        ] {
            assert_eq!(is_gpu_key(key), expected, "{key}");
        }
    }

    #[test]
    fn the_average_leaves_out_sensors_that_are_off_or_nonsense() {
        assert_eq!(
            average([
                Some(60.0),
                Some(70.0),
                Some(0.0),
                Some(3.5),
                None,
                Some(400.0),
                Some(f64::NAN)
            ]),
            Some(65.0)
        );
        assert_eq!(average([Some(150.0)]), Some(150.0));
        assert_eq!(average([Some(61.66), Some(61.74)]), Some(61.7));
    }

    #[test]
    fn without_a_valid_reading_there_is_no_temperature() {
        assert_eq!(average([]), None);
        assert_eq!(
            average([None, Some(0.0), Some(10.0), Some(f64::INFINITY)]),
            None
        );
    }

    #[test]
    fn key_names_round_trip() {
        for key in ["#KEY", "Tp0W", "flt "] {
            assert_eq!(key_name(four_cc(key)), key);
        }
    }

    #[test]
    fn four_cc_is_big_endian() {
        assert_eq!(four_cc("flt "), 0x666C_7420);
    }

    // Only a Mac with Apple silicon has these sensors; anywhere else open() must say so instead of failing.
    #[test]
    fn on_an_apple_silicon_mac_the_readings_are_plausible() {
        let smc = AppleSmc::open();
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert!(smc.is_none());
            return;
        }
        let Some(smc) = smc else { return }; // macOS 13 and older expose no temperature keys
        let (cpu, gpu) = smc.read().unwrap();
        assert!(cpu.is_none_or(|c| c > 10.0 && c <= 150.0), "cpu {cpu:?}");
        assert!(gpu.is_none_or(|g| g > 10.0 && g <= 150.0), "gpu {gpu:?}");
        assert!(cpu.is_some() || gpu.is_some());
    }
}
