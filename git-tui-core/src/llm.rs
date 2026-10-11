//! LLM-assisted commit message generation from staged changes.
//!
//! The TUI collects the staged (index vs HEAD) diff for every staged file,
//! builds a single prompt that references each path, and calls the
//! configured provider. All network I/O is blocking [`ureq`] so it can run
//! on the [`crate::jobqueue`] worker thread without a runtime.

use crate::diff::{self, FileDiff, LineKind};
use crate::error::GitError;
use crate::status::{self, FileState};

/// Max total prompt context (chars) sent to the provider. Staged diffs are
/// truncated per-file first, then overall, so a huge vendor drop cannot blow
/// the request up.
const MAX_TOTAL_CHARS: usize = 12_000;
const MAX_FILE_CHARS: usize = 4_000;

/// Providers accepted in `[llm] provider` (also used by the TUI setup form).
pub const PROVIDERS: &[&str] = &[
    "openai",
    "openrouter",
    "ollama",
    "anthropic",
    "gemini",
    "custom",
];

/// Fallback model when `[llm] model` is unset. Not a curated list: the
/// setup form's model options always come from the provider (see
/// [`fetch_models`]), this is only the pre-network default.
const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// Model ids the provider currently offers, for the setup form's model
/// row. Blocking [`ureq`] like the chat calls, so the TUI runs it on the
/// [`crate::jobqueue`] worker and the row can show "loading…".
///
/// Model lists change constantly, so nothing here is hardcoded per
/// provider: every provider is asked over HTTP. Ids come back sorted and
/// deduplicated so `←/→` cycles them in a stable order. A provider that
/// can't be reached returns the error, and the row stays free text.
pub fn fetch_models(config: &LlmConfig) -> Result<Vec<String>, GitError> {
    if !config.is_configured() {
        return Err(GitError::LlmModels(missing_key_hint(config)));
    }
    let mut models = match config.provider.as_str() {
        "anthropic" => fetch_anthropic_models(config),
        "gemini" => fetch_gemini_models(config),
        _ => fetch_openai_models(config),
    }
    .map_err(|e| GitError::LlmModels(llm_error_message(&e)))?;
    models.sort_unstable();
    models.dedup();
    Ok(models)
}

/// Strip the `GitError` wrapper's own prefix: [`GitError::LlmModels`]
/// supplies the context for this call, so the inner provider message must
/// not repeat it.
fn llm_error_message(e: &GitError) -> String {
    let full = e.to_string();
    full.split_once(": ")
        .map(|(_, rest)| rest.to_string())
        .filter(|rest| !rest.is_empty())
        .unwrap_or(full)
}

/// `GET {base}/models` — the OpenAI-compatible listing, shared by
/// `openai`, `openrouter`, `ollama`, and `custom`.
fn fetch_openai_models(config: &LlmConfig) -> Result<Vec<String>, GitError> {
    let base = require_base_url(config)?;
    let url = format!("{base}/models");
    let mut req = agent().get(&url);
    let key = config.effective_api_key();
    if !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    model_ids(req.call().map_err(map_http_err)?)
}

/// Anthropic's listing (`GET /v1/models`), same `data[].id` shape.
fn fetch_anthropic_models(config: &LlmConfig) -> Result<Vec<String>, GitError> {
    let base = config
        .base_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://api.anthropic.com/v1".into());
    let url = format!("{base}/models");
    let req = agent()
        .get(&url)
        .set("x-api-key", &config.effective_api_key())
        .set("anthropic-version", "2023-06-01");
    model_ids(req.call().map_err(map_http_err)?)
}

