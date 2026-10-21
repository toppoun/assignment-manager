//! Explicit manual synthetic environment. Never uses the ordinary local vault.
use assignment_manager_web_server::{
    DEFAULT_PORT, PocServer, SystemClock, WebError, auth, poc::PocDatabase, vault,
};

#[tokio::main]
async fn main() -> Result<(), WebError> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args != ["bootstrap"] && args != ["init-vault-key"] && args != ["serve"] {
        return Err(WebError::InvalidRequest);
    }
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("web-3b-browser-smoke");
    let db = PocDatabase::prepare(&directory)?;
    match args[0].as_str() {
        "bootstrap" => {
            let password = zeroize::Zeroizing::new(
                rpassword::prompt_password(
                    "Synthetic Web-only smoke password (hidden, 15-256 bytes): ",
                )
                .map_err(|_| WebError::InvalidRequest)?,
            );
            auth::bootstrap_admin(&db, "synthetic-smoke", password)?;
            println!(
                "Isolated synthetic-smoke account created. Initialize its vault key explicitly."
            );
        }
        "init-vault-key" => {
            vault::initialize_local_key(&db)?;
            println!("Isolated smoke key initialized/verified.");
        }
        "serve" => {
            let server = PocServer::start(&db, SystemClock, DEFAULT_PORT).await?;
            println!(
                "Isolated synthetic Browser smoke at http://127.0.0.1:3000/ (Ctrl+C to stop)."
            );
            server
                .serve(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        _ => return Err(WebError::InvalidRequest),
    }
    Ok(())
}
