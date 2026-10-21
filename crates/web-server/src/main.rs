use assignment_manager_web_server::{
    DEFAULT_PORT, PocServer, SystemClock, WebError, poc::PocDatabase,
};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Web-3A PoC: {}", error.code());
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), WebError> {
    // In particular, do not silently accept --host/--bind/--database options.
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !args.is_empty() && args != ["bootstrap-admin"] {
        return Err(WebError::InvalidRequest);
    }
    let database = PocDatabase::prepare(&PocDatabase::default_directory())?;
    if args == ["bootstrap-admin"] {
        use std::io::{self, Write};
        print!("Web-only login ID (not a university ID): ");
        io::stdout().flush().map_err(|_| WebError::Internal)?;
        let mut login = String::new();
        io::stdin()
            .read_line(&mut login)
            .map_err(|_| WebError::InvalidRequest)?;
        let password = zeroize::Zeroizing::new(
            rpassword::prompt_password("Web-only password (15-256 bytes, hidden): ")
                .map_err(|_| WebError::InvalidRequest)?,
        );
        let confirmation = zeroize::Zeroizing::new(
            rpassword::prompt_password("Confirm password (hidden): ")
                .map_err(|_| WebError::InvalidRequest)?,
        );
        if password.as_str() != confirmation.as_str() {
            return Err(WebError::InvalidRequest);
        }
        assignment_manager_web_server::auth::bootstrap_admin(&database, login.trim(), password)?;
        println!("Web account created. Start the server without arguments.");
        return Ok(());
    }
    let server = PocServer::start(&database, SystemClock, DEFAULT_PORT).await?;
    println!(
        "Synthetic Web-3A loopback PoC: http://{}/",
        server.local_addr()
    );
    println!(
        "Web login required. HTTP development only; public deployment unsupported. Ctrl+C to stop."
    );
    server
        .serve(async {
            if tokio::signal::ctrl_c().await.is_err() {
                eprintln!("Web-3A PoC: signal unavailable; stopping");
            }
        })
        .await
}
