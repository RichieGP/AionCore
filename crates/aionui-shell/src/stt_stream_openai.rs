//! OpenAI Realtime transcription upstream for STT streaming.
//!
//! Connects to OpenAI's `/v1/realtime?intent=transcription` WebSocket and
//! adapts it to the transport-agnostic [`UpstreamStream`] interface driven by
//! `stt_stream::run_stream_session`. Base-URL resolution, auth scheme, and
//! language normalization mirror the file-endpoint implementation in
//! `stt_openai.rs`.
//!
//! Protocol (GA Realtime API): after the WS handshake the adapter sends a
//! `session.update` configuring a transcription session (model, optional
//! language, `audio/pcm` input). Audio goes upstream as base64
//! `input_audio_buffer.append` events; `finish()` sends
//! `input_audio_buffer.commit`. Transcripts come back as
//! `conversation.item.input_audio_transcription.delta` / `.completed` events.

use aionui_api_types::{OpenAISpeechToTextConfig, SpeechToTextConfig};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::SttError;
use crate::stt_openai::resolve_base_url;
use crate::stt_stream::{UpstreamEvent, UpstreamFactory, UpstreamStream};

/// Sample rate OpenAI expects for `audio/pcm` realtime input.
const EXPECTED_SAMPLE_RATE: u32 = 24_000;

/// OpenAI Realtime leg of the upstream factory.
///
/// Kept as a per-provider unit struct so the route layer (Task 2A-5) can
/// either use it directly or compose it into a provider-dispatch factory that
/// matches on `config.provider` and delegates here for OpenAI.
pub struct OpenAIRealtimeUpstreamFactory;

#[async_trait::async_trait]
impl UpstreamFactory for OpenAIRealtimeUpstreamFactory {
    async fn connect(
        &self,
        config: &SpeechToTextConfig,
        sample_rate: u32,
        language_hint: Option<&str>,
    ) -> Result<Box<dyn UpstreamStream>, SttError> {
        // The session validates the config before connecting, but re-check
        // here so the factory is safe standalone (mirrors stt_openai::transcribe).
        let openai = config.openai.as_ref().ok_or(SttError::OpenaiNotConfigured)?;
        if openai.api_key.is_empty() {
            return Err(SttError::OpenaiNotConfigured);
        }
        Ok(Box::new(connect(openai, sample_rate, language_hint).await?))
    }
}

/// Open a realtime transcription WebSocket to OpenAI.
///
/// `sample_rate` describes the mono PCM16 audio the client will stream;
/// `language_hint` is the already-resolved language (config language wins
/// over the client hint upstream of this call, but the same precedence is
/// re-applied here to mirror the file path and stay safe standalone).
pub async fn connect(
    config: &OpenAISpeechToTextConfig,
    sample_rate: u32,
    language_hint: Option<&str>,
) -> Result<OpenAIRealtimeStream, SttError> {
    let url = build_ws_url(config.base_url.as_deref());
    let mut request = url
        .clone()
        .into_client_request()
        .map_err(|e| SttError::RequestFailed(format!("invalid OpenAI realtime WS URL {url}: {e}")))?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
        .map_err(|e| SttError::RequestFailed(format!("invalid OpenAI API key for Authorization header: {e}")))?;
    request.headers_mut().insert("Authorization", auth);

    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.map_err(connect_error)?;

    if sample_rate != EXPECTED_SAMPLE_RATE {
        // OpenAI's audio/pcm realtime input is specified as 24 kHz; the
        // renderer contract sends 24000, so anything else is unexpected but
        // forwarded as-is (a custom base_url proxy may accept it).
        tracing::warn!(
            sample_rate,
            expected = EXPECTED_SAMPLE_RATE,
            "stt openai stream: non-standard sample rate for audio/pcm input"
        );
    }

    let language = resolve_language(config.language.as_deref(), language_hint);
    let payload = session_update_payload(&config.model, sample_rate, language.as_deref());
    ws.send(Message::Text(payload.into()))
        .await
        .map_err(|e| SttError::RequestFailed(format!("OpenAI session.update send failed: {e}")))?;

    Ok(OpenAIRealtimeStream {
        ws,
        partial_buf: String::new(),
        finished: false,
        final_delivered: false,
        close_sent: false,
    })
}

