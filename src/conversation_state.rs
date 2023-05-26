use crate::consts::{ASCII_CLAUSE_ENDINGS, POLITENESS_DELAY_MILLIS};
use crate::db_types::{Call, Caller};
use crate::deepgram_types::{StreamMessage, StreamingResponse};
use crate::error::AppError;
use crate::openai_types::{
    OpenAIBatchResponse, OpenAIMessage, OpenAIPayload, OpenAIStreamResponse, StreamDelta,
};
use crate::texttospeech_v1_types::AudioConfigAudioEncoding;
use crate::twilio_types::{TwilioConnectPayload, TwilioOutbound};
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
use uuid::Uuid;

pub struct ConversationState {
    pub pause_msg: TwilioOutbound,
    /// Place to store spawned task handles for duration of caller interaction
    pub join_handles: Vec<task::JoinHandle<Result<(), AppError>>>,
    /// How we track whether we are currently speaking or listening
    pub current_bot_action: CurrentBotAction,
    /// Data structure to store the running context of our conversation
    pub conversation_turns: Vec<Arc<ConversationTurn>>,
    pub conversation_summaries: Option<Vec<ConversationSummary>>,
    /// Structure to measure how long since the caller has paused
    pub last_talking: Instant,
    /// The stream id collected from the Twilio Media message stream
    pub twilio_stream_id: String,
    /// Connect payload from the initial Twilio connection
    pub twilio_connect_payload: TwilioConnectPayload,
    /// The request id for our Deepgram stream; we won't know this until the end of the stream
    pub dg_request_id: Option<Uuid>,
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
        twilio_connect_payload: TwilioConnectPayload,
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
        let dg_request_id: Option<Uuid> = None;
        let conversation_summaries: Option<Vec<ConversationSummary>> = None;

        Self {
            pause_msg,
            join_handles,
            current_bot_action,
            conversation_turns,
            conversation_summaries,
            last_talking,
            twilio_stream_id,
            twilio_connect_payload,
            dg_request_id,
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
                    StreamMessage::StreamingMeta(streaming_meta) => {
                        self.dg_request_id = Some(streaming_meta.request_id);
                        Ok(())
                    }
                }
            }
            _ => Ok(()),
        }
    }

    pub async fn get_conversation_summaries(
        &mut self,
    ) -> Result<&Vec<ConversationSummary>, AppError> {
        debug!("in get_conversation_summaries");
        if self.conversation_summaries.is_none() {
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
            for (idx, turn) in self.conversation_turns.iter().enumerate() {
                let bot_side = turn.bot_side.read().await;
                // TODO: check that we're not sending empty strings or junk.  Perhaps have chatgpt tell
                // us if what we're sending is worth summarizing???
                let summary = self
                    .get_conversation_summary_for_bot_side(idx, &bot_side)
                    .await;
                if summary.is_err() {
                    continue;
                }
                let summary = summary.unwrap();
                debug!(summary=?summary, "summary");
                summaries.push(summary);
            }
            self.conversation_summaries = Some(summaries);
        }

        self.conversation_summaries
            .as_ref()
            .ok_or(AppError("Failed to access conversation summaries"))
    }

    async fn get_conversation_summary_for_bot_side(
        &self,
        idx: usize,
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

        Ok(ConversationSummary {
            idx,
            topic,
            summary,
        })
    }

    pub async fn insert_db_row(&self) -> Result<(), AppError> {
        let pool = &self.app_state.db_pool;
        let caller_phone = self.twilio_connect_payload.from.as_str();
        let caller = sqlx::query_as!(
            Caller,
            "
            select *
            from callers
            where phone = $1
            ",
            caller_phone,
        )
        .fetch_one(pool)
        .await;
        let caller_id = if let Ok(caller) = caller {
            Ok(caller.id)
        } else {
            sqlx::query_as!(
                Caller,
                "
                insert into callers (
                  phone,
                  city,
                  state,
                  country,
                  zip
                ) values (
                  $1,
                  $2,
                  $3,
                  $4,
                  $5
                )
                returning *
                ",
                self.twilio_connect_payload.from,
                self.twilio_connect_payload.from_city,
                self.twilio_connect_payload.from_state,
                self.twilio_connect_payload.from_country,
                self.twilio_connect_payload.from_zip,
            )
            .fetch_one(pool)
            .await
            .map_err(|e| {
                error!(error=%e, "failed to insert caller row");
                AppError("db error")
            })
            .map(|c| c.id)
        }?;

        if self.dg_request_id.is_none() {
            return Err(AppError("dg request id unavailable"));
        }
        let mut tx = pool.begin().await.map_err(|e| {
            error!(error=%e, "failed to begin transaction");
            AppError("db error")
        })?;
        let call = sqlx::query_as!(
            Call,
            "
            insert into calls (
              caller,
              twilio_call_sid,
              twilio_stream_sid,
              dg_request_id
            )
            values (
              $1,
              $2,
              $3,
              $4
            )
            returning *
            ",
            caller_id,
            self.twilio_connect_payload.call_sid,
            self.twilio_stream_id,
            self.dg_request_id.unwrap(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            error!(error=%e, "failed to insert call row");
            AppError("db error")
        })?;

        let call_ids: Vec<i32> = vec![call.id; self.conversation_turns.len()];
        let call_idxs: Vec<i32> = (0..self.conversation_turns.len())
            .map(|v| v as i32)
            .collect();
        let mut caller_sides: Vec<String> = vec![];
        for t in &self.conversation_turns {
            let s = t.caller_side.read().await;
            caller_sides.push(s.to_string())
        }
        let mut bot_sides: Vec<String> = vec![];
        for t in &self.conversation_turns {
            let s = t.bot_side.read().await;
            bot_sides.push(s.to_string())
        }
        let topics: Vec<String> = if let Some(conversation_summaries) = &self.conversation_summaries
        {
            let mut res: Vec<String> = vec!["".to_string(); self.conversation_turns.len()];
            for summary in conversation_summaries {
                res[summary.idx] = summary.topic.to_string();
            }
            res
        } else {
            vec!["".to_string(); self.conversation_turns.len()]
        };
        let summaries: Vec<String> =
            if let Some(conversation_summaries) = &self.conversation_summaries {
                let mut res: Vec<String> = vec!["".to_string(); self.conversation_turns.len()];
                for summary in conversation_summaries {
                    res[summary.idx] = summary.summary.to_string();
                }
                res
            } else {
                vec!["".to_string(); self.conversation_turns.len()]
            };
        sqlx::query!(
            "
            insert into turns (
              call,
              call_idx,
              caller_side,
              bot_side,
              topic,
              summary
            )
            select * from unnest (
              $1::integer[],
              $2::integer[],
              $3::text[],
              $4::text[],
              $5::text[],
              $6::text[]
            )
            ",
            &call_ids,
            &call_idxs,
            &caller_sides,
            &bot_sides,
            &topics,
            &summaries,
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            error!(error=%e, "failed to batch insert turn rows");
            AppError("db error")
        })?;

        tx.commit().await.map_err(|e| {
            error!(error=%e, "db error");
            AppError("db error")
        })?;

        Ok(())
    }
}

// TODO: move these into `tasks.rs`
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
