use crate::conversation_state::ConversationState;
use crate::error::AppError;
use crate::twilio_types::{StartMeta, TwilioConnectPayload, TwilioMessage};
use crate::types::AppState;
use crate::utils::b64_decode_to_buf;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_tungstenite::{tungstenite, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

/// Task that streams all Twilio media messages with encoded caller-side audio to Deepgram.
pub async fn stream_twilio_audio_to_deepgram(
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

/// Task that handles the back and forth of the bot's interaction with a caller
pub async fn manage_conversation(
    mut dg_stream: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    twilio_sink: SplitSink<WebSocket, Message>,
    twilio_start_meta: StartMeta,
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
