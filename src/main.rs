mod conversation_state;
mod deepgram_types;
mod error;
mod openai_types;
#[allow(clippy::all)]
mod texttospeech_v1_types;
mod twilio_types;
mod types;

use crate::consts::GOOGLE_WAV_HEADER_SZ;
use crate::conversation_state::ConversationState;
use crate::error::{handle_error, AppError};
use crate::texttospeech_v1_types::{AudioConfigAudioEncoding, TextService};
use crate::twilio_types::*;
use crate::types::AppState;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Host, State,
    },
    http::{header, HeaderMap},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use base64::{engine, read, Engine};
use futures_util::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use gcs_common::yup_oauth2;
use std::collections::HashMap;
use std::env;
use std::io::{Cursor, Read};
use std::sync::{Arc, Mutex};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};
use tracing_subscriber::prelude::*;

pub mod consts {
    pub const ASCII_CLAUSE_ENDINGS: &[&str] = &[".", "?", "!", ";"];
    pub const GOOGLE_WAV_HEADER_SZ: usize = 58;
    pub const POLITENESS_DELAY_MILLIS: u128 = 1_500;
}

/// Capture the Twilio Start media message from the beginning of a Twilio websocket stream for the
/// stream id.
async fn get_twilio_start_meta(
    twilio_stream: &mut SplitStream<WebSocket>,
) -> Result<twilio_types::StartMeta, AppError> {
    loop {
        match twilio_stream.next().await {
            Some(msg) => match msg {
                Ok(Message::Text(json)) => match serde_json::from_str(&json) {
                    Ok(message) => match message {
                        TwilioMessage::Connected { protocol, version } => {
                            debug!("Got connected message with {protocol} and {version}");
                        }
                        TwilioMessage::Start {
                            start: start_meta, ..
                        } => {
                            break Ok(start_meta);
                        }
                        _ => {
                            break Err(AppError("At this point in a stream, we only expect a Connected message or a Start message.  Any others constitute an error."));
                        }
                    },
                    Err(e) => {
                        error!(error=%e, "failed to deserialize Twilio text message");
                        break Err(AppError("Error deserializing twilio text message"));
                    }
                },
                _ => {
                    break Err(AppError(
                        "Got unexpected websocket message type from Twilio!",
                    ));
                }
            },
            None => break Err(AppError("End of stream")),
        }
    }
}

/// Task that streams all Twilio media messages with encoded caller-side audio to Deepgram.
async fn stream_twilio_audio_to_deepgram(
    mut twilio_stream: SplitStream<WebSocket>,
    mut dg_sink: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, tungstenite::Message>,
) -> Result<(), AppError> {
    loop {
        match twilio_stream.next().await {
            Some(msg) => match msg {
                Ok(Message::Text(json)) => match serde_json::from_str(&json) {
                    Ok(message) => match message {
                        TwilioMessage::Media { media, .. } => {
                            let mut chunk = Vec::new();
                            b64_decode_to_buf(media.payload, &mut chunk);
                            dg_sink
                                .send(tungstenite::Message::Binary(chunk))
                                .await
                                .unwrap();
                        }
                        TwilioMessage::Stop {
                            sequence_number, ..
                        } => {
                            debug!("Got stop message {sequence_number}");
                            break Ok(());
                        }
                        TwilioMessage::Mark { .. } => {
                            debug!("Got mark message; not sure what to do with this???");
                        }
                        _ => {
                            break Err(AppError(
                                "We should not be getting Connected or Start messages now!",
                            ));
                        }
                    },
                    Err(e) => {
                        error!(error=%e, "failed to parse Twilio text message");
                        break Err(AppError("Failed to parse incoming text message"));
                    }
                },
                Ok(Message::Ping(_)) => (),
                Ok(m) => {
                    warn!(message=?m, "unsupported message type from Twilio");
                    continue;
                }
                Err(e) => {
                    error!(error=%e, "failed to receive message from Twilio");
                    break Err(AppError("Failed to receive message from Twilio stream"));
                }
            },
            None => {
                info!("end of twilio stream");
                break Ok(());
            }
        }
    }
}

/// Open websocket connection to Deepgram.
async fn open_dg_stream(
    app_state: &Arc<AppState>,
) -> Result<
    (
        SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, tungstenite::Message>,
        SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ),
    AppError,
