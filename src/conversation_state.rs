use crate::consts::{ASCII_CLAUSE_ENDINGS, POLITENESS_DELAY_MILLIS};
use crate::deepgram_types::{StreamMessage, StreamingResponse};
use crate::error::AppError;
use crate::openai_types::{
    OpenAIBatchResponse, OpenAIMessage, OpenAIPayload, OpenAIStreamResponse, StreamDelta,
};
use crate::texttospeech_v1_types::AudioConfigAudioEncoding;
use crate::twilio_types::TwilioOutbound;
use crate::types::{
    AppState, ConversationSignal, ConversationSummary, ConversationTurn, CurrentBotAction,
};
use crate::utils::google2twilio;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{
    sink::SinkExt,
    stream::{SplitSink, StreamExt},
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite;
use tracing::{debug, error, warn};

pub struct ConversationState {
    pub pause_msg: TwilioOutbound,
    /// Place to store spawned task handles for duration of caller interaction
    pub join_handles: Vec<task::JoinHandle<Result<(), AppError>>>,
    /// How we track whether we are currently speaking or listening
    pub current_bot_action: CurrentBotAction,
    /// Data structure to store the running context of our conversation
    pub conversation_turns: Vec<Arc<ConversationTurn>>,
    /// Structure to measure how long since the caller has paused
    pub last_talking: Instant,
    /// The stream id collected from the Twilio Media message stream
    pub twilio_stream_id: String,
    /// Task by which, when talking, we are sending Media messages to Twilio.  It is decoupled from
    /// the main `twilio_sink`, and made Option, in order to facilitate stopping talking when the
    /// caller cuts us off, but still collect a complete response from OpenAI for complete context.
    pub current_twilio_media_message_stream: Option<task::JoinHandle<Result<(), AppError>>>,
    /// Sink by which we send TwilioOutbound messages to the main `twilio_sink` manager
    pub twilio_outbound_sink: mpsc::Sender<TwilioOutbound>,
    pub app_state: Arc<AppState>,
}

impl ConversationState {
    pub async fn new(
        twilio_stream_id: String,
        twilio_sink: SplitSink<WebSocket, Message>,
        app_state: Arc<AppState>,
    ) -> Self {
        // TODO: move base64-encoded tts to static files
        let pause_tts = app_state
            .get_google_tts("Got it.", AudioConfigAudioEncoding::MULAW)
            .await;
        let pause_msg = google2twilio(pause_tts, &twilio_stream_id).await;
        let mut join_handles: Vec<task::JoinHandle<Result<(), AppError>>> = vec![];
        let (twilio_outbound_sink, twilio_outbound_stream) = mpsc::channel(1);
        let twilio_ws_message_handle =
            tokio::spawn(send_twilio_ws_messages(twilio_outbound_stream, twilio_sink));
        join_handles.push(twilio_ws_message_handle);
        let current_bot_action = CurrentBotAction::Talking;
        let conversation_turns: Vec<Arc<ConversationTurn>> = vec![];
        let last_talking = Instant::now();
        let current_twilio_media_message_stream: Option<task::JoinHandle<Result<(), AppError>>> =
            None;

        Self {
            pause_msg,
            join_handles,
            current_bot_action,
            conversation_turns,
            last_talking,
            twilio_stream_id,
            current_twilio_media_message_stream,
            twilio_outbound_sink,
            app_state,
        }
    }

    /// Aggregate conversation up till now; to be sent to OpenAI as context for continuation prompt
    pub async fn get_conversation_context(&self) -> Vec<OpenAIMessage> {
        let mut conversation: Vec<OpenAIMessage> = vec![OpenAIMessage {
            role: "system".to_string(),
            content: "You are a friendly and helpful assistant.".to_string(),
        }];
        for turn in &self.conversation_turns {
            let caller_side = turn.caller_side.read().await;
            if !caller_side.is_empty() {
                conversation.push(OpenAIMessage {
                    role: "user".to_string(),
                    content: caller_side.to_string(),
                });
            }
            let bot_side = turn.bot_side.read().await;
            if !bot_side.is_empty() {
                conversation.push(OpenAIMessage {
                    role: "assistant".to_string(),
                    content: bot_side.to_string(),
                });
            }
        }

        conversation
    }

    /// Logic that controls the bot's action when we detect that the user is listening while we are
    /// listening
    async fn caller_listens_while_we_are_listening(&mut self) -> Result<(), AppError> {
        match &self.current_bot_action {
            CurrentBotAction::Listening(turn_sink) => {
                let since_last_talking = self.last_talking.elapsed().as_millis();
                // If not enough time has passed to give the caller polite space, then do
                // nothing.
                if since_last_talking < POLITENESS_DELAY_MILLIS {
                    return Ok(());
                }
                // Otherwise, acknowledge the caller...
                self.twilio_outbound_sink
                    .send(self.pause_msg.clone())
                    .await
                    .map_err(|e| {
                        error!(error=%e, "failed to send clear message to twilio");
                        AppError("Error sending twilio outbound message")
                    })?;
                // and prepare a response
                turn_sink.send(ConversationSignal::Go).await.map_err(|e| {
                    error!(error=%e, "failed to send message to turn handler");
                    AppError("Failed to stop message to turn manager.")
                })?;
                self.current_bot_action = CurrentBotAction::Talking;
                let current_conversation = self.get_conversation_context().await;
                debug!(conversation=?current_conversation, "sending conversation to openai");
                let (media_sink, media_stream) = mpsc::channel::<String>(1);
                let this_turn = self.conversation_turns.len() - 1;
                let handle = tokio::spawn(bot_side_manager(
                    current_conversation,
                    self.conversation_turns[this_turn].clone(),
                    media_sink,
                    self.app_state.clone(),
                ));
                self.join_handles.push(handle);
                let media_message_stream_handle =
                    tokio::spawn(send_twilio_media_messages_until_aborted(
                        media_stream,
                        self.twilio_stream_id.clone(),
                        self.twilio_outbound_sink.clone(),
                    ));
                self.current_twilio_media_message_stream = Some(media_message_stream_handle);

                Ok(())
            }
            CurrentBotAction::Talking => Err(AppError("Conversation state out of whack")),
        }
    }

    /// Logic that controls the bot's action when we detect that the user is talking while we
    /// are listening
    async fn caller_speaks_while_we_are_listening(
        &mut self,
        streaming_response: StreamingResponse,
    ) -> Result<(), AppError> {
        match &self.current_bot_action {
            CurrentBotAction::Listening(turn_sink) => {
                self.last_talking = Instant::now();
                turn_sink
                    .send(ConversationSignal::StreamingResponse(streaming_response))
                    .await
                    .map_err(|e| {
                        error!(error=%e, "failed to send transcript to turn manager");
                        AppError("Failed to send transcript to turn manager.")
                    })
            }
            CurrentBotAction::Talking => Err(AppError("Conversation state out of whack")),
        }
    }

    /// Logic that controls the bot's action when we detect that the user starts talking while we
    /// are talking
    async fn caller_speaks_while_we_are_talking(
        &mut self,
        streaming_response: StreamingResponse,
    ) -> Result<(), AppError> {
        // 1. send twilio clear message and shut up
        if self.current_twilio_media_message_stream.is_some() {
            let outbound_clear = TwilioOutbound::Clear {
                stream_sid: self.twilio_stream_id.clone(),
            };
            self.twilio_outbound_sink
                .send(outbound_clear)
                .await
                .map_err(|e| {
                    error!(error=%e, "failed to send clear message to twilio");
                    AppError("Error sending twilio outbound message")
                })?;
            let handle = self.current_twilio_media_message_stream.as_ref().unwrap();
            handle.abort();
            self.current_twilio_media_message_stream = None;
        }
        // 2. change state to listening and push new turn to turns vec
        self.last_talking = Instant::now();
        let (turn_sink, turn_stream) = mpsc::channel::<ConversationSignal>(1);
        turn_sink
            .send(ConversationSignal::StreamingResponse(streaming_response))
            .await
            .map_err(|e| {
                error!(error=%e, "failed to send transcript via channel");
                AppError("Failed to send transcript to turn manager.")
            })?;
        self.current_bot_action = CurrentBotAction::Listening(turn_sink);
        self.conversation_turns
            .push(Arc::new(ConversationTurn::new()));
        // 3. spawn turn manager -> handler
        let this_turn = self.conversation_turns.len() - 1;
        let handle = tokio::spawn(caller_side_manager(
            self.conversation_turns[this_turn].clone(),
            turn_stream,
        ));
        // 4. push handler to handlers vec
        self.join_handles.push(handle);

        Ok(())
    }

    /// Decisions about next actions are a function of our current state (talking/listening) and
    /// what we get back from DG
    pub async fn handle_dg_message(
        &mut self,
        dg_msg: tungstenite::Message,
    ) -> Result<(), AppError> {
        match dg_msg {
            tungstenite::Message::Text(msg) => {
                let stream_message = serde_json::from_str::<StreamMessage>(&msg).map_err(|e| {
                    error!(msg=%msg, error=%e, "failed to deserialize DG stream message");
                    AppError("deserialization error")
                })?;
                match stream_message {
                    StreamMessage::StreamingResponse(streaming_response) => {
                        let transcript = &streaming_response.channel.alternatives[0].transcript;
                        debug!(transcript=%transcript, "got dg text message");
                        match (&self.current_bot_action, transcript.is_empty()) {
                            (CurrentBotAction::Listening(_), true) => {
                                self.caller_listens_while_we_are_listening().await
                            }
                            (CurrentBotAction::Listening(_), false) => {
                                self.caller_speaks_while_we_are_listening(streaming_response)
                                    .await
                            }
                            (CurrentBotAction::Talking, true) => Ok(()),
                            (CurrentBotAction::Talking, false) => {
                                self.caller_speaks_while_we_are_talking(streaming_response)
                                    .await
                            }
                        }
                    }
                    StreamMessage::StreamingMeta(_) => Ok(()),
                }
            }
            _ => Ok(()),
        }
    }

    pub async fn get_conversation_summaries(
        &mut self,
    ) -> Result<Vec<ConversationSummary>, AppError> {
        debug!("in get_conversation_summaries");
        // wait up to 5 seconds for all unfinished tasks to complete
        let mut tries = 0;
        while tries < 5 {
            let ok = self.join_handles.iter().all(|h| h.is_finished());
            if ok {
                break;
            } else {
                tries += 1;
                sleep(Duration::from_millis(1_000)).await;
            }
        }

        debug!("done waiting on tasks; getting summaries");
        let mut summaries: Vec<ConversationSummary> = vec![];
        for turn in &self.conversation_turns {
            let bot_side = turn.bot_side.read().await;
            // TODO: check that we're not sending empty strings or junk.  Perhaps have chatgpt tell
            // us if what we're sending is worth summarizing???
            let summary = self.get_conversation_summary_for_bot_side(&bot_side).await;
            if summary.is_err() {
                continue;
            }
            let summary = summary.unwrap();
            debug!(summary=?summary, "summary");
            summaries.push(summary);
        }

        Ok(summaries)
    }

    async fn get_conversation_summary_for_bot_side(
        &self,
        bot_side: &str,
    ) -> Result<ConversationSummary, AppError> {
        let url = "https://api.openai.com/v1/chat/completions";
        let key = self.app_state.openai_api_key.as_str();
        let topic = {
            let prompt = vec![
                OpenAIMessage {
                    role: "system".to_string(),
                    content: "You are a knowledgeable editor.".to_string(),
                },
                OpenAIMessage {
                    role: "assistant".to_string(),
                    content: format!("In one to three words, please give the topic of the following:\n\n{bot_side}"),
                },
            ];
            let payload = OpenAIPayload {
                model: "gpt-3.5-turbo".to_string(),
                messages: prompt,
                ..Default::default()
            };
            let resp = self
                .app_state
                .http_client
                .post(url)
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
                .json(&payload)
                .send()
                .await
                .map_err(|e| {
                    error!(error=%e, "failed to send request to OpenAI");
                    AppError("Failed to send request to OpenAI")
                })?;
            let resp = resp.json::<OpenAIBatchResponse>().await.map_err(|e| {
                error!(error=%e, "failed to deserialize openai topic response");
                AppError("deserialize")
            })?;
            resp.choices[0].message.content.to_string()
        };
        let summary = {
            let prompt = vec![
                OpenAIMessage {
                    role: "system".to_string(),
                    content: "You are a knowledgeable editor.".to_string(),
                },
                OpenAIMessage {
                    role: "assistant".to_string(),
                    content: format!("In fewer than twenty words, please give a summary of the following:\n\n{bot_side}"),
                },
            ];
            let payload = OpenAIPayload {
                model: "gpt-3.5-turbo".to_string(),
                messages: prompt,
                ..Default::default()
            };
            let resp = self
                .app_state
                .http_client
                .post(url)
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
                .json(&payload)
                .send()
                .await
                .map_err(|e| {
                    error!(error=%e, "failed to send request to OpenAI");
                    AppError("Failed to send request to OpenAI")
                })?;
            let resp = resp.json::<OpenAIBatchResponse>().await.map_err(|e| {
                error!(error=%e, "failed to deserialize openai topic response");
                AppError("deserialize")
            })?;
            resp.choices[0].message.content.to_string()
        };

        Ok(ConversationSummary { topic, summary })
    }
}

/// Task that is the funnel of all TwilioOutbound messages going to Twilio.
async fn send_twilio_ws_messages(
    mut twilio_outbound_stream: mpsc::Receiver<TwilioOutbound>,
    mut twilio_ws_sink: SplitSink<WebSocket, Message>,
) -> Result<(), AppError> {
    while let Some(twilio_outbound) = twilio_outbound_stream.recv().await {
        let json = serde_json::to_string(&twilio_outbound).map_err(|e| {
            error!(error=%e, "failed to serialize Twilio outbound");
            AppError("Twilio message serialization error")
        })?;
        let message = Message::Text(json);
        twilio_ws_sink.send(message).await.map_err(|e| {
            error!(error=%e, "failed to send message to Twilio");
            AppError("Failed to send message to Twilio")
        })?;
    }

    Ok(())
}

async fn bot_side_manager(
    current_conversation: Vec<OpenAIMessage>,
    turn: Arc<ConversationTurn>,
    media_sink: mpsc::Sender<String>,
    app_state: Arc<AppState>,
) -> Result<(), AppError> {
    let url = "https://api.openai.com/v1/chat/completions";
    let payload = OpenAIPayload {
        model: "gpt-3.5-turbo".to_string(),
        messages: current_conversation,
        stream: Some(true),
        ..Default::default()
    };
    let key = app_state.openai_api_key.as_str();
    let resp = app_state
        .http_client
        .post(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            error!(error=%e, "failed to send request to OpenAI");
            AppError("Failed to send request to OpenAI")
        })?;
    let mut response_stream = resp.bytes_stream();
    let mut chat_buffer = String::new();
    while let Some(chunk) = response_stream.next().await {
        match chunk {
            Ok(chunk) => {
                // chunk looks like `data: { ..OpenAIStreamResponse }\n\ndata: {
                // ..OpenAIStreamResponse }\n\n`
                // If I were cool, I would implement a deserializer to turn this format into a
                // Vec<OpenAIStreamResponse>, but I'm lazy for this iteration and just want
                // something quick and dirty that works.
                let response_string = String::from_utf8(chunk.to_vec()).map_err(|e| {
                    error!(error=%e, "failed to parse string from OpenAI streamed chunk");
                    AppError("Error deserializing OpenAI streamed choice")
                })?;
                debug!(response=%response_string, "got openai string");
                let responses: Vec<OpenAIStreamResponse> = response_string
                    .lines()
                    .filter_map(|l| l.strip_prefix("data: "))
                    .filter_map(|l| serde_json::from_str::<OpenAIStreamResponse>(l).ok())
                    .collect();
                debug!(responses=?responses, "got openai responses");
                for response in responses {
                    match &response.choices[0].delta {
                        StreamDelta::Role { role } => {
                            if role != "assistant" {
                                warn!(role=%role, "got openai delta with unexpected role");
                            }
                        }
                        StreamDelta::Content { content } => {
                            debug!(content=%content, "handling Content response");
                            extend_bot_side(&turn, content).await;
                            chat_buffer.push_str(content);
                            if ASCII_CLAUSE_ENDINGS.contains(&content.as_str()) {
                                let tts_text = chat_buffer.clone();
                                chat_buffer.clear();
                                let payload = app_state
                                    .get_google_tts(&tts_text, AudioConfigAudioEncoding::MULAW)
                                    .await;
                                debug!("got google tts payload");

                                let res = media_sink.try_send(payload);
                                if let Err(mpsc::error::TrySendError::Full(_)) = res {
                                    // we only care about errors arising from full channels
                                    // errors from closed channels aren't really errors, since they (probably)
                                    // just mean that we've stopped talking to allow the caller to speak.
                                    error!("encountered full tts payload channel");
                                    return Err(AppError(
                                        "Error sending tts payload through channel",
                                    ));
                                }
                            }
                        }
                        StreamDelta::Empty {} => {
                            // This should signal the end of the stream.  May also check
                            // `finish_reason`
                            let tts_text = chat_buffer.clone();
                            chat_buffer.clear();
                            let payload = app_state
                                .get_google_tts(&tts_text, AudioConfigAudioEncoding::MULAW)
                                .await;
                            debug!("got google tts payload");

                            let res = media_sink.try_send(payload);
                            if let Err(mpsc::error::TrySendError::Full(_)) = res {
                                // we only care about errors arising from full channels
                                // errors from closed channels aren't really errors, since they (probably)
                                // just mean that we've stopped talking to allow the caller to speak.
                                error!("encountered full tts payload channel");
                                return Err(AppError("Error sending tts payload through channel"));
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!(error=%e, "failed to read bytes from OpenAI response");
                return Err(AppError(
                    "Error getting bytes chunk from OpenAI response stream",
                ));
            }
        }
    }

    Ok(())
}

async fn extend_bot_side(turn: &Arc<ConversationTurn>, chunk: &str) {
    let mut buf = turn.bot_side.write().await;
    buf.push_str(chunk);
}

async fn send_twilio_media_messages_until_aborted(
    mut media_stream: mpsc::Receiver<String>,
    stream_id: String,
    twilio_outbound_sink: mpsc::Sender<TwilioOutbound>,
) -> Result<(), AppError> {
    while let Some(encoded_tts) = media_stream.recv().await {
        debug!("sending tts to twilio");
        let twilio_media_msg = google2twilio(encoded_tts, &stream_id).await;
        twilio_outbound_sink
            .send(twilio_media_msg)
            .await
            .map_err(|e| {
                error!(error=%e, "failed to send twilio outbound message through channel");
                AppError("Error sending twilio outbound through channel")
            })?;
    }

    Ok(())
}

async fn extend_caller_side(turn: &Arc<ConversationTurn>, chunk: &str) {
    let mut buf = turn.caller_side.write().await;
    if !buf.is_empty() {
        buf.push(' ');
    }
    buf.push_str(chunk);
}

async fn caller_side_manager(
    turn: Arc<ConversationTurn>,
    mut turn_stream: mpsc::Receiver<ConversationSignal>,
) -> Result<(), AppError> {
    while let Some(thang) = turn_stream.recv().await {
        match thang {
            ConversationSignal::Go => break,
            ConversationSignal::StreamingResponse(streaming_response) => {
                if streaming_response.is_final {
                    extend_caller_side(
                        &turn,
                        &streaming_response.channel.alternatives[0].transcript,
                    )
                    .await
                }
            }
        }
    }

    Ok(())
}
