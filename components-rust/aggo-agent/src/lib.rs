use golem_rust::golem_ai::golem::llm::llm;
use golem_rust::golem_ai::golem::llm::llm::{Config, ContentPart, Message, Role};
use golem_rust::golem_ai::golem::web_search::types;
use golem_rust::golem_ai::golem::web_search::web_search;
use golem_rust::{agent_definition, agent_implementation, description, Schema};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebSearchProvider {
    Brave,
    Google,
    Serper,
    Tavily,
}

impl WebSearchProvider {
    fn from_env() -> Self {
        match std::env::var("WEB_SEARCH_PROVIDER")
            .unwrap_or_else(|_| "brave".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "brave" => Self::Brave,
            "google" => Self::Google,
            "serper" => Self::Serper,
            "tavily" => Self::Tavily,
            other => panic!(
                "Unsupported WEB_SEARCH_PROVIDER={other}. Supported: brave|google|serper|tavily"
            ),
        }
    }

    fn required_env_vars(&self) -> &'static [&'static str] {
        match self {
            Self::Brave => &["BRAVE_API_KEY"],
            Self::Google => &["GOOGLE_API_KEY", "GOOGLE_SEARCH_ENGINE_ID"],
            Self::Serper => &["SERPER_API_KEY"],
            Self::Tavily => &["TAVILY_API_KEY"],
        }
    }

    fn display_name(&self) -> &'static str {
        match self {
            Self::Brave => "Brave",
            Self::Google => "Google",
            Self::Serper => "Serper",
            Self::Tavily => "Tavily",
        }
    }
}

#[derive(Clone, Schema, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    url: String,
    title: String,
    snippet: String,
}

#[agent_definition]
pub trait ResearchAgent {
    fn new() -> Self;

    #[description("Research and summarize a topic")]
    fn research(&self, topic: String) -> String;
}

struct ResearchAgentImpl {
    model: String,
    web_search_provider: WebSearchProvider,
}

#[agent_implementation]
impl ResearchAgent for ResearchAgentImpl {
    fn new() -> Self {
        // Prefer a component-specific variable to avoid collisions with provider/runtime env.
        let model = std::env::var("AGGO_LLM_MODEL")
            .or_else(|_| std::env::var("LLM_MODEL"))
            .unwrap_or_else(|_| "gpt-4".to_string());
        if model.trim().is_empty() || model.trim() == "changeme" {
            panic!(
                "LLM_MODEL env var not configured. Check the component env in golem.yaml."
            );
        }

        let web_search_provider = WebSearchProvider::from_env();

        for key in web_search_provider.required_env_vars() {
            match std::env::var(key) {
                Ok(value) if !value.trim().is_empty() && value.trim() != "changeme" => {}
                _ => panic!(
                    "{key} env var not configured (required for {} web search). Check the component env in golem.yaml.",
                    web_search_provider.display_name()
                ),
            }
        }

        Self {
            model,
            web_search_provider,
        }
    }

