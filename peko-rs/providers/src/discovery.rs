//! What an endpoint says about the models it offers, and what the
//! public models.dev catalog suggests where it says nothing.
//!
//! There are no presets: everything here is read when a model is being
//! chosen, so a model released tomorrow is described tomorrow without
//! a new runtime. Two sources, kept apart so a client can tell them
//! apart:
//!
//! - `reported` — the endpoint's own model list. Lists differ in shape
//!   (Anthropic `max_input_tokens` + `capabilities`, Gemini
//!   `inputTokenLimit`, OpenRouter / Kimi `context_length` + reasoning
//!   efforts, the ChatGPT plan's `context_window` +
//!   `supported_reasoning_levels`, vLLM `max_model_len`, …). Fields are
//!   recognised by name, never by vendor; many lists (OpenAI API keys,
//!   GLM, MiniMax, DeepSeek) carry ids only.
//! - `suggested` — the models.dev entry for the same model, fetched on
//!   the fly (never bundled) and only offered as a suggestion the
//!   person accepts or edits. Matched by the endpoint's host first, then
//!   by the model's first-party (canonical) entry.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::spec::PricingHint;

/// What is known about one model's limits and abilities. Every field
/// is optional: `None` is "not said", not "no".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelFacts {
    /// Tokens the model takes in (its context window).
    pub context_window: Option<u32>,
    /// Most tokens one reply may have.
    pub max_output_tokens: Option<u32>,
    pub image_input: Option<bool>,
    pub audio_input: Option<bool>,
    pub video_input: Option<bool>,
    /// Function calling.
    pub tools: Option<bool>,
    pub thinking: Option<ThinkingFacts>,
    /// USD per 1M tokens.
    pub pricing: Option<PricingHint>,
}

impl ModelFacts {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// How a model thinks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThinkingFacts {
    /// The model can think.
    pub supported: bool,
    /// `Some(true)`: thinking cannot be turned off.
    pub required: Option<bool>,
    /// The effort levels the model takes, lowercase, in the source's
    /// order (`low`, `medium`, `high`, `xhigh`, `max`, `none`,
    /// `minimal`, …). Empty when it only switches thinking on or off,
    /// or takes a token budget.
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}

/// One model an endpoint lists.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredModel {
    /// The wire model id.
    pub id: String,
    pub display_name: Option<String>,
    /// What the endpoint's own list says.
    pub reported: ModelFacts,
    /// What models.dev says, when it knows the model.
    pub suggested: Option<ModelFacts>,
}

// ---------------------------------------------------------------------
// The endpoint's own list
// ---------------------------------------------------------------------

/// The models in a model-list body (`data[]`, or `models[]` as the
/// ChatGPT plan, Gemini and Ollama write it). Models a plan hides
/// (`visibility` other than `list`) are left out; a Gemini-style
/// `models/` prefix is removed.
#[must_use]
pub fn parse_list(body: &Value) -> Vec<DiscoveredModel> {
    body.get("data")
        .or_else(|| body.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_item)
        .collect()
}

fn parse_item(m: &Value) -> Option<DiscoveredModel> {
    if str_at(m, &["visibility"]).is_some_and(|v| v != "list") {
        return None;
    }
    let raw_id = str_at(m, &["id"])
        .or_else(|| str_at(m, &["slug"]))
        .or_else(|| str_at(m, &["name"]))?;
    let id = raw_id.strip_prefix("models/").unwrap_or(raw_id).trim();
    if id.is_empty() {
        return None;
    }
    // `name` is the id itself on Gemini and Ollama lists.
    let display_name = str_at(m, &["display_name"])
        .or_else(|| str_at(m, &["displayName"]))
        .or_else(|| str_at(m, &["name"]).filter(|n| *n != raw_id && *n != id))
        .map(str::to_string);
    Some(DiscoveredModel {
        id: id.to_string(),
        display_name,
        reported: reported_facts(m),
        suggested: None,
    })
}

