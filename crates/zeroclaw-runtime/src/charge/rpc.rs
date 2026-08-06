//! Minimal Solana JSON-RPC client for settlement checks.
//!
//! Deliberately hand-rolled over `reqwest` rather than pulling in
//! `solana-client`: settlement needs exactly two methods, and the official
//! client drags in the full RPC/QUIC/websocket stack for them.
//!
//! Every method here is read-only. Nothing in this module can move funds — it
//! observes the chain to decide whether a customer already paid.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// How many signatures to inspect per reference before giving up.
///
/// A legitimate Solana Pay reference is touched by exactly one transaction, but
/// the key is public: anyone can spam it with dummy transfers to bury the real
/// payment. Scanning a window (rather than trusting the newest) means a
/// griefer cannot hide a genuine payment, while the cap keeps a spammed
/// reference from costing unbounded RPC calls.
pub const SIGNATURE_SCAN_LIMIT: usize = 10;

/// One entry from `getSignaturesForAddress`.
#[derive(Debug, Clone, Deserialize)]
pub struct SignatureInfo {
    pub signature: String,
    /// Present and non-null when the transaction failed on-chain.
    #[serde(default)]
    pub err: Option<serde_json::Value>,
}

impl SignatureInfo {
    /// A failed transaction moved no funds and can never settle an invoice.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.err.is_none()
    }
}

pub struct SolanaRpc {
    client: reqwest::Client,
    url: String,
}

impl SolanaRpc {
    pub fn new(url: impl Into<String>, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build Solana RPC HTTP client")?;
        Ok(Self {
            client,
            url: url.into(),
        })
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });

        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Solana RPC {method} request failed"))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .with_context(|| format!("Solana RPC {method} response unreadable"))?;

        if !status.is_success() {
            // 429 is the common case on public endpoints; surface it verbatim
            // so the caller can back off rather than treat it as "unpaid".
            anyhow::bail!("Solana RPC {method} returned HTTP {status}: {text}");
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("Solana RPC {method} returned invalid JSON: {text}"))?;

        if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!("Solana RPC {method} error: {err}");
        }

        Ok(parsed
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Signatures that touched `address`, newest first.
    ///
    /// For a Solana Pay reference this is the settlement lookup: the payer
    /// includes the reference as a read-only account key, so the paying
    /// transaction shows up here.
    pub async fn get_signatures_for_address(
        &self,
        address: &str,
        limit: usize,
    ) -> Result<Vec<SignatureInfo>> {
        let result = self
            .call(
                "getSignaturesForAddress",
                serde_json::json!([address, {"limit": limit, "commitment": "finalized"}]),
            )
            .await?;
        let sigs: Vec<SignatureInfo> = serde_json::from_value(result)
            .context("unexpected getSignaturesForAddress response shape")?;
        Ok(sigs)
    }

    /// Full transaction detail at `finalized` commitment.
    ///
    /// `finalized` is not negotiable for settlement: a `confirmed` transaction
    /// can still be dropped by a fork, which would mark a bill paid that never
    /// actually settled. `Ok(None)` means the node has no record yet.
    pub async fn get_transaction(&self, signature: &str) -> Result<Option<serde_json::Value>> {
        let result = self
            .call(
                "getTransaction",
                serde_json::json!([
                    signature,
                    {
                        "encoding": "jsonParsed",
                        "commitment": "finalized",
                        "maxSupportedTransactionVersion": 0,
                    }
                ]),
            )
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_info_treats_a_chain_error_as_unsuccessful() {
        let ok: SignatureInfo =
            serde_json::from_value(serde_json::json!({"signature": "s1"})).unwrap();
        assert!(ok.succeeded());

        let failed: SignatureInfo = serde_json::from_value(
            serde_json::json!({"signature": "s2", "err": {"InstructionError": [0, "Custom"]}}),
        )
        .unwrap();
        assert!(
            !failed.succeeded(),
            "a transaction that errored on-chain moved no funds"
        );

        let explicit_null: SignatureInfo =
            serde_json::from_value(serde_json::json!({"signature": "s3", "err": null})).unwrap();
        assert!(explicit_null.succeeded());
    }

    #[tokio::test]
    async fn rpc_errors_surface_instead_of_looking_like_no_payment() {
        // A transport failure must be an Err the caller can back off on. If it
        // degraded to "no signatures", a rate-limited poll would silently read
        // as "customer has not paid".
        let rpc = SolanaRpc::new("http://127.0.0.1:1", Duration::from_millis(200)).unwrap();
        assert!(rpc.get_signatures_for_address("Ref111", 10).await.is_err());
        assert!(rpc.get_transaction("sig").await.is_err());
    }
}
