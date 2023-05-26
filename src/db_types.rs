use sqlx::types::time::OffsetDateTime;
use uuid::Uuid;

pub struct Caller {
    pub id: i32,
    pub phone: String,
    pub city: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub zip: Option<String>,
}

pub struct Call {
    pub id: i32,
    pub caller: i32,
    pub twilio_call_sid: String,
    pub twilio_stream_sid: String,
    pub dg_request_id: Uuid,
    pub created: OffsetDateTime,
}

#[allow(dead_code)]
pub struct Turn {
    pub id: i32,
    pub call: i32,
    pub call_idx: i32,
    pub caller_side: String,
    pub bot_side: String,
    pub topic: Option<String>,
    pub summary: Option<String>,
}