/// Gemini's listing (`GET /v1beta/models?key=…`): `models[].name`, each
/// prefixed with `models/`.
fn fetch_gemini_models(config: &LlmConfig) -> Result<Vec<String>, GitError> {
    let base = config
        .base_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com".into());
    let key = config.effective_api_key();
    let url = format!("{base}/v1beta/models?key={key}");
    let json: serde_json::Value = agent()
        .get(&url)
        .call()
        .map_err(map_http_err)?
        .into_json()
        .map_err(|e| GitError::Llm(e.to_string()))?;
    Ok(json["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["name"].as_str())
                .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

/// Pull `data[].id` out of an OpenAI-shaped listing response.
fn model_ids(resp: ureq::Response) -> Result<Vec<String>, GitError> {
    let json: serde_json::Value = resp.into_json().map_err(|e| GitError::Llm(e.to_string()))?;
    Ok(json["data"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["id"].as_str())
                .map(|id| id.to_string())
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default())
}

fn require_base_url(config: &LlmConfig) -> Result<String, GitError> {
    let base = config.effective_base_url();
    if base.is_empty() {
        return Err(GitError::Llm(
            "provider \"custom\" needs [llm] base_url set to list models".into(),
        ));
    }
    Ok(base)
}

/// Configuration for one LLM provider. Lives in core so the worker thread
/// can use it without depending on the TUI crate; the TUI `[llm]` config
/// section parses directly into this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    /// `openai` (default), `openrouter`, `ollama`, `anthropic`, `gemini`,
    /// or `custom` (OpenAI-compatible + explicit `base_url`).
    pub provider: String,
    pub model: String,
    /// May be empty: [`Self::effective_api_key`] falls back to env vars.
    pub api_key: String,
    pub base_url: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            model: DEFAULT_MODEL.into(),
            api_key: String::new(),
            base_url: None,
        }
    }
}

impl LlmConfig {
    /// API key from config, else well-known env vars. Provider-specific vars
    /// win; `LLM_API_KEY` is the generic fallback.
    pub fn effective_api_key(&self) -> String {
        if !self.api_key.trim().is_empty() {
            return self.api_key.trim().to_string();
        }
        let provider_vars: &[&str] = match self.provider.as_str() {
            "anthropic" => &["ANTHROPIC_API_KEY"],
            "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            "openrouter" => &["OPENROUTER_API_KEY"],
            "ollama" => &[],
            _ => &["OPENAI_API_KEY"],
        };
        for var in provider_vars.iter().copied().chain(["LLM_API_KEY"]) {
            if let Ok(v) = std::env::var(var) {
                if !v.trim().is_empty() {
                    return v.trim().to_string();
                }
            }
        }
        String::new()
    }

    pub fn is_configured(&self) -> bool {
        // Local ollama needs no key.
        self.provider == "ollama" || !self.effective_api_key().is_empty()
    }

    /// Base URL for OpenAI-compatible providers.
    pub fn effective_base_url(&self) -> String {
        if let Some(u) = &self.base_url {
            if !u.trim().is_empty() {
                return u.trim().trim_end_matches('/').to_string();
            }
        }
        match self.provider.as_str() {
            "openrouter" => "https://openrouter.ai/api/v1".into(),
            "ollama" => "http://localhost:11434/v1".into(),
            "custom" => String::new(),
            _ => "https://api.openai.com/v1".into(),
        }
    }

    pub fn effective_model(&self) -> String {
        if self.model.trim().is_empty() {
            LlmConfig::default().model
        } else {
            self.model.trim().to_string()
        }
    }
}

/// Largest `i <= index` that is a UTF-8 char boundary in `s`.
/// (`str::floor_char_boundary` is only stable since Rust 1.91; the
/// workspace MSRV is 1.88, so this manual loop keeps CI green.)
fn floor_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Truncate `s` to at most `max_len` bytes without splitting a UTF-8
/// code point (`String::truncate` panics otherwise).
fn truncate_chars(s: &mut String, max_len: usize) {
    if s.len() > max_len {
        s.truncate(floor_boundary(s, max_len));
    }
}

/// Byte prefix of `s` limited to `max_len` bytes on a char boundary.
fn char_prefix(s: &str, max_len: usize) -> &str {
    &s[..floor_boundary(s, max_len.min(s.len()))]
}

