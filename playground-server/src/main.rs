use axum::{routing::post, Json, Router, http::Method};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

#[derive(Deserialize)]
struct RunRequest {
    source: String,
}

#[derive(Serialize)]
struct RunResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
    elapsed_ms: u64,
}

async fn run_handler(Json(req): Json<RunRequest>) -> Json<RunResponse> {
    let t0 = Instant::now();

    let (file_arg, _tmpfile) = if req.source.contains('\n') {
        let tmp = tempfile::NamedTempFile::with_suffix(".frog")
            .expect("failed to create tempfile");
        std::fs::write(tmp.path(), &req.source).expect("failed to write tempfile");
        let path = tmp.path().to_string_lossy().into_owned();
        (path, Some(tmp))
    } else {
        (req.source.clone(), None)
    };

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("froglang-core")
            .args(["run", &file_arg])
            .output(),
    )
    .await;

    let elapsed_ms = t0.elapsed().as_millis() as u64;

    match result {
        Err(_) => Json(RunResponse {
            stdout: String::new(),
            stderr: "Timeout (10s exceeded)".into(),
            exit_code: -1,
            elapsed_ms,
        }),
        Ok(Err(e)) => Json(RunResponse {
            stdout: String::new(),
            stderr: format!("Failed to run froglang-core: {}", e),
            exit_code: -1,
            elapsed_ms,
        }),
        Ok(Ok(output)) => Json(RunResponse {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
            elapsed_ms,
        }),
    }
}

#[tokio::main]
async fn main() {
    let cors = CorsLayer::new()
        .allow_methods([Method::POST, Method::GET])
        .allow_headers(Any)
        .allow_origin(Any);

    let app = Router::new()
        .route("/run", post(run_handler))
        .fallback_service(ServeDir::new("/app/ui"))
        .layer(cors);

    let addr = "0.0.0.0:8080";
    println!("Listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
