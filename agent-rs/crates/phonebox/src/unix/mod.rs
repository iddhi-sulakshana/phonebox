//! Port of `Unix/`: helpers the Linux and macOS code paths share (`Sh`, `UnixSystem`, `AppleSmc`, `UnixProc`).
//! Like the C# files they compile on every OS; the OS-specific calls inside are `cfg`-gated.

pub mod apple_smc;
pub mod audio_control;
pub mod mac_audio_probe;
pub mod sh;
pub mod unix_proc;
pub mod unix_system;
