use crate::deepgram_types::StreamingResponse;
use crate::texttospeech_v1_types::{
    AudioConfig, AudioConfigAudioEncoding, SynthesisInput, SynthesizeSpeechRequest, TextService,
    TextSynthesizeParams, VoiceSelectionParams, VoiceSelectionParamsSsmlGender,
};
use crate::twilio_types::TwilioConnectPayload;

use sqlx::{Pool, Postgres};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, RwLock};

/// Indicator of whether we are currently listening or talking
pub enum CurrentBotAction {
    Listening(mpsc::Sender<ConversationSignal>),
    Talking,
}

/// A ConversationTurn represents an exchange of 1) caller inquiry and 2) bot response
pub struct ConversationTurn {
    pub caller_side: Arc<RwLock<String>>,
    pub bot_side: Arc<RwLock<String>>,
}

impl ConversationTurn {
    pub fn new() -> Self {
        Self {
            caller_side: Arc::new(RwLock::new(String::new())),
            bot_side: Arc::new(RwLock::new(String::new())),
        }
    }
}

/// Message type consumed by the caller-side turn manager.  `StreamingResponse` tells the manager
/// to continue handling DG streaming responses; `Go` tells the manager to respond to the caller.
pub enum ConversationSignal {
    StreamingResponse(StreamingResponse),
    Go,
}

#[derive(Debug)]
pub struct ConversationSummary {
    pub idx: usize,
    pub topic: String,
    pub summary: String,
}

pub struct AppState {
    pub console_api_key: String,
    pub openai_api_key: String,
    pub twilio_account_sid: String,
    pub twilio_auth_token: String,
    pub gcs_client: TextService,
    pub http_client: reqwest::Client,
    // call sid => twilio connect meta
    pub streams: Arc<Mutex<HashMap<String, TwilioConnectPayload>>>,
    pub db_pool: Pool<Postgres>,
}

impl AppState {
    pub async fn get_google_tts(&self, text: &str, encoding: AudioConfigAudioEncoding) -> String {
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
