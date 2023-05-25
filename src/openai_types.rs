use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct OpenAIMessage {
    pub role: String,
    pub content: String,
}

#[derive(Serialize, Default)]
pub struct OpenAIPayload {
    pub model: String,
    pub messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Deserialize, Debug)]
pub struct OpenAIBatchResponse {
    pub id: String,
    pub object: String,
    // TODO: this is a timestamp
    pub created: usize,
    pub model: String,
    pub usage: OpenAIUsageStats,
    pub choices: Vec<OpenAIBatchChoice>,
}

#[derive(Deserialize, Debug)]
pub struct OpenAIBatchChoice {
    pub message: OpenAIMessage,
    pub finish_reason: Option<String>,
    pub index: u32,
}

#[derive(Deserialize, Debug)]
pub struct OpenAIUsageStats {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Deserialize)]
pub struct OpenAIChoice {
    pub message: OpenAIMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
    pub index: u32,
}

#[derive(Deserialize, Debug)]
pub struct OpenAIStreamResponse {
    pub id: String,
    pub object: String,
    // TODO: this is a timestamp
    pub created: usize,
    pub model: String,
    pub choices: Vec<OpenAIStreamChoice>,
}

#[derive(Deserialize, Debug)]
pub struct OpenAIStreamChoice {
    pub delta: StreamDelta,
    pub finish_reason: Option<String>,
    pub index: u32,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum StreamDelta {
    Role { role: String },
    Content { content: String },
    Empty {},
}