fn reported_facts(m: &Value) -> ModelFacts {
    let anthropic_shaped = m.get("max_input_tokens").is_some();
    let context_window = first_u32(
        m,
        &[
            &["max_input_tokens"],
            &["context_window"],
            &["context_length"],
            &["max_context_length"],
            &["max_model_len"],
            &["inputTokenLimit"],
            &["limit", "context"],
            &["top_provider", "context_length"],
        ],
    );
    let mut output_paths: Vec<&[&str]> = vec![
        &["max_output_tokens"],
        &["max_completion_tokens"],
        &["outputTokenLimit"],
        &["limit", "output"],
        &["top_provider", "max_completion_tokens"],
    ];
    // Anthropic's `max_tokens` is the reply ceiling; elsewhere the name
    // is not used consistently enough to trust.
    if anthropic_shaped {
        output_paths.insert(0, &["max_tokens"]);
    }
    let max_output_tokens = first_u32(m, &output_paths);

    let (mut image_input, mut audio_input, mut video_input) = (None, None, None);
    if let Some(inputs) = [
        &["architecture", "input_modalities"][..],
        &["modalities", "input"],
        &["input_modalities"],
    ]
    .iter()
    .find_map(|p| strings_at(m, p))
    {
        let has = |k: &str| Some(inputs.iter().any(|i| i == k));
        image_input = has("image");
        audio_input = has("audio");
        video_input = has("video");
    }
    let ollama_caps = strings_at(m, &["capabilities"]);
    let ollama_has = |k: &str| ollama_caps.as_ref().map(|c| c.iter().any(|i| i == k));
    image_input = bool_at(m, &["capabilities", "image_input", "supported"])
        .or_else(|| bool_at(m, &["supports_image_in"]))
        .or_else(|| bool_at(m, &["capabilities", "vision"]))
        .or_else(|| ollama_has("vision"))
        .or(image_input);
    video_input = bool_at(m, &["supports_video_in"]).or(video_input);

    let tools = strings_at(m, &["supported_parameters"])
        .map(|p| p.iter().any(|s| s == "tools"))
        .or_else(|| bool_at(m, &["capabilities", "function_calling"]))
        .or_else(|| ollama_has("tools"));

    ModelFacts {
        context_window,
        max_output_tokens,
        image_input,
        audio_input,
        video_input,
        tools,
        thinking: reported_thinking(m, ollama_has("thinking")),
        pricing: None,
    }
}

fn reported_thinking(m: &Value, ollama_thinking: Option<bool>) -> Option<ThinkingFacts> {
    const ANTHROPIC_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

    // Anthropic: `capabilities.thinking` + `capabilities.effort`.
    if let Some(supported) = bool_at(m, &["capabilities", "thinking", "supported"]) {
        let efforts = if bool_at(m, &["capabilities", "effort", "supported"]) == Some(true) {
            ANTHROPIC_EFFORTS
                .iter()
                .filter(|e| bool_at(m, &["capabilities", "effort", e, "supported"]) == Some(true))
                .map(|e| (*e).to_string())
                .collect()
        } else {
            Vec::new()
        };
        let required = bool_at(
            m,
            &["capabilities", "thinking", "types", "disabled", "supported"],
        )
        .map(|can_turn_off| supported && !can_turn_off);
        return Some(ThinkingFacts {
            supported: supported || !efforts.is_empty(),
            required,
            efforts,
            default_effort: None,
        });
    }

    // The ChatGPT plan: `supported_reasoning_levels[].effort`.
    if let Some(levels) = m
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
    {
        let efforts: Vec<String> = levels
            .iter()
            .filter_map(|l| str_at(l, &["effort"]).or_else(|| l.as_str()))
            .map(str::to_ascii_lowercase)
            .collect();
        return Some(ThinkingFacts {
            supported: !efforts.is_empty(),
            required: None,
            default_effort: str_at(m, &["default_reasoning_level"]).map(str::to_ascii_lowercase),
            efforts,
        });
    }

    // OpenRouter and Kimi: a `reasoning` object; Kimi also
    // `think_efforts`.
    if let Some(r) = m.get("reasoning").filter(|r| r.is_object()) {
        let efforts = strings_at(r, &["supported_efforts"])
            .or_else(|| strings_at(r, &["effort", "valid"]))
            .or_else(|| strings_at(m, &["think_efforts", "valid_efforts"]))
            .unwrap_or_default();
        let required = bool_at(r, &["mandatory"])
            .or_else(|| str_at(r, &["type"]).map(|t| t == "only"))
            .or_else(|| str_at(m, &["supports_thinking_type"]).map(|t| t == "only"));
        return Some(ThinkingFacts {
            supported: true,
            required,
            default_effort: str_at(r, &["default_effort"])
                .or_else(|| str_at(r, &["effort", "default"]))
                .or_else(|| str_at(m, &["think_efforts", "default_effort"]))
                .map(str::to_ascii_lowercase),
            efforts,
        });
    }
    if let Some(efforts) = strings_at(m, &["think_efforts", "valid_efforts"]) {
        return Some(ThinkingFacts {
            supported: true,
            required: str_at(m, &["supports_thinking_type"]).map(|t| t == "only"),
            default_effort: str_at(m, &["think_efforts", "default_effort"])
                .map(str::to_ascii_lowercase),
            efforts,
        });
    }

    let supported = bool_at(m, &["supports_reasoning"])
        .or_else(|| bool_at(m, &["thinking"]))
        .or_else(|| bool_at(m, &["reasoning"]))
        .or_else(|| {
            strings_at(m, &["supported_parameters"]).map(|p| {
                p.iter()
                    .any(|s| s == "reasoning" || s == "reasoning_effort")
            })
        })
        .or(ollama_thinking)?;
    Some(ThinkingFacts {
        supported,
        ..ThinkingFacts::default()
    })
}

fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(k))
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    at(v, path)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn bool_at(v: &Value, path: &[&str]) -> Option<bool> {
    at(v, path).and_then(Value::as_bool)
}

/// A positive token count; `0` and `null` are "not said".
fn u32_at(v: &Value, path: &[&str]) -> Option<u32> {
    let n = at(v, path)?;
    let n = n
        .as_u64()
        .or_else(|| n.as_f64().filter(|f| *f >= 1.0).map(|f| f as u64))
        .or_else(|| n.as_str().and_then(|s| s.trim().parse().ok()))?;
    u32::try_from(n).ok().filter(|n| *n > 0)
}

fn first_u32(v: &Value, paths: &[&[&str]]) -> Option<u32> {
    paths.iter().find_map(|p| u32_at(v, p))
}

fn strings_at(v: &Value, path: &[&str]) -> Option<Vec<String>> {
    let list = at(v, path)?.as_array()?;
    Some(
        list.iter()
            .filter_map(Value::as_str)
            .map(str::to_ascii_lowercase)
            .collect(),
    )
}

// ---------------------------------------------------------------------
// models.dev suggestions
// ---------------------------------------------------------------------

/// Where suggestions come from unless `PEKO_MODEL_SUGGESTIONS` says
/// otherwise (`off` turns them off; a URL replaces this one).
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// How long one fetched catalog is used before it is fetched again.
const SUGGESTIONS_KEPT: Duration = Duration::from_secs(6 * 60 * 60);
/// The catalog is several MB; a slow fetch gives up and the person
/// just sees no suggestions.
const SUGGESTIONS_TIMEOUT: Duration = Duration::from_secs(15);

/// The suggestion source this process uses: `None` when turned off.
#[must_use]
pub fn suggestions_source() -> Option<String> {
    match std::env::var("PEKO_MODEL_SUGGESTIONS") {
        Ok(v) if v.trim().eq_ignore_ascii_case("off") => None,
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => Some(MODELS_DEV_URL.to_string()),
    }
}

/// The models.dev catalog, reduced to what suggestions need.
#[derive(Debug, Default)]
pub struct Suggestions {
    providers: BTreeMap<String, SuggestedProvider>,
    /// Every `provider/model` some entry names as its canonical one.
    pointed_at: std::collections::BTreeSet<String>,
}

#[derive(Debug, Default)]
struct SuggestedProvider {
    /// `host/path` of its API, lowercase, no scheme or trailing `/`.
    api: Option<String>,
    /// Keyed by lowercase model id.
    models: BTreeMap<String, SuggestedModel>,
}

#[derive(Debug)]
struct SuggestedModel {
    canonical: Option<String>,
    facts: ModelFacts,
}

#[derive(Deserialize)]
struct DevProvider {
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct DevModel {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    reasoning_options: Vec<Value>,
    #[serde(default)]
    tool_call: Option<bool>,
    #[serde(default)]
    modalities: Option<DevModalities>,
    #[serde(default)]
    limit: Option<DevLimit>,
    #[serde(default)]
    cost: Option<DevCost>,
    #[serde(default)]
    canonical_model_id: Option<String>,
}

#[derive(Deserialize)]
struct DevModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Deserialize)]
struct DevLimit {
    #[serde(default)]
    context: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
}

