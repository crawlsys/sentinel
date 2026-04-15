//! Qdrant Cloud adapter — implements VectorStorePort.
//!
//! Handles HTTP client lifecycle, auth, URL construction, and
//! server-side embedding model configuration.

use anyhow::{Context, Result};
use sentinel_domain::ports::{VectorPoint, VectorScrollResult, VectorSearchHit, VectorStorePort};
use tracing::debug;

/// Qdrant Cloud configuration.
#[derive(Clone, serde::Deserialize)]
pub struct QdrantConfig {
    pub cluster_url: String,
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
}

fn default_model() -> String {
    "sentence-transformers/all-MiniLM-L6-v2".to_string()
}

impl QdrantConfig {
    /// Load from `~/.qdrant/config.json`.
    pub fn load() -> Option<Self> {
        let path = dirs::home_dir()?.join(".qdrant").join("config.json");
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }
}

/// Infrastructure adapter implementing `VectorStorePort` via Qdrant REST API.
pub struct QdrantAdapter {
    client: reqwest::Client,
    config: QdrantConfig,
}

impl QdrantAdapter {
    /// Create a new adapter from config. Returns `None` if no config found.
    pub fn from_config() -> Option<Self> {
        let config = QdrantConfig::load()?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .ok()?;
        Some(Self { client, config })
    }

    fn url(&self, collection: &str, path: &str) -> String {
        format!(
            "{}/collections/{}/{}",
            self.config.cluster_url, collection, path
        )
    }
}

#[async_trait::async_trait]
impl VectorStorePort for QdrantAdapter {
    async fn query(
        &self,
        collection: &str,
        query_text: &str,
        limit: u32,
        min_score: f64,
    ) -> Result<Vec<VectorSearchHit>> {
        let body = serde_json::json!({
            "query": { "text": query_text, "model": self.config.model },
            "using": "text-dense",
            "limit": limit,
            "with_payload": true,
            "params": { "hnsw_ef": 64 }
        });

        let resp = self
            .client
            .post(&self.url(collection, "points/query"))
            .header("api-key", &self.config.api_key)
            .json(&body)
            .send()
            .await
            .context("Qdrant query request failed")?;

        let json: serde_json::Value = resp.json().await.context("Qdrant query parse failed")?;

        let points = json
            .get("result")
            .and_then(|r| r.get("points"))
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        let hits = points
            .iter()
            .filter_map(|p| {
                let score = p.get("score")?.as_f64()?;
                if score < min_score {
                    return None;
                }
                let id = p
                    .get("id")
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                let payload = p.get("payload").cloned().unwrap_or(serde_json::json!({}));
                Some(VectorSearchHit { id, score, payload })
            })
            .collect();

        debug!(collection, hits = points.len(), "Qdrant query complete");
        Ok(hits)
    }

    async fn upsert_points(&self, collection: &str, points: Vec<VectorPoint>) -> Result<()> {
        let qdrant_points: Vec<serde_json::Value> = points
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "vector": {
                        "text-dense": {
                            "text": p.text,
                            "model": self.config.model
                        }
                    },
                    "payload": p.payload
                })
            })
            .collect();

        // Batch in chunks of 20
        for batch in qdrant_points.chunks(20) {
            let body = serde_json::json!({ "points": batch });
            self.client
                .put(&self.url(collection, "points?wait=true"))
                .header("api-key", &self.config.api_key)
                .json(&body)
                .send()
                .await
                .context("Qdrant upsert failed")?;
        }

        debug!(collection, count = points.len(), "Qdrant upsert complete");
        Ok(())
    }

    async fn scroll(
        &self,
        collection: &str,
        filter: Option<serde_json::Value>,
        limit: u32,
    ) -> Result<Vec<VectorScrollResult>> {
        let mut body = serde_json::json!({
            "limit": limit,
            "with_payload": true
        });
        if let Some(f) = filter {
            body["filter"] = f;
        }

        let resp = self
            .client
            .post(&self.url(collection, "points/scroll"))
            .header("api-key", &self.config.api_key)
            .json(&body)
            .send()
            .await
            .context("Qdrant scroll failed")?;

        let json: serde_json::Value = resp.json().await?;

        let points = json
            .get("result")
            .and_then(|r| r.get("points"))
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        let results = points
            .iter()
            .filter_map(|p| {
                let id = p.get("id")?.as_str()?.to_string();
                let payload = p.get("payload").cloned().unwrap_or(serde_json::json!({}));
                Some(VectorScrollResult { id, payload })
            })
            .collect();

        Ok(results)
    }

    async fn set_payload(
        &self,
        collection: &str,
        point_ids: &[String],
        payload: serde_json::Value,
    ) -> Result<()> {
        let body = serde_json::json!({
            "payload": payload,
            "points": point_ids
        });

        self.client
            .post(&self.url(collection, "points/payload"))
            .header("api-key", &self.config.api_key)
            .json(&body)
            .send()
            .await
            .context("Qdrant set_payload failed")?;

        debug!(
            collection,
            count = point_ids.len(),
            "Qdrant set_payload complete"
        );
        Ok(())
    }

    async fn get_points(
        &self,
        collection: &str,
        ids: &[String],
        payload_fields: &[&str],
    ) -> Result<Vec<VectorScrollResult>> {
        let body = serde_json::json!({
            "ids": ids,
            "with_payload": if payload_fields.is_empty() {
                serde_json::json!(true)
            } else {
                serde_json::json!(payload_fields)
            }
        });

        let resp = self
            .client
            .post(&self.url(collection, "points"))
            .header("api-key", &self.config.api_key)
            .json(&body)
            .send()
            .await
            .context("Qdrant get_points failed")?;

        let json: serde_json::Value = resp.json().await?;

        let points = json
            .get("result")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        let results = points
            .iter()
            .filter_map(|p| {
                let id = p.get("id")?.as_str()?.to_string();
                let payload = p.get("payload").cloned().unwrap_or(serde_json::json!({}));
                Some(VectorScrollResult { id, payload })
            })
            .collect();

        Ok(results)
    }
}
