mod cli;
mod index;
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
