use crate::config::Config;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::{debug, warn};

/// Resolves iMessage handles (phone/email) to contact display names via a
/// BlueBubbles instance that has macOS Contacts access (read-only). In this
/// deployment the message account has no address book, so this points at the
/// personal account. Results are cached in-memory for the life of the process.
pub struct ContactResolver {
    client: reqwest::Client,
    base_url: Option<String>,
    password: String,
    cache: Mutex<HashMap<String, Option<String>>>,
}

impl ContactResolver {
    pub fn new(config: &Config) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: config.contacts_url(),
            password: config.contacts_password(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Whether contact resolution is configured/enabled.
    pub fn enabled(&self) -> bool {
        self.base_url.is_some()
    }

    /// Resolve a handle to a display name. Returns None if disabled, the handle
    /// is empty/unknown, the contact isn't found, or the lookup fails.
    pub async fn resolve(&self, handle: &str) -> Option<String> {
        let base = self.base_url.as_ref()?;
        if handle.is_empty() || handle == "unknown" {
            return None;
        }

        // Cache hit (includes cached "not found" as Some(None)).
        if let Ok(cache) = self.cache.lock() {
            if let Some(hit) = cache.get(handle) {
                return hit.clone();
            }
        }

        let url = format!(
            "{}/api/v1/contact/query?password={}",
            base,
            urlencoding::encode(&self.password)
        );
        let body = serde_json::json!({ "addresses": [handle] });

        let name = match self.client.post(&url).json(&body).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
                Ok(v) => extract_name(&v),
                Err(e) => {
                    warn!("Contact query parse failed for {}: {}", handle, e);
                    return None; // transient — don't cache, retry next time
                }
            },
            Ok(resp) => {
                warn!("Contact query for {} returned status {}", handle, resp.status());
                return None;
            }
            Err(e) => {
                warn!("Contact query for {} failed: {}", handle, e);
                return None;
            }
        };

        debug!("Resolved handle {} -> {:?}", handle, name);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(handle.to_string(), name.clone());
        }
        name
    }
}

/// Pull a display name out of a BlueBubbles /contact/query response, preferring
/// displayName, then firstName + lastName.
fn extract_name(v: &serde_json::Value) -> Option<String> {
    let c = v.get("data")?.as_array()?.first()?;

    if let Some(dn) = c.get("displayName").and_then(|x| x.as_str()) {
        let dn = dn.trim();
        if !dn.is_empty() {
            return Some(dn.to_string());
        }
    }

    let first = c.get("firstName").and_then(|x| x.as_str()).unwrap_or("");
    let last = c.get("lastName").and_then(|x| x.as_str()).unwrap_or("");
    let full = format!("{} {}", first, last).trim().to_string();
    if full.is_empty() {
        None
    } else {
        Some(full)
    }
}