#[derive(Deserialize)]
struct DevCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

fn tokens(n: Option<f64>) -> Option<u32> {
    n.filter(|n| *n >= 1.0 && *n <= f64::from(u32::MAX))
        .map(|n| n as u32)
}

fn dev_facts(m: DevModel) -> ModelFacts {
    let inputs = m.modalities.map(|x| x.input).unwrap_or_default();
    let has = |k: &str| (!inputs.is_empty()).then(|| inputs.iter().any(|i| i == k));
    let efforts: Vec<String> = m
        .reasoning_options
        .iter()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("effort"))
        .flat_map(|o| strings_at(o, &["values"]).unwrap_or_default())
        .collect();
    let thinking = m.reasoning.map(|supported| ThinkingFacts {
        supported,
        required: None,
        efforts: if supported { efforts } else { Vec::new() },
        default_effort: None,
    });
    let pricing = m.cost.and_then(|c| {
        let p = PricingHint {
            input_per_million: c.input,
            output_per_million: c.output,
            // `0` means "no cache price" there, not "free".
            cache_read_per_million: c.cache_read.filter(|v| *v > 0.0),
            cache_write_per_million: c.cache_write.filter(|v| *v > 0.0),
        };
        (p.input_per_million.is_some() || p.output_per_million.is_some()).then_some(p)
    });
    ModelFacts {
        context_window: m.limit.as_ref().and_then(|l| tokens(l.context)),
        max_output_tokens: m.limit.as_ref().and_then(|l| tokens(l.output)),
        image_input: has("image"),
        audio_input: has("audio"),
        video_input: has("video"),
        tools: m.tool_call,
        thinking,
        pricing,
    }
}

/// `https://Host/Path/` → `host/path`.
fn normalize_url(url: &str) -> String {
    let url = url.trim().to_ascii_lowercase();
    let url = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
    url.trim_end_matches('/').to_string()
}

fn host(normalized: &str) -> &str {
    normalized.split('/').next().unwrap_or(normalized)
}

impl Suggestions {
    /// Read a models.dev `api.json` body. Entries that do not parse are
    /// skipped, so one odd model never loses the rest.
    pub fn parse(body: &[u8]) -> serde_json::Result<Self> {
        let raw: BTreeMap<String, Value> = serde_json::from_slice(body)?;
        let mut providers = BTreeMap::new();
        for (id, value) in raw {
            let Ok(p) = serde_json::from_value::<DevProvider>(value) else {
                continue;
            };
            let mut models = BTreeMap::new();
            for (model_id, value) in p.models {
                let Ok(m) = serde_json::from_value::<DevModel>(value) else {
                    continue;
                };
                // Embedding, image, speech… models are not chat models.
                if m.kind.as_deref().is_some_and(|k| k != "chat") {
                    continue;
                }
                models.insert(
                    model_id.to_ascii_lowercase(),
                    SuggestedModel {
                        canonical: m.canonical_model_id.clone().map(|c| c.to_ascii_lowercase()),
                        facts: dev_facts(m),
                    },
                );
            }
            providers.insert(
                id.to_ascii_lowercase(),
                SuggestedProvider {
                    api: p.api.as_deref().map(normalize_url),
                    models,
                },
            );
        }
        let pointed_at = providers
            .values()
            .flat_map(|p| p.models.values())
            .filter_map(|m| m.canonical.clone())
            .collect();
        Ok(Self {
            providers,
            pointed_at,
        })
    }

