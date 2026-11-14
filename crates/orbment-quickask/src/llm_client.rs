use crate::context_capture::QuickContext;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use orbment_core::{
    action_bus::ActionBus,
    config::{McpConfig, QuickAskConfig, QuickAskProvider, ThinkingEffort},
    credentials,
    i18n::tr,
    ipc::{ActionRequest, PendingRequest},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::sync::{oneshot, watch};

const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Conversation {
    messages: Vec<Value>,
    pending: Option<PendingTurn>,
}

#[derive(Clone, Debug)]
struct PendingTurn {
    history_len: usize,
    text: String,
}

impl Conversation {
    fn begin_turn(&mut self, provider: QuickAskProvider, context: &QuickContext, prompt: &str) {
        let history_len = self.messages.len();
        self.messages
            .push(user_message(provider, context, prompt, history_len == 0));
        self.pending = Some(PendingTurn {
            history_len,
            text: String::new(),
        });
    }

    fn request_conversation(&self) -> Self {
        Self {
            messages: self.messages.clone(),
            pending: None,
        }
    }

    pub(crate) fn push_text(&mut self, text: &str) {
        if let Some(pending) = &mut self.pending {
            pending.text.push_str(text);
        }
    }

    fn push_assistant(&mut self, text: String) {
        self.messages
            .push(json!({"role":"assistant","content":text}));
    }

    pub(crate) fn cancel_turn(&mut self) {
        if let Some(pending) = self.pending.take()
            && !pending.text.is_empty()
        {
            // Only displayed text is retained; an interrupted tool round may
            // contain tool calls without their required results.
            self.push_assistant(pending.text);
        }
    }

    pub(crate) fn fail_turn(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.text.trim().is_empty())
        {
            self.cancel_turn();
        } else if let Some(pending) = self.pending.take() {
            self.messages.truncate(pending.history_len);
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

pub struct PendingToolCall {
    pub call: ToolCall,
    reply: oneshot::Sender<std::result::Result<Value, String>>,
    cancelled: Arc<AtomicBool>,
}
impl PendingToolCall {
    pub fn into_request(self, policy: &McpConfig) -> Option<PendingRequest> {
        if self.cancelled.load(Ordering::Acquire) || self.reply.is_closed() {
            return None;
        }
        let action = (|| -> Result<&'static str> {
            let action = orbment_mcp::tool_to_action(&self.call.name)
                .context(tr("quick_ask.unknown_tool"))?;
            orbment_mcp::check_action_risk(policy, action)?;
            ensure!(
                self.call.arguments.is_object(),
                "{}",
                tr("quick_ask.invalid_tool_arguments")
            );
            Ok(action)
        })();
        match action {
            Ok(action) => Some(PendingRequest::new(
                ActionRequest::new(action, self.call.arguments),
                self.cancelled,
                move |result| {
                    let _ = self
                        .reply
                        .send(result.map_err(|error| format!("{error:#}")));
                },
            )),
            Err(error) => {
                let _ = self.reply.send(Err(format!("{error:#}")));
                None
            }
        }
    }
}

pub fn available_tools(bus: &ActionBus, policy: &McpConfig) -> Vec<Value> {
    orbment_mcp::tool_definitions()
        .into_iter()
        .filter(|definition| {
            definition["name"]
                .as_str()
                .and_then(orbment_mcp::tool_to_action)
                .is_some_and(|action| {
                    bus.names().any(|name| name == action)
                        && orbment_mcp::check_action_risk(policy, action).is_ok()
                })
        })
        .collect()
}

pub enum LlmEvent {
    Text(String),
    Tool(PendingToolCall),
    Finished(Conversation),
    Failed(String),
}

pub struct LlmJob {
    cancel: watch::Sender<bool>,
    cancelled: Arc<AtomicBool>,
}
impl LlmJob {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.cancel.send(true);
    }
}
impl Drop for LlmJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// One owned worker/runtime per active request; no HTTP runtime exists while idle.
pub fn start(
    config: QuickAskConfig,
    context: QuickContext,
    prompt: String,
    conversation: Conversation,
    tools: Vec<Value>,
    events: mpsc::Sender<LlmEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<LlmJob> {
    start_turn(config, context, prompt, conversation, tools, events, wake).map(|(job, _)| job)
}

pub(crate) fn start_turn(
    config: QuickAskConfig,
    context: QuickContext,
    prompt: String,
    mut conversation: Conversation,
    tools: Vec<Value>,
    events: mpsc::Sender<LlmEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<(LlmJob, Conversation)> {
    config.validate()?;
    context.validate()?;
    conversation.begin_turn(config.provider, &context, &prompt);
    let request_conversation = conversation.request_conversation();
    let (cancel, mut cancellation) = watch::channel(false);
    let cancelled = Arc::new(AtomicBool::new(false));
    let task_cancelled = cancelled.clone();
    std::thread::Builder::new().name("orbment-quickask-http".into()).spawn(move || {
        let publish = |event| {
            let _ = events.send(event);
            wake();
        };
        let result = (|| -> Result<Option<Conversation>> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancellation.changed() => Ok(None),
                    result = run(config, request_conversation, tools, &publish, task_cancelled) => result.map(Some),
                }
            })
        })();
        match result {
            Ok(Some(conversation)) => publish(LlmEvent::Finished(conversation)),
            Ok(None) => {}
            Err(error) => publish(LlmEvent::Failed(format!("{error:#}"))),
        }
    })?;
    Ok((LlmJob { cancel, cancelled }, conversation))
}

fn user_message(
    provider: QuickAskProvider,
    context: &QuickContext,
    prompt: &str,
    first: bool,
) -> Value {
    let text = if first && (!context.text.is_empty() || !context.source_title.is_empty()) {
        format!(
            "<context source={}>\n{}\n</context>\n\n{}",
            json!(context.source_title),
            context.text,
            prompt
        )
    } else {
        prompt.to_owned()
    };
    let mut content = vec![json!({"type":"text","text":text})];
    if first && let Some(image) = &context.image {
        let data = STANDARD.encode(&image.bytes);
        content.push(match provider {
            QuickAskProvider::OpenAI | QuickAskProvider::OpenAiCompatible => json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{data}", image.mime_type)}}),
            QuickAskProvider::Anthropic => json!({"type":"image","source":{"type":"base64","media_type":image.mime_type,"data":data}}),
            QuickAskProvider::Google => json!({"inlineData":{"mimeType":image.mime_type,"data":data}}),
        });
    }
    json!({"role":"user","content":content})
}

/// Read the active channel's key, including the legacy default OpenAI fallback.
pub fn read_api_key(config: &QuickAskConfig) -> Result<Option<String>> {
    credentials::read_quick_ask_api_key(config)
}

fn resolve_api_key(
    override_key: Option<&str>,
    read: impl FnOnce() -> Result<Option<String>>,
) -> Result<String> {
    if let Some(key) = override_key.map(str::trim).filter(|key| !key.is_empty()) {
        return Ok(key.to_owned());
    }
    read()?.context(tr("quick_ask.missing_api_key"))
}

fn http_client(config: &QuickAskConfig) -> Result<reqwest::Client> {
    ensure!(
        (1..=3600).contains(&config.request_timeout_seconds),
        "{}: quick_ask.request_timeout_seconds",
        tr("config.invalid_value")
    );
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(config.request_timeout_seconds as u64))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn authenticated(
    request: reqwest::RequestBuilder,
    provider: QuickAskProvider,
    key: &str,
) -> reqwest::RequestBuilder {
    let request = request.bearer_auth(key);
    match provider {
        QuickAskProvider::OpenAI | QuickAskProvider::OpenAiCompatible => request,
        QuickAskProvider::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        QuickAskProvider::Google => request.header("x-goog-api-key", key),
    }
}

fn model_names(provider: QuickAskProvider, body: &Value) -> Result<Vec<String>> {
    ensure!(
        body.get("error").is_none(),
        "{}",
        tr("quick_ask.provider_error")
    );
    let (list, field) = if provider == QuickAskProvider::Google {
        ("models", "name")
    } else {
        ("data", "id")
    };
    let entries = body[list]
        .as_array()
        .context("Invalid model listing: expected a model array")?;
    let names: BTreeSet<String> = entries
        .iter()
        .filter_map(|entry| entry[field].as_str())
        .map(str::trim)
        .map(|name| {
            if provider == QuickAskProvider::Google {
                name.strip_prefix("models/").unwrap_or(name)
            } else {
                name
            }
        })
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    ensure!(!names.is_empty(), "The provider returned no models");
    Ok(names.into_iter().collect())
}

/// Blocking entry point for a settings worker; the unsaved key never enters config.
pub fn fetch_models(config: &QuickAskConfig, override_key: Option<&str>) -> Result<Vec<String>> {
    let url = config.models_endpoint_url()?;
    let key = resolve_api_key(override_key, || read_api_key(config))?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let response = authenticated(
                http_client(config)?
                    .get(url)
                    .header("accept", "application/json"),
                config.provider,
                &key,
            )
            .send()
            .await
            .context(tr("quick_ask.request_failed"))?;
            ensure!(
                response.status().is_success(),
                "{}: HTTP {}",
                tr("quick_ask.request_failed"),
                response.status()
            );
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context(tr("quick_ask.request_failed"))?;
                ensure!(
                    bytes.len() + chunk.len() <= MAX_STREAM_BYTES,
                    "{}",
                    tr("quick_ask.response_too_large")
                );
                bytes.extend_from_slice(&chunk);
            }
            model_names(config.provider, &serde_json::from_slice(&bytes)?)
        })
}

fn thinking_budget(effort: ThinkingEffort) -> Option<u32> {
    match effort {
        ThinkingEffort::Off => None,
        ThinkingEffort::Low => Some(2048),
        ThinkingEffort::Medium => Some(8192),
        ThinkingEffort::High => Some(16384),
        ThinkingEffort::Max => Some(32000),
    }
}

fn google_schema(mut schema: Value) -> Value {
    match &mut schema {
        Value::Object(object) => {
            object.remove("$schema");
            object.remove("additionalProperties");
            for value in object.values_mut() {
                *value = google_schema(std::mem::take(value));
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = google_schema(std::mem::take(value));
            }
        }
        _ => {}
    }
    schema
}