/// One staged file + its staged (index vs HEAD) diff, truncated for prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    pub path: String,
    /// Human status: `staged`, `staged (new)`, `staged + unstaged`, ...
    pub status_label: String,
    pub unified: String,
    pub added: usize,
    pub removed: usize,
}

/// Collect every staged file (index vs HEAD) with a truncated unified diff.
/// `Staged` and `BothStagedAndUnstaged` both contribute their *staged*
/// portion only — unstaged workdir edits are never sent to the provider.
pub fn staged_context(repo: &git2::Repository) -> Result<Vec<StagedFile>, GitError> {
    let st = status::repo_status(repo)?;
    let mut out = Vec::new();
    for entry in st.files.iter().filter(|e| {
        matches!(
            e.state,
            FileState::Staged | FileState::BothStagedAndUnstaged
        )
    }) {
        let diff = diff::staged_diff(repo, &entry.path).unwrap_or(FileDiff {
            path: entry.path.clone(),
            hunks: Vec::new(),
            binary: false,
        });
        let (added, removed) = count_lines(&diff);
        let mut unified = if diff.binary {
            "(binary file; content not shown)".to_string()
        } else {
            render_unified(&diff)
        };
        if unified.len() > MAX_FILE_CHARS {
            truncate_chars(&mut unified, MAX_FILE_CHARS);
            unified.push_str("\n…(truncated)");
        }
        let status_label = match entry.state {
            FileState::Staged => {
                if diff.hunks.iter().all(|h| h.old_start == 0) && added > 0 && removed == 0 {
                    "staged (new file)".to_string()
                } else {
                    "staged".to_string()
                }
            }
            _ => "staged (plus unstaged changes not included)".to_string(),
        };
        out.push(StagedFile {
            path: entry.path.clone(),
            status_label,
            unified,
            added,
            removed,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn count_lines(diff: &FileDiff) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for h in &diff.hunks {
        for l in &h.lines {
            match l.kind {
                LineKind::Add => added += 1,
                LineKind::Del => removed += 1,
                _ => {}
            }
        }
    }
    (added, removed)
}

/// Unified text for one staged diff (`path`, `@@` headers, `+/-` lines).
pub fn render_unified(diff: &FileDiff) -> String {
    let mut s = String::new();
    s.push_str(&format!("diff --git staged {}\n", diff.path));
    for h in &diff.hunks {
        s.push_str(&format!("{}\n", h.header));
        for l in &h.lines {
            let marker = match l.kind {
                LineKind::Add => "+",
                LineKind::Del => "-",
                LineKind::HunkHeader => continue,
                LineKind::Context => " ",
            };
            s.push_str(marker);
            s.push_str(&l.text);
            s.push('\n');
        }
    }
    s
}

/// Prompt sent to the provider. Every staged path is referenced explicitly
/// so the model can name them ("proper reference").
pub fn build_commit_prompt(files: &[StagedFile]) -> String {
    let mut s = String::new();
    s.push_str(
        "You write git commit messages. Rules:\n\
- Output ONLY the commit message, no explanations, no code fences.\n\
- Use Conventional Commits: <type>(<scope>): <subject> where scope is the\n\
  main directory or file stem when obvious, else omit scope.\n\
- Subject: imperative mood, <= 72 chars, lowercase start after colon.\n\
- Reference every staged file when naming the change; if several files,\n\
  add a short body with one bullet per file or area.\n\
- Prefer types: feat, fix, refactor, chore, docs, test, style, perf, build, ci.\n\n",
    );
    s.push_str("Staged files (index vs HEAD only):\n");
    let mut total = s.len();
    for f in files {
        let header = format!(
            "\n--- {} [{}] (+{} -{}) ---\n",
            f.path, f.status_label, f.added, f.removed
        );
        if total + header.len() > MAX_TOTAL_CHARS {
            s.push_str("\n…(remaining files omitted for length)\n");
            break;
        }
        s.push_str(&header);
        total += header.len();
        let remaining = MAX_TOTAL_CHARS.saturating_sub(total);
        if remaining == 0 {
            s.push_str("…(truncated)\n");
            break;
        }
        if f.unified.len() <= remaining {
            s.push_str(&f.unified);
            total += f.unified.len();
        } else {
            let prefix = char_prefix(&f.unified, remaining);
            s.push_str(prefix);
            s.push_str("\n…(truncated)\n");
            total = MAX_TOTAL_CHARS;
        }
    }
    s.push_str("\nWrite the commit message now.\n");
    s
}

/// Offline fallback when no provider is configured/reachable in tests: a
/// deterministic Conventional-Commits-style summary referencing each file.
pub fn heuristic_message(files: &[StagedFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let scope = common_scope(files);
    let noun = if files.len() == 1 {
        files[0].path.clone()
    } else {
        format!("{} files", files.len())
    };
    let total_added: usize = files.iter().map(|f| f.added).sum();
    let total_removed: usize = files.iter().map(|f| f.removed).sum();
    let any_new = files.iter().any(|f| f.status_label.contains("new file"));
    let verb = if any_new && total_removed == 0 {
        "add"
    } else if total_removed > total_added * 2 {
        "remove"
    } else {
        "update"
    };
    let mut msg = if scope.is_empty() {
        format!("chore: {verb} {noun}")
    } else {
        format!("chore({scope}): {verb} {noun}")
    };
    if files.len() > 1 {
        msg.push_str("\n\n");
        for f in files {
            msg.push_str(&format!("- {} (+{} -{})\n", f.path, f.added, f.removed));
        }
        msg.pop();
    }
    msg
}

fn common_scope(files: &[StagedFile]) -> String {
    let dirs: Vec<&str> = files
        .iter()
        .map(|f| f.path.rfind('/').map(|i| &f.path[..i]).unwrap_or_default())
        .collect();
    if dirs.is_empty() || !dirs.iter().all(|d| *d == dirs[0]) || dirs[0].is_empty() {
        return String::new();
    }
    dirs[0].rsplit('/').next().unwrap_or_default().to_string()
}

/// Strip markdown fences / surrounding quotes models love to add.
pub fn clean_message(raw: &str) -> String {
    let mut t = raw.trim().to_string();
    for fence in ["```", "'''"] {
        if t.starts_with(fence) {
            t = t.strip_prefix(fence).unwrap_or(&t).to_string();
            if let Some(nl) = t.find('\n') {
                // Drop a leading language tag line (e.g. ```text).
                let first = t[..nl].trim();
                if !first.is_empty() && !first.contains(' ') && first.len() < 20 {
                    t = t[nl + 1..].to_string();
                }
            }
            if let Some(end) = t.rfind(fence) {
                t = t[..end].to_string();
            }
            t = t.trim().to_string();
        }
    }
    // Keep it commit-sized: subject + up to ~10 body lines.
    let lines: Vec<&str> = t.lines().collect();
    let kept: Vec<&str> = lines.into_iter().take(11).collect();
    let mut out = kept.join("\n").trim().to_string();
    if out.len() > 1000 {
        truncate_chars(&mut out, 1000);
        out.push('…');
    }
    out
}

/// End-to-end: gather staged context, call the provider, return the message.
pub fn generate_commit_message(
    repo: &git2::Repository,
    config: &LlmConfig,
) -> Result<String, GitError> {
    let files = staged_context(repo)?;
    if files.is_empty() {
        return Err(GitError::Llm(
            "nothing staged: press space on a file to stage it, then Ctrl+G to generate".into(),
        ));
    }
    if !config.is_configured() {
        return Err(GitError::Llm(missing_key_hint(config)));
    }
    let prompt = build_commit_prompt(&files);
    let raw = match config.provider.as_str() {
        "anthropic" => call_anthropic(config, &prompt)?,
        "gemini" => call_gemini(config, &prompt)?,
        _ => call_openai_compatible(config, &prompt)?,
    };
    let msg = clean_message(&raw);
    if msg.trim().is_empty() {
        return Err(GitError::Llm("provider returned an empty message".into()));
    }
    Ok(msg)
}

fn missing_key_hint(config: &LlmConfig) -> String {
    let var = match config.provider.as_str() {
        "anthropic" => "ANTHROPIC_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        _ => "OPENAI_API_KEY",
    };
    format!(
        "no API key for provider {:?}: press A in the file list to configure it via TUI, \
         or set [llm] api_key in ~/.config/activegit/config.toml or ${var} (or $LLM_API_KEY)",
        config.provider
    )
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(60))
        .build()
}

fn call_openai_compatible(config: &LlmConfig, prompt: &str) -> Result<String, GitError> {
    let base = config.effective_base_url();
    if base.is_empty() {
        return Err(GitError::Llm(
            "provider \"custom\" needs [llm] base_url set".into(),
        ));
    }
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": config.effective_model(),
        "temperature": 0.2,
        "max_tokens": 300,
        "messages": [
            {"role": "system", "content": "You write concise Conventional Commits messages. Output only the message."},
            {"role": "user", "content": prompt},
        ],
    });
    let mut req = agent().post(&url).set("Content-Type", "application/json");
    let key = config.effective_api_key();
    if !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    if config.provider == "openrouter" {
        req = req
            .set("HTTP-Referer", "https://github.com/DevarshVasani/activegit")
            .set("X-Title", "activegit");
    }
    let resp = req.send_json(body).map_err(map_http_err)?;
    let json: serde_json::Value = resp.into_json().map_err(|e| GitError::Llm(e.to_string()))?;
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| GitError::Llm(format!("unexpected chat response: {json}")))
}

