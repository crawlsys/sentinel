//! Git safety interceptor binary.
//!
//! Shadows the real `git` at `~/bin/git`. Evaluates every git command
//! against sentinel's domain policy, blocks dangerous operations, and
//! offers a `--bypass` escape hatch with native OS confirmation dialog.

use std::process::ExitCode;

use sentinel_application::interceptor::{GitInterceptorService, GitResult};
use sentinel_infrastructure::interceptor::{
    NativeBypassDialog, PathBinaryResolver, PrecomputedInteractiveCheck, SystemProcessExecutor,
};

fn is_interactive() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::AsRawHandle;
        extern "system" {
            fn GetConsoleMode(h: *mut std::ffi::c_void, m: *mut u32) -> i32;
        }
        let mut m = 0u32;
        unsafe { GetConsoleMode(std::io::stdin().as_raw_handle().cast(), &raw mut m) != 0 }
    }

    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::io::AsRawFd;
        extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        unsafe { isatty(std::io::stdin().as_raw_fd()) != 0 }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let self_path = std::env::current_exe().unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let resolver = PathBinaryResolver;
    let bypass = NativeBypassDialog;
    let executor = SystemProcessExecutor;
    let interactive = PrecomputedInteractiveCheck {
        interactive: is_interactive(),
    };

    let service = GitInterceptorService::new(&resolver, &bypass, &executor, &interactive);

    match service.run(&args, &self_path, &cwd) {
        GitResult::Executed(code) => code,
        GitResult::Blocked | GitResult::Declined => ExitCode::from(1),
        GitResult::BinaryNotFound => {
            eprintln!("\x1b[0;31mGit not found\x1b[0m");
            ExitCode::from(127)
        }
    }
}