fn google_content(message: &Value) -> Value {
    let role = if message["role"] == "assistant" {
        "model"
    } else {
        "user"
    };
    let parts = match &message["content"] {
        Value::String(text) => vec![json!({"text":text})],
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                if part["type"] == "text" {
                    json!({"text":part["text"]})
                } else {
                    part.clone()
                }
            })
            .collect(),
        _ => unreachable!("conversation content is text or parts"),
    };
    json!({"role":role,"parts":parts})
}

fn openai_thinking_effort(effort: ThinkingEffort) -> Option<&'static str> {
    match effort {
        ThinkingEffort::Off => None,
        ThinkingEffort::Low => Some("low"),
        ThinkingEffort::Medium => Some("medium"),
        ThinkingEffort::High => Some("high"),
        ThinkingEffort::Max => Some("xhigh"),
    }
}

fn responses_input(messages: &[Value]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        if message["role"] == "tool" {
            input.push(json!({
                "type":"function_call_output",
                "call_id":message["tool_call_id"],
                "output":message["content"]
            }));
            continue;
        }
        let assistant = message["role"] == "assistant";
        let text_type = if assistant {
            "output_text"
        } else {
            "input_text"
        };
        let content: Vec<_> = match &message["content"] {
            Value::String(text) if text.is_empty() && assistant => Vec::new(),
            Value::String(text) => vec![json!({"type":text_type,"text":text})],
            Value::Array(parts) => parts
                .iter()
                .map(|part| match part["type"].as_str() {
                    Some("text") => json!({"type":text_type,"text":part["text"]}),
                    Some("image_url") => {
                        json!({"type":"input_image","image_url":part["image_url"]["url"]})
                    }
                    _ => unreachable!("OpenAI conversation parts are text or images"),
                })
                .collect(),
            _ => unreachable!("conversation content is text or parts"),
        };
        if !content.is_empty() {
            input.push(json!({"type":"message","role":message["role"],"content":content}));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            input.extend(calls.iter().map(|call| {
                json!({
                    "type":"function_call",
                    "call_id":call["id"],
                    "name":call["function"]["name"],
                    "arguments":call["function"]["arguments"]
                })
            }));
        }
    }
    input
}

fn request_body(config: &QuickAskConfig, conversation: &Conversation, tools: &[Value]) -> Value {
    let mut messages = conversation.messages.clone();
    match config.provider {
        QuickAskProvider::OpenAI => {
            let mut body = json!({
                "model":config.model.trim(),
                "input":responses_input(&messages),
                "stream":true,
                "max_output_tokens":config.max_tokens
            });
            if !config.system_prompt.is_empty() {
                body["instructions"] = json!(config.system_prompt);
            }
            if let Some(effort) = openai_thinking_effort(config.thinking_effort) {
                body["reasoning"] = json!({"effort":effort});
            }
            if !tools.is_empty() {
                body["tools"] = json!(tools.iter().map(|tool| json!({
                    "type":"function","name":tool["name"],"description":tool["description"],"parameters":tool["inputSchema"]
                })).collect::<Vec<_>>());
            }
            body
        }
        QuickAskProvider::OpenAiCompatible => {
            if !config.system_prompt.is_empty() {
                messages.insert(0, json!({"role":"system","content":config.system_prompt}));
            }
            let mut body = json!({"model":config.model.trim(),"messages":messages,"stream":true,"max_tokens":config.max_tokens});
            if let Some(effort) = openai_thinking_effort(config.thinking_effort) {
                body["reasoning_effort"] = json!(effort);
            }
            if !tools.is_empty() {
                body["tools"] = json!(tools.iter().map(|tool| json!({
                    "type":"function","function":{"name":tool["name"],"description":tool["description"],"parameters":tool["inputSchema"]}
                })).collect::<Vec<_>>());
            }
            body
        }
        QuickAskProvider::Anthropic => {
            let mut body = json!({"model":config.model.trim(),"messages":messages,"stream":true,"max_tokens":config.max_tokens});
            if !config.system_prompt.is_empty() {
                body["system"] = json!(config.system_prompt);
            }
            if let Some(budget) = thinking_budget(config.thinking_effort) {
                body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
                body["max_tokens"] = json!(config.max_tokens.max(budget + 4096));
            }
            if !tools.is_empty() {
                body["tools"] = json!(tools.iter().map(|tool| json!({
                    "name":tool["name"],"description":tool["description"],"input_schema":tool["inputSchema"]
                })).collect::<Vec<_>>());
            }
            body
        }
        QuickAskProvider::Google => {
            let mut body = json!({
                "contents":messages.iter().map(google_content).collect::<Vec<_>>(),
                "generationConfig":{"maxOutputTokens":config.max_tokens}
            });
            if !config.system_prompt.is_empty() {
                body["systemInstruction"] = json!({"parts":[{"text":config.system_prompt}]});
            }
            if let Some(budget) = thinking_budget(config.thinking_effort) {
                body["generationConfig"]["thinkingConfig"] = if config.model.contains("gemini-2.5")
                {
                    // 24,576 is supported by both Gemini 2.5 Flash and Pro.
                    json!({"thinkingBudget":budget.min(24576)})
                } else {
                    json!({"thinkingLevel":match config.thinking_effort {
                        ThinkingEffort::Low => "LOW",
                        ThinkingEffort::Medium => "MEDIUM",
                        ThinkingEffort::High | ThinkingEffort::Max => "HIGH",
                        ThinkingEffort::Off => unreachable!(),
                    }})
                };
            }
            if !tools.is_empty() {
                body["tools"] = json!([{"functionDeclarations":tools.iter().map(|tool| json!({
                    "name":tool["name"],"description":tool["description"],"parameters":google_schema(tool["inputSchema"].clone())
                })).collect::<Vec<_>>()}]);
            }
            body
        }
    }
}

fn google_tool_result(name: &str, id: Option<Value>, value: Value) -> Value {
    let mut response = json!({
        "name":name,
        "response":if value.is_object() { value } else { json!({"result":value}) }
    });
    if let Some(id) = id {
        response["id"] = id;
    }
    json!({"functionResponse":response})
}

async fn run(
    config: QuickAskConfig,
    mut conversation: Conversation,
    tools: Vec<Value>,
    publish: &impl Fn(LlmEvent),
    cancelled: Arc<AtomicBool>,
) -> Result<Conversation> {
    let key = resolve_api_key(None, || read_api_key(&config))?;
    let client = http_client(&config)?;
    let url = config.chat_endpoint_url()?;
    for round in 0..=config.max_tool_rounds {
        let body = request_body(&config, &conversation, &tools);
        let request = authenticated(
            client
                .post(url.clone())
                .json(&body)
                .header("accept", "text/event-stream"),
            config.provider,
            &key,
        );
        let response = request
            .send()
            .await
            .context(tr("quick_ask.request_failed"))?;
        ensure!(
            response.status().is_success(),
            "{}: HTTP {}",
            tr("quick_ask.request_failed"),
            response.status()
        );
        ensure!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.split(';').next() == Some("text/event-stream")),
            "{}",
            tr("quick_ask.invalid_stream")
        );
        let turn = read_stream(response, config.provider, publish).await?;
        if turn.calls.is_empty() {
            conversation
                .messages
                .push(turn.assistant_message(config.provider));
            return Ok(conversation);
        }
        ensure!(
            round < config.max_tool_rounds,
            "{}",
            tr("quick_ask.tool_limit")
        );
        // Only names advertised for this request are eligible, even if a new tool
        // was registered or enabled while the model was producing its response.
        ensure!(
            turn.calls.iter().all(|call| tools
                .iter()
                .any(|tool| tool["name"].as_str() == Some(&call.name))),
            "{}",
            tr("quick_ask.unknown_tool")
        );
        conversation
            .messages
            .push(turn.assistant_message(config.provider));
        let mut results = Vec::new();
        for call in turn.calls {
            let (reply, receive) = oneshot::channel();
            let google_id = turn.google_parts.iter().find_map(|part| {
                let function = &part["functionCall"];
                (function["id"].as_str() == Some(&call.id)).then(|| function["id"].clone())
            });
            let id = call.id.clone();
            let name = call.name.clone();
            publish(LlmEvent::Tool(PendingToolCall {
                call,
                reply,
                cancelled: cancelled.clone(),
            }));
            let result = receive.await.context(tr("quick_ask.tool_cancelled"))?;
            let failed = result.is_err();
            let value = match result {
                Ok(value) => value,
                Err(error) => json!({"error":error}),
            };
            match config.provider {
                QuickAskProvider::OpenAI | QuickAskProvider::OpenAiCompatible => conversation.messages.push(json!({"role":"tool","tool_call_id":id,"content":value.to_string()})),
                QuickAskProvider::Anthropic => results.push(json!({"type":"tool_result","tool_use_id":id,"content":value.to_string(),"is_error":failed})),
                QuickAskProvider::Google => results.push(google_tool_result(&name, google_id, value)),
            }
        }
        if matches!(
            config.provider,
            QuickAskProvider::Anthropic | QuickAskProvider::Google
        ) {
            conversation
                .messages
                .push(json!({"role":"user","content":results}));
        }
    }
    bail!("{}", tr("quick_ask.tool_limit"))
}

#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    data: String,
    skip_lf: bool,
}
impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.skip_lf = byte == b'\r';
                let line =
                    std::str::from_utf8(&self.line).context(tr("quick_ask.invalid_stream"))?;
                if line.is_empty() {
                    if !self.data.is_empty() {
                        self.data.pop();
                        events.push(std::mem::take(&mut self.data));
                    }
                } else if let Some(value) = line.strip_prefix("data:") {
                    self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
                    self.data.push('\n');
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
            ensure!(
                self.line.len() + self.data.len() <= MAX_EVENT_BYTES,
                "{}",
                tr("quick_ask.response_too_large")
            );
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<String>> {
        self.push(b"\n\n")
    }
}

