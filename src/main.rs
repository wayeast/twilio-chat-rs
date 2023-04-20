mod deepgram_types;
#[allow(clippy::all)]
mod texttospeech_v1_types;
mod twilio_types;

use crate::twilio_types::*;

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
use std::collections::VecDeque;
use std::env;
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::time::Instant;
use texttospeech_v1_types::{
    AudioConfig, AudioConfigAudioEncoding, SynthesisInput, SynthesizeSpeechRequest, TextService,
    TextSynthesizeParams, VoiceSelectionParams, VoiceSelectionParamsSsmlGender,
};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(app_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    println!("ws handler!!!");
    ws.on_upgrade(move |socket| socket_handler(socket, app_state))
}

async fn socket_handler(socket: WebSocket, app_state: Arc<AppState>) {
    println!("created websocket; splitting streams");
    let (sender, receiver) = socket.split();
    let (sid_sink, sid_src) = oneshot::channel();

    // connect to Deepgram
    println!("Connecting to DG");
    // not sure if endpointing on/off makes much difference???
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
    let (dg_sender, dg_receiver) = ws_stream.split();

    println!("handling ws streams");
    let _res = tokio::try_join!(
        hear_stuff(receiver, sid_sink, dg_sender),
        say_something(sender, sid_src, dg_receiver, app_state)
    );
}

async fn hear_stuff(
    mut receiver: SplitStream<WebSocket>,
    sid_sink: oneshot::Sender<String>,
    mut dg_sender: SplitSink<
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::Message,
    >,
) -> Result<(), ()> {
    // TODO: instead of this weirdness, wrap sid_sink in something (Option?) like in
    // buttercup::handlers::twilio::handle_from_twilio_ws
    let mut stream_sid = String::new();
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(json)) => match serde_json::from_str(&json) {
                Ok(message) => match message {
                    TwilioMessage::Connected { protocol, version } => {
                        println!("Got connected message with {protocol} and {version}");
                    }
                    TwilioMessage::Start {
                        start:
                            StartMeta {
                                stream_sid: meta_sid,
                                ..
                            },
                        stream_sid: msg_sid,
                        ..
                    } => {
                        println!("Got start message with stream sid's {meta_sid} and {msg_sid}");
                        stream_sid = meta_sid;
                        break;
                    }
                    _ => {
                        println!("Hm, got media (or stop, or mark) messages before we were expecting them.");
                    }
                },
                Err(e) => {
                    println!("Error deserializing twilio text message: {e}");
                }
            },
            Ok(_) => {
                println!("Got an unsupported message type from Twilio.");
            }
            Err(e) => {
                println!("Error getting message from Twilio: {e}");
            }
        }
    }
    if !stream_sid.is_empty() {
        sid_sink.send(stream_sid).unwrap();
    }

    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(json)) => {
                match serde_json::from_str(&json) {
                    Ok(message) => match message {
                        TwilioMessage::Media { media, .. } => {
                            let mut chunk = Vec::new();
                            b64_decode_to_buf(media.payload, &mut chunk);
                            dg_sender
                                .send(tokio_tungstenite::tungstenite::Message::Binary(chunk))
                                .await
                                .unwrap();
                            // base64 bytes = media.payload
                            // println!("Got media message {sequence_number} for {stream_sid}");
                        }
                        TwilioMessage::Stop {
                            sequence_number, ..
                        } => {
                            println!("Got stop message {sequence_number}");
                        }
                        TwilioMessage::Mark { .. } => {
                            println!("Got mark message.");
                        }
                        _ => {
                            println!("We should not be getting Connected or Start messages now!");
                        }
                    },
                    Err(e) => println!("Failed to parse incoming text message: {e}"),
                }
            }
            Ok(_) => {
                println!("Got an unsupported message type from Twilio.");
            }
            Err(e) => {
                println!("Failed to receive message from Twilio stream: {e}");
            }
        }
    }

    Ok(())
}

async fn twilio_stop_talking(stream_sid: &str, twilio_sink: &mut SplitSink<WebSocket, Message>) {
    let outbound_clear = TwilioOutbound::Clear {
        stream_sid: stream_sid.to_string(),
    };
    let json = serde_json::to_string(&outbound_clear).unwrap();
    let message = Message::Text(json.clone());
    twilio_sink.send(message).await.unwrap();
}

