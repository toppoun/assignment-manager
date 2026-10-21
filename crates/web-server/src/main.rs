use assignment_manager_web_server::{
    DEFAULT_PORT, PocServer, SystemClock, WebError, poc::PocDatabase,
};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Web-2 PoC: {}", error.code());
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), WebError> {
    // In particular, do not silently accept --host/--bind/--database options.
    if std::env::args_os().len() != 1 {
        return Err(WebError::InvalidRequest);
    }
    let database = PocDatabase::prepare(&PocDatabase::default_directory())?;
    let server = PocServer::start(&database, SystemClock, DEFAULT_PORT).await?;
    println!("Synthetic Web-2 PoC only: http://{}/", server.local_addr());
    println!("No authentication. Keep this loopback endpoint private. Ctrl+C to stop.");
    server
        .serve(async {
            if tokio::signal::ctrl_c().await.is_err() {
                eprintln!("Web-2 PoC: signal unavailable; stopping");
            }
        })
        .await
}
