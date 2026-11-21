//! Port of `Unix/`: helpers the Linux and macOS code paths share (`Sh`, `UnixSystem`, `AppleSmc`, `UnixProc`).
//! Like the C# files they compile on every OS; the OS-specific calls inside are `cfg`-gated.

pub mod sh;
