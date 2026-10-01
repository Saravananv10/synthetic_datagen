//! `web_search` tool — Tavily API. (DuckDuckGo's scrape endpoints were tried
//! first but actively CAPTCHA-block automated/datacenter traffic — confirmed
//! live, not a fluke — so this needs `TAVILY_API_KEY` set in the environment.)

use std::sync::Arc;

use loop_agent::types::{AgentTool, AgentToolResult};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    results: Vec<TavilyResult>,
}

pub fn build_tool() -> AgentTool {
    AgentTool {
        name: "web_search".to_string(),
        label: "Web Search".to_string(),
        description: "Search the live web. Use this to ground advice in real, current \
            information rather than guessing."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query" }
            },
            "required": ["query"]
        }),
        execution_mode: None,
        prepare_arguments: None,
        execute: Arc::new(move |_tool_call_id, args: Value, _cancel, _update_cb| {
            Box::pin(async move { execute(args).await })
        }),
    }
}

async fn execute(args: Value) -> Result<AgentToolResult, String> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing query".to_string())?;
    let api_key = std::env::var("TAVILY_API_KEY")
        .map_err(|_| "TAVILY_API_KEY not set".to_string())?;

    let resp = reqwest::Client::new()
        .post("https://api.tavily.com/search")
        .json(&json!({
            "api_key": api_key,
            "query": query,
            "max_results": 5,
            "include_answer": true,
        }))
        .send()
        .await
        .map_err(|e| format!("web_search request: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("web_search: HTTP {status}: {body}"));
    }

    let parsed: TavilyResponse = resp
        .json()
        .await
        .map_err(|e| format!("web_search parse response: {e}"))?;

    let formatted = format_results(query, &parsed);
    Ok(AgentToolResult {
        content: vec![loop_ai::ToolResultContent::Text(loop_ai::TextContent {
            text: formatted,
            text_signature: None,
        })],
        details: json!({
            "query": query,
            "answer": parsed.answer,
            "results": parsed.results.iter().map(|r| json!({"title": r.title, "url": r.url})).collect::<Vec<_>>(),
        }),
        usage: None,
        added_tool_names: None,
        terminate: None,
    })
}

fn format_results(query: &str, resp: &TavilyResponse) -> String {
    let mut out = format!("Search results for \"{query}\":\n\n");
    if let Some(answer) = &resp.answer {
        if !answer.trim().is_empty() {
            out.push_str(&format!("Summary: {answer}\n\n"));
        }
    }
    for (i, r) in resp.results.iter().enumerate() {
        let snippet: String = r.content.chars().take(400).collect();
        out.push_str(&format!("{}. {}\n   {}\n   {}\n\n", i + 1, r.title, r.url, snippet));
    }
    if resp.results.is_empty() && resp.answer.is_none() {
        out.push_str("No results found.\n");
    }
    out
}
