mod adapter;
mod commands;
mod model;
mod policy;
mod repository;
mod resolver;
mod util;

fn main() {
    if let Err(error) = commands::run() {
        eprintln!("crane: {error}");
        std::process::exit(1);
    }
}
