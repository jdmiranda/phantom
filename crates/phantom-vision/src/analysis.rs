//! GPT-4V analysis pipeline for phantom-vision.
//!
//! The [`VisionBackend`] trait is the sole public surface for callers. Pass a
//! [`Screenshot`] and a plain-English `prompt`; receive an [`Analysis`] in
//! return. Concrete implementations ([`OpenAiVisionBackend`] and
//! [`MockVisionBackend`]) are wired at the call site so the trait can be
//! swapped for local vision models without touching callers.
//!
//! # Cost guard
//!
//! [`OpenAiVisionBackend`] refuses any screenshot whose PNG byte length exceeds
//! [`MAX_IMAGE_BYTES`] (100 KB). The image is sent as an inline `data:` URI in
//! the GPT-4V `image_url` message part.
//!
//! # Prompt templates
//!
//! Three stock prompts are exposed via [`PromptTemplate`]:
//! - [`PromptTemplate::Summarize`] — one-sentence summary of what is visible.
//! - [`PromptTemplate::ExtractText`] — all readable text, preserving layout.
//! - [`PromptTemplate::IdentifyUiElements`] — structured list of UI elements.
//! - [`PromptTemplate::TerminalAnomalies`] — terminal-optimised: flags errors and crashes.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{format::Screenshot, VisionError};

/// Maximum PNG byte size accepted by [`OpenAiVisionBackend`].
pub const MAX_IMAGE_BYTES: usize = 100 * 1024; // 100 KB

// ── Domain types ──────────────────────────────────────────────────────────────

/// A detected UI element inside the screenshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiElement {
    /// Human-readable label, e.g. `"Submit button"`, `"Terminal pane"`.
    pub label: String,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f32,
    /// Optional bounding-box in pixels `(x, y, width, height)`.
    pub bounding_box: Option<(u32, u32, u32, u32)>,
}

/// Structured output produced by a [`VisionBackend`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    /// All readable text extracted from the frame.
    pub text_content: String,
    /// UI elements detected (may be empty when the prompt is not UI-oriented).
    pub ui_elements: Vec<UiElement>,
    /// One-sentence summary of the frame.
    pub summary: String,
    /// Optional embedding vector for downstream vector search.
    pub embedding: Vec<f32>,
}

/// Canned prompt templates understood by all backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTemplate {
    /// Produce a one-sentence summary of what is visible.
    Summarize,
    /// Extract all readable text, preserving approximate spatial layout.
    ExtractText,
    /// Return a structured list of UI elements with labels and positions.
    IdentifyUiElements,
    /// Terminal screenshot optimised: describe content and flag errors/crashes.
    TerminalAnomalies,
}

impl PromptTemplate {
    /// Resolve the template to a concrete prompt string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarize => {
                "Provide a single concise sentence summarising what is visible in this screenshot."
            }
            Self::ExtractText => {
                "Extract all readable text from this screenshot, preserving the approximate \
                 spatial layout using newlines and indentation where helpful."
            }
            Self::IdentifyUiElements => {
                "List every UI element visible in this screenshot. For each element provide: \
                 a short label, its approximate location (top-left quadrant, centre, etc.), \
                 and whether it appears interactive."
            }
            Self::TerminalAnomalies => {
                "Describe what you see in this terminal screenshot. Be concise. \
                 Flag any errors, crashes, or anomalies."
            }
        }
    }
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Implemented by concrete vision providers (OpenAI GPT-4V, local models, mocks).
#[async_trait]
pub trait VisionBackend: Send + Sync {
    /// Stable identifier for this backend, e.g. `"openai-gpt4v"` or `"mock"`.
    fn name(&self) -> &'static str;

    /// Analyse `screenshot` using the given free-form `prompt`.
    ///
    /// # Errors
    ///
    /// Implementations must return [`VisionError::ImageTooLarge`] when the PNG
    /// payload exceeds [`MAX_IMAGE_BYTES`], and [`VisionError::Backend`] for
    /// provider-specific failures.
    async fn analyze(
        &self,
        screenshot: &Screenshot,
        prompt: &str,
    ) -> Result<Analysis, VisionError>;

