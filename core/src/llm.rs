//! One client for every LLM provider (PDA LLM-1 … LLM-9).
//!
//! LM Studio, OpenAI, Gemini (through Google's OpenAI-compatible endpoint)
//! and any custom endpoint all speak the OpenAI chat-completions format, so
//! they differ only by base URL, key and model. Errors are classified so the
//! queue can tell "wait and try again later" (server down, bad key, rate
//! limit, quota, model not loaded) from a real failure.

use std::future::Future;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    LmStudio,
    OpenAi,
    Gemini,
    Custom,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::LmStudio => "lmstudio",
            ProviderKind::OpenAi => "openai",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Custom => "custom",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "lmstudio" => Some(ProviderKind::LmStudio),
            "openai" => Some(ProviderKind::OpenAi),
            "gemini" => Some(ProviderKind::Gemini),
            "custom" => Some(ProviderKind::Custom),
            _ => None,
        }
    }

    /// `None` for custom endpoints, which always need a base URL.
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            ProviderKind::LmStudio => Some("http://localhost:1234/v1"),
            ProviderKind::OpenAi => Some("https://api.openai.com/v1"),
            ProviderKind::Gemini => Some("https://generativelanguage.googleapis.com/v1beta/openai"),
            ProviderKind::Custom => None,
        }
    }

    /// Local providers run one request at a time; cloud ones can run a few
    /// (PDA LLM-7).
    pub fn is_local(self) -> bool {
        self == ProviderKind::LmStudio
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Provider {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
}

impl Provider {
    /// `provider/model`, as written to note frontmatter (`cleaned_by`).
    pub fn label(&self) -> String {
        format!("{}/{}", self.kind.as_str(), self.model)
    }
}

/// Keys never reach logs (PDA LLM-5): `Debug` shows only whether one is set.
impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .field("model", &self.model)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    pub text: String,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmError {
    /// Connection refused, DNS, timeout: LM Studio not running, no network.
    #[error("LLM not reachable at {url}: {reason}")]
    Unreachable { url: String, reason: String },

    #[error("the LLM provider rejected the API key")]
    InvalidKey,

    #[error("rate limited by the LLM provider")]
    RateLimited { retry_after: Option<Duration> },

    #[error("the LLM provider's quota is used up")]
    QuotaExhausted,

    #[error("model not found or not loaded: {0}")]
    ModelNotFound(String),

    /// 5xx: the provider is having trouble.
    #[error("LLM server error ({status}): {body}")]
    Server { status: u16, body: String },

    /// 4xx we can't act on, or a reply we can't read.
    #[error("unexpected LLM response: {0}")]
    BadResponse(String),
}

impl LlmError {
    /// Whether a job should wait and try again later instead of failing
    /// (PDA CL-8, LLM-6). Only an unusable reply is a real failure.
    pub fn should_wait(&self) -> bool {
        // Exhaustive on purpose: a new variant must decide wait vs fail.
        match self {
            LlmError::Unreachable { .. }
            | LlmError::InvalidKey
            | LlmError::RateLimited { .. }
            | LlmError::QuotaExhausted
            | LlmError::ModelNotFound(_)
            | LlmError::Server { .. } => true,
            LlmError::BadResponse(_) => false,
        }
    }

    /// How long to wait before trying again.
    pub fn retry_after(&self) -> Duration {
        match self {
            LlmError::RateLimited {
                retry_after: Some(d),
            } => *d,
            LlmError::RateLimited { retry_after: None } => Duration::from_secs(30),
            LlmError::Server { .. } | LlmError::Unreachable { .. } => Duration::from_secs(60),
            LlmError::ModelNotFound(_) | LlmError::InvalidKey => Duration::from_secs(5 * 60),
            // Free-tier daily quotas reset on the provider's schedule; check
            // back hourly rather than guessing it.
            LlmError::QuotaExhausted => Duration::from_secs(60 * 60),
            LlmError::BadResponse(_) => Duration::ZERO,
        }
    }
}

/// What cleanup and the folder sorter need from a model. [`LlmClient`] is
/// the real one; tests use fakes.
pub trait Llm {
    fn chat(
        &self,
        system: &str,
        user: &str,
    ) -> impl Future<Output = std::result::Result<ChatReply, LlmError>> + Send;

    /// `provider/model`.
    fn label(&self) -> String;
}

pub struct LlmClient {
    http: reqwest::Client,
    provider: Provider,
}

/// A local 31B model on CPU can take minutes for one long chunk.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_ERROR_BODY: usize = 500;

impl LlmClient {
    pub fn new(provider: Provider) -> std::result::Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| LlmError::BadResponse(format!("could not build the HTTP client: {e}")))?;
        Ok(LlmClient { http, provider })
    }

    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.provider.base_url.trim_end_matches('/'))
    }

    fn request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.provider.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }

    /// The provider's own model list, so names are never hardcoded (LLM-3).
    pub async fn list_models(&self) -> std::result::Result<Vec<String>, LlmError> {
        let url = self.url("models");
        let response = self
            .request(self.http.get(&url))
            .send()
            .await
            .map_err(|e| unreachable_error(&url, e))?;
        let body: ModelList = parse_body(response).await?;
        let mut ids: Vec<String> = body
            .data
            .into_iter()
            .map(|m| m.id.trim_start_matches("models/").to_string())
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// A one-word round trip, timed (LLM-4).
    pub async fn test(&self) -> std::result::Result<Duration, LlmError> {
        let started = Instant::now();
        self.complete(
            "You are a connectivity check.",
            "Reply with the single word OK.",
        )
        .await?;
        Ok(started.elapsed())
    }

    async fn complete(&self, system: &str, user: &str) -> std::result::Result<ChatReply, LlmError> {
        let url = self.url("chat/completions");
        let request = ChatRequest {
            model: &self.provider.model,
            messages: [
                Message {
                    role: "system",
                    content: system,
                },
                Message {
                    role: "user",
                    content: user,
                },
            ],
            // Low temperature: cleanup is editing, not writing.
            temperature: 0.2,
            // Gemma 4 (and other reasoning models) otherwise spend the
            // whole token budget on hidden "thinking" and return an empty
            // answer; measured on CPU, a cleanup never finished. Only LM
            // Studio is sent this: cloud APIs may reject the parameter.
            reasoning_effort: (self.provider.kind == ProviderKind::LmStudio).then_some("none"),
        };
        let response = self
            .request(self.http.post(&url))
            .json(&request)
            .send()
            .await
            .map_err(|e| unreachable_error(&url, e))?;
        let body: ChatResponse = parse_body(response).await?;
        let text = body
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| {
                LlmError::BadResponse(
                    "the model returned no answer (a reasoning model may have spent it all thinking)"
                        .into(),
                )
            })?;
        Ok(ChatReply {
            text,
            tokens_in: body.usage.as_ref().and_then(|u| u.prompt_tokens),
            tokens_out: body.usage.as_ref().and_then(|u| u.completion_tokens),
        })
    }
}

