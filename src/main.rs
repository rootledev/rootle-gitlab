//! The stdin loop: read a line, dispatch, reply, flush. Nothing else —
//! rootle owns this process's lifecycle and may respawn it at any
//! time (initialize runs once per generation, cheaply).

use rootle_gitlab::{Handler, api, respond};

fn main() {
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

    let handler = Handler::new(&instance, &token_env, cache_base);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    use std::io::BufRead;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = respond(&handler, &line) {
            println!("{reply}");
            use std::io::Write;
            let _ = out.flush();
        }
    }
}