fn call_anthropic(config: &LlmConfig, prompt: &str) -> Result<String, GitError> {
    let url = config
        .base_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".into());
    let body = serde_json::json!({
        "model": config.effective_model(),
        "max_tokens": 300,
        "system": "You write concise Conventional Commits messages. Output only the message.",
        "messages": [{"role": "user", "content": prompt}],
    });
    let key = config.effective_api_key();
    let resp = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .set("x-api-key", &key)
        .set("anthropic-version", "2023-06-01")
        .send_json(body)
        .map_err(map_http_err)?;
    let json: serde_json::Value = resp.into_json().map_err(|e| GitError::Llm(e.to_string()))?;
    json["content"]
        .as_array()
        .and_then(|a| a.iter().find_map(|b| b["text"].as_str()))
        .map(|s| s.to_string())
        .ok_or_else(|| GitError::Llm(format!("unexpected anthropic response: {json}")))
}

fn call_gemini(config: &LlmConfig, prompt: &str) -> Result<String, GitError> {
    let key = config.effective_api_key();
    let model = config.effective_model();
    let base = config
        .base_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com".into());
    let url = format!(
        "{}/v1beta/models/{}:generateContent?key={}",
        base.trim_end_matches('/'),
        model,
        key
    );
    let body = serde_json::json!({
        "contents": [{"parts": [{"text": prompt}]}],
        "generationConfig": {"maxOutputTokens": 300, "temperature": 0.2},
    });
    let resp = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(map_http_err)?;
    let json: serde_json::Value = resp.into_json().map_err(|e| GitError::Llm(e.to_string()))?;
    json["candidates"][0]["content"]["parts"]
        .as_array()
        .and_then(|a| a.iter().find_map(|b| b["text"].as_str()))
        .map(|s| s.to_string())
        .ok_or_else(|| GitError::Llm(format!("unexpected gemini response: {json}")))
}

