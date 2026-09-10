mod buildsort;
mod casefold;
mod cli;
mod config;
#[cfg(feature = "daemon")]
mod daemon;
mod filetype;
mod index;
#[cfg(target_os = "macos")]
pub mod metal;
mod persist;
mod postenc;
mod render;
mod searcher;
mod trigram;

fn main() {
    // grep/ripgrep-compatible exit status: 0 = something matched (or a
    // subcommand succeeded), 1 = nothing matched, 2 = error (bad pattern,
    // unreadable index, invalid flag combination, …).
    match cli::run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("Error: {:#}", e);
            std::process::exit(2);
        }
    }
}
