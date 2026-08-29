//! Argument parsing plus the stdin loop (in `rootle_gitlab::serve_stdio`):
//! read lines on this thread, dispatch each request on its own worker
//! thread, write the reply lines through a shared line-atomic stdout
//! writer (v1.3 progressive results may interleave `/$partial`
//! notifications between requests — every line is id-tagged), flush
//! per line. Nothing else — rootle owns this process's lifecycle and
//! may respawn it at any time (initialize runs once per generation,
//! cheaply).

use rootle_gitlab::{Handler, api, serve_stdio};

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("rootle-gitlab {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut instance = api::DEFAULT_INSTANCE.to_string();
    let mut token_env = api::DEFAULT_TOKEN_ENV.to_string();
    let mut cache_base: Option<std::path::PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--instance" => instance = args.next().unwrap_or_default(),
            "--token-env" => token_env = args.next().unwrap_or_default(),
            "--cache" => cache_base = args.next().map(std::path::PathBuf::from),
            other => {
                eprintln!("rootle-gitlab: unknown flag {other:?}");
                std::process::exit(2);
            }
        }
    }

    serve_stdio(&Handler::new(&instance, &token_env, cache_base));
}