/// Map a handshake/connect failure, surfacing the HTTP status (e.g. 401 for
/// a bad API key) when OpenAI rejected the upgrade.
fn connect_error(e: tungstenite::Error) -> SttError {
    match e {
        tungstenite::Error::Http(response) => {
            let status = response.status();
            let body = response
                .body()
                .as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            SttError::RequestFailed(format!("OpenAI realtime WS handshake returned {status}: {body}"))
        }
        other => SttError::RequestFailed(format!("OpenAI realtime WS connect error: {other}")),
    }
}

/// Build the `wss://.../v1/realtime?intent=transcription` URL.
///
/// Base resolution mirrors `stt_openai::resolve_base_url` (default
/// `https://api.openai.com`, trailing `/` and `/v1` stripped), then the HTTP
/// scheme is swapped for the WebSocket equivalent.
fn build_ws_url(base_url: Option<&str>) -> String {
    let base = resolve_base_url(base_url);
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // Already a ws:// / wss:// custom base: pass through unchanged.
        base.to_owned()
    };
    format!("{ws_base}/v1/realtime?intent=transcription")
}

/// Resolve and normalize the transcription language.
///
/// The hint wins because the session already applied config-over-hint
/// precedence and passes the resolved value down. Codes are normalized like
/// the file path (`stt_openai::transcribe`): `en-US` → `en`.
fn resolve_language(config_language: Option<&str>, language_hint: Option<&str>) -> Option<String> {
    let non_blank = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    non_blank(language_hint)
        .or_else(|| non_blank(config_language))
        .map(|lang| lang.split('-').next().unwrap_or(&lang).to_owned())
}

/// Build the `session.update` event configuring a transcription session.
///
/// Uses the GA Realtime API shape: `session.type = "transcription"` with the
/// input format and transcription model under `session.audio.input`.
/// `turn_detection` is left at the server default (VAD) so partial
/// transcripts flow while the user is still speaking.
fn session_update_payload(model: &str, sample_rate: u32, language: Option<&str>) -> String {
    let mut transcription = serde_json::json!({ "model": model });
    if let Some(lang) = language {
        transcription["language"] = serde_json::Value::String(lang.to_owned());
    }
    serde_json::json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": sample_rate },
                    "transcription": transcription,
                },
            },
        },
    })
    .to_string()
}

/// Live OpenAI realtime WebSocket adapted to [`UpstreamStream`].
pub struct OpenAIRealtimeStream {
    ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    /// Accumulated delta text for the item currently being transcribed.
    /// `Partial` events carry the whole un-finalized text, not the delta.
    partial_buf: String,
    /// `finish()` was called (the commit event was sent).
    finished: bool,
    /// A `completed` transcript (or a benign empty-commit error) has been
    /// observed, i.e. no further transcript is expected after a commit.
    final_delivered: bool,
    /// The client-side close handshake has been initiated.
    close_sent: bool,
}

/// Outcome of decoding one OpenAI realtime text frame. Synchronous on
/// purpose: `next_event` must not hold a decoded event across an await.
enum Parsed {
    /// Transcription delta text for the current item.
    Delta(String),
    /// Final transcript for the current item.
    Completed(String),
    /// Fatal upstream error.
    Error(SttError),
    /// `input_audio_buffer.commit` on an empty/too-short buffer: benign after
    /// `finish()` — it just means no trailing transcript is coming.
    CommitEmpty,
    /// Lifecycle/bookkeeping frame the session has no use for.
    Skip,
}

fn parse_text_frame(text: &str) -> Parsed {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(e) => {
            tracing::debug!(error = %e, "stt openai stream: ignoring unparseable text frame");
            return Parsed::Skip;
        }
    };

    match value["type"].as_str() {
        Some("conversation.item.input_audio_transcription.delta") => {
            Parsed::Delta(value["delta"].as_str().unwrap_or("").to_owned())
        }
        Some("conversation.item.input_audio_transcription.completed") => {
            Parsed::Completed(value["transcript"].as_str().unwrap_or("").to_owned())
        }
        Some("conversation.item.input_audio_transcription.failed") => {
            let message = value["error"]["message"].as_str().unwrap_or("unknown error");
            Parsed::Error(SttError::RequestFailed(format!(
                "OpenAI transcription failed: {message}"
            )))
        }
        Some("error") => {
            let code = value["error"]["code"].as_str().unwrap_or("");
            if code == "input_audio_buffer_commit_empty" {
                return Parsed::CommitEmpty;
            }
            let message = value["error"]["message"].as_str().unwrap_or("unknown error");
            Parsed::Error(SttError::RequestFailed(format!(
                "OpenAI realtime error ({code}): {message}"
            )))
        }
        // Session/buffer/item lifecycle frames (session.created, session.updated,
        // input_audio_buffer.committed, speech_started/stopped,
        // conversation.item.created, rate_limits.updated, ...).
        other => {
            tracing::debug!(frame_type = ?other, "stt openai stream: ignoring lifecycle/unknown frame");
            Parsed::Skip
        }
    }
}

