mod cli;
mod index;
#[cfg(target_os = "macos")]
pub mod metal;
mod persist;
mod searcher;
mod sparse;
mod trigram;

fn main() {
    if let Err(e) = cli::run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}
