//! The Voicemeeter Remote DLL (`ControlDeck.Integrations.cs:47-94`): loaded from the path the owner configured, its
//! five exports bound, logged in; logged out and freed when it is dropped.

use std::ffi::{CString, c_char};

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{HSTRING, s};

type Login = unsafe extern "system" fn() -> i32;
type Get = unsafe extern "system" fn(*const c_char, *mut f32) -> i32;
type Set = unsafe extern "system" fn(*const c_char, f32) -> i32;
type Export = unsafe extern "system" fn() -> isize;

pub(super) struct Link {
    library: HMODULE,
    path: String,
    logout: Login,
    dirty: Login,
    get: Get,
    set: Set,
}

// SAFETY: a module handle and plain function pointers into it, which any thread may call (C# called them from any)
unsafe impl Send for Link {}

impl Link {
    /// The DLL at `path`, logged in; None when it does not load, lacks an export or refuses the login.
    pub(super) fn load(path: &str) -> Option<Link> {
        // SAFETY: the DLL the owner configured, as C#'s NativeLibrary.Load; its exports cast to the signatures
        // Voicemeeter documents for them
        unsafe {
            let library = LoadLibraryW(&HSTRING::from(path)).ok()?;
            let exports = (
                GetProcAddress(library, s!("VBVMR_Login")),
                GetProcAddress(library, s!("VBVMR_Logout")),
                GetProcAddress(library, s!("VBVMR_IsParametersDirty")),
                GetProcAddress(library, s!("VBVMR_GetParameterFloat")),
                GetProcAddress(library, s!("VBVMR_SetParameterFloat")),
            );
            let (Some(login), Some(logout), Some(dirty), Some(get), Some(set)) = exports else {
                let _ = FreeLibrary(library);
                return None;
            };
            let link = Link {
                library,
                path: path.to_string(),
                logout: std::mem::transmute::<Export, Login>(logout),
                dirty: std::mem::transmute::<Export, Login>(dirty),
                get: std::mem::transmute::<Export, Get>(get),
                set: std::mem::transmute::<Export, Set>(set),
            };
            let login = std::mem::transmute::<Export, Login>(login);
            // a refused login logs out and frees the DLL, as C#'s DisposeMixer
            (login() >= 0).then_some(link)
        }
    }

    pub(super) fn path(&self) -> &str {
        &self.path
    }

    /// `VBVMR_IsParametersDirty` answered (not an error): the engine runs and the parameters can be read.
    pub(super) fn ready(&self) -> bool {
        // SAFETY: an export of the loaded DLL, no arguments
        unsafe { (self.dirty)() >= 0 }
    }

    pub(super) fn get(&self, name: &str) -> Option<f32> {
        let name = CString::new(name).ok()?;
        let mut value = 0f32;
        // SAFETY: a NUL-terminated name and a float to write
        let read = unsafe { (self.get)(name.as_ptr(), &mut value) };
        (read == 0).then_some(value)
    }

    pub(super) fn set(&self, name: &str, value: f32) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        // SAFETY: a NUL-terminated name and a value
        unsafe { (self.set)(name.as_ptr(), value) == 0 }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // SAFETY: the DLL's own logout, then the module loaded in `load`, freed once
        unsafe {
            (self.logout)();
            let _ = FreeLibrary(self.library);
        }
    }
}