const GOOGLE_WAV_HEADER_SZ: usize = 58;
async fn say_something(
    mut sender: SplitSink<WebSocket, Message>,
    sid_src: oneshot::Receiver<String>,
    mut dg_receiver: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    app_state: Arc<AppState>,
) -> Result<(), ()> {
    // Get stream sid from incoming ws channel
    let res = sid_src.await;
    if res.is_err() {
        println!("sid_sink dropped");
        return Ok(());
    }
    let stream_sid = res.unwrap();
    println!("say_something got stream sid {stream_sid}");

    const POLITENESS_DELAY_MILLIS: u128 = 1_500;
    let mut last_talking = Instant::now();
    let mut media_buff = VecDeque::new();
    while let Some(Ok(msg)) = dg_receiver.next().await {
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(msg) => {
                let streaming_response =
                    serde_json::from_str::<deepgram_types::StreamingResponse>(&msg).unwrap();
                let transcript = &streaming_response.channel.alternatives[0].transcript;
                if !transcript.is_empty() {
                    // caller has either been talking and continues to talk, or has just started talking;
                    // We first need to tell Twilio to stop talking...
                    println!("Got non-empty transcript result from DG!");
                    twilio_stop_talking(&stream_sid, &mut sender).await;
                    if streaming_response.is_final {
                        println!("    ...non-empty transcript result is final!");
                        let payload = app_state
                            .get_google_tts(transcript, AudioConfigAudioEncoding::MULAW)
                            .await;
                        let twilio_media_msg = google2twilio(payload, &stream_sid).await;
                        media_buff.push_back(twilio_media_msg);
                    }
                    last_talking = Instant::now();
                } else {
                    // caller has been talking but stopped; if, politeness delay has transpired,
                    // try to respond.
                    println!("Got empty transcript result from DG!");
                    let since_last_talking = last_talking.elapsed().as_millis();
                    if since_last_talking > POLITENESS_DELAY_MILLIS {
                        while let Some(msg) = media_buff.pop_front() {
                            let json = serde_json::to_string(&msg).unwrap();
                            let message = Message::Text(json.clone());
                            sender.send(message).await.unwrap();
                        }
                    } else {
                        // we've detected the caller stopped talking, but not enough time has
                        // elapsed for us to cut in (politely)
                    }
                }
            }
            _ => println!("Got unsupported message type from Deepgram."),
        }
    }

    Ok(())
}

async fn google2twilio(google_tts: String, stream_sid: &str) -> TwilioOutbound {
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
async fn twiml_start_connect(Host(host): Host) -> impl IntoResponse {
    let say_action = SayAction {
        text: "Hi. I'm your Twilio host. Welcome!".to_string(),
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
    println!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (headers, twiml)
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
    println!("twiml: '{}'", twiml);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    (headers, twiml)
}

struct AppState {
    console_api_key: String,
    gcs_client: TextService,
}

impl AppState {
    async fn get_google_tts(&self, text: &str, encoding: AudioConfigAudioEncoding) -> String {
        let params = TextSynthesizeParams::default();
        let audio_config = AudioConfig {
            audio_encoding: Some(encoding),
            sample_rate_hertz: Some(8_000),
            ..Default::default()
        };
        let input = SynthesisInput {
            text: Some(text.to_string()),
            ..Default::default()
        };
        let voice = VoiceSelectionParams {
            language_code: Some("en-US".to_string()),
            name: Some("en-US-Standard-E".to_string()),
            ssml_gender: Some(VoiceSelectionParamsSsmlGender::FEMALE),
            ..Default::default()
        };
        let speech_request = SynthesizeSpeechRequest {
            audio_config: Some(audio_config),
            input: Some(input),
            voice: Some(voice),
        };
        let synthesize_response = self
            .gcs_client
            .synthesize(&params, &speech_request)
            .await
            .unwrap();

        synthesize_response.audio_content.unwrap()
    }
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

    let console_api_key = env::var("CONSOLE_API_KEY").unwrap();
    let gcs_client = gcs_client().await;

    let app_state = Arc::new(AppState {
        console_api_key,
        gcs_client,
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