    /// What models.dev says about `model_id` served at `base_url`:
    /// first from a provider whose API is on the same host (the one
    /// sharing the longest path wins), then from the model's
    /// first-party entry (`canonical_model_id` naming its own
    /// provider), trying the part after a `vendor/` prefix too.
    #[must_use]
    pub fn suggest(&self, base_url: &str, model_id: &str) -> Option<ModelFacts> {
        let id = model_id.trim().to_ascii_lowercase();
        if id.is_empty() {
            return None;
        }
        let base = normalize_url(base_url);
        let shared = |api: &str| {
            base.split('/')
                .zip(api.split('/'))
                .take_while(|(a, b)| a == b)
                .count()
        };
        let by_host = self
            .providers
            .values()
            .filter_map(|p| {
                let api = p.api.as_deref()?;
                (host(api) == host(&base)).then(|| (shared(api), p))
            })
            // Ties keep the first provider by id: deterministic.
            .fold(
                None::<(usize, &SuggestedProvider)>,
                |best, (n, p)| match best {
                    Some((b, _)) if b >= n => best,
                    _ => Some((n, p)),
                },
            )
            .and_then(|(_, p)| p.models.get(&id));
        if let Some(m) = by_host {
            return Some(m.facts.clone());
        }
        // The first-party entry is the one canonical ids point at: its
        // own, or (as MiniMax's own entries have none) the resellers'.
        let bare = id.rsplit('/').next().unwrap_or(&id);
        [id.as_str(), bare].iter().find_map(|want| {
            self.providers.iter().find_map(|(pid, p)| {
                let m = p.models.get(*want)?;
                let me = format!("{pid}/{want}");
                (m.canonical.as_deref() == Some(me.as_str()) || self.pointed_at.contains(&me))
                    .then(|| m.facts.clone())
            })
        })
    }

    /// Fill `suggested` on each model.
    pub fn apply(&self, base_url: &str, models: &mut [DiscoveredModel]) {
        for m in models {
            m.suggested = self.suggest(base_url, &m.id);
        }
    }
}

type Kept = Option<(Instant, String, Arc<Suggestions>)>;
static KEPT: tokio::sync::Mutex<Kept> = tokio::sync::Mutex::const_new(None);

/// The catalog at `source`, fetched at most once every few hours per
/// process. `None` when it cannot be had (offline, a slow or changed
/// catalog): suggestions are a convenience, never a reason to fail.
pub async fn suggestions(source: &str) -> Option<Arc<Suggestions>> {
    let mut kept = KEPT.lock().await;
    if let Some((at, from, catalog)) = kept.as_ref() {
        if from == source && at.elapsed() < SUGGESTIONS_KEPT {
            return Some(catalog.clone());
        }
    }
    let fetched = async {
        let body = reqwest::Client::builder()
            .timeout(SUGGESTIONS_TIMEOUT)
            .build()
            .ok()?
            .get(source)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .bytes()
            .await
            .ok()?;
        Suggestions::parse(&body).ok()
    }
    .await;
    match fetched {
        Some(catalog) => {
            let catalog = Arc::new(catalog);
            *kept = Some((Instant::now(), source.to_string(), catalog.clone()));
            Some(catalog)
        }
        None => {
            tracing::debug!(source, "model suggestions are unavailable");
            None
        }
    }
}

