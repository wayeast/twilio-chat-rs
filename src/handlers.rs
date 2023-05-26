use crate::consts::APP_GREETING;
use crate::error::{handle_error, AppError};
use crate::tasks::{manage_conversation, stream_twilio_audio_to_deepgram};
use crate::texttospeech_v1_types::AudioConfigAudioEncoding;
use crate::twilio_types::{
    wrap_twiml, ConnectAction, Connection, PlayAction, Response, ResponseAction, SayAction,
    StartMeta, StreamAction, StreamTrack, TwilioConnectPayload, TwilioMessage,
};
use crate::types::AppState;
use crate::utils::b64_decode_to_buf;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Host, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, client::IntoClientRequest},
    MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, trace};

/// Capture the Twilio Start media message from the beginning of a Twilio websocket stream for the
/// stream id.
async fn get_twilio_start_meta(
    twilio_stream: &mut SplitStream<WebSocket>,
) -> Result<StartMeta, AppError> {
    loop {
        match twilio_stream.next().await {
            Some(msg) => match msg {
                Ok(Message::Text(json)) => match serde_json::from_str(&json) {
                    Ok(message) => match message {
                        TwilioMessage::Connected { protocol, version } => {
                            trace!("Got connected message with {protocol} and {version}");
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
    trace!("Connecting to DG");
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

pub async fn ws_handler(
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

pub async fn play_handler(State(app_state): State<Arc<AppState>>) -> impl IntoResponse {
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
pub async fn twiml_start_connect(
    Host(host): Host,
    State(app_state): State<Arc<AppState>>,
    body: String,
) -> impl IntoResponse {
    trace!(body=%body, "start request body");
    let payload = serde_urlencoded::from_str::<TwilioConnectPayload>(&body);
    if let Err(e) = payload {
        error!(error=%e, "failed to deserialize Twilio connect payload");
        return (
            StatusCode::BAD_REQUEST,
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
        text: APP_GREETING.to_string(),
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
    trace!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (StatusCode::OK, headers, twiml)
}

#[allow(dead_code)]
pub async fn twiml_start_play(Host(host): Host) -> impl IntoResponse {
    let url = format!("https://{}/play", host);
    let play_action = PlayAction {
        url,
        ..Default::default()
    };
    let response = Response {
        actions: vec![ResponseAction::Play(play_action)],
    };
    let twiml = wrap_twiml(xmlserde::xml_serialize(response));
    trace!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (headers, twiml)
}
