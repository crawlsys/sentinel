//! Npx interceptor binary.
//!
//! Intercepts the real `npx` at `~/bin/npx`. Redirects known Node package
//! commands to local Rust CLI equivalents for faster execution.
//! Unknown packages pass through to the real npx.

use std::process::ExitCode;

use sentinel_application::interceptor::{NpxInterceptorService, NpxResult};
use sentinel_infrastructure::interceptor::{
    PathBinaryResolver, SystemProcessExecutor, TomlRedirectLoader,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let self_path = std::env::current_exe().unwrap_or_default();

    let resolver = PathBinaryResolver;
    let executor = SystemProcessExecutor;
    let loader = TomlRedirectLoader;

    let service = NpxInterceptorService::new(&resolver, &executor, &loader);

    match service.run(&args, &self_path) {
        NpxResult::Executed(code) => code,
        NpxResult::BinaryNotFound => {
            eprintln!("\x1b[0;31mCould not find real npx executable\x1b[0m");
            ExitCode::from(127)
        }
        NpxResult::RedirectNotFound(target) => {
            eprintln!("\x1b[0;31mRedirect target not found: {target}\x1b[0m");
            ExitCode::from(1)
        }
    }
}