#[derive(Default)]
struct CallFragments {
    id: String,
    name: String,
    arguments: String,
    initial: Option<Value>,
    argument_deltas: bool,
}
#[derive(Default)]
struct TurnBuilder {
    text: String,
    calls: BTreeMap<u64, CallFragments>,
    google_parts: Vec<Value>,
    thinking_blocks: BTreeMap<u64, Value>,
    finish: Option<String>,
    done: bool,
}
struct Turn {
    text: String,
    calls: Vec<ToolCall>,
    google_parts: Vec<Value>,
    thinking_blocks: Vec<Value>,
}
impl Turn {
    fn assistant_message(&self, provider: QuickAskProvider) -> Value {
        match provider {
            QuickAskProvider::OpenAI | QuickAskProvider::OpenAiCompatible => {
                let mut message = json!({"role":"assistant","content":self.text});
                if !self.calls.is_empty() {
                    message["tool_calls"] = json!(self.calls.iter().map(|call| json!({
                        "id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}
                    })).collect::<Vec<_>>());
                }
                message
            }
            QuickAskProvider::Anthropic => {
                // Signed thinking must be replayed during tool use, never shown as answer text.
                let mut content = self.thinking_blocks.clone();
                if !self.text.is_empty() {
                    content.push(json!({"type":"text","text":self.text}));
                }
                content.extend(self.calls.iter().map(|call| {
                    json!({
                        "type":"tool_use","id":call.id,"name":call.name,"input":call.arguments
                    })
                }));
                json!({"role":"assistant","content":content})
            }
            QuickAskProvider::Google => {
                // Keep part boundaries and thoughtSignature attached to their original parts.
                json!({"role":"assistant","content":self.google_parts})
            }
        }
    }
}

