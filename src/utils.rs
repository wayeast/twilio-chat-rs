use crate::consts::GOOGLE_WAV_HEADER_SZ;
use crate::texttospeech_v1_types::TextService;
use crate::twilio_types::{OutboundMediaMeta, TwilioOutbound};

use base64::{engine, read, Engine};
use gcs_common::yup_oauth2;
use std::io::{Cursor, Read};
use std::sync::Arc;

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

pub fn b64_decode_to_buf(enc: String, buf: &mut Vec<u8>) {
    let mut cur = Cursor::new(enc);
    let mut decoder = read::DecoderReader::new(&mut cur, &engine::general_purpose::STANDARD);
    decoder.read_to_end(buf).unwrap();
}

pub async fn gcs_client(gcs_credentials: &str) -> TextService {
    let conn = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_or_http()
        .enable_http2()
        .build();
    let tls_client = hyper::Client::builder().build(conn);
    let service_account_key = yup_oauth2::parse_service_account_key(gcs_credentials)
        .expect("failed to read GCS account key");
    let gcs_authenticator = yup_oauth2::ServiceAccountAuthenticator::builder(service_account_key)
        .hyper_client(tls_client.clone())
        .persist_tokens_to_disk("tokencache.json")
        .build()
        .await
        .expect("ServiceAccount authenticator failed.");
    TextService::new(tls_client, Arc::new(gcs_authenticator))
}
