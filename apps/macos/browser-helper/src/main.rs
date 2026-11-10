//! The in-app browser's child processes. Chromium starts its renderers, GPU, network and
//! utility processes from copies of this executable, each in its own helper bundle under
//! the app's Contents/Frameworks, so none of them loads the app itself.

use std::process::ExitCode;

use cef::args::Args;
use cef::library_loader::LibraryLoader;
use cef::sandbox::Sandbox;

fn main() -> ExitCode {
    let args = Args::new();
    // The sandbox locks the process down before any web content is parsed.
    let mut sandbox = Sandbox::new();
    sandbox.initialize(args.as_main_args());
    let Ok(executable) = std::env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let library = LibraryLoader::new(&executable, true);
    if !library.load() {
        return ExitCode::FAILURE;
    }
    cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let code = cef::execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    // The framework unloads before the sandbox context it ran in is destroyed.
    drop(library);
    drop(sandbox);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
