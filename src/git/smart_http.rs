use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use tracing::{debug, info};

use crate::git::protocol::{delim_pkt, encode_pkt_line, extract_pack_from_sideband, flush_pkt, parse_pkt_lines};

#[derive(Clone)]
pub struct GitSmartHttpClient {
    client: Client,
    base_url: String,
}

impl GitSmartHttpClient {
    pub fn new(url: &str) -> Self {
        let mut base_url = url.trim_end_matches('/').to_string();
        if !base_url.ends_with(".git") {
            base_url.push_str(".git");
        }
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build HTTP client");

        Self { client, base_url }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Resolves the default branch HEAD commit OID.
    pub async fn resolve_head(&self, branch_override: Option<&str>) -> Result<(String, String)> {
        info!("Discovering refs for {}", self.base_url);
        let endpoint = format!("{}/git-upload-pack", self.base_url);

        let mut body = Vec::new();
        body.extend(encode_pkt_line("command=ls-refs"));
        body.extend(encode_pkt_line("agent=git-nfs"));
        body.extend(delim_pkt());
        body.extend(encode_pkt_line("ref-prefix HEAD"));
        body.extend(encode_pkt_line("ref-prefix refs/heads/"));
        body.extend(flush_pkt());

        let res = self
            .client
            .post(&endpoint)
            .header("Git-Protocol", "version=2")
            .header("Content-Type", "application/x-git-upload-pack-request")
            .body(body)
            .send()
            .await
            .context("Sending ls-refs request")?;

        if !res.status().is_success() {
            return Err(anyhow!(
                "ls-refs failed with status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }

        let resp_bytes = res.bytes().await.context("Reading ls-refs response")?;
        let lines = parse_pkt_lines(&resp_bytes)?;

        let mut head_oid = None;
        let mut branch_oid = None;
        let mut target_branch = branch_override.unwrap_or("master");

        for line in lines {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let oid = parts[0];
                let refname = parts[1];

                if refname == "HEAD" {
                    head_oid = Some(oid.to_string());
                }
                if let Some(target) = branch_override {
                    if refname == format!("refs/heads/{target}") {
                        branch_oid = Some(oid.to_string());
                        target_branch = target;
                    }
                } else if refname == "refs/heads/master" || refname == "refs/heads/main" {
                    branch_oid = Some(oid.to_string());
                    target_branch = if refname.ends_with("main") { "main" } else { "master" };
                }
            }
        }

        let chosen_oid = branch_oid
            .or(head_oid)
            .ok_or_else(|| anyhow!("Failed to locate HEAD or main/master ref from repository"))?;

        info!("Resolved branch '{target_branch}' to commit {chosen_oid}");
        Ok((chosen_oid, target_branch.to_string()))
    }

    /// Fetches all trees and commits for a given commit with `deepen 1` and `filter blob:none`.
    pub async fn fetch_trees_pack(&self, commit_oid: &str) -> Result<Vec<u8>> {
        info!("Fetching tree hierarchy for commit {commit_oid}...");
        let endpoint = format!("{}/git-upload-pack", self.base_url);

        let mut body = Vec::new();
        body.extend(encode_pkt_line("command=fetch"));
        body.extend(encode_pkt_line("agent=git-nfs"));
        body.extend(delim_pkt());
        body.extend(encode_pkt_line(&format!("want {commit_oid}")));
        body.extend(encode_pkt_line("deepen 1"));
        body.extend(encode_pkt_line("filter blob:none"));
        body.extend(encode_pkt_line("done"));
        body.extend(flush_pkt());

        let res = self
            .client
            .post(&endpoint)
            .header("Git-Protocol", "version=2")
            .header("Content-Type", "application/x-git-upload-pack-request")
            .body(body)
            .send()
            .await
            .context("Sending fetch trees request")?;

        if !res.status().is_success() {
            return Err(anyhow!(
                "fetch trees failed with status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }

        let resp_bytes = res.bytes().await.context("Reading fetch trees response")?;
        let pack = extract_pack_from_sideband(&resp_bytes)?;
        info!("Downloaded trees packfile ({} bytes)", pack.len());
        Ok(pack)
    }

    /// Lazily fetches a single blob by its OID.
    pub async fn fetch_blob_pack(&self, blob_oid: &str) -> Result<Vec<u8>> {
        debug!("Fetching blob {blob_oid}...");
        let endpoint = format!("{}/git-upload-pack", self.base_url);

        let mut body = Vec::new();
        body.extend(encode_pkt_line("command=fetch"));
        body.extend(encode_pkt_line("agent=git-nfs"));
        body.extend(delim_pkt());
        body.extend(encode_pkt_line(&format!("want {blob_oid}")));
        body.extend(encode_pkt_line("done"));
        body.extend(flush_pkt());

        let res = self
            .client
            .post(&endpoint)
            .header("Git-Protocol", "version=2")
            .header("Content-Type", "application/x-git-upload-pack-request")
            .body(body)
            .send()
            .await
            .context(format!("Sending fetch blob request for {blob_oid}"))?;

        if !res.status().is_success() {
            return Err(anyhow!(
                "fetch blob {blob_oid} failed with status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }

        let resp_bytes = res.bytes().await.context("Reading fetch blob response")?;
        let pack = extract_pack_from_sideband(&resp_bytes)?;
        Ok(pack)
    }
}
