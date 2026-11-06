//! Yappr command line entry point.

mod cli;

use std::io;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    // Pairing may leave a blocking stdin read pending. Exit explicitly so
    // runtime shutdown does not wait for that read.
    match cli::run().await {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            let code = cli::exit_code(&e);
            if code != 0 {
                eprintln!("error: {}", cli::shown(&e));
            }
            std::process::exit(code)
        }
    }
}