impl Llm for LlmClient {
    async fn chat(&self, system: &str, user: &str) -> std::result::Result<ChatReply, LlmError> {
        self.complete(system, user).await
    }

    fn label(&self) -> String {
        self.provider.label()
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [Message<'a>; 2],
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ReplyMessage,
}

#[derive(Deserialize)]
struct ReplyMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

fn unreachable_error(url: &str, e: reqwest::Error) -> LlmError {
    let reason = if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "could not connect".to_string()
    } else {
        e.to_string()
    };
    LlmError::Unreachable {
        url: url.to_string(),
        reason,
    }
}

async fn parse_body<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> std::result::Result<T, LlmError> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = read_capped(response).await?;
    if status.is_success() {
        return serde_json::from_str(&body)
            .map_err(|e| LlmError::BadResponse(format!("could not parse the reply: {e}")));
    }
    Err(classify(status.as_u16(), &body, retry_after))
}

/// Replies bigger than this are refused: a cleanup reply is a few hundred
/// KB at most, and a misbehaving endpoint shouldn't exhaust memory.
const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

async fn read_capped(mut response: reqwest::Response) -> std::result::Result<String, LlmError> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_REPLY_BYTES as u64)
    {
        return Err(LlmError::BadResponse("the reply is too large".into()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| LlmError::BadResponse(format!("could not read the reply: {e}")))?
    {
        if bytes.len() + chunk.len() > MAX_REPLY_BYTES {
            return Err(LlmError::BadResponse("the reply is too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Maps an HTTP error to what the queue should do about it.
fn classify(status: u16, body: &str, retry_after: Option<Duration>) -> LlmError {
    let lower = body.to_ascii_lowercase();
    let short: String = body.chars().take(MAX_ERROR_BODY).collect();
    let model_missing =
        lower.contains("model") && (lower.contains("not found") || lower.contains("not loaded"));
    match status {
        401 | 403 => LlmError::InvalidKey,
        // OpenAI says "insufficient_quota", Gemini "RESOURCE_EXHAUSTED" with
        // a quota message; both arrive as 429 like plain rate limits.
        429 if lower.contains("quota") => LlmError::QuotaExhausted,
        429 => LlmError::RateLimited { retry_after },
        404 => LlmError::ModelNotFound(short),
        400 if model_missing => LlmError::ModelNotFound(short),
        500..=599 => LlmError::Server {
            status,
            body: short,
        },
        _ => LlmError::BadResponse(format!("HTTP {status}: {short}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn provider(base_url: String, api_key: Option<&str>) -> Provider {
        Provider {
            kind: ProviderKind::Custom,
            base_url,
            api_key: api_key.map(str::to_string),
            model: "gemma-4-31b".into(),
        }
    }

    fn client(server: &MockServer, key: Option<&str>) -> LlmClient {
        LlmClient::new(provider(format!("{}/v1/", server.uri()), key)).unwrap()
    }

    fn completion(text: &str) -> serde_json::Value {
        serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": text}}],
            "usage": {"prompt_tokens": 120, "completion_tokens": 80}
        })
    }

    #[tokio::test]
    async fn chat_sends_openai_format_and_reads_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_partial_json(serde_json::json!({
                "model": "gemma-4-31b",
                "messages": [
                    {"role": "system", "content": "rules"},
                    {"role": "user", "content": "transcript"}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion("Cleaned.")))
            .expect(1)
            .mount(&server)
            .await;
        let reply = client(&server, Some("sk-test"))
            .chat("rules", "transcript")
            .await
            .unwrap();
        assert_eq!(
            reply,
            ChatReply {
                text: "Cleaned.".into(),
                tokens_in: Some(120),
                tokens_out: Some(80)
            }
        );
    }

    #[tokio::test]
    async fn no_key_means_no_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|req: &Request| {
                assert!(!req.headers.contains_key("authorization"));
                ResponseTemplate::new(200).set_body_json(completion("ok"))
            })
            .mount(&server)
            .await;
        client(&server, None).chat("s", "u").await.unwrap();
    }

    #[tokio::test]
    async fn models_come_from_the_provider() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "models/gemini-flash-lite"}, {"id": "gemma-4-31b"}]
            })))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server, None).list_models().await.unwrap(),
            ["gemini-flash-lite", "gemma-4-31b"]
        );
    }

    async fn error_for(status: u16, body: &str, retry_after: Option<&str>) -> LlmError {
        let server = MockServer::start().await;
        let mut response = ResponseTemplate::new(status).set_body_string(body);
        if let Some(r) = retry_after {
            response = response.insert_header("retry-after", r);
        }
        Mock::given(method("POST"))
            .respond_with(response)
            .mount(&server)
            .await;
        client(&server, Some("k")).chat("s", "u").await.unwrap_err()
    }

    #[tokio::test]
    async fn errors_are_classified_for_the_queue() {
        assert_eq!(error_for(401, "{}", None).await, LlmError::InvalidKey);
        assert_eq!(
            error_for(429, r#"{"error":{"message":"slow down"}}"#, Some("12")).await,
            LlmError::RateLimited {
                retry_after: Some(Duration::from_secs(12))
            }
        );
        assert_eq!(
            error_for(429, r#"{"error":{"code":"insufficient_quota"}}"#, None).await,
            LlmError::QuotaExhausted
        );
        assert_eq!(
            error_for(
                429,
                r#"[{"error":{"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded"}}]"#,
                None
            )
            .await,
            LlmError::QuotaExhausted
        );
        assert!(matches!(
            error_for(404, "no such model", None).await,
            LlmError::ModelNotFound(_)
        ));
        assert!(matches!(
            error_for(400, r#"{"error":"Model gemma is not loaded"}"#, None).await,
            LlmError::ModelNotFound(_)
        ));
        assert!(matches!(
            error_for(503, "overloaded", None).await,
            LlmError::Server { status: 503, .. }
        ));
        assert!(matches!(
            error_for(400, "bad request", None).await,
            LlmError::BadResponse(_)
        ));
    }

    #[tokio::test]
    async fn unreadable_replies_are_failures_not_waits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;
        let err = client(&server, None).chat("s", "u").await.unwrap_err();
        assert!(matches!(err, LlmError::BadResponse(_)));
        assert!(!err.should_wait());

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices": []})),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server, None).chat("s", "u").await.unwrap_err(),
            LlmError::BadResponse(_)
        ));
    }

    #[tokio::test]
    async fn a_server_that_is_not_running_is_unreachable_and_waits() {
        // Port 1 is privileged: no mock server from a parallel test can be
        // listening there, unlike a freshly released ephemeral port.
        let client = LlmClient::new(provider("http://127.0.0.1:1/v1".into(), None)).unwrap();
        let err = client.chat("s", "u").await.unwrap_err();
        assert!(matches!(err, LlmError::Unreachable { .. }), "{err:?}");
        assert!(err.should_wait());
    }

    #[test]
    fn waits_and_backoffs() {
        let server = LlmError::Server {
            status: 503,
            body: String::new(),
        };
        let unreachable = LlmError::Unreachable {
            url: "u".into(),
            reason: "r".into(),
        };
        for (e, secs) in [
            (LlmError::QuotaExhausted, 3600),
            (LlmError::RateLimited { retry_after: None }, 30),
            (
                LlmError::RateLimited {
                    retry_after: Some(Duration::from_secs(5)),
                },
                5,
            ),
            (server, 60),
            (unreachable, 60),
            (LlmError::ModelNotFound("m".into()), 300),
            (LlmError::InvalidKey, 300),
        ] {
            assert!(e.should_wait(), "{e:?}");
            assert_eq!(e.retry_after(), Duration::from_secs(secs), "{e:?}");
        }
        assert!(!LlmError::BadResponse("x".into()).should_wait());
    }

    #[tokio::test]
    async fn lm_studio_requests_ask_for_no_hidden_reasoning() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                serde_json::json!({"reasoning_effort": "none"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")))
            .expect(1)
            .mount(&server)
            .await;
        let p = Provider {
            kind: ProviderKind::LmStudio,
            ..provider(format!("{}/v1", server.uri()), None)
        };
        LlmClient::new(p).unwrap().chat("s", "u").await.unwrap();
    }

    #[tokio::test]
    async fn cloud_requests_never_send_the_lm_studio_reasoning_flag() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                assert!(body.get("reasoning_effort").is_none(), "{body}");
                ResponseTemplate::new(200).set_body_json(completion("ok"))
            })
            .mount(&server)
            .await;
        let p = Provider {
            kind: ProviderKind::Gemini,
            ..provider(format!("{}/v1", server.uri()), Some("k"))
        };
        LlmClient::new(p).unwrap().chat("s", "u").await.unwrap();
    }

    #[tokio::test]
    async fn an_empty_answer_is_a_failure_not_an_empty_cleanup() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "", "reasoning_content": "thinking…"}}]
            })))
            .mount(&server)
            .await;
        let err = client(&server, None).chat("s", "u").await.unwrap_err();
        assert!(
            matches!(&err, LlmError::BadResponse(m) if m.contains("no answer")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn oversized_replies_are_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_REPLY_BYTES + 1)),
            )
            .mount(&server)
            .await;
        let err = client(&server, None).chat("s", "u").await.unwrap_err();
        assert!(
            matches!(&err, LlmError::BadResponse(m) if m.contains("too large")),
            "{err:?}"
        );
    }

    #[test]
    fn keys_never_show_in_debug_output() {
        let p = provider("http://x".into(), Some("sk-very-secret"));
        let shown = format!("{p:?}");
        assert!(!shown.contains("sk-very-secret"), "{shown}");
        assert!(shown.contains("<set>"));
        assert_eq!(p.label(), "custom/gemma-4-31b");
    }

    #[test]
    fn provider_presets() {
        assert_eq!(
            ProviderKind::LmStudio.default_base_url(),
            Some("http://localhost:1234/v1")
        );
        assert_eq!(
            ProviderKind::Gemini.default_base_url(),
            Some("https://generativelanguage.googleapis.com/v1beta/openai")
        );
        assert_eq!(ProviderKind::Custom.default_base_url(), None);
        for kind in [
            ProviderKind::LmStudio,
            ProviderKind::OpenAi,
            ProviderKind::Gemini,
            ProviderKind::Custom,
        ] {
            assert_eq!(ProviderKind::parse(kind.as_str()), Some(kind));
        }
        assert!(ProviderKind::LmStudio.is_local() && !ProviderKind::Gemini.is_local());
    }
}
