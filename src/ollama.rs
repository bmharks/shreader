//! Asks a local Ollama language model whether two headlines are the same
//! news story. Used once, to label recent stories as examples for the light
//! grouping model (which you then correct); not needed day to day.
//!
//! Talks only to Ollama on this machine (its standard address), with its own
//! client: the feed and image downloads are deliberately unable to reach
//! local addresses.

use std::time::Duration;

/// Where Ollama listens (its default; on this machine only)
pub const OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// The model used unless another is chosen in Settings
pub const DEFAULT_MODEL: &str = "llama3.1:8b";

/// The instructions. Small models drift towards "same topic = same story"
/// unless told plainly what an event is and what isn't one; this wording was
/// chosen by testing against hand-labelled pairs of real headlines.
const SYSTEM: &str = "You decide whether two news headlines report the same specific event: one particular happening, \
such as an announcement, incident, deal, decision, launch, death or disaster. \
Follow-ups, reactions and different angles on that one happening count as the same event. \
Answer no when the headlines only share a company, person, product, place or theme, \
or are general commentary on a theme. Analysis or opinion about a particular happening counts as that happening. \
Reply with exactly one word: yes or no.";

/// Worked examples, given as earlier turns of the conversation (asking for
/// JSON, or putting them in one prompt, made small models answer "no" to
/// everything, or explain instead of answering). Most are the near misses
/// models get wrong: same company or theme, different events.
const EXAMPLES: [(&str, &str, &str); 8] = [
    (
        "Microsoft outage takes down Outlook and Teams worldwide",
        "Outlook and Teams return to normal after massive Microsoft outage",
        "yes",
    ),
    ("Apple announces new MacBook Pro with M5 chip", "Apple faces EU fine over App Store rules", "no"),
    ("Why AI regulation will stifle innovation", "The hidden costs of running large language models", "no"),
    ("Hurricane Milton makes landfall in Florida", "Millions without power after Milton hits Florida coast", "yes"),
    ("Google Chrome adds tab grouping suggestions", "Google Chrome tests a new download manager", "no"),
    (
        "Ransomware attacks on hospitals are rising, report says",
        "Hackers steal patient data in cyberattack on Ohio clinic",
        "no",
    ),
    ("Tesla recalls 200,000 cars over seatbelt fault", "Tesla stock rises after strong delivery numbers", "no"),
    ("Boeing CEO steps down amid safety crisis", "Who will replace Boeing's departing chief executive?", "yes"),
];

#[derive(Clone)]
pub struct Ollama {
    client: reqwest::Client,
}

impl Default for Ollama {
    fn default() -> Self {
        Self::new()
    }
}

impl Ollama {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300)) // loading a model can take a while
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("the Ollama client's settings are valid");
        Self { client }
    }

    /// The models installed in Ollama, or why it couldn't be reached.
    pub async fn models(&self) -> Result<Vec<String>, String> {
        let response = self
            .client
            .get(format!("{}/api/tags", OLLAMA_URL))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| "Ollama isn't running on this computer".to_string())?;
        let body = read_json(response).await?;
        let mut names: Vec<String> = body["models"]
            .as_array()
            .map(|models| models.iter().filter_map(|m| m["name"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        names.sort();
        Ok(names)
    }

    /// Whether `model` judges the two headlines to be the same news story.
    pub async fn same_story(&self, model: &str, a: &str, b: &str) -> Result<bool, String> {
        let pair = |a: &str, b: &str| format!("Headline 1: {}\nHeadline 2: {}", a, b);
        let mut messages = vec![serde_json::json!({ "role": "system", "content": SYSTEM })];
        for (x, y, answer) in EXAMPLES {
            messages.push(serde_json::json!({ "role": "user", "content": pair(x, y) }));
            messages.push(serde_json::json!({ "role": "assistant", "content": answer }));
        }
        messages.push(serde_json::json!({ "role": "user", "content": pair(&one_line(a), &one_line(b)) }));
        let request = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": false,
            "options": { "temperature": 0, "num_predict": 4 },
        });
        let response = self
            .client
            .post(format!("{}/api/chat", OLLAMA_URL))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(request.to_string())
            .send()
            .await
            .map_err(|e| format!("Ollama didn't answer: {}", e))?;
        if !response.status().is_success() {
            return Err(format!("Ollama answered HTTP {}", response.status().as_u16()));
        }
        let body = read_json(response).await?;
        let answer = body["message"]["content"].as_str().unwrap_or("").trim().to_lowercase();
        if answer.starts_with("yes") {
            Ok(true)
        } else if answer.starts_with("no") {
            Ok(false)
        } else {
            Err(format!("unclear answer: {:?}", answer.chars().take(30).collect::<String>()))
        }
    }
}

/// A reply's JSON body (at most 1 MB: Ollama's replies here are tiny)
async fn read_json(response: reqwest::Response) -> Result<serde_json::Value, String> {
    let bytes = response.bytes().await.map_err(|e| format!("reading Ollama's reply: {}", e))?;
    if bytes.len() > 1024 * 1024 {
        return Err("Ollama's reply was unexpectedly large".to_string());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("unexpected reply: {}", e))
}

/// A headline on one line, at most 300 characters (they go into the prompt)
fn one_line(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect()
}