/// Fill `suggested` from the process's suggestion source, if any.
pub async fn add_suggestions(base_url: &str, models: &mut [DiscoveredModel]) {
    let Some(source) = suggestions_source() else {
        return;
    };
    if let Some(catalog) = suggestions(&source).await {
        catalog.apply(base_url, models);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(item: Value) -> DiscoveredModel {
        parse_list(&json!({ "data": [item] })).remove(0)
    }

    #[test]
    fn an_anthropic_list_gives_limits_inputs_and_effort_levels() {
        let m = one(json!({
            "type": "model", "id": "claude-opus-5-5", "display_name": "Claude Opus 5.5",
            "max_input_tokens": 1_000_000, "max_tokens": 128_000,
            "capabilities": {
                "image_input": {"supported": true}, "pdf_input": {"supported": true},
                "thinking": {"supported": true, "types": {
                    "adaptive": {"supported": true}, "enabled": {"supported": false},
                    "disabled": {"supported": true}}},
                "effort": {"supported": true, "low": {"supported": true},
                    "medium": {"supported": true}, "high": {"supported": true},
                    "xhigh": null, "max": {"supported": true}}
            }
        }));
        assert_eq!(m.display_name.as_deref(), Some("Claude Opus 5.5"));
        assert_eq!(m.reported.context_window, Some(1_000_000));
        assert_eq!(m.reported.max_output_tokens, Some(128_000));
        assert_eq!(m.reported.image_input, Some(true));
        let t = m.reported.thinking.unwrap();
        assert!(t.supported);
        assert_eq!(t.required, Some(false));
        assert_eq!(t.efforts, ["low", "medium", "high", "max"]);
    }

    #[test]
    fn placeholder_zeros_are_not_limits() {
        let m = one(json!({"id": "x", "max_input_tokens": 0, "max_tokens": 0}));
        assert_eq!(m.reported.context_window, None);
        assert_eq!(m.reported.max_output_tokens, None);
    }

    #[test]
    fn a_kimi_list_gives_context_inputs_and_efforts() {
        // As Kimi Code's `/v1/models` answered on 2026-11-25.
        let m = one(json!({
            "id": "kimi-for-coding", "object": "model", "display_name": "K2.8 Preview",
            "context_length": 1_048_576, "supports_reasoning": true,
            "supports_image_in": true, "supports_video_in": true,
            "supports_thinking_type": "only",
            "think_efforts": {"support": true, "valid_efforts": ["low", "high", "max"], "default_effort": "max"},
            "modalities": {"input": ["text", "image", "video"], "output": ["text"]},
            "reasoning": {"type": "only", "effort": {"valid": ["low", "high", "max"], "default": "max"}},
            "limit": {"context": 1_048_576}
        }));
        assert_eq!(m.reported.context_window, Some(1_048_576));
        assert_eq!(m.reported.image_input, Some(true));
        assert_eq!(m.reported.video_input, Some(true));
        assert_eq!(m.reported.audio_input, Some(false));
        let t = m.reported.thinking.unwrap();
        assert_eq!(t.required, Some(true));
        assert_eq!(t.efforts, ["low", "high", "max"]);
        assert_eq!(t.default_effort.as_deref(), Some("max"));
    }

    #[test]
    fn an_openrouter_list_gives_output_tools_and_efforts() {
        let m = one(json!({
            "id": "z-ai/glm-5.3", "name": "Z.ai: GLM 5.3", "context_length": 1_048_576,
            "architecture": {"input_modalities": ["text", "image"]},
            "top_provider": {"context_length": 1_048_576, "max_completion_tokens": 131_072},
            "supported_parameters": ["max_tokens", "reasoning", "tools"],
            "reasoning": {"mandatory": true, "supported_efforts": ["max", "high", "low"], "default_effort": "max"}
        }));
        assert_eq!(m.display_name.as_deref(), Some("Z.ai: GLM 5.3"));
        assert_eq!(m.reported.max_output_tokens, Some(131_072));
        assert_eq!(m.reported.tools, Some(true));
        assert_eq!(m.reported.image_input, Some(true));
        let t = m.reported.thinking.unwrap();
        assert_eq!(t.required, Some(true));
        assert_eq!(t.efforts, ["max", "high", "low"]);
    }

    #[test]
    fn a_plan_list_gives_its_window_and_reasoning_levels() {
        let list = parse_list(&json!({"models": [
            {"slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list",
             "context_window": 272_000, "default_reasoning_level": "medium",
             "supported_reasoning_levels": [{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}]},
            {"slug": "internal", "visibility": "hide"}
        ]}));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "gpt-5.5");
        assert_eq!(list[0].reported.context_window, Some(272_000));
        let t = list[0].reported.thinking.clone().unwrap();
        assert_eq!(t.efforts, ["low", "medium", "high"]);
        assert_eq!(t.default_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn gemini_and_local_server_lists_give_their_limits() {
        let g = parse_list(&json!({"models": [{"name": "models/gemini-3.5-flash",
            "displayName": "Gemini 3.5 Flash", "inputTokenLimit": 1_048_576,
            "outputTokenLimit": 65_536, "thinking": true}]}))
        .remove(0);
        assert_eq!(g.id, "gemini-3.5-flash");
        assert_eq!(g.reported.context_window, Some(1_048_576));
        assert_eq!(g.reported.max_output_tokens, Some(65_536));
        assert!(g.reported.thinking.unwrap().supported);
        let v = one(json!({"id": "qwen", "max_model_len": 32_768}));
        assert_eq!(v.reported.context_window, Some(32_768));
    }

    #[test]
    fn an_id_only_list_reports_nothing() {
        // As GLM and MiniMax answered on 2026-11-25.
        let m = one(json!({"id": "glm-5.4", "object": "model", "created": 1, "owned_by": "z-ai"}));
        assert_eq!(m.display_name, None);
        assert!(m.reported.is_empty());
    }

    fn catalog() -> Suggestions {
        Suggestions::parse(
            json!({
                "zhipuai": {"api": "https://open.bigmodel.cn/api/paas/v4", "models": {
                    "glm-5.3": {"reasoning": true, "reasoning_options": [{"type": "toggle"}],
                        "tool_call": true, "modalities": {"input": ["text"]},
                        "limit": {"context": 200_000, "output": 131_072},
                        "cost": {"input": 0.75, "output": 3.3, "cache_read": 0.18, "cache_write": 0},
                        "canonical_model_id": "zhipuai/glm-5.3"}}},
                "zhipuai-coding-plan": {"api": "https://open.bigmodel.cn/api/coding/paas/v4", "models": {
                    "glm-5.3": {"reasoning": true, "limit": {"context": 180_000, "output": 1}}}},
                "openai": {"models": {
                    "gpt-5.5": {"reasoning": true, "tool_call": true,
                        "reasoning_options": [{"type": "effort", "values": ["none", "low", "medium", "high", "xhigh"]}],
                        "modalities": {"input": ["text", "image", "pdf"]},
                        "limit": {"context": 1_050_000, "output": 128_000},
                        "canonical_model_id": "openai/gpt-5.5"},
                    "text-embedding-3-small": {"type": "embedding", "limit": {"context": 8192}}}},
                "someproxy": {"api": "https://proxy.example/v1", "models": {
                    "gpt-5.5": {"reasoning": false, "limit": {"context": 64_000},
                        "canonical_model_id": "openai/gpt-5.5"}}},
                "minimax": {"api": "https://api.minimax.io/anthropic/v1", "models": {
                    "MiniMax-M3": {"reasoning": true, "limit": {"context": 1_000_000, "output": 524_288}}}},
                "reseller": {"api": "https://api.reseller.example/v1", "models": {
                    "MiniMax-M3": {"reasoning": true, "limit": {"context": 524_288},
                        "canonical_model_id": "minimax/MiniMax-M3"}}},
                "broken": {"models": {"odd": {"limit": "lots"}}}
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn a_suggestion_comes_from_the_provider_on_the_same_host() {
        let c = catalog();
        let s = c
            .suggest("https://open.bigmodel.cn/api/paas/v4/", "GLM-5.3")
            .unwrap();
        assert_eq!(s.context_window, Some(200_000));
        assert_eq!(s.tools, Some(true));
        assert_eq!(s.image_input, Some(false));
        let t = s.thinking.unwrap();
        assert!(t.supported && t.efforts.is_empty());
        let p = s.pricing.unwrap();
        assert_eq!(p.cache_read_per_million, Some(0.18));
        assert_eq!(p.cache_write_per_million, None);
        // The coding plan's path shares more with its own entry.
        let coding = c
            .suggest("https://open.bigmodel.cn/api/coding/paas/v4", "glm-5.3")
            .unwrap();
        assert_eq!(coding.context_window, Some(180_000));
        assert_eq!(coding.max_output_tokens, Some(1));
    }

    #[test]
    fn elsewhere_a_suggestion_comes_from_the_first_party_entry() {
        let c = catalog();
        let s = c.suggest("https://gateway.internal/v1", "gpt-5.5").unwrap();
        assert_eq!(s.context_window, Some(1_050_000));
        assert_eq!(
            s.thinking.unwrap().efforts,
            ["none", "low", "medium", "high", "xhigh"]
        );
        let s = c
            .suggest("https://gateway.internal/v1", "openai/gpt-5.5")
            .unwrap();
        assert_eq!(s.max_output_tokens, Some(128_000));
        assert!(c.suggest("https://gateway.internal/v1", "glm-9").is_none());
        // A first-party entry without a canonical id of its own, found
        // through the entries that point at it.
        let m = c
            .suggest("https://api.minimaxi.com/v1", "MiniMax-M3")
            .unwrap();
        assert_eq!(m.context_window, Some(1_000_000));
        assert!(c
            .suggest("https://api.openai.com/v1", "text-embedding-3-small")
            .is_none());
    }

    #[test]
    fn suggestions_fill_each_listed_model() {
        let mut models = parse_list(&json!({"data": [{"id": "glm-5.3"}, {"id": "glm-9"}]}));
        catalog().apply("https://open.bigmodel.cn/api/paas/v4", &mut models);
        assert_eq!(
            models[0].suggested.as_ref().unwrap().context_window,
            Some(200_000)
        );
        assert!(models[1].suggested.is_none());
    }

    #[tokio::test]
    async fn an_unreachable_catalog_gives_no_suggestions() {
        assert!(suggestions("http://127.0.0.1:9/api.json").await.is_none());
    }
}