impl TurnBuilder {
    fn event(
        &mut self,
        provider: QuickAskProvider,
        event: &str,
        publish: &impl Fn(LlmEvent),
    ) -> Result<()> {
        if matches!(
            provider,
            QuickAskProvider::OpenAI | QuickAskProvider::OpenAiCompatible
        ) && event == "[DONE]"
        {
            self.done = true;
            if provider == QuickAskProvider::OpenAI {
                self.finish = Some("completed".into());
            }
            return Ok(());
        }
        let value: Value = serde_json::from_str(event).context(tr("quick_ask.invalid_stream"))?;
        ensure!(
            value.get("error").is_none() && value["type"] != "error",
            "{}",
            tr("quick_ask.provider_error")
        );
        match provider {
            QuickAskProvider::OpenAI => {
                ensure!(
                    value["type"] != "response.failed" && value["response"]["status"] != "failed",
                    "{}",
                    tr("quick_ask.provider_error")
                );
                match value["type"].as_str() {
                    Some("response.output_text.delta") => self.text(
                        value["delta"]
                            .as_str()
                            .context(tr("quick_ask.invalid_stream"))?,
                        publish,
                    ),
                    Some("response.output_item.added" | "response.output_item.done")
                        if value["item"]["type"] == "function_call" =>
                    {
                        let item = &value["item"];
                        let index = value["output_index"].as_u64().unwrap_or(0);
                        let entry = self.calls.entry(index).or_default();
                        if let Some(id) = item["call_id"].as_str().filter(|id| !id.is_empty()) {
                            entry.id = id.into();
                        } else if entry.id.is_empty()
                            && let Some(id) = item["id"].as_str()
                        {
                            entry.id = id.into();
                        }
                        if let Some(name) = item["name"].as_str() {
                            entry.name = name.into();
                        }
                        // The done event repeats the full argument string after its deltas.
                        if !entry.argument_deltas
                            && let Some(arguments) = item["arguments"]
                                .as_str()
                                .filter(|arguments| !arguments.is_empty())
                        {
                            entry.arguments = arguments.into();
                        }
                    }
                    Some("response.function_call_arguments.delta") => {
                        let index = value["output_index"].as_u64().unwrap_or(0);
                        let entry = self.calls.entry(index).or_default();
                        entry.arguments.push_str(
                            value["delta"]
                                .as_str()
                                .context(tr("quick_ask.invalid_stream"))?,
                        );
                        entry.argument_deltas = true;
                    }
                    Some("response.completed") => {
                        self.finish = Some("completed".into());
                        self.done = true;
                    }
                    _ => {}
                }
            }
            QuickAskProvider::OpenAiCompatible => {
                let Some(choices) = value["choices"].as_array() else {
                    bail!("{}", tr("quick_ask.invalid_stream"));
                };
                for choice in choices {
                    ensure!(
                        choice["index"].as_u64() == Some(0),
                        "{}",
                        tr("quick_ask.invalid_stream")
                    );
                    if let Some(text) = choice["delta"]["content"].as_str() {
                        self.text(text, publish);
                    }
                    if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
                        for call in calls {
                            let index = call["index"]
                                .as_u64()
                                .context(tr("quick_ask.invalid_stream"))?;
                            let entry = self.calls.entry(index).or_default();
                            if let Some(id) = call["id"].as_str() {
                                entry.id.push_str(id);
                            }
                            if let Some(name) = call["function"]["name"].as_str() {
                                entry.name.push_str(name);
                            }
                            if let Some(arguments) = call["function"]["arguments"].as_str() {
                                entry.arguments.push_str(arguments);
                            }
                        }
                    }
                    if let Some(reason) = choice["finish_reason"].as_str() {
                        self.finish = Some(reason.into());
                    }
                }
            }
            QuickAskProvider::Anthropic => match value["type"].as_str() {
                Some("content_block_start") => {
                    let block = &value["content_block"];
                    match block["type"].as_str() {
                        Some("text") => {
                            if let Some(text) = block["text"].as_str() {
                                self.text(text, publish);
                            }
                        }
                        Some("tool_use") => {
                            let index = value["index"]
                                .as_u64()
                                .context(tr("quick_ask.invalid_stream"))?;
                            let entry = CallFragments {
                                id: block["id"]
                                    .as_str()
                                    .context(tr("quick_ask.invalid_stream"))?
                                    .into(),
                                name: block["name"]
                                    .as_str()
                                    .context(tr("quick_ask.invalid_stream"))?
                                    .into(),
                                initial: Some(block["input"].clone()),
                                ..Default::default()
                            };
                            ensure!(
                                self.calls.insert(index, entry).is_none(),
                                "{}",
                                tr("quick_ask.invalid_stream")
                            );
                        }
                        Some("thinking" | "redacted_thinking") => {
                            let index = value["index"]
                                .as_u64()
                                .context(tr("quick_ask.invalid_stream"))?;
                            self.thinking_blocks.insert(index, block.clone());
                        }
                        _ => bail!("{}", tr("quick_ask.invalid_stream")),
                    }
                }
                Some("content_block_delta") => match value["delta"]["type"].as_str() {
                    Some("text_delta") => self.text(
                        value["delta"]["text"]
                            .as_str()
                            .context(tr("quick_ask.invalid_stream"))?,
                        publish,
                    ),
                    Some("input_json_delta") => {
                        let index = value["index"]
                            .as_u64()
                            .context(tr("quick_ask.invalid_stream"))?;
                        self.calls
                            .get_mut(&index)
                            .context(tr("quick_ask.invalid_stream"))?
                            .arguments
                            .push_str(
                                value["delta"]["partial_json"]
                                    .as_str()
                                    .context(tr("quick_ask.invalid_stream"))?,
                            );
                    }
                    Some("thinking_delta" | "signature_delta") => {
                        let index = value["index"]
                            .as_u64()
                            .context(tr("quick_ask.invalid_stream"))?;
                        let field = if value["delta"]["type"] == "thinking_delta" {
                            "thinking"
                        } else {
                            "signature"
                        };
                        let fragment = value["delta"][field]
                            .as_str()
                            .context(tr("quick_ask.invalid_stream"))?;
                        if let Some(block) = self.thinking_blocks.get_mut(&index) {
                            let mut text = block[field].as_str().unwrap_or_default().to_owned();
                            text.push_str(fragment);
                            block[field] = json!(text);
                        }
                    }
                    _ => bail!("{}", tr("quick_ask.invalid_stream")),
                },
                Some("message_delta") => {
                    self.finish = value["delta"]["stop_reason"].as_str().map(str::to_owned);
                }
                Some("message_stop") => self.done = true,
                Some("message_start" | "content_block_stop" | "ping") => {}
                _ => bail!("{}", tr("quick_ask.invalid_stream")),
            },
            QuickAskProvider::Google => {
                ensure!(
                    value["promptFeedback"]["blockReason"].is_null(),
                    "{}",
                    tr("quick_ask.provider_error")
                );
                let candidates = value["candidates"]
                    .as_array()
                    .context(tr("quick_ask.invalid_stream"))?;
                if let Some(candidate) = candidates.first() {
                    if let Some(parts) = candidate["content"]["parts"].as_array() {
                        for part in parts {
                            if part["thought"] != true
                                && let Some(text) = part["text"].as_str()
                            {
                                self.text(text, publish);
                            }
                            if let Some(call) = part.get("functionCall") {
                                let index = self.calls.len() as u64;
                                let entry = CallFragments {
                                    id: call["id"]
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| format!("gemini-call-{index}")),
                                    name: call["name"]
                                        .as_str()
                                        .context(tr("quick_ask.invalid_stream"))?
                                        .into(),
                                    initial: Some(
                                        call.get("args").cloned().unwrap_or_else(|| json!({})),
                                    ),
                                    ..Default::default()
                                };
                                self.calls.insert(index, entry);
                            }
                            self.google_parts.push(part.clone());
                        }
                    }
                    if let Some(reason) = candidate["finishReason"].as_str() {
                        ensure!(
                            matches!(reason, "STOP" | "MAX_TOKENS"),
                            "{}: {reason}",
                            tr("quick_ask.provider_error")
                        );
                        self.finish = Some(reason.into());
                        self.done = true;
                    }
                }
            }
        }
        ensure!(self.calls.len() <= 64, "{}", tr("quick_ask.tool_limit"));
        Ok(())
    }
    fn text(&mut self, text: &str, publish: &impl Fn(LlmEvent)) {
        if !text.is_empty() {
            self.text.push_str(text);
            publish(LlmEvent::Text(text.into()));
        }
    }
    fn finish(self, provider: QuickAskProvider) -> Result<Turn> {
        let expected = match (provider, self.calls.is_empty()) {
            (QuickAskProvider::OpenAI, _) => &["completed"][..],
            (QuickAskProvider::OpenAiCompatible, true) => &["stop", "length"][..],
            (QuickAskProvider::OpenAiCompatible, false) => &["tool_calls"][..],
            (QuickAskProvider::Anthropic, true) => &["end_turn", "stop_sequence", "max_tokens"][..],
            (QuickAskProvider::Anthropic, false) => &["tool_use"][..],
            (QuickAskProvider::Google, _) => &["STOP", "MAX_TOKENS"][..],
        };
        let google_eof = provider == QuickAskProvider::Google
            && self.finish.is_none()
            && (!self.text.is_empty() || !self.calls.is_empty());
        ensure!(
            google_eof
                || (self.done
                    && self
                        .finish
                        .as_ref()
                        .is_some_and(|reason| expected.contains(&reason.as_str()))),
            "{}",
            tr("quick_ask.incomplete_stream")
        );
        let mut ids = std::collections::BTreeSet::new();
        let calls = self
            .calls
            .into_values()
            .map(|call| {
                ensure!(
                    !call.id.is_empty() && ids.insert(call.id.clone()) && !call.name.is_empty(),
                    "{}",
                    tr("quick_ask.invalid_stream")
                );
                let arguments = if call.arguments.is_empty() {
                    call.initial
                        .context(tr("quick_ask.invalid_tool_arguments"))?
                } else {
                    serde_json::from_str(&call.arguments)
                        .context(tr("quick_ask.invalid_tool_arguments"))?
                };
                ensure!(
                    arguments.is_object(),
                    "{}",
                    tr("quick_ask.invalid_tool_arguments")
                );
                Ok(ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Turn {
            text: self.text,
            calls,
            google_parts: self.google_parts,
            thinking_blocks: self.thinking_blocks.into_values().collect(),
        })
    }
}

async fn read_stream(
    response: reqwest::Response,
    provider: QuickAskProvider,
    publish: &impl Fn(LlmEvent),
) -> Result<Turn> {
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut turn = TurnBuilder::default();
    let mut bytes = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context(tr("quick_ask.request_failed"))?;
        bytes += chunk.len();
        ensure!(
            bytes <= MAX_STREAM_BYTES,
            "{}",
            tr("quick_ask.response_too_large")
        );
        for event in decoder.push(&chunk)? {
            turn.event(provider, &event, publish)?;
            if turn.done {
                return turn.finish(provider);
            }
        }
    }
    for event in decoder.finish()? {
        turn.event(provider, &event, publish)?;
    }
    turn.finish(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        io::{Read, Write},
        net::TcpListener,
    };

    const PROVIDERS: [QuickAskProvider; 4] = [
        QuickAskProvider::OpenAI,
        QuickAskProvider::OpenAiCompatible,
        QuickAskProvider::Anthropic,
        QuickAskProvider::Google,
    ];

    fn provider_config(provider: QuickAskProvider) -> QuickAskConfig {
        QuickAskConfig {
            provider,
            base_url: provider.default_base_url().into(),
            model: provider.default_model().into(),
            credential_target: provider.default_credential_target().into(),
            ..Default::default()
        }
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    fn parse_sse(
        provider: QuickAskProvider,
        input: &str,
        chunk_size: usize,
    ) -> Result<(Turn, Vec<String>)> {
        let text = RefCell::new(Vec::new());
        let publish = |event| {
            if let LlmEvent::Text(delta) = event {
                text.borrow_mut().push(delta);
            }
        };
        let mut decoder = SseDecoder::default();
        let mut turn = TurnBuilder::default();
        for chunk in input.as_bytes().chunks(chunk_size) {
            for event in decoder.push(chunk)? {
                turn.event(provider, &event, &publish)?;
            }
        }
        for event in decoder.finish()? {
            turn.event(provider, &event, &publish)?;
        }
        Ok((turn.finish(provider)?, text.into_inner()))
    }

    fn serve_response(
        status: u16,
        content_type: &str,
        body: String,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let content_type = content_type.to_owned();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            let header_end = request
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap()
                + 4;
            let content_length = std::str::from_utf8(&request[..header_end])
                .unwrap()
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, length)| length.trim().parse::<usize>().unwrap());
            while request.len() < header_end + content_length {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, worker)
    }

    #[test]
    fn openai_and_anthropic_endpoints_accept_roots_prefixes_and_full_urls() {
        for provider in [
            QuickAskProvider::OpenAI,
            QuickAskProvider::OpenAiCompatible,
            QuickAskProvider::Anthropic,
        ] {
            let suffix = match provider {
                QuickAskProvider::OpenAI => "responses",
                QuickAskProvider::OpenAiCompatible => "chat/completions",
                QuickAskProvider::Anthropic => "messages",
                QuickAskProvider::Google => unreachable!(),
            };
            for (base, prefix) in [
                ("https://example.test".to_owned(), "/v1"),
                ("https://example.test/".to_owned(), "/v1"),
                ("https://example.test/v1/".to_owned(), "/v1"),
                ("https://example.test/gateway/v2".to_owned(), "/gateway/v2"),
                (
                    format!("https://example.test/gateway/v2/{suffix}/"),
                    "/gateway/v2",
                ),
                (
                    "https://example.test/gateway/v2/models".to_owned(),
                    "/gateway/v2",
                ),
                (format!("https://example.test/{suffix}"), ""),
                ("https://example.test/models".to_owned(), ""),
            ] {
                let mut config = provider_config(provider);
                config.base_url = base;
                assert_eq!(
                    config.chat_endpoint_url().unwrap().as_str(),
                    format!("https://example.test{prefix}/{suffix}")
                );
                assert_eq!(
                    config.models_endpoint_url().unwrap().as_str(),
                    format!("https://example.test{prefix}/models")
                );
            }
        }
    }

    #[test]
    fn google_endpoints_normalize_model_names_and_reuse_full_endpoint() {
        let mut config = provider_config(QuickAskProvider::Google);
        config.model = "  models/gemini-2.5-flash  ".into();
        for (base, prefix) in [
            ("https://example.test", "/v1beta"),
            ("https://example.test/", "/v1beta"),
            ("https://example.test/v1beta/", "/v1beta"),
            ("https://example.test/proxy/v1/models/", "/proxy/v1"),
            ("https://example.test/models", ""),
        ] {
            config.base_url = base.into();
            assert_eq!(
                config.chat_endpoint_url().unwrap().as_str(),
                format!(
                    "https://example.test{prefix}/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
                )
            );
            assert_eq!(
                config.models_endpoint_url().unwrap().as_str(),
                format!("https://example.test{prefix}/models")
            );
        }
        config.base_url =
            "https://example.test/proxy/v1beta/models/custom:streamGenerateContent?alt=sse".into();
        assert_eq!(
            config.chat_endpoint_url().unwrap().as_str(),
            config.base_url
        );
        assert_eq!(
            config.models_endpoint_url().unwrap().as_str(),
            "https://example.test/proxy/v1beta/models"
        );
        config.base_url = "https://example.test/models/custom:streamGenerateContent".into();
        assert_eq!(
            config.chat_endpoint_url().unwrap().as_str(),
            "https://example.test/models/custom:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            config.models_endpoint_url().unwrap().as_str(),
            "https://example.test/models"
        );
        config.model.clear();
        assert!(config.models_endpoint_url().is_ok());
        config.base_url = "https://example.test".into();
        assert!(config.chat_endpoint_url().is_err());
    }

    #[test]
    fn endpoints_reject_non_http_credentials_fragments_and_path_injection() {
        let mut config = provider_config(QuickAskProvider::Google);
        for base in [
            "file:///tmp/key",
            "ftp://example.test",
            "https://user:secret@example.test",
            "https://example.test/#fragment",
            "https://exa\nmple.test",
        ] {
            config.base_url = base.into();
            assert!(config.models_endpoint_url().is_err(), "{base}");
        }
        config.base_url = "https://example.test".into();
        for model in [
            "",
            "models/",
            "../other",
            "model?key=value",
            "model#fragment",
        ] {
            config.model = model.into();
            assert!(config.chat_endpoint_url().is_err(), "{model}");
        }
    }

    #[test]
    fn each_provider_receives_its_authentication_headers() {
        let client = reqwest::Client::new();
        for provider in PROVIDERS {
            let request = authenticated(client.get("https://example.test"), provider, "test-key")
                .build()
                .unwrap();
            let headers = request.headers();
            assert_eq!(headers["authorization"], "Bearer test-key");
            assert_eq!(
                headers.get("x-api-key").is_some(),
                provider == QuickAskProvider::Anthropic
            );
            assert_eq!(
                headers.get("x-goog-api-key").is_some(),
                provider == QuickAskProvider::Google
            );
            if provider == QuickAskProvider::Anthropic {
                assert_eq!(headers["x-api-key"], "test-key");
                assert_eq!(headers["anthropic-version"], "2023-06-01");
            }
            if provider == QuickAskProvider::Google {
                assert_eq!(headers["x-goog-api-key"], "test-key");
            }
        }
    }

    #[test]
    fn unsaved_key_overrides_storage_only_when_nonempty() {
        assert_eq!(
            resolve_api_key(Some(" draft-key "), || panic!("must not read storage")).unwrap(),
            "draft-key"
        );
        for override_key in [None, Some(""), Some(" \t ")] {
            assert_eq!(
                resolve_api_key(override_key, || Ok(Some("saved-key".into()))).unwrap(),
                "saved-key"
            );
            assert!(resolve_api_key(override_key, || Ok(None)).is_err());
            assert!(
                resolve_api_key(override_key, || bail!("storage denied"))
                    .unwrap_err()
                    .to_string()
                    .contains("storage denied")
            );
        }
    }

    #[test]
    fn model_lists_are_trimmed_deduplicated_sorted_and_nonempty() {
        for provider in PROVIDERS {
            let body = if provider == QuickAskProvider::Google {
                json!({"models":[{"name":"models/z"},{"name":" models/a "},{"name":"a"},{"name":""},{"name":"models/"},{}]})
            } else {
                json!({"data":[{"id":"z"},{"id":" a "},{"id":"a"},{"id":""},{}]})
            };
            assert_eq!(model_names(provider, &body).unwrap(), ["a", "z"]);
            for body in [
                json!({}),
                json!({"data":[],"models":[]}),
                json!({"data":[{"id":null}],"models":[{"name":""}]}),
                json!({"error":{"message":"denied"}}),
            ] {
                assert!(model_names(provider, &body).is_err());
            }
        }
    }

    #[test]
    fn only_google_model_resource_names_lose_the_models_prefix() {
        for provider in [
            QuickAskProvider::OpenAI,
            QuickAskProvider::OpenAiCompatible,
            QuickAskProvider::Anthropic,
        ] {
            assert_eq!(
                model_names(
                    provider,
                    &json!({"data":[{"id":"models/foo"},{"id":"foo"}]})
                )
                .unwrap(),
                ["foo", "models/foo"]
            );
        }
        assert_eq!(
            model_names(
                QuickAskProvider::Google,
                &json!({"models":[{"name":"models/foo"},{"name":"foo"}]})
            )
            .unwrap(),
            ["foo"]
        );
    }

    #[test]
    fn fetch_models_uses_unsaved_key_and_allows_an_empty_model_draft() {
        for provider in PROVIDERS {
            let body = if provider == QuickAskProvider::Google {
                json!({"models":[{"name":"models/z"},{"name":"models/a"},{"name":"models/a"}]})
            } else {
                json!({"data":[{"id":"z"},{"id":"a"},{"id":"a"}]})
            };
            let (url, worker) = serve_response(200, "application/json", body.to_string());
            let mut config = provider_config(provider);
            config.base_url = url;
            config.model.clear();
            config.credential_target = "\0".into();
            assert_eq!(
                fetch_models(&config, Some("draft-key")).unwrap(),
                ["a", "z"]
            );
            let request = worker.join().unwrap().to_ascii_lowercase();
            let prefix = if provider == QuickAskProvider::Google {
                "/v1beta"
            } else {
                "/v1"
            };
            assert!(request.starts_with(&format!("get {prefix}/models http/1.1\r\n")));
            assert!(request.contains("authorization: bearer draft-key\r\n"));
            assert!(!request.contains("draft-key?"));
        }
    }

    #[test]
    fn fetch_models_rejects_http_errors_empty_lists_and_bad_json() {
        for (status, body) in [
            (403, r#"{"error":"denied"}"#),
            (200, r#"{"data":[]}"#),
            (200, "not json"),
        ] {
            let (url, worker) = serve_response(status, "application/json", body.into());
            let config = QuickAskConfig {
                base_url: url,
                ..Default::default()
            };
            assert!(fetch_models(&config, Some("draft-key")).is_err());
            worker.join().unwrap();
        }
    }

    fn sample_tool() -> Value {
        json!({
            "name":"file_search_query",
            "description":"Find a file",
            "inputSchema":{
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "additionalProperties":false,
                "properties":{"filters":{"type":"array","items":{
                    "type":"object","additionalProperties":false,
                    "properties":{"name":{"type":"string"}},
                    "allOf":[{"$schema":"ignored","additionalProperties":false,"type":"object"}]
                }}},
                "required":["filters"]
            }
        })
    }

    #[test]
    fn bodies_convert_system_images_and_tool_declarations_for_each_provider() {
        let tool = sample_tool();
        for provider in PROVIDERS {
            let mut config = provider_config(provider);
            config.system_prompt = "Be concise".into();
            let mut conversation = Conversation::default();
            conversation.begin_turn(provider, &selected_context(), "question");
            let body = request_body(&config, &conversation, std::slice::from_ref(&tool));
            match provider {
                QuickAskProvider::OpenAI => {
                    assert_eq!(body["instructions"], "Be concise");
                    assert_eq!(body["input"][0]["type"], "message");
                    assert_eq!(body["input"][0]["role"], "user");
                    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
                    assert_eq!(
                        body["input"][0]["content"][1],
                        json!({"type":"input_image","image_url":"data:image/png;base64,AQID"})
                    );
                    assert_eq!(
                        body["tools"][0],
                        json!({
                            "type":"function","name":tool["name"],"description":tool["description"],"parameters":tool["inputSchema"]
                        })
                    );
                    assert_eq!(body["max_output_tokens"], config.max_tokens);
                    assert_eq!(body["stream"], true);
                    assert!(body.get("messages").is_none());
                    assert!(body.get("max_tokens").is_none());
                }
                QuickAskProvider::OpenAiCompatible => {
                    assert_eq!(
                        body["messages"][0],
                        json!({"role":"system","content":"Be concise"})
                    );
                    assert_eq!(
                        body["messages"][1]["content"][1]["image_url"]["url"],
                        "data:image/png;base64,AQID"
                    );
                    assert_eq!(
                        body["tools"][0]["function"]["parameters"],
                        tool["inputSchema"]
                    );
                    assert_eq!(body["tools"][0]["type"], "function");
                    assert_eq!(body["stream"], true);
                }
                QuickAskProvider::Anthropic => {
                    assert_eq!(body["system"], "Be concise");
                    assert_eq!(
                        body["messages"][0]["content"][1]["source"],
                        json!({"type":"base64","media_type":"image/png","data":"AQID"})
                    );
                    assert_eq!(body["tools"][0]["input_schema"], tool["inputSchema"]);
                    assert_eq!(body["stream"], true);
                }
                QuickAskProvider::Google => {
                    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be concise");
                    assert_eq!(body["contents"][0]["role"], "user");
                    assert!(body["contents"][0]["parts"][0].get("type").is_none());
                    assert_eq!(
                        body["contents"][0]["parts"][1],
                        json!({"inlineData":{"mimeType":"image/png","data":"AQID"}})
                    );
                    let parameters = &body["tools"][0]["functionDeclarations"][0]["parameters"];
                    assert!(parameters.get("$schema").is_none());
                    assert!(parameters.get("additionalProperties").is_none());
                    assert!(
                        parameters["properties"]["filters"]["items"]
                            .get("additionalProperties")
                            .is_none()
                    );
                    assert_eq!(
                        parameters["properties"]["filters"]["items"]["allOf"][0],
                        json!({"type":"object"})
                    );
                    assert_eq!(parameters["required"], json!(["filters"]));
                    assert_eq!(
                        body["generationConfig"]["maxOutputTokens"],
                        config.max_tokens
                    );
                }
            }
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert!(
                request_body(&config, &conversation, &[])
                    .get("tools")
                    .is_none()
            );
        }
    }

    #[test]
    fn thinking_levels_map_to_provider_controls_and_safe_anthropic_token_budgets() {
        for (effort, openai, budget, google) in [
            (ThinkingEffort::Low, "low", 2048u32, "LOW"),
            (ThinkingEffort::Medium, "medium", 8192, "MEDIUM"),
            (ThinkingEffort::High, "high", 16384, "HIGH"),
            (ThinkingEffort::Max, "xhigh", 32000, "HIGH"),
        ] {
            for provider in PROVIDERS {
                let mut config = provider_config(provider);
                config.thinking_effort = effort;
                config.max_tokens = 100;
                config.model = "gemini-3-pro-preview".into();
                let body = request_body(&config, &Conversation::default(), &[]);
                match provider {
                    QuickAskProvider::OpenAI => {
                        assert_eq!(body["reasoning"], json!({"effort":openai}));
                        assert!(body.get("reasoning_effort").is_none());
                    }
                    QuickAskProvider::OpenAiCompatible => {
                        assert_eq!(body["reasoning_effort"], openai)
                    }
                    QuickAskProvider::Anthropic => {
                        assert_eq!(
                            body["thinking"],
                            json!({"type":"enabled","budget_tokens":budget})
                        );
                        assert_eq!(body["max_tokens"], budget + 4096);
                        config.max_tokens = 64000;
                        assert_eq!(
                            request_body(&config, &Conversation::default(), &[])["max_tokens"],
                            64000
                        );
                    }
                    QuickAskProvider::Google => {
                        assert_eq!(
                            body["generationConfig"]["thinkingConfig"],
                            json!({"thinkingLevel":google})
                        );
                        config.model = "models/gemini-2.5-flash".into();
                        assert_eq!(
                            request_body(&config, &Conversation::default(), &[])["generationConfig"]
                                ["thinkingConfig"],
                            json!({"thinkingBudget":budget.min(24576)})
                        );
                        config.model = "custom-2.5-preview".into();
                        assert_eq!(
                            request_body(&config, &Conversation::default(), &[])["generationConfig"]
                                ["thinkingConfig"],
                            json!({"thinkingLevel":google})
                        );
                    }
                }
                config.thinking_effort = ThinkingEffort::Off;
                let body = request_body(&config, &Conversation::default(), &[]);
                assert!(body.get("reasoning_effort").is_none());
                assert!(body.get("reasoning").is_none());
                assert!(body.get("thinking").is_none());
                assert!(body["generationConfig"].get("thinkingConfig").is_none());
            }
        }
    }

    #[test]
    fn responses_stream_publishes_only_text_deltas_and_requires_completion() {
        let events = [
            json!({"type":"response.created","response":{"status":"in_progress"}}),
            json!({"type":"response.in_progress","response":{"status":"in_progress"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs-1","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","delta":"private reasoning"}),
            json!({"type":"response.content_part.added","part":{"type":"output_text","text":""}}),
            json!({"type":"response.output_text.delta","delta":"Hello "}),
            json!({"type":"response.output_text.delta","delta":"世界"}),
            json!({"type":"response.output_text.done","text":"Hello 世界"}),
            json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello 世界"}]}}),
        ];
        let partial = sse(&events);
        assert!(parse_sse(QuickAskProvider::OpenAI, &partial, 1).is_err());
        for completion in [
            sse(&[
                json!({"type":"response.completed","response":{"status":"completed","usage":{"total_tokens":42}}}),
            ]),
            "data: [DONE]\n\n".into(),
        ] {
            let (turn, displayed) = parse_sse(
                QuickAskProvider::OpenAI,
                &(partial.clone() + &completion),
                1,
            )
            .unwrap();
            assert_eq!(turn.text, "Hello 世界");
            assert_eq!(displayed, ["Hello ", "世界"]);
            assert!(turn.calls.is_empty());
        }
    }

    #[test]
    fn responses_stream_reassembles_parallel_calls_without_repeating_final_arguments() {
        let stream = sse(&[
            json!({"type":"response.output_item.added","output_index":2,"item":{
                "type":"function_call","id":"fc-2","call_id":"call-2","name":"search","arguments":"{\"query\":"
            }}),
            json!({"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","id":"fc-0","name":"search","arguments":"{}"
            }}),
            json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"\"second\"}"}),
            json!({"type":"response.function_call_arguments.done","output_index":2,"arguments":"{\"query\":\"second\"}"}),
            json!({"type":"response.output_item.done","output_index":2,"item":{
                "type":"function_call","id":"fc-2","arguments":"{\"query\":\"must not replace deltas\"}"
            }}),
            json!({"type":"response.output_item.done","output_index":0,"item":{
                "type":"function_call","id":"fc-0","call_id":"call-0","name":"search","arguments":"{\"query\":\"first\"}"
            }}),
            json!({"type":"response.output_item.done","output_index":4,"item":{
                "type":"function_call","id":"fc-4","name":"search","arguments":"{}"
            }}),
            json!({"type":"response.completed","response":{"status":"completed"}}),
        ]);
        let (turn, displayed) = parse_sse(QuickAskProvider::OpenAI, &stream, 2).unwrap();
        assert!(displayed.is_empty());
        assert_eq!(turn.calls.len(), 3);
        assert_eq!(turn.calls[0].id, "call-0");
        assert_eq!(turn.calls[0].arguments, json!({"query":"first"}));
        assert_eq!(turn.calls[1].id, "call-2");
        assert_eq!(turn.calls[1].arguments, json!({"query":"second"}));
        assert_eq!(turn.calls[2].id, "fc-4");
        assert_eq!(turn.calls[2].arguments, json!({}));
    }

    #[test]
    fn responses_calls_accept_default_index_late_metadata_and_complete_added_arguments() {
        let late = sse(&[
            json!({"type":"response.function_call_arguments.delta","delta":"{\"query\":"}),
            json!({"type":"response.function_call_arguments.delta","delta":"\"report\"}"}),
            json!({"type":"response.output_item.done","item":{
                "type":"function_call","id":"fc-1","call_id":"call-1","name":"search","arguments":"{\"query\":\"report\"}"
            }}),
        ]) + "data: [DONE]\n\n";
        let (turn, _) = parse_sse(QuickAskProvider::OpenAI, &late, 3).unwrap();
        assert_eq!(turn.calls[0].id, "call-1");
        assert_eq!(turn.calls[0].name, "search");
        assert_eq!(turn.calls[0].arguments, json!({"query":"report"}));
        let added = sse(&[
            json!({"type":"response.output_item.added","item":{
                "type":"function_call","id":"fc-2","call_id":"","name":"search","arguments":"{}"
            }}),
            json!({"type":"response.completed"}),
        ]);
        let (turn, _) = parse_sse(QuickAskProvider::OpenAI, &added, 3).unwrap();
        assert_eq!(turn.calls[0].id, "fc-2");
        assert_eq!(turn.calls[0].arguments, json!({}));
    }

    #[test]
    fn responses_failures_and_invalid_calls_cannot_complete_or_dispatch() {
        for event in [
            json!({"type":"response.failed","response":{"status":"failed","error":{"message":"denied"}}}),
            json!({"type":"error","message":"denied"}),
            json!({"error":{"message":"denied"}}),
            json!({"type":"response.completed","response":{"status":"failed"}}),
            json!({"type":"response.in_progress","response":{"status":"failed"}}),
        ] {
            let stream = sse(&[
                json!({"type":"response.output_text.delta","delta":"partial"}),
                event,
            ]) + "data: [DONE]\n\n";
            assert!(parse_sse(QuickAskProvider::OpenAI, &stream, 1).is_err());
        }
        for item in [
            json!({"type":"function_call","call_id":"call-1","name":"search","arguments":"{\"query\":"}),
            json!({"type":"function_call","call_id":"call-1","name":"search","arguments":"[]"}),
            json!({"type":"function_call","call_id":"call-1","name":"search","arguments":"null"}),
            json!({"type":"function_call","call_id":"call-1","name":"search","arguments":""}),
            json!({"type":"function_call","name":"search","arguments":"{}"}),
            json!({"type":"function_call","call_id":"call-1","arguments":"{}"}),
        ] {
            let stream = sse(&[
                json!({"type":"response.output_item.done","item":item}),
                json!({"type":"response.completed"}),
            ]);
            assert!(parse_sse(QuickAskProvider::OpenAI, &stream, 3).is_err());
        }
        for (count, same_id) in [(2, true), (65, false)] {
            let mut events: Vec<_> = (0..count).map(|index| json!({
                "type":"response.output_item.added","output_index":index,
                "item":{"type":"function_call","call_id":if same_id { "same".into() } else { format!("call-{index}") },"name":"search","arguments":"{}"}
            })).collect();
            events.push(json!({"type":"response.completed"}));
            assert!(parse_sse(QuickAskProvider::OpenAI, &sse(&events), 7).is_err());
        }
    }

    #[test]
    fn responses_tool_continuation_uses_separate_call_and_output_items() {
        let stream = sse(&[
            json!({"type":"response.output_text.delta","delta":"Checking files"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{
                "type":"function_call","id":"fc-1","call_id":"call-1","name":"file_search_query","arguments":""
            }}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"query\":\"report\"}"}),
            json!({"type":"response.output_item.done","output_index":1,"item":{
                "type":"function_call","id":"fc-1","call_id":"call-1","name":"file_search_query","arguments":"{\"query\":\"report\"}"
            }}),
            json!({"type":"response.completed"}),
        ]);
        let (turn, _) = parse_sse(QuickAskProvider::OpenAI, &stream, 5).unwrap();
        let mut conversation = Conversation::default();
        conversation.begin_turn(QuickAskProvider::OpenAI, &selected_context(), "first");
        conversation = conversation.request_conversation();
        conversation
            .messages
            .push(turn.assistant_message(QuickAskProvider::OpenAI));
        conversation.messages.push(json!({
            "role":"tool","tool_call_id":turn.calls[0].id,"content":json!({"matches":["report.txt"]}).to_string()
        }));
        conversation.push_assistant("Found report.txt".into());
        conversation.begin_turn(QuickAskProvider::OpenAI, &selected_context(), "followup");
        let mut config = provider_config(QuickAskProvider::OpenAI);
        config.system_prompt.clear();
        config.thinking_effort = ThinkingEffort::Off;
        let body = request_body(&config, &conversation, &[]);
        assert!(body.get("instructions").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("tools").is_none());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 6);
        assert_eq!(input[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(
            &input[1..],
            &[
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking files"}]}),
                json!({"type":"function_call","call_id":"call-1","name":"file_search_query","arguments":"{\"query\":\"report\"}"}),
                json!({"type":"function_call_output","call_id":"call-1","output":"{\"matches\":[\"report.txt\"]}"}),
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Found report.txt"}]}),
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"followup"}]}),
            ]
        );
        let tool_only = Conversation {
            messages: vec![json!({"role":"assistant","content":"","tool_calls":[{
                "id":"call-2","type":"function","function":{"name":"search","arguments":"{}"}
            }]})],
            pending: None,
        };
        assert_eq!(
            request_body(&config, &tool_only, &[])["input"],
            json!([{"type":"function_call","call_id":"call-2","name":"search","arguments":"{}"}])
        );
    }

    #[test]
    fn responses_cancelled_and_failed_turns_keep_only_displayed_text_and_original_image() {
        let config = provider_config(QuickAskProvider::OpenAI);
        for failed in [false, true] {
            let mut conversation = Conversation::default();
            conversation.begin_turn(config.provider, &selected_context(), "first");
            conversation.push_text("partial answer");
            if failed {
                conversation.fail_turn();
            } else {
                conversation.cancel_turn();
            }
            conversation.begin_turn(config.provider, &selected_context(), "followup");
            let body = request_body(&config, &conversation, &[]);
            assert_eq!(body["input"].as_array().unwrap().len(), 3);
            assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
            assert_eq!(
                body["input"][1],
                json!({
                    "type":"message","role":"assistant","content":[{"type":"output_text","text":"partial answer"}]
                })
            );
            assert_eq!(
                body["input"][2],
                json!({
                    "type":"message","role":"user","content":[{"type":"input_text","text":"followup"}]
                })
            );
        }
        let mut conversation = Conversation::default();
        conversation.begin_turn(config.provider, &selected_context(), "failed before text");
        conversation.fail_turn();
        conversation.begin_turn(config.provider, &selected_context(), "retry");
        let input = request_body(&config, &conversation, &[])["input"].clone();
        assert_eq!(input.as_array().unwrap().len(), 1);
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert!(
            input[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .ends_with("retry")
        );
    }

    #[test]
    fn official_and_compatible_requests_use_distinct_wire_protocols() {
        for provider in [QuickAskProvider::OpenAI, QuickAskProvider::OpenAiCompatible] {
            let response = if provider == QuickAskProvider::OpenAI {
                sse(&[
                    json!({"type":"response.output_text.delta","delta":"answer"}),
                    json!({"type":"response.completed","response":{"status":"completed"}}),
                ])
            } else {
                sse(&[
                    json!({"choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}]}),
                ]) + "data: [DONE]\n\n"
            };
            let (url, worker) = serve_response(200, "text/event-stream", response);
            let mut config = provider_config(provider);
            config.base_url = url;
            config.model = " test-model ".into();
            config.max_tokens = 1234;
            config.system_prompt = "Keep instructions separate".into();
            config.thinking_effort = ThinkingEffort::High;
            let mut conversation = Conversation::default();
            conversation.begin_turn(provider, &selected_context(), "question");
            let tool = sample_tool();
            let body = request_body(&config, &conversation, std::slice::from_ref(&tool));
            let turn = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let response = authenticated(
                        http_client(&config)
                            .unwrap()
                            .post(config.chat_endpoint_url().unwrap())
                            .json(&body)
                            .header("accept", "text/event-stream"),
                        provider,
                        "unsaved-test-key",
                    )
                    .send()
                    .await
                    .unwrap();
                    read_stream(response, provider, &|_| {}).await.unwrap()
                });
            assert_eq!(turn.text, "answer");
            let request = worker.join().unwrap();
            let (headers, encoded) = request.split_once("\r\n\r\n").unwrap();
            assert!(headers.lines().any(|line| {
                line.eq_ignore_ascii_case("authorization: Bearer unsaved-test-key")
            }));
            let sent: Value = serde_json::from_str(encoded).unwrap();
            assert_eq!(sent["model"], "test-model");
            assert_eq!(sent["stream"], true);
            if provider == QuickAskProvider::OpenAI {
                assert!(headers.starts_with("POST /v1/responses HTTP/1.1\r\n"));
                assert_eq!(sent["max_output_tokens"], 1234);
                assert_eq!(sent["instructions"], config.system_prompt);
                assert_eq!(sent["reasoning"], json!({"effort":"high"}));
                assert_eq!(sent["tools"][0]["parameters"], tool["inputSchema"]);
                assert!(sent["tools"][0].get("function").is_none());
                assert_eq!(sent["input"][0]["content"][1]["type"], "input_image");
                assert!(sent.get("messages").is_none());
                assert!(sent.get("max_tokens").is_none());
            } else {
                assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
                assert_eq!(sent["max_tokens"], 1234);
                assert_eq!(sent["reasoning_effort"], "high");
                assert_eq!(sent["messages"][0]["role"], "system");
                assert_eq!(sent["messages"][1]["content"][1]["type"], "image_url");
                assert_eq!(
                    sent["tools"][0]["function"]["parameters"],
                    tool["inputSchema"]
                );
                assert!(sent.get("input").is_none());
                assert!(sent.get("max_output_tokens").is_none());
            }
        }
    }

    #[test]
    fn compatible_stream_ignores_reasoning_and_usage_and_accepts_length_finish() {
        for reason in ["stop", "length"] {
            let stream = sse(&[
                json!({"choices":[{"index":0,"delta":{"reasoning_content":"hidden"}}]}),
                json!({"choices":[{"index":0,"delta":{"content":"Hello 世界"}}]}),
                json!({"choices":[],"usage":{"total_tokens":42}}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]}),
            ]) + "data: [DONE]\n\n";
            let provider = QuickAskProvider::OpenAiCompatible;
            let (turn, displayed) = parse_sse(provider, &stream, 1).unwrap();
            assert_eq!(turn.text, "Hello 世界");
            assert_eq!(displayed, ["Hello 世界"]);
            assert!(turn.calls.is_empty());
            assert!(parse_sse(provider, stream.trim_end_matches("data: [DONE]\n\n"), 3).is_err());
        }
    }

    #[test]
    fn compatible_stream_reassembles_parallel_tool_calls_by_index() {
        let stream = sse(&[
            json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":1,"id":"second","function":{"name":"file_search_query","arguments":"{\"query\":"}},
                {"index":0,"id":"first","function":{"name":"file_search_query","arguments":"{\"query\":\"a\"}"}}
            ]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"\"b\"}"}}]},"finish_reason":"tool_calls"}]}),
        ]) + "data: [DONE]\n\n";
        let provider = QuickAskProvider::OpenAiCompatible;
        let (turn, displayed) = parse_sse(provider, &stream, 2).unwrap();
        assert!(displayed.is_empty());
        assert_eq!(turn.calls[0].id, "first");
        assert_eq!(turn.calls[1].arguments, json!({"query":"b"}));
        let message = turn.assistant_message(provider);
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"query\":\"a\"}"
        );
        assert!(
            parse_sse(
                provider,
                &stream.replace(
                    "\"finish_reason\":\"tool_calls\"",
                    "\"finish_reason\":\"length\""
                ),
                3
            )
            .is_err()
        );
    }

    #[test]
    fn anthropic_thinking_and_signatures_are_hidden_but_replayed_with_tools() {
        let stream = sse(&[
            json!({"type":"message_start","message":{}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hidden"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"end"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":"Checking "}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"files"}}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"tool-1","name":"file_search_query","input":{}}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"\"report\"}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
            json!({"type":"message_stop"}),
        ]);
        let (turn, displayed) = parse_sse(QuickAskProvider::Anthropic, &stream, 7).unwrap();
        assert_eq!(displayed, ["Checking ", "files"]);
        assert_eq!(turn.calls[0].arguments, json!({"query":"report"}));
        let message = turn.assistant_message(QuickAskProvider::Anthropic);
        assert_eq!(
            message["content"][0],
            json!({"type":"thinking","thinking":"hidden","signature":"sig-end"})
        );
        assert_eq!(
            message["content"][1],
            json!({"type":"redacted_thinking","data":"opaque"})
        );
        assert_eq!(
            message["content"][2],
            json!({"type":"text","text":"Checking files"})
        );
        assert_eq!(message["content"][3]["type"], "tool_use");
    }

    #[test]
    fn gemini_stream_preserves_signed_parts_and_returns_native_tool_results() {
        let parts = vec![
            json!({"text":"private reasoning","thought":true}),
            json!({"text":"Checking","thoughtSignature":"text-signature"}),
            json!({"functionCall":{"name":"file_search_query","args":{"query":"a"},"id":"remote-id"},"thoughtSignature":"call-signature"}),
            json!({"functionCall":{"name":"file_search_query","args":{"query":"b"}}}),
        ];
        let stream = sse(&[
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":&parts[..2]}}]}),
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":&parts[2..]},"finishReason":"STOP"}]}),
        ]);
        let (turn, displayed) = parse_sse(QuickAskProvider::Google, &stream, 1).unwrap();
        assert_eq!(displayed, ["Checking"]);
        assert_eq!(turn.calls.len(), 2);
        assert_eq!(turn.calls[0].id, "remote-id");
        assert_eq!(turn.calls[1].arguments, json!({"query":"b"}));
        let assistant = turn.assistant_message(QuickAskProvider::Google);
        let response = google_tool_result(
            &turn.calls[0].name,
            Some(json!("remote-id")),
            json!({"matches":["a"]}),
        );
        let conversation = Conversation {
            messages: vec![assistant, json!({"role":"user","content":[response]})],
            pending: None,
        };
        let body = request_body(
            &provider_config(QuickAskProvider::Google),
            &conversation,
            &[],
        );
        assert_eq!(body["contents"][0], json!({"role":"model","parts":parts}));
        assert_eq!(
            body["contents"][1],
            json!({"role":"user","parts":[{
                "functionResponse":{"id":"remote-id","name":"file_search_query","response":{"matches":["a"]}}
            }]})
        );
        assert_eq!(
            google_tool_result("search", None, json!(["a"]))["functionResponse"],
            json!({"name":"search","response":{"result":["a"]}})
        );
        assert_eq!(
            google_tool_result("search", None, json!({"error":"denied"}))["functionResponse"]["response"],
            json!({"error":"denied"})
        );
    }

    #[test]
    fn gemini_cancelled_answer_converts_to_model_text_before_followup() {
        let mut conversation = Conversation::default();
        conversation.begin_turn(QuickAskProvider::Google, &selected_context(), "first");
        conversation.push_text("partial answer");
        conversation.cancel_turn();
        conversation.begin_turn(QuickAskProvider::Google, &selected_context(), "followup");
        let body = request_body(
            &provider_config(QuickAskProvider::Google),
            &conversation,
            &[],
        );
        assert_eq!(body["contents"][0]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["contents"][1],
            json!({"role":"model","parts":[{"text":"partial answer"}]})
        );
        assert_eq!(
            body["contents"][2],
            json!({"role":"user","parts":[{"text":"followup"}]})
        );
    }

    #[test]
    fn gemini_accepts_complete_candidate_eof_and_token_limit_but_not_blocked_or_empty_streams() {
        let event = json!({"candidates":[{"content":{"parts":[{"text":"answer"}]}}]});
        let input = format!("data: {event}");
        let (turn, _) = parse_sse(QuickAskProvider::Google, &input, 3).unwrap();
        assert_eq!(turn.text, "answer");
        let max = sse(&[
            json!({"candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"MAX_TOKENS"}]}),
        ]);
        assert_eq!(
            parse_sse(QuickAskProvider::Google, &max, 3).unwrap().0.text,
            "partial"
        );
        for event in [
            json!({"candidates":[]}),
            json!({"candidates":[{"content":{"parts":[{"thought":true,"text":"hidden"}]}}]}),
            json!({"candidates":[{"finishReason":"SAFETY"}]}),
            json!({"promptFeedback":{"blockReason":"PROHIBITED_CONTENT"}}),
            json!({"error":{"message":"quota exceeded"}}),
        ] {
            assert!(parse_sse(QuickAskProvider::Google, &sse(&[event]), 3).is_err());
        }
        assert!(parse_sse(QuickAskProvider::Google, "data: {\"candidates\":[", 3).is_err());
    }

    #[test]
    fn gemini_uses_only_the_first_candidate_regardless_of_index_metadata() {
        for index in [Value::Null, json!(7)] {
            let stream = sse(&[json!({"candidates":[
                {"index":index,"content":{"parts":[{"text":"first"}]},"finishReason":"STOP"},
                {"content":{"parts":[{"text":"other"},{"functionCall":{"name":"ignored","args":{}}}]},"finishReason":"SAFETY"}
            ]})]);
            let (turn, displayed) = parse_sse(QuickAskProvider::Google, &stream, 3).unwrap();
            assert_eq!(turn.text, "first");
            assert_eq!(displayed, ["first"]);
            assert!(turn.calls.is_empty());
            assert_eq!(turn.google_parts, vec![json!({"text":"first"})]);
        }
    }

    #[test]
    fn streams_reject_invalid_or_excessive_tool_calls_before_dispatch() {
        for args in [json!("not an object"), json!([]), Value::Null] {
            let stream = sse(&[json!({"candidates":[{"content":{"parts":[{
                "functionCall":{"name":"search","args":args}
            }]},"finishReason":"STOP"}]})]);
            assert!(parse_sse(QuickAskProvider::Google, &stream, 3).is_err());
        }
        let duplicate = sse(&[json!({"candidates":[{"content":{"parts":[
            {"functionCall":{"id":"same","name":"search","args":{}}},
            {"functionCall":{"id":"same","name":"search","args":{}}}
        ]},"finishReason":"STOP"}]})]);
        assert!(parse_sse(QuickAskProvider::Google, &duplicate, 3).is_err());
        let parts: Vec<_> = (0..65)
            .map(|_| json!({"functionCall":{"name":"search","args":{}}}))
            .collect();
        let excessive =
            sse(&[json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]})]);
        assert!(parse_sse(QuickAskProvider::Google, &excessive, 3).is_err());
        let invalid_json = sse(&[json!({"choices":[{"index":0,"delta":{"tool_calls":[{
            "index":0,"id":"bad","function":{"name":"search","arguments":"{\"unfinished\":"}
        }]},"finish_reason":"tool_calls"}]})])
            + "data: [DONE]\n\n";
        assert!(parse_sse(QuickAskProvider::OpenAiCompatible, &invalid_json, 3).is_err());
    }

    #[test]
    fn sse_decoder_handles_crlf_comments_multiline_data_and_invalid_utf8() {
        let input = ": keepalive\r\nevent: response\r\ndata: {\"candidates\":\r\ndata: [{\"content\":{\"parts\":[{\"text\":\"世界\"}]},\"finishReason\":\"STOP\"}]}\r\n\r\n";
        assert_eq!(
            parse_sse(QuickAskProvider::Google, input, 1)
                .unwrap()
                .0
                .text,
            "世界"
        );
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: \xff\n\n").is_err());
        let oversized = vec![b'a'; MAX_EVENT_BYTES + 1];
        assert!(SseDecoder::default().push(&oversized).is_err());
    }

    #[test]
    fn http_stream_eof_finishes_gemini_but_rejects_unterminated_openai_turns() {
        for provider in [
            QuickAskProvider::Google,
            QuickAskProvider::OpenAI,
            QuickAskProvider::OpenAiCompatible,
        ] {
            let body = if provider == QuickAskProvider::Google {
                format!(
                    "data: {}",
                    json!({"candidates":[{"content":{"parts":[{"text":"answer"}]}}]})
                )
            } else if provider == QuickAskProvider::OpenAI {
                sse(&[json!({"type":"response.output_text.delta","delta":"answer"})])
            } else {
                sse(&[
                    json!({"choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":"stop"}]}),
                ])
            };
            let (url, worker) = serve_response(200, "text/event-stream", body);
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let response = reqwest::Client::new().get(url).send().await.unwrap();
                    read_stream(response, provider, &|_| {}).await
                });
            if provider == QuickAskProvider::Google {
                assert_eq!(result.unwrap().text, "answer");
            } else {
                assert!(result.is_err());
            }
            worker.join().unwrap();
        }
    }

    fn selected_context() -> QuickContext {
        QuickContext {
            text: "selected text".into(),
            source_title: "source window".into(),
            image: Some(crate::ContextImage {
                mime_type: "image/png".into(),
                bytes: vec![1, 2, 3],
            }),
        }
    }

    #[test]
    fn cancelled_first_turn_keeps_context_and_partial_answer_for_followup() {
        for provider in PROVIDERS {
            let context = selected_context();
            let mut conversation = Conversation::default();
            conversation.begin_turn(provider, &context, "first question");
            let first_user = conversation.messages[0].clone();
            assert_eq!(first_user["content"].as_array().unwrap().len(), 2);
            assert!(
                first_user["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("selected text")
            );
            conversation.push_text("partial ");
            conversation.push_text("answer");
            conversation.cancel_turn();
            conversation.cancel_turn();
            conversation.push_text("late delta");
            conversation.begin_turn(provider, &context, "followup");
            assert_eq!(
                conversation.messages,
                vec![
                    first_user,
                    json!({"role":"assistant","content":"partial answer"}),
                    json!({"role":"user","content":[{"type":"text","text":"followup"}]}),
                ]
            );
        }
    }

    #[test]
    fn cancelling_followup_preserves_completed_history_without_duplicate_messages() {
        let context = selected_context();
        let mut conversation = Conversation::default();
        conversation.begin_turn(QuickAskProvider::OpenAI, &context, "first");
        conversation.push_text("first answer");
        conversation.cancel_turn();
        let previous = conversation.messages.clone();
        conversation.begin_turn(QuickAskProvider::OpenAI, &context, "second");
        conversation.push_text("second partial");
        conversation.cancel_turn();
        conversation.begin_turn(QuickAskProvider::OpenAI, &context, "third");
        assert_eq!(&conversation.messages[..2], previous.as_slice());
        assert_eq!(
            &conversation.messages[2..],
            &[
                json!({"role":"user","content":[{"type":"text","text":"second"}]}),
                json!({"role":"assistant","content":"second partial"}),
                json!({"role":"user","content":[{"type":"text","text":"third"}]}),
            ]
        );
    }

    #[test]
    fn immediate_cancel_and_empty_deltas_keep_user_without_empty_assistant() {
        for empty_delta in [false, true] {
            let context = selected_context();
            let mut conversation = Conversation::default();
            conversation.begin_turn(QuickAskProvider::Anthropic, &context, "first");
            let first_user = conversation.messages[0].clone();
            if empty_delta {
                conversation.push_text("");
            }
            conversation.cancel_turn();
            conversation.begin_turn(QuickAskProvider::Anthropic, &context, "second");
            assert_eq!(
                conversation.messages,
                vec![
                    first_user,
                    json!({"role":"user","content":[{"type":"text","text":"second"}]}),
                ]
            );
        }
    }

    #[test]
    fn failed_turn_rolls_back_before_cancel_and_preserves_first_context_on_retry() {
        let context = selected_context();
        let mut conversation = Conversation::default();
        for partial in ["", " \n\t"] {
            conversation.begin_turn(QuickAskProvider::OpenAI, &context, "failed");
            conversation.push_text(partial);
            conversation.fail_turn();
            conversation.cancel_turn();
            assert!(conversation.messages.is_empty());
        }
        conversation.begin_turn(QuickAskProvider::OpenAI, &context, "retry");
        assert_eq!(
            conversation.messages,
            vec![user_message(
                QuickAskProvider::OpenAI,
                &context,
                "retry",
                true
            )]
        );
        conversation.push_text("answer");
        conversation.cancel_turn();
        let previous = conversation.messages.clone();
        conversation.begin_turn(QuickAskProvider::OpenAI, &context, "failed followup");
        conversation.fail_turn();
        conversation.cancel_turn();
        assert_eq!(conversation.messages, previous);
    }

    #[test]
    fn failed_turn_with_partial_answer_preserves_first_and_followup_messages() {
        for provider in PROVIDERS {
            let context = selected_context();
            let mut conversation = Conversation::default();
            conversation.begin_turn(provider, &context, "first");
            let first_user = conversation.messages[0].clone();
            conversation.push_text("first partial");
            conversation.fail_turn();
            conversation.cancel_turn();
            assert_eq!(
                conversation.messages,
                vec![
                    first_user.clone(),
                    json!({"role":"assistant","content":"first partial"}),
                ]
            );
            conversation.begin_turn(provider, &context, "followup");
            conversation.push_text("second ");
            conversation.push_text("partial");
            conversation.fail_turn();
            conversation.cancel_turn();
            conversation.push_text("late delta");
            assert_eq!(
                conversation.messages,
                vec![
                    first_user,
                    json!({"role":"assistant","content":"first partial"}),
                    json!({"role":"user","content":[{"type":"text","text":"followup"}]}),
                    json!({"role":"assistant","content":"second partial"}),
                ]
            );
        }
    }

    #[test]
    fn clear_drops_pending_turn_and_restores_context_for_next_first_message() {
        let context = selected_context();
        let mut conversation = Conversation::default();
        conversation.begin_turn(QuickAskProvider::Anthropic, &context, "discarded");
        conversation.push_text("discarded answer");
        conversation.cancel_turn();
        conversation = Conversation::default();
        conversation.push_text("late delta");
        conversation.cancel_turn();
        conversation.begin_turn(QuickAskProvider::Anthropic, &context, "new first");
        assert_eq!(
            conversation.messages,
            vec![user_message(
                QuickAskProvider::Anthropic,
                &context,
                "new first",
                true,
            )]
        );
    }

    #[test]
    fn cancellation_between_tool_rounds_keeps_text_without_unpaired_tool_calls() {
        for provider in PROVIDERS {
            let context = selected_context();
            let mut conversation = Conversation::default();
            conversation.begin_turn(provider, &context, "question");
            // Text before and after a tool result belongs to the same user turn.
            conversation.push_text("checking files...");
            conversation.push_text("found a match");
            conversation.cancel_turn();
            assert_eq!(conversation.messages.len(), 2);
            assert_eq!(
                conversation.messages[1],
                json!({"role":"assistant","content":"checking files...found a match"})
            );
        }
    }

    #[test]
    fn finished_turn_then_cancel_preserves_complete_tool_protocol_once() {
        let mut conversation = Conversation::default();
        conversation.begin_turn(QuickAskProvider::OpenAI, &selected_context(), "question");
        let mut finished = conversation.request_conversation();
        finished.messages.extend([
            json!({"role":"assistant","content":"checking","tool_calls":[
                {"id":"call-1","type":"function","function":{"name":"file_search_query","arguments":"{}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"call-1","content":"found"}),
        ]);
        finished.push_assistant("answer".into());
        conversation.push_text("checking");
        conversation.push_text("answer");
        let expected = finished.messages.clone();
        // A queued Finished replaces the provisional turn before the UI Cancel.
        conversation = finished;
        conversation.cancel_turn();
        conversation.push_text("late delta");
        conversation.cancel_turn();
        assert_eq!(conversation.messages, expected);
    }

    fn pending(
        name: &str,
    ) -> (
        PendingToolCall,
        oneshot::Receiver<std::result::Result<Value, String>>,
    ) {
        let (reply, receiver) = oneshot::channel();
        (
            PendingToolCall {
                call: ToolCall {
                    id: "test".into(),
                    name: name.into(),
                    arguments: json!({}),
                },
                reply,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        )
    }

    #[test]
    fn tools_wait_for_the_real_async_result() {
        let (call, mut receiver) = pending("file_search_query");
        let request = call.into_request(&McpConfig::default()).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(request.request.action, "file_search.query");
        request.reply(Ok(json!({"results": ["found.txt"]})));
        assert_eq!(
            receiver.try_recv().unwrap().unwrap()["results"][0],
            "found.txt"
        );
    }

    #[test]
    fn denied_and_cancelled_tools_are_not_dispatched() {
        let (call, mut receiver) = pending("memory_clean");
        assert!(call.into_request(&McpConfig::default()).is_none());
        assert!(receiver.try_recv().unwrap().is_err());
        let (call, _receiver) = pending("file_search_query");
        call.cancelled.store(true, Ordering::Release);
        assert!(call.into_request(&McpConfig::default()).is_none());
    }
}
