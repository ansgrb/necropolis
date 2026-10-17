//! LLM provider trait and Gemini structured-output implementation (blueprint §15.1).

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error ({status}): {body}")]
    Api { status: u16, body: String },
    #[error("missing candidate text in LLM response")]
    EmptyResponse,
}

#[derive(Debug, Clone)]
pub struct GeminiClient {
    model: String,
    api_key: String,
    http: reqwest::Client,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerateRequest<'a> {
    system_instruction: Content<'a>,
    contents: Vec<Content<'a>>,
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct Content<'a> {
    parts: Vec<Part<'a>>,
}

#[derive(Serialize)]
struct Part<'a> {
    text: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    response_mime_type: &'static str,
    temperature: f32,
}

#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<CandidateContent>,
}

#[derive(Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<CandidatePart>,
}

#[derive(Deserialize)]
struct CandidatePart {
    text: Option<String>,
}

impl GeminiClient {
    pub fn new(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Calls Gemini `generateContent` with `responseMimeType: "application/json"` and strict
    /// untrusted-data framing in the system instruction.
    pub async fn generate_json(&self, system_prompt: &str, user_bundle: &str) -> Result<String, LlmError> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );
        let req = GenerateRequest {
            system_instruction: Content { parts: vec![Part { text: system_prompt }] },
            contents: vec![Content { parts: vec![Part { text: user_bundle }] }],
            generation_config: GenerationConfig {
                response_mime_type: "application/json",
                temperature: 0.1,
            },
        };
        let resp = self.http.post(&url).json(&req).send().await?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api { status, body });
        }
        let parsed: GenerateResponse = resp.json().await?;
        parsed
            .candidates
            .into_iter()
            .find_map(|c| c.content?.parts.into_iter().find_map(|p| p.text))
            .ok_or(LlmError::EmptyResponse)
    }
}