    /// Convenience wrapper that resolves a [`PromptTemplate`] and delegates to
    /// [`Self::analyze`].
    async fn analyze_with_template(
        &self,
        screenshot: &Screenshot,
        template: PromptTemplate,
    ) -> Result<Analysis, VisionError> {
        self.analyze(screenshot, template.as_str()).await
    }
}

// ── OpenAI GPT-4V backend ─────────────────────────────────────────────────────

/// OpenAI GPT-4V vision backend.
///
/// Construct via [`OpenAiVisionBackend::from_env`] (reads `OPENAI_API_KEY`) or
/// [`OpenAiVisionBackend::new`]. Calls the `/chat/completions` endpoint with the
/// `gpt-4o` model, which supports inline image data URIs.
pub struct OpenAiVisionBackend {
    api_key: String,
    base_url: String,
    model: String,
}

impl std::fmt::Debug for OpenAiVisionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiVisionBackend")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

/// Default model — `gpt-4o` supports the vision (image_url) message part.
const DEFAULT_MODEL: &str = "gpt-4o";
/// Default OpenAI API base URL.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

impl OpenAiVisionBackend {
    /// Build from `OPENAI_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`VisionError::Backend`] if the variable is absent or empty.
    pub fn from_env() -> Result<Self, VisionError> {
        let key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            VisionError::Backend("OPENAI_API_KEY environment variable not set".to_string())
        })?;
        if key.is_empty() {
            return Err(VisionError::Backend(
                "OPENAI_API_KEY environment variable is empty".to_string(),
            ));
        }
        Ok(Self::new(key))
    }

    /// Build with an explicit API key.
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Override the base URL — used for tests against a local mock server.
    #[must_use]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Override the model identifier.
    #[must_use]
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }
}

// ── OpenAI wire types ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: Vec<ContentPart<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: ImageUrl<'a> },
}

#[derive(Serialize)]
struct ImageUrl<'a> {
    url: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[async_trait]
impl VisionBackend for OpenAiVisionBackend {
    fn name(&self) -> &'static str {
        "openai-gpt4v"
    }

    async fn analyze(
        &self,
        screenshot: &Screenshot,
        prompt: &str,
    ) -> Result<Analysis, VisionError> {
        let png = screenshot.png_bytes();

        if png.len() > MAX_IMAGE_BYTES {
            return Err(VisionError::ImageTooLarge {
                size: png.len(),
                limit: MAX_IMAGE_BYTES,
            });
        }

        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png);
        let data_uri = format!("data:image/png;base64,{b64}");

        let system_prompt =
            "Describe what you see in this terminal screenshot. Be concise. \
             Flag any errors, crashes, or anomalies.";

        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                Message {
                    role: "system",
                    content: vec![ContentPart::Text { text: system_prompt }],
                },
                Message {
                    role: "user",
                    content: vec![
                        ContentPart::ImageUrl {
                            image_url: ImageUrl { url: &data_uri },
                        },
                        ContentPart::Text { text: prompt },
                    ],
                },
            ],
            max_tokens: 1024,
        };

        let url = format!("{}/chat/completions", self.base_url);
        let client = reqwest::Client::new();

        let resp = client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| VisionError::Backend(format!("request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(VisionError::Backend(format!(
                "OpenAI returned {status}: {text}"
            )));
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| VisionError::Backend(format!("decode failed: {e}")))?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        Ok(Analysis {
            text_content: content.clone(),
            ui_elements: Vec::new(),
            summary: content,
            embedding: Vec::new(),
        })
    }
}

// ── Mock backend ──────────────────────────────────────────────────────────────

/// Deterministic in-memory backend for tests and offline development.
///
/// Always returns a fixed [`Analysis`] derived from the prompt string and the
/// screenshot dimensions. Never makes network calls.
pub struct MockVisionBackend;