fn map_http_err(e: ureq::Error) -> GitError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_else(|_| "<unreadable>".into());
            let short: String = body.chars().take(300).collect();
            GitError::Llm(format!("provider HTTP {code}: {short}"))
        }
        other => GitError::Llm(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use crate::testutil::ENV_LOCK;

    /// One-shot mock provider: replies to the first request with `body`
    /// and records the request line, so a test can assert both the parsed
    /// ids and the URL/auth actually used.
    fn mock_provider(body: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = stream;
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            let _ = tx.send(request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        (port, rx)
    }

    #[test]
    fn openai_shaped_listing_is_parsed_sorted_and_deduped() {
        let (port, request) = mock_provider(
            r#"{"data":[{"id":"gpt-4o"},{"id":"gpt-4o-mini"},{"id":"gpt-4o-mini"},{"id":""}]}"#,
        );
        let cfg = LlmConfig {
            provider: "openai".into(),
            model: String::new(),
            api_key: "sk-test".into(),
            base_url: Some(format!("http://127.0.0.1:{port}/v1")),
        };
        let models = fetch_models(&cfg).unwrap();
        // Sorted, deduped, and the empty id dropped.
        assert_eq!(models, vec!["gpt-4o", "gpt-4o-mini"]);
        let line = request
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(line.starts_with("GET /v1/models "), "got {line}");
    }

    #[test]
    fn each_provider_hits_its_own_listing_endpoint() {
        // Anthropic: `data[].id` like OpenAI, but under /v1/models.
        let (port, request) = mock_provider(
            r#"{"data":[{"id":"claude-opus-4-1"},{"id":"claude-3-5-haiku-latest"}]}"#,
        );
        let cfg = LlmConfig {
            provider: "anthropic".into(),
            model: String::new(),
            api_key: "sk-test".into(),
            base_url: Some(format!("http://127.0.0.1:{port}/anthropic/v1")),
        };
        let models = fetch_models(&cfg).unwrap();
        assert_eq!(models, vec!["claude-3-5-haiku-latest", "claude-opus-4-1"]);
        assert!(request
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .starts_with("GET /anthropic/v1/models "));

        // Gemini: `models[].name`, each prefixed with `models/`.
        let (port, request) = mock_provider(
            r#"{"models":[{"name":"models/gemini-2.5-pro"},{"name":"models/gemini-2.0-flash"}]}"#,
        );
        let cfg = LlmConfig {
            provider: "gemini".into(),
            model: String::new(),
            api_key: "sk-test".into(),
            base_url: Some(format!("http://127.0.0.1:{port}")),
        };
        let models = fetch_models(&cfg).unwrap();
        assert_eq!(models, vec!["gemini-2.0-flash", "gemini-2.5-pro"]);
        assert!(request
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .starts_with("GET /v1beta/models?key="));
    }

    #[test]
    fn model_fetch_errors_name_the_provider_problem() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        // No key: the caller is told why rather than getting a 401 back.
        let err = fetch_models(&LlmConfig {
            api_key: String::new(),
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("could not list provider models"), "got {err}");
        assert!(err.contains("API key"), "got {err}");
        match prev {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
    }

    #[test]
    fn custom_provider_without_base_url_cannot_list_models() {
        let err = fetch_models(&LlmConfig {
            provider: "custom".into(),
            api_key: "sk-test".into(),
            base_url: None,
            ..Default::default()
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("base_url"), "got {err}");
    }

    #[test]
    fn api_key_prefers_config_then_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let cfg = LlmConfig {
            api_key: "cfg-key".into(),
            ..Default::default()
        };
        assert_eq!(cfg.effective_api_key(), "cfg-key");
        // Save/restore so parallel suites never observe our value.
        let prev = std::env::var("LLM_API_KEY").ok();
        std::env::set_var("LLM_API_KEY", "env-key-xyz");
        assert_eq!(LlmConfig::default().effective_api_key(), "env-key-xyz");
        match prev {
            Some(v) => std::env::set_var("LLM_API_KEY", v),
            None => std::env::remove_var("LLM_API_KEY"),
        }
    }

    #[test]
    fn base_urls_resolve_per_provider() {
        let _guard = ENV_LOCK.lock().unwrap();
        // Pin env so `is_configured` below is deterministic.
        let prev_openai = std::env::var("OPENAI_API_KEY").ok();
        let prev_llm = std::env::var("LLM_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("LLM_API_KEY");
        assert!(LlmConfig::default().effective_base_url().contains("openai"));
        let o = LlmConfig {
            provider: "openrouter".into(),
            ..Default::default()
        };
        assert!(o.effective_base_url().contains("openrouter"));
        let l = LlmConfig {
            provider: "ollama".into(),
            ..Default::default()
        };
        assert!(l.effective_base_url().contains("11434"));
        assert!(l.is_configured(), "ollama needs no key");
        assert!(
            !LlmConfig::default().is_configured(),
            "default provider without key must be unconfigured"
        );
        match prev_openai {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        match prev_llm {
            Some(v) => std::env::set_var("LLM_API_KEY", v),
            None => std::env::remove_var("LLM_API_KEY"),
        }
    }

    #[test]
    fn staged_context_only_includes_staged_portion() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        testutil::dirty_file(&repo, "a.txt", "more\n");
        // Nothing staged yet.
        assert!(staged_context(&repo).unwrap().is_empty());
        crate::stage::stage_file(&repo, "a.txt").unwrap();
        let ctx = staged_context(&repo).unwrap();
        assert_eq!(ctx.len(), 1);
        assert_eq!(ctx[0].path, "a.txt");
        assert!(ctx[0].unified.contains("more") || ctx[0].added > 0);
    }

    #[test]
    fn prompt_references_every_staged_file() {
        let files = vec![
            StagedFile {
                path: "src/api.rs".into(),
                status_label: "staged".into(),
                unified: "+fn x() {}".into(),
                added: 1,
                removed: 0,
            },
            StagedFile {
                path: "README.md".into(),
                status_label: "staged".into(),
                unified: "+docs".into(),
                added: 1,
                removed: 0,
            },
        ];
        let p = build_commit_prompt(&files);
        assert!(p.contains("src/api.rs"), "prompt must reference files");
        assert!(p.contains("README.md"));
        assert!(
            p.contains("Conventional"),
            "prompt must ask conventional commits"
        );
    }

    #[test]
    fn heuristic_references_files_deterministically() {
        let files = vec![
            StagedFile {
                path: "src/a.rs".into(),
                status_label: "staged".into(),
                unified: String::new(),
                added: 5,
                removed: 1,
            },
            StagedFile {
                path: "src/b.rs".into(),
                status_label: "staged".into(),
                unified: String::new(),
                added: 2,
                removed: 0,
            },
        ];
        let m = heuristic_message(&files);
        assert!(m.contains("src/a.rs") && m.contains("src/b.rs"));
    }

    #[test]
    fn cleaner_strips_fences() {
        assert_eq!(clean_message("```\nfeat: x\n```"), "feat: x");
        assert_eq!(clean_message("  fix: y  "), "fix: y");
    }

    #[test]
    fn truncation_never_splits_utf8() {
        // `truncate_chars` at a mid-code-point index must back off, not panic.
        let mut s = "a".repeat(3999) + "érest";
        truncate_chars(&mut s, MAX_FILE_CHARS);
        assert!(s.len() <= MAX_FILE_CHARS);
        assert!(s.is_char_boundary(s.len()));

        // `build_commit_prompt` slicing a multi-byte diff must not panic.
        let files = vec![StagedFile {
            path: "emoji.txt".into(),
            status_label: "staged".into(),
            unified: "é".repeat(20_000),
            added: 1,
            removed: 0,
        }];
        let p = build_commit_prompt(&files);
        assert!(p.contains("emoji.txt"));

        // `clean_message` truncation of multi-byte text must not panic.
        let m = clean_message(&"é".repeat(2000));
        assert!(m.len() <= 1000 + "…".len());
    }

    #[test]
    fn empty_staged_errors_without_network() {
        let (_dir, repo) = testutil::init_repo();
        testutil::commit_file(&repo, "a.txt", "a\n", "init");
        let err = generate_commit_message(&repo, &LlmConfig::default()).unwrap_err();
        assert!(err.to_string().contains("nothing staged"), "got {err}");
    }
}