    fn research(&self, topic: String) -> String {
        let search_results = match search_web_for_topic(self.web_search_provider, &topic) {
            Ok(sr) => sr,
            Err(err) => {
                // If the web search provider fails (rate limit, invalid key, backend error),
                // proceed with an empty/annotated result set so the LLM can still respond.
                // This avoids returning a 400/500 to HTTP callers when third-party search fails.
                vec![SearchResult {
                    url: "".to_string(),
                    title: "search-failed".to_string(),
                    snippet: format!("Web search failed: {}", err),
                }]
            }
        };

        let search_results_json = serde_json::to_string(&search_results).unwrap_or_else(|_| "[]".to_string());

        let prompt = format!(
            "I'm writing a report on the topic \"{}\",\n\
             Your job is to be a research-assistant and provide me an initial overview on the topic so I can dive into it in more detail.\n\
             At the bottom are top search results from a search engine in json format. Use your own knowledge and the snippets from the search results to create the overview.\n\
             Also include the best links to look into to learn more about the topic. Prioritize objective and reliable sources.\n\
             \n\
             Search results: {}",
            topic, search_results_json
        );

        let config = Config {
            model: self.model.clone(),
            temperature: None,
            max_tokens: None,
            stop_sequences: None,
            tools: None,
            tool_choice: None,
            provider_options: None,
        };

        let events = vec![llm::Event::Message(Message {
            role: Role::Assistant,
            name: Some("research-agent".to_string()),
            content: vec![ContentPart::Text(prompt)],
        })];

        let response = match llm::send(&events, &config) {
            Ok(r) => r,
            Err(e) => {
                let env_aggo_llm_model = std::env::var("AGGO_LLM_MODEL").ok();
                let env_llm_model = std::env::var("LLM_MODEL").ok();
                let ollama_base_url = std::env::var("GOLEM_OLLAMA_BASE_URL").ok();
                return format!(
                    "LLM call failed (model: {:?}, env AGGO_LLM_MODEL: {:?}, env LLM_MODEL: {:?}, GOLEM_OLLAMA_BASE_URL: {:?}). Display: {}. Debug: {:?}\n\nHint: for Ollama, ensure the model exists locally (e.g. `ollama pull {}`) and that GOLEM_OLLAMA_BASE_URL points to the right server.",
                    self.model,
                    env_aggo_llm_model,
                    env_llm_model,
                    ollama_base_url,
                    e,
                    e,
                    self.model
                );
            }
        };

        let text_result = response
            .content
            .iter()
            .filter_map(|content_part| match content_part {
                ContentPart::Text(txt) => Some(txt.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!("Finished research for topic {}:\n{}", topic, text_result)
    }
}

fn search_web_for_topic(provider: WebSearchProvider, topic: &str) -> Result<Vec<SearchResult>, String> {
    fn truncate_for_log(input: &str, max_len: usize) -> String {
        if input.len() <= max_len {
            return input.to_string();
        }
        format!("{}…<truncated>", &input[..max_len])
    }

    fn brave_http_search(topic: &str) -> Result<Vec<SearchResult>, String> {
        // Brave docs: https://api-dashboard.search.brave.com/app/documentation/web-search/get-started
        // GET https://api.search.brave.com/res/v1/web/search?q=...
        // Headers: Accept: application/json, X-Subscription-Token: <API_KEY>
        let token = match std::env::var("BRAVE_API_KEY") {
            Ok(v) if !v.trim().is_empty() && v.trim() != "changeme" => v,
            _ => return Err("BRAVE_API_KEY env var not configured".to_string()),
        };

        #[derive(serde::Serialize)]
        struct BraveQuery<'a> {
            q: &'a str,
            count: u8,
            offset: u8,
            search_lang: &'a str,
            safesearch: &'a str,
        }

        // count max is 20 per Brave docs.
        let query = BraveQuery {
            q: topic,
            count: 10,
            offset: 0,
            search_lang: "en",
            safesearch: "off",
        };

        let client = golem_wasi_http::Client::new();
        let response = client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("Accept", "application/json")
            // Avoid gzip unless we implement decompression.
            .header("Accept-Encoding", "identity")
            .header("X-Subscription-Token", token)
            .query(&query)
            .send()
            .map_err(|e| format!("Brave HTTP request failed: {}", e))?;

        let status = response.status();
        let body_text = response
            .text()
            .map_err(|e| format!("Failed to read Brave response body: {}", e))?;

        if !status.is_success() {
            return Err(format!(
                "Brave HTTP error status={} body={}",
                status.as_u16(),
                truncate_for_log(&body_text, 2000)
            ));
        }

        #[derive(serde::Deserialize)]
        struct BraveWebSearchApiResponse {
            web: Option<BraveWebSection>,
        }

        #[derive(serde::Deserialize)]
        struct BraveWebSection {
            results: Vec<BraveWebResult>,
        }

        #[derive(serde::Deserialize)]
        struct BraveWebResult {
            title: Option<String>,
            url: Option<String>,
            description: Option<String>,
        }

        let parsed: BraveWebSearchApiResponse = serde_json::from_str(&body_text).map_err(|e| {
            format!(
                "Failed to parse Brave JSON: {} body={}",
                e,
                truncate_for_log(&body_text, 2000)
            )
        })?;

        let mut out = Vec::new();
        let web = match parsed.web {
            Some(w) => w,
            None => return Ok(out),
        };

        for r in web.results {
            let Some(url) = r.url.filter(|u| !u.trim().is_empty()) else {
                continue;
            };
            let title = r.title.unwrap_or_else(|| "(no title)".to_string());
            let snippet = r.description.unwrap_or_default();
            out.push(SearchResult { url, title, snippet });
        }

        Ok(out)
    }

    if provider == WebSearchProvider::Brave {
        return brave_http_search(topic);
    }

    let pages_to_retrieve = 3;

    let session = match web_search::start_search(&web_search::SearchParams {
        query: topic.to_string(),
        language: Some("lang_en".to_string()),
        safe_search: Some(types::SafeSearchLevel::Off),
        max_results: Some(10),
        time_range: None,
        include_domains: None,
        exclude_domains: None,
        include_images: None,
        include_html: None,
        advanced_answer: Some(true),
        region: None,
    }) {
        Ok(s) => s,
        Err(e) => {
            return Err(format!(
                "Failed to start web search (provider: {}, query: {:?}). Display: {}. Debug: {:?}",
                provider.display_name(),
                topic,
                e,
                e
            ))
        }
    };

    let mut content: Vec<SearchResult> = Vec::new();

    for page_index in 0..pages_to_retrieve {
        match session.next_page() {
            Ok(page) => {
                for item in page {
                    content.push(SearchResult {
                        url: item.url.clone(),
                        title: item.title.clone(),
                        snippet: item.snippet.clone(),
                    });
                }
            }
            Err(e) => {
                return Err(format!(
                    "Failed to retrieve web search page {}/{} (provider: {}, query: {:?}). Display: {}. Debug: {:?}",
                    page_index + 1,
                    pages_to_retrieve,
                    provider.display_name(),
                    topic,
                    e,
                    e
                ));
            }
        }
    }

    Ok(content)
}
