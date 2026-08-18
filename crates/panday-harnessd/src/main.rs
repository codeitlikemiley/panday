//! panday-harnessd — the cloud session service (docs/22 shape 2).
//!
//! Thin binary over the library, per docs/02.

#[tokio::main]
async fn main() {
    let addr =
        std::env::var("PANDAY_HARNESSD_ADDR").unwrap_or_else(|_| "127.0.0.1:8081".to_string());

    let state = panday_harnessd::AppState::new();
    let app = panday_harnessd::router(state);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("panday-harnessd: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("panday-harnessd listening on {addr}");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("panday-harnessd: {e}");
        std::process::exit(1);
    }
}
