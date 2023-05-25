mod conversation_state;
mod deepgram_types;
mod error;
mod handlers;
mod openai_types;
mod tasks;
#[allow(clippy::all)]
mod texttospeech_v1_types;
mod twilio_types;
mod types;
mod utils;

use crate::types::AppState;

use axum::{
    routing::{get, post},
    Router,
};
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use tracing_subscriber::prelude::*;

pub mod consts {
    pub const APP_GREETING: &str = "Hi.  How may I help you?";
    pub const ASCII_CLAUSE_ENDINGS: &[&str] = &[".", "?", "!", ";"];
    pub const GOOGLE_WAV_HEADER_SZ: usize = 58;
    pub const POLITENESS_DELAY_MILLIS: u128 = 1_500;
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().unwrap();
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_file(true)
                .with_line_number(true),
        )
        .with(tracing_subscriber::filter::Targets::new().with_targets([
            ("hyper", tracing_subscriber::filter::LevelFilter::OFF),
            ("twilio_rs", tracing_subscriber::filter::LevelFilter::DEBUG),
        ]));
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let console_api_key = env::var("CONSOLE_API_KEY").expect("CONSOLE_API_KEY not set!");
    let openai_api_key = env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY not set!");
    let twilio_account_sid = env::var("TWILIO_ACCOUNT_SID").expect("TWILIO_ACCOUNT_SID not set!");
    let twilio_auth_token = env::var("TWILIO_AUTH_TOKEN").expect("TWILIO_AUTH_TOKEN not set!");
    let gcs_credentials = env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .expect("No google application credentials location set.");
    let gcs_client = utils::gcs_client(&gcs_credentials).await;
    let http_client = reqwest::Client::new();
    let streams = Arc::new(Mutex::new(HashMap::new()));

    let app_state = Arc::new(AppState {
        console_api_key,
        openai_api_key,
        twilio_account_sid,
        twilio_auth_token,
        gcs_client,
        http_client,
        streams,
    });

    let app = Router::new()
        .route("/connect", get(handlers::ws_handler))
        .route("/play", get(handlers::play_handler))
        // Choose whether to use a Play verb or Connect verb in start Twiml.
        // .route("/twilio/twiml/start", post(handlers::twiml_start_play))
        .route("/twilio/twiml/start", post(handlers::twiml_start_connect))
        .route("/", get(|| async { "Hello, World!" }))
        .with_state(app_state);

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}