#[async_trait]
impl VisionBackend for MockVisionBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn analyze(
        &self,
        screenshot: &Screenshot,
        prompt: &str,
    ) -> Result<Analysis, VisionError> {
        let (w, h) = screenshot.dimensions();

        if screenshot.png_bytes().len() > MAX_IMAGE_BYTES {
            return Err(VisionError::ImageTooLarge {
                size: screenshot.png_bytes().len(),
                limit: MAX_IMAGE_BYTES,
            });
        }

        let summary = format!("Mock analysis of {w}x{h} frame: {prompt}");
        Ok(Analysis {
            text_content: format!("Text from mock {w}x{h}"),
            ui_elements: vec![UiElement {
                label: "mock element".to_string(),
                confidence: 1.0,
                bounding_box: Some((0, 0, w, h)),
            }],
            summary,
            embedding: vec![0.0_f32; 16],
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::ScreenshotSource;

    fn small_screenshot() -> Screenshot {
        // 8x8 solid grey — tiny PNG, well under 100 KB.
        let rgba: Vec<u8> = vec![128u8; 8 * 8 * 4];
        Screenshot::new(&rgba, 8, 8, 0, ScreenshotSource::FullDesktop).unwrap()
    }

    fn oversized_screenshot() -> Screenshot {
        // 300x300 pseudo-random pixels — hard to compress below 100 KB.
        let side = 300u32;
        let mut rgba = Vec::with_capacity((side as usize) * (side as usize) * 4);
        for i in 0..(side * side) {
            let v = (i % 251) as u8;
            let w = ((i * 7 + 13) % 251) as u8;
            rgba.extend_from_slice(&[v, w, v ^ w, 255]);
        }
        Screenshot::new(&rgba, side, side, 0, ScreenshotSource::FullDesktop).unwrap()
    }

    // ── Mock backend ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn mock_analyze_returns_analysis() {
        let s = small_screenshot();
        let backend = MockVisionBackend;
        let result = backend.analyze(&s, "summarize this").await.unwrap();
        assert!(!result.summary.is_empty());
        assert!(!result.text_content.is_empty());
        assert_eq!(result.embedding.len(), 16);
    }

    #[tokio::test]
    async fn mock_analyze_includes_dimensions_in_summary() {
        let s = small_screenshot();
        let backend = MockVisionBackend;
        let result = backend.analyze(&s, "summarize").await.unwrap();
        assert!(
            result.summary.contains("8x8"),
            "summary should mention dimensions: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn mock_analyze_returns_one_ui_element() {
        let s = small_screenshot();
        let backend = MockVisionBackend;
        let result = backend
            .analyze_with_template(&s, PromptTemplate::IdentifyUiElements)
            .await
            .unwrap();
        assert_eq!(result.ui_elements.len(), 1);
        assert_eq!(result.ui_elements[0].label, "mock element");
    }

    #[tokio::test]
    async fn mock_cost_guard_rejects_oversized_image() {
        let s = oversized_screenshot();
        if s.png_bytes().len() <= MAX_IMAGE_BYTES {
            return; // PNG compressed it below limit — skip
        }
        let backend = MockVisionBackend;
        let err = backend
            .analyze(&s, "anything")
            .await
            .expect_err("oversized image should be rejected");
        assert!(
            matches!(err, VisionError::ImageTooLarge { .. }),
            "expected ImageTooLarge, got {err:?}"
        );
    }

    #[tokio::test]
    async fn mock_anomaly_detection_flag() {
        // Confirm that the TerminalAnomalies template prompt string is forwarded
        // and appears in the mock summary (which echoes the prompt).
        let s = small_screenshot();
        let backend = MockVisionBackend;
        let result = backend
            .analyze_with_template(&s, PromptTemplate::TerminalAnomalies)
            .await
            .unwrap();
        assert!(
            result.summary.contains("anomal"),
            "terminal anomalies template should surface in mock summary: {}",
            result.summary
        );
    }

    // ── Prompt templates ──────────────────────────────────────────────────────

    #[test]
    fn summarize_template_is_non_empty() {
        assert!(!PromptTemplate::Summarize.as_str().is_empty());
    }

    #[test]
    fn extract_text_template_is_non_empty() {
        assert!(!PromptTemplate::ExtractText.as_str().is_empty());
    }

    #[test]
    fn identify_ui_elements_template_is_non_empty() {
        assert!(!PromptTemplate::IdentifyUiElements.as_str().is_empty());
    }

    #[test]
    fn terminal_anomalies_template_is_non_empty() {
        assert!(!PromptTemplate::TerminalAnomalies.as_str().is_empty());
    }

    #[test]
    fn all_templates_are_distinct() {
        let s = PromptTemplate::Summarize.as_str();
        let e = PromptTemplate::ExtractText.as_str();
        let u = PromptTemplate::IdentifyUiElements.as_str();
        let t = PromptTemplate::TerminalAnomalies.as_str();
        assert_ne!(s, e);
        assert_ne!(s, u);
        assert_ne!(e, u);
        assert_ne!(s, t);
        assert_ne!(e, t);
        assert_ne!(u, t);
    }

    // ── OpenAI backend unit tests (no network) ────────────────────────────────

    #[test]
    fn openai_backend_name_is_correct() {
        let b = OpenAiVisionBackend::new("sk-x".into());
        assert_eq!(b.name(), "openai-gpt4v");
    }

    #[test]
    fn openai_from_env_fails_when_missing() {
        if std::env::var("OPENAI_API_KEY").is_ok() {
            return;
        }
        let err = OpenAiVisionBackend::from_env()
            .expect_err("should fail without OPENAI_API_KEY");
        assert!(matches!(err, VisionError::Backend(_)));
    }

    #[tokio::test]
    async fn openai_cost_guard_rejects_oversized_image() {
        let s = oversized_screenshot();
        if s.png_bytes().len() <= MAX_IMAGE_BYTES {
            return; // PNG too compressible to trigger guard — skip
        }
        let backend = OpenAiVisionBackend::new("sk-fake".into());
        let err = backend
            .analyze(&s, "any prompt")
            .await
            .expect_err("oversized image must be rejected before network call");
        assert!(
            matches!(err, VisionError::ImageTooLarge { .. }),
            "expected ImageTooLarge, got {err:?}"
        );
    }

    /// Live integration test — skipped in CI, run manually with a real key.
    #[tokio::test]
    #[ignore = "requires live OPENAI_API_KEY + network"]
    async fn openai_analyze_small_screenshot_returns_sensible_analysis() {
        let backend = OpenAiVisionBackend::from_env()
            .expect("set OPENAI_API_KEY for live test");
        let s = small_screenshot();
        let result = backend
            .analyze_with_template(&s, PromptTemplate::TerminalAnomalies)
            .await
            .expect("live call should succeed");
        assert!(!result.summary.is_empty(), "summary must not be empty");
    }

    // ── UiElement serde ───────────────────────────────────────────────────────

    #[test]
    fn ui_element_serde_round_trips() {
        let el = UiElement {
            label: "Terminal pane".into(),
            confidence: 0.95,
            bounding_box: Some((10, 20, 640, 480)),
        };
        let json = serde_json::to_string(&el).unwrap();
        let back: UiElement = serde_json::from_str(&json).unwrap();
        assert_eq!(back.label, el.label);
        assert!((back.confidence - el.confidence).abs() < 1e-6);
        assert_eq!(back.bounding_box, el.bounding_box);
    }

    #[test]
    fn analysis_embedding_storable_as_vec_f32() {
        let a = Analysis {
            text_content: "hello".into(),
            ui_elements: Vec::new(),
            summary: "a frame".into(),
            embedding: vec![0.1, 0.2, 0.3],
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: Analysis = serde_json::from_str(&json).unwrap();
        assert_eq!(back.embedding.len(), 3);
    }
}