> {
    debug!("Connecting to DG");
    // TODO: not sure if endpointing on/off makes much difference???
    let uri = "wss://api.deepgram.com/v1/listen\
               ?encoding=mulaw\
               &sample_rate=8000\
               &interim_results=true\
               &endpointing=false";
    let mut rq = uri.into_client_request().unwrap();
    rq.headers_mut()
        .entry(http::header::AUTHORIZATION)
        .or_insert(
            http::header::HeaderValue::from_str(&format!("Token {}", app_state.console_api_key))
                .unwrap(),
        );
    let (ws_stream, _) = connect_async(rq).await.unwrap();
    Ok(ws_stream.split())
}

/// Task that handles the back and forth of the bot's interaction with a caller
async fn manage_conversation(
    mut dg_stream: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    twilio_sink: SplitSink<WebSocket, Message>,
    twilio_start_meta: twilio_types::StartMeta,
    app_state: Arc<AppState>,
) -> Result<(), AppError> {
    let mut state =
        ConversationState::new(twilio_start_meta.stream_sid, twilio_sink, app_state.clone()).await;

    // While a call is ongoing, we continuously loop over streaming responses from DG
    loop {
        if let Some(res) = dg_stream.next().await {
            match res {
                Ok(dg_msg) => state.handle_dg_message(dg_msg).await?,
                Err(e) => {
                    error!(error=%e, "failed to handle DG message");
                    break Err(AppError("dg stream error"));
                }
            }
        } else {
            debug!(twilio_stream=%state.twilio_stream_id, "dg stream completed");
            break Ok(());
        }
    }?;

    // TODO: After the call, prepare a summary to send via text
    let conversation_summaries = state.get_conversation_summaries().await?;
    let sms_summary = conversation_summaries
        .iter()
        .map(|s| format!("{}: {}", s.topic, s.summary))
        .collect::<Vec<String>>()
        .join("\n\n");
    debug!(summay=?sms_summary, "sms summary");

    // remoce connect payload from app state cache
    let connect_payload: TwilioConnectPayload = {
        let mut streams = app_state.streams.lock().unwrap();
        streams.remove(&twilio_start_meta.call_sid)
    }
    .ok_or_else(|| {
        error!("failed to remove twilio connect payload from app state streams");
        AppError("app state streams error")
    })?;

    // Depending on whether the summary result was an error, send a summary or an apology
    let account_sid = &app_state.twilio_account_sid;
    let url = format!("https://api.twilio.com/2010-04-01/Accounts/{account_sid}/Messages.json");
    let mut form = HashMap::new();
    form.insert("From", connect_payload.to);
    form.insert("To", connect_payload.from);
    form.insert("Body", sms_summary);
    let resp = app_state
        .http_client
        .post(url)
        .basic_auth(account_sid, Some(&app_state.twilio_auth_token))
        .form(&form)
        .send()
        .await
        .map_err(|e| {
            error!(error=%e, "failed to send sms reqeust to twilio");
            AppError("twilio sms api")
        }); // we don't really care if this succeeds or fails
    debug!(twilio_resp=?resp, "twilio sms resp");

    // Insert stuff into db

    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(app_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| socket_handler(socket, app_state))
}

async fn socket_handler(socket: WebSocket, app_state: Arc<AppState>) {
    let (twilio_sink, mut twilio_stream) = socket.split();
    // Get Twilio stream id
    let start_meta = get_twilio_start_meta(&mut twilio_stream).await;
    if let Err(e) = start_meta {
        handle_error(e).await;
        return;
    }
    let start_meta = start_meta.unwrap();
    debug!(meta = ?start_meta, "got start meta from twilio stream");

    // Open streaming connection to DG
    let res = open_dg_stream(&app_state).await;
    if let Err(e) = res {
        handle_error(e).await;
        return;
    }
    let (dg_sink, dg_stream) = res.unwrap();
    info!("opened connection to Deepgram");

    // Start
    let _res = tokio::try_join!(
        stream_twilio_audio_to_deepgram(twilio_stream, dg_sink),
        manage_conversation(dg_stream, twilio_sink, start_meta, app_state.clone()),
    );
}

