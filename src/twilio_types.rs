pub fn wrap_twiml(twiml: String) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{twiml}")
}

mod twiml {
    use xmlserde::xml_serde_enum;
    use xmlserde_derives::XmlSerialize;

    #[derive(PartialEq, Eq, XmlSerialize)]
    #[xmlserde(root = b"Response")]
    pub struct Response {
        #[xmlserde(ty = "untag")]
        pub actions: Vec<ResponseAction>,
    }

    #[derive(PartialEq, Eq, XmlSerialize)]
    pub enum ResponseAction {
        #[xmlserde(name = b"Say")]
        Say(SayAction),
        #[xmlserde(name = b"Play")]
        Play(PlayAction),
        #[xmlserde(name = b"Connect")]
        Connect(ConnectAction),
    }

    #[derive(PartialEq, Eq, XmlSerialize, Default)]
    pub struct SayAction {
        #[xmlserde(ty = "text")]
        pub text: String,
        #[xmlserde(name = b"voice", ty = "attr")]
        pub voice: Option<String>,
        #[xmlserde(name = b"loop", ty = "attr")]
        pub lp: Option<u16>,
        #[xmlserde(name = b"language", ty = "attr")]
        pub language: Option<String>,
    }

    #[derive(PartialEq, Eq, XmlSerialize, Default)]
    pub struct PlayAction {
        #[xmlserde(ty = "text")]
        pub url: String,
        #[xmlserde(name = b"loop", ty = "attr")]
        pub lp: Option<u16>,
    }

    #[derive(PartialEq, Eq, XmlSerialize)]
    pub struct ConnectAction {
        #[xmlserde(ty = "untag")]
        pub connection: Connection,
    }

    #[derive(PartialEq, Eq, XmlSerialize)]
    pub enum Connection {
        #[xmlserde(name = b"Stream")]
        Stream(StreamAction),
    }

    #[derive(PartialEq, Eq, XmlSerialize, Default)]
    pub struct StreamAction {
        #[xmlserde(name = b"url", ty = "attr")]
        pub url: String,
        #[xmlserde(name = b"name", ty = "attr")]
        pub name: Option<String>,
        #[xmlserde(name = b"track", ty = "attr")]
        pub track: Option<StreamTrack>,
    }

    xml_serde_enum! {
        #[derive(PartialEq, Eq, Debug)]
        StreamTrack {
            Inbound => "inbound_track",
            Outbound => "outbound_track",
            Both => "both_tracks",
        }
    }
}
pub use twiml::*;

mod ws {
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    #[derive(Serialize, Deserialize)]
    pub struct OutboundMarkMeta {
        pub name: String,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "lowercase", tag = "event")]
    pub enum TwilioOutbound {
        Mark {
            mark: OutboundMarkMeta,
            #[serde(rename = "streamSid")]
            stream_sid: String,
        },
        Media {
            media: OutboundMediaMeta,
            #[serde(rename = "streamSid")]
            stream_sid: String,
        },
        Clear {
            #[serde(rename = "streamSid")]
            stream_sid: String,
        },
    }

    #[derive(Serialize, Deserialize)]
    pub struct OutboundMediaMeta {
        pub payload: String,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "lowercase", tag = "event")]
    pub enum TwilioMessage {
        Connected {
            protocol: String,
            version: String,
        },
        Start {
            #[serde(rename = "sequenceNumber")]
            sequence_number: String,
            start: StartMeta,
            #[serde(rename = "streamSid")]
            stream_sid: String,
        },
        Media {
            #[serde(rename = "sequenceNumber")]
            sequence_number: String,
            media: MediaMeta,
            #[serde(rename = "streamSid")]
            stream_sid: String,
        },
        Stop {
            #[serde(rename = "sequenceNumber")]
            sequence_number: String,
            stop: StopMeta,
            #[serde(rename = "streamSid")]
            stream_sid: String,
        },
        Mark {
            #[serde(rename = "sequenceNumber")]
            sequence_number: String,
            mark: MarkMeta,
            #[serde(rename = "streamSid")]
            stream_sid: String,
        },
    }

    #[derive(Serialize, Deserialize)]
    pub struct StartMeta {
        #[serde(rename = "streamSid")]
        pub stream_sid: String,
        #[serde(rename = "accountSid")]
        pub account_sid: String,
        #[serde(rename = "callSid")]
        pub call_sid: String,
        #[serde(default)]
        pub tracks: Vec<String>,
        #[serde(rename = "customParameters", default)]
        pub custom_parameters: HashMap<String, String>,
        #[serde(rename = "mediaFormat")]
        pub media_format: MediaFormat,
    }

    #[derive(Serialize, Deserialize)]
    pub struct MediaFormat {
        pub encoding: String,
        #[serde(rename = "sampleRate")]
        pub sample_rate: u32,
        pub channels: u16,
    }

    #[derive(Serialize, Deserialize)]
    pub struct MediaMeta {
        pub track: MediaTrack,
        pub chunk: String,
        pub timestamp: String,
        pub payload: String,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum MediaTrack {
        Inbound,
        Outbound,
    }

    #[derive(Serialize, Deserialize)]
    pub struct StopMeta {
        #[serde(rename = "accountSid")]
        pub account_sid: String,
        #[serde(rename = "callSid")]
        pub call_sid: String,
    }

    #[derive(Serialize, Deserialize)]
    pub struct MarkMeta {
        pub name: String,
    }
}
pub use ws::*;
