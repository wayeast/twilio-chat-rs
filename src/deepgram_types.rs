use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StreamingResponse {
    pub channel_index: (u16, u16),
    pub duration: f32,
    pub start: f32,
    pub is_final: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_final: Option<bool>,
    pub channel: Channel,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct Channel {
    pub alternatives: Vec<Alternative>,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct Alternative {
    pub transcript: String,
    pub confidence: f32,
    pub words: Vec<Word>,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct Word {
    pub word: String,
    pub start: f32,
    pub end: f32,
    pub confidence: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StreamingMeta {
    transaction_key: Option<String>,
    request_id: Uuid,
    sha256: String,
    // created: OffsetDateTime,
    duration: f32,
    channels: u16,
    models: Vec<Uuid>,
    model_info: HashMap<Uuid, ModelInfo>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ModelInfo {
    name: String,
    version: String,
    arch: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum StreamMessage {
    StreamingResponse(StreamingResponse),
    StreamingMeta(StreamingMeta),
}