/// Prepare a TwilioOutbound message from a Google TTS payload.
pub async fn google2twilio(google_tts: String, stream_sid: &str) -> TwilioOutbound {
    let mut body = Vec::new();
    b64_decode_to_buf(google_tts, &mut body);
    let trimmed = body[GOOGLE_WAV_HEADER_SZ..].to_vec();
    let re_encoded: String = engine::general_purpose::STANDARD.encode(trimmed);
    let outbound_media_meta = OutboundMediaMeta {
        payload: re_encoded,
    };
    TwilioOutbound::Media {
        media: outbound_media_meta,
        stream_sid: stream_sid.to_string(),
    }
}

fn b64_decode_to_buf(enc: String, buf: &mut Vec<u8>) {
    let mut cur = Cursor::new(enc);
    let mut decoder = read::DecoderReader::new(&mut cur, &engine::general_purpose::STANDARD);
    decoder.read_to_end(buf).unwrap();
}

async fn play_handler(State(app_state): State<Arc<AppState>>) -> impl IntoResponse {
    let text = r#"Hello. You are hearing this
    from a Twilio Play verb. These bytes are
    MP3 encoded."#;
    let tts = app_state
        .get_google_tts(text, AudioConfigAudioEncoding::MP3)
        .await;
    let mut body = Vec::new();
    b64_decode_to_buf(tts, &mut body);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "audio/mpeg".parse().unwrap());

    (headers, body)
}

#[allow(dead_code)]
async fn twiml_start_connect(
    Host(host): Host,
    State(app_state): State<Arc<AppState>>,
    body: String,
) -> impl IntoResponse {
    debug!(body=%body, "start request body");
    let payload = serde_urlencoded::from_str::<TwilioConnectPayload>(&body);
    if let Err(e) = payload {
        error!(error=%e, "failed to deserialize Twilio connect payload");
        return (
            axum::http::StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            "Bad request".to_string(),
        );
    }
    let payload = payload.unwrap();
    {
        let mut streams = app_state.streams.lock().unwrap();
        streams.insert(payload.call_sid.clone(), payload);
    }

    let say_action = SayAction {
        text: "Hi. How can I help you?".to_string(),
        ..Default::default()
    };
    let url = format!("wss://{}/connect", host);
    let stream_action = StreamAction {
        url,
        track: Some(StreamTrack::Inbound),
        ..Default::default()
    };
    let connect_action = ConnectAction {
        connection: Connection::Stream(stream_action),
    };
    let response = Response {
        actions: vec![
            ResponseAction::Say(say_action),
            ResponseAction::Connect(connect_action),
        ],
    };

    let twiml = wrap_twiml(xmlserde::xml_serialize(response));
    debug!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (axum::http::StatusCode::OK, headers, twiml)
}

#[allow(dead_code)]
async fn twiml_start_play(Host(host): Host) -> impl IntoResponse {
    let url = format!("https://{}/play", host);
    let play_action = PlayAction {
        url,
        ..Default::default()
    };
    let response = Response {
        actions: vec![ResponseAction::Play(play_action)],
    };
    let twiml = wrap_twiml(xmlserde::xml_serialize(response));
    debug!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (headers, twiml)
}

async fn gcs_client() -> TextService {
    let gcs_credentials = env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .expect("No google application credentials location set.");
    let conn = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_or_http()
        .enable_http2()
        .build();
    let tls_client = hyper::Client::builder().build(conn);
    let service_account_key = yup_oauth2::read_service_account_key(&gcs_credentials)
        .await
        .expect("failed to read GCS account key");
    let gcs_authenticator = yup_oauth2::ServiceAccountAuthenticator::builder(service_account_key)
        .hyper_client(tls_client.clone())
        .persist_tokens_to_disk("tokencache.json")
        .build()
        .await
        .expect("ServiceAccount authenticator failed.");
    TextService::new(tls_client, Arc::new(gcs_authenticator))
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

    let console_api_key = env::var("CONSOLE_API_KEY").unwrap();
    let openai_api_key = env::var("OPENAI_API_KEY").unwrap();
    let twilio_account_sid = env::var("TWILIO_ACCOUNT_SID").unwrap();
    let twilio_auth_token = env::var("TWILIO_AUTH_TOKEN").unwrap();
    let gcs_client = gcs_client().await;
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
        .route("/connect", get(ws_handler))
        .route("/play", get(play_handler))
        // Choose whether to use a Play verb or Connect verb in start Twiml.
        // .route("/twilio/twiml/start", post(twiml_start_play))
        .route("/twilio/twiml/start", post(twiml_start_connect))
        .route("/", get(|| async { "Hello, World!" }))
        .with_state(app_state);

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await
        .unwrap();
}