#[async_trait::async_trait]
impl UpstreamStream for OpenAIRealtimeStream {
    async fn send_audio(&mut self, pcm: &[u8]) -> Result<(), SttError> {
        let audio = base64::engine::general_purpose::STANDARD.encode(pcm);
        let payload = serde_json::json!({ "type": "input_audio_buffer.append", "audio": audio }).to_string();
        self.ws
            .send(Message::Text(payload.into()))
            .await
            .map_err(|e| SttError::RequestFailed(format!("OpenAI audio send failed: {e}")))
    }

    async fn finish(&mut self) -> Result<(), SttError> {
        self.finished = true;
        self.ws
            .send(Message::Text(r#"{"type":"input_audio_buffer.commit"}"#.into()))
            .await
            .map_err(|e| SttError::RequestFailed(format!("OpenAI commit send failed: {e}")))
    }

    // Cancel-safe: each loop iteration awaits a single `ws.next()` followed by
    // synchronous parsing — nothing is held across an await; delta text is
    // buffered internally in `partial_buf`. The close-initiation await below
    // is guarded by `close_sent` and only reachable once `finished` is set,
    // i.e. after the session's `stop` when `next_event` is the only branch
    // left in its `select!` and can no longer be cancelled.
    async fn next_event(&mut self) -> Option<Result<UpstreamEvent, SttError>> {
        loop {
            // Unlike Deepgram, OpenAI keeps the socket open after the commit
            // has produced its final transcript. Initiate the close handshake
            // exactly once so the session can observe `Closed` and emit `Done`.
            if self.finished && self.final_delivered && !self.close_sent {
                self.close_sent = true;
                if let Err(e) = self.ws.close(None).await {
                    tracing::debug!(error = %e, "stt openai stream: close handshake send failed");
                    return Some(Ok(UpstreamEvent::Closed));
                }
            }
            match self.ws.next().await {
                // Stream ended after a close handshake: clean shutdown.
                None | Some(Ok(Message::Close(_))) => return Some(Ok(UpstreamEvent::Closed)),
                Some(Ok(Message::Text(text))) => match parse_text_frame(text.as_str()) {
                    Parsed::Delta(delta) => {
                        if delta.is_empty() {
                            continue;
                        }
                        self.partial_buf.push_str(&delta);
                        return Some(Ok(UpstreamEvent::Partial(self.partial_buf.clone())));
                    }
                    Parsed::Completed(transcript) => {
                        self.partial_buf.clear();
                        self.final_delivered = true;
                        if transcript.trim().is_empty() {
                            continue; // nothing recognized; still counts for close tracking
                        }
                        return Some(Ok(UpstreamEvent::Final(transcript)));
                    }
                    Parsed::Error(e) => return Some(Err(e)),
                    Parsed::CommitEmpty => {
                        // The buffer held no (or too little) trailing audio:
                        // no further transcript will arrive for the commit.
                        self.final_delivered = true;
                    }
                    Parsed::Skip => {}
                },
                // Pings are answered by tungstenite automatically; OpenAI
                // sends no binary frames we consume.
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    if self.close_sent {
                        // Transport teardown racing our own close handshake
                        // is not a session error.
                        tracing::debug!(error = %e, "stt openai stream: error after close initiated");
                        return Some(Ok(UpstreamEvent::Closed));
                    }
                    return Some(Err(SttError::RequestFailed(format!(
                        "OpenAI realtime stream error: {e}"
                    ))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- build_ws_url ---------------------------------------------------------

    #[test]
    fn url_uses_default_base_with_wss_scheme_and_transcription_intent() {
        assert_eq!(
            build_ws_url(None),
            "wss://api.openai.com/v1/realtime?intent=transcription"
        );
    }

    #[test]
    fn url_swaps_http_base_to_ws_and_strips_trailing_slash_and_v1() {
        assert_eq!(
            build_ws_url(Some("http://127.0.0.1:9999/")),
            "ws://127.0.0.1:9999/v1/realtime?intent=transcription"
        );
        assert_eq!(
            build_ws_url(Some("https://api.groq.com/openai/v1")),
            "wss://api.groq.com/openai/v1/realtime?intent=transcription"
        );
    }

    #[test]
    fn url_blank_base_falls_back_to_default() {
        // Settings UI saves unfilled base_url as "" — mirrors the file path.
        assert_eq!(
            build_ws_url(Some("   ")),
            "wss://api.openai.com/v1/realtime?intent=transcription"
        );
    }

    #[test]
    fn url_passes_through_explicit_ws_scheme() {
        assert_eq!(
            build_ws_url(Some("ws://localhost:1234")),
            "ws://localhost:1234/v1/realtime?intent=transcription"
        );
    }

    // -- resolve_language -------------------------------------------------------

    #[test]
    fn language_hint_wins_and_is_normalized() {
        assert_eq!(resolve_language(Some("es"), Some("en-US")), Some("en".into()));
        assert_eq!(resolve_language(Some("es-MX"), None), Some("es".into()));
        assert_eq!(resolve_language(None, None), None);
    }

    #[test]
    fn blank_languages_are_treated_as_unset() {
        assert_eq!(resolve_language(Some("  "), None), None);
        assert_eq!(resolve_language(Some("es"), Some("")), Some("es".into()));
    }

    // -- session_update_payload ---------------------------------------------------

    #[test]
    fn session_update_carries_model_format_and_language() {
        let payload = session_update_payload("gpt-4o-transcribe", 24000, Some("en"));
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["type"], "session.update");
        assert_eq!(value["session"]["type"], "transcription");
        assert_eq!(value["session"]["audio"]["input"]["format"]["type"], "audio/pcm");
        assert_eq!(value["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(
            value["session"]["audio"]["input"]["transcription"]["model"],
            "gpt-4o-transcribe"
        );
        assert_eq!(value["session"]["audio"]["input"]["transcription"]["language"], "en");
    }

    #[test]
    fn session_update_omits_language_when_unset() {
        let payload = session_update_payload("gpt-4o-mini-transcribe", 24000, None);
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(
            value["session"]["audio"]["input"]["transcription"]
                .get("language")
                .is_none()
        );
    }

    // -- parse_text_frame -----------------------------------------------------

    #[test]
    fn delta_and_completed_frames_are_decoded() {
        let delta = r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"i1","content_index":0,"delta":"he"}"#;
        assert!(matches!(parse_text_frame(delta), Parsed::Delta(d) if d == "he"));

        let completed = r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"i1","content_index":0,"transcript":"hello"}"#;
        assert!(matches!(parse_text_frame(completed), Parsed::Completed(t) if t == "hello"));
    }

    #[test]
    fn error_frame_maps_to_request_failed_with_code_and_message() {
        let frame = r#"{"type":"error","event_id":"e1","error":{"type":"invalid_request_error","code":"invalid_value","message":"boom"}}"#;
        match parse_text_frame(frame) {
            Parsed::Error(e) => {
                assert_eq!(e.error_code(), "STT_REQUEST_FAILED");
                let msg = e.to_string();
                assert!(msg.contains("invalid_value") && msg.contains("boom"), "got: {msg}");
            }
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn transcription_failed_frame_maps_to_request_failed() {
        let frame = r#"{"type":"conversation.item.input_audio_transcription.failed","item_id":"i1","error":{"type":"server_error","message":"asr down"}}"#;
        match parse_text_frame(frame) {
            Parsed::Error(e) => assert!(e.to_string().contains("asr down"), "got: {e}"),
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn commit_empty_error_is_benign() {
        let frame = r#"{"type":"error","error":{"type":"invalid_request_error","code":"input_audio_buffer_commit_empty","message":"buffer too small"}}"#;
        assert!(matches!(parse_text_frame(frame), Parsed::CommitEmpty));
    }

    #[test]
    fn lifecycle_and_unknown_frames_are_skipped() {
        for frame in [
            r#"{"type":"session.created","session":{}}"#,
            r#"{"type":"session.updated","session":{}}"#,
            r#"{"type":"input_audio_buffer.committed","item_id":"i1"}"#,
            r#"{"type":"input_audio_buffer.speech_started","audio_start_ms":10}"#,
            r#"{"type":"input_audio_buffer.speech_stopped","audio_end_ms":900}"#,
            r#"{"type":"conversation.item.created","item":{}}"#,
            r#"{"type":"rate_limits.updated","rate_limits":[]}"#,
            r#"{"type":"something.future"}"#,
            r#"{"no_type":true}"#,
            "not json",
        ] {
            assert!(matches!(parse_text_frame(frame), Parsed::Skip), "frame: {frame}");
        }
    }
}
