//! Deciding whether an invoice was actually paid.
//!
//! # Why this is not "a signature exists"
//!
//! A Solana Pay reference is a public marker. Finding *a* transaction that
//! touched it proves only that somebody referenced the invoice — not that they
//! paid it. Anyone can submit a zero-value transfer carrying the reference, and
//! a naive `getSignaturesForAddress` hit would clear the bill.
//!
//! So settlement requires, from the transaction itself:
//!
//! 1. the transaction succeeded on-chain (`meta.err == null`),
//! 2. the credit landed on the **merchant's** account, not somebody else's,
//! 3. for SPL payments, the credit is in the **expected mint** (a worthless
//!    look-alike token must not settle a USDC bill),
//! 4. the credited amount is **at least** the invoiced amount, compared in
//!    integer base units.
//!
//! [`verify_transaction`] is pure so every one of those rules is testable
//! against fixture JSON, with no network involved.

use anyhow::Result;
use chrono::Utc;
use zeroclaw_config::schema::Config;

use super::rpc::{SIGNATURE_SCAN_LIMIT, SolanaRpc};
use super::store;
use super::types::Invoice;

/// Verdict for one candidate transaction against one invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyResult {
    /// Settles the invoice: correct recipient, correct mint, sufficient amount.
    Settles { credited_base_units: i64 },
    /// The transaction failed on-chain, so it moved nothing.
    ChainError,
    /// Credited the merchant in the right asset, but for less than invoiced.
    Underpaid { credited: i64, required: i64 },
    /// Nothing credited the merchant in the expected asset — the common shape
    /// of a reference-spam transaction.
    NoQualifyingCredit,
}

impl VerifyResult {
    #[must_use]
    pub fn settles(&self) -> bool {
        matches!(self, Self::Settles { .. })
    }
}

/// Base-unit credit to `recipient` in `mint` implied by a transaction's token
/// balance deltas. `None` when the transaction touches no such balance.
fn spl_credit(tx: &serde_json::Value, recipient: &str, mint: &str) -> Option<i64> {
    let meta = tx.get("meta")?;

    // Index by account so a pre-balance can be matched to its post-balance.
    // A first-time payer has no pre-entry at all, which reads as a zero base.
    let read = |key: &str| -> std::collections::HashMap<i64, i64> {
        meta.get(key)
            .and_then(|v| v.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| {
                        e.get("mint").and_then(|m| m.as_str()) == Some(mint)
                            && e.get("owner").and_then(|o| o.as_str()) == Some(recipient)
                    })
                    .filter_map(|e| {
                        let idx = e.get("accountIndex")?.as_i64()?;
                        // `amount` is a decimal string in base units. Parsing
                        // it as an integer is what keeps the whole comparison
                        // exact — `uiAmount` is a float and must never be used.
                        let amount = e
                            .get("uiTokenAmount")?
                            .get("amount")?
                            .as_str()?
                            .parse::<i64>()
                            .ok()?;
                        Some((idx, amount))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let pre = read("preTokenBalances");
    let post = read("postTokenBalances");
    if post.is_empty() {
        return None;
    }

    let mut credited: i64 = 0;
    for (idx, post_amount) in &post {
        let pre_amount = pre.get(idx).copied().unwrap_or(0);
        credited = credited.saturating_add(post_amount.saturating_sub(pre_amount));
    }
    Some(credited)
}

/// Lamport credit to `recipient` implied by native balance deltas.
fn sol_credit(tx: &serde_json::Value, recipient: &str) -> Option<i64> {
    let meta = tx.get("meta")?;
    let keys = tx
        .get("transaction")?
        .get("message")?
        .get("accountKeys")?
        .as_array()?;

    // `jsonParsed` yields objects with a `pubkey`; other encodings yield bare
    // strings. Accept both so the verdict does not depend on encoding.
    let index = keys.iter().position(|k| {
        k.as_str() == Some(recipient) || k.get("pubkey").and_then(|p| p.as_str()) == Some(recipient)
    })?;

    let pre = meta.get("preBalances")?.as_array()?.get(index)?.as_i64()?;
    let post = meta.get("postBalances")?.as_array()?.get(index)?.as_i64()?;
    Some(post.saturating_sub(pre))
}

/// Decide whether `tx` settles `invoice`. Pure — no network, no clock.
#[must_use]
pub fn verify_transaction(
    tx: &serde_json::Value,
    invoice: &Invoice,
    usdc_mint: &str,
) -> VerifyResult {
    // A transaction that errored on-chain moved no funds, whatever it references.
    let errored = tx
        .get("meta")
        .and_then(|m| m.get("err"))
        .is_some_and(|e| !e.is_null());
    if errored {
        return VerifyResult::ChainError;
    }

    let native = invoice.currency.eq_ignore_ascii_case("SOL");
    let credited = if native {
        sol_credit(tx, &invoice.recipient)
    } else {
        // Non-native settles only in the configured mint. A payment in some
        // other SPL token — including a worthless clone with the same ticker —
        // must not clear the bill.
        spl_credit(tx, &invoice.recipient, usdc_mint)
    };

    let Some(credited) = credited.filter(|c| *c > 0) else {
        return VerifyResult::NoQualifyingCredit;
    };

    if credited < invoice.amount_base_units {
        return VerifyResult::Underpaid {
            credited,
            required: invoice.amount_base_units,
        };
    }
    VerifyResult::Settles {
        credited_base_units: credited,
    }
}

/// Outcome of checking one invoice against the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// A qualifying transaction was found and the ledger row was settled.
    Settled { signature: String },
    /// Nothing qualifying yet — the invoice stays pending.
    StillPending,
}

/// Check one invoice against the chain and settle it if a qualifying payment
/// exists.
///
/// Scans the reference's recent signatures rather than trusting the newest, so
/// dummy transactions cannot bury a real payment. Returns `Err` only for RPC
/// failures — which must **not** be read as "unpaid", so the caller leaves the
/// invoice pending and retries later.
pub async fn check_invoice(
    rpc: &SolanaRpc,
    config: &Config,
    invoice: &Invoice,
) -> Result<CheckOutcome> {
    let signatures = rpc
        .get_signatures_for_address(&invoice.reference, SIGNATURE_SCAN_LIMIT)
        .await?;

    for sig in signatures.iter().filter(|s| s.succeeded()) {
        let Some(tx) = rpc.get_transaction(&sig.signature).await? else {
            continue;
        };
        let verdict = verify_transaction(&tx, invoice, config.charge.usdc_mint.trim());
        match &verdict {
            VerifyResult::Settles {
                credited_base_units,
            } => {
                let settled = store::mark_paid(config, &invoice.id, &sig.signature, Utc::now())?;
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "invoice_id": invoice.id,
                            "signature": sig.signature,
                            "credited_base_units": credited_base_units,
                            "required_base_units": invoice.amount_base_units,
                            "already_settled": !settled,
                        })),
                    "invoice settled on-chain"
                );
                return Ok(CheckOutcome::Settled {
                    signature: sig.signature.clone(),
                });
            }
            // Log rejections: an underpayment or wrong-mint transfer against a
            // real invoice is exactly what an operator needs to see, and it is
            // invisible if we silently keep waiting.
            other => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "invoice_id": invoice.id,
                            "signature": sig.signature,
                            "verdict": format!("{other:?}"),
                        })),
                    "transaction referenced an invoice but does not settle it"
                );
            }
        }
    }

    store::mark_checked(config, &invoice.id, Utc::now())?;
    Ok(CheckOutcome::StillPending)
}

/// Deliver the payment confirmation for one settled invoice back to the channel
/// the charge was requested on.
///
/// The message is built from the ledger row, not by the model: a payment
/// receipt must say what was actually recorded, and an LLM paraphrasing a row
/// is how a customer ends up with a wrong figure. This also costs no tokens and
/// cannot be derailed by conversation history.
///
/// `mark_notified` claims the row first and reports whether this call won, so
/// two concurrent passes can never announce the same payment twice.
pub async fn notify_paid(config: &Config, invoice: &Invoice) -> Result<bool> {
    if !invoice.is_notifiable() {
        return Ok(false);
    }
    let Some(target) = invoice.reply_target.as_deref() else {
        return Ok(false);
    };

    crate::cron::scheduler::deliver_announcement(
        config,
        &invoice.channel,
        target,
        invoice.thread_id.as_deref(),
        &invoice.confirmation_message(),
    )
    .await?;

    // Stamped only after delivery succeeded: a send failure leaves the row
    // unnotified so the next pass retries, rather than losing the confirmation.
    let claimed = store::mark_notified(config, &invoice.id, Utc::now())?;
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "invoice_id": invoice.id,
                "channel": invoice.channel,
                "claimed": claimed,
            })
        ),
        "payment confirmation delivered"
    );
    Ok(claimed)
}

/// Summary of one settlement pass, for CLI output and logging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettlementSummary {
    pub checked: usize,
    pub settled: usize,
    pub still_pending: usize,
    pub expired: usize,
    /// Invoices whose check failed (RPC error). They stay pending.
    pub errors: usize,
    /// Payment confirmations delivered to their originating channel.
    pub notified: usize,
    /// Confirmations that could not be delivered. The invoice stays paid and
    /// unnotified, so a later pass retries.
    pub notify_failures: usize,
}

/// Run one settlement pass over the pending ledger.
///
/// Expires stale invoices first so they stop consuming RPC calls, then checks
/// up to `max_checks_per_run` invoices, least-recently-checked first.
///
/// A per-invoice RPC failure is counted and skipped rather than aborting the
/// pass: one unlucky invoice must not stop the rest of the batch from settling.
pub async fn run_settlement_pass(config: &Config) -> Result<SettlementSummary> {
    let cfg = &config.charge;
    let mut summary = SettlementSummary {
        expired: store::expire_stale_invoices(config, cfg.invoice_expiry_hours, Utc::now())?,
        ..Default::default()
    };

    let due = store::due_for_settlement_check(config, cfg.max_checks_per_run)?;
    if due.is_empty() {
        return Ok(summary);
    }

    let rpc = SolanaRpc::new(cfg.effective_rpc_url(), std::time::Duration::from_secs(20))?;

    for invoice in due {
        summary.checked += 1;
        match check_invoice(&rpc, config, &invoice).await {
            Ok(CheckOutcome::Settled { .. }) => summary.settled += 1,
            Ok(CheckOutcome::StillPending) => summary.still_pending += 1,
            Err(e) => {
                summary.errors += 1;
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "invoice_id": invoice.id,
                            "error": format!("{e:#}"),
                        })),
                    "settlement check failed; invoice stays pending"
                );
            }
        }
    }

    // Announce every settled-but-unannounced invoice, including any left over
    // from an earlier pass whose channel was down. Runs after the checks so
    // payments settled in this same pass are announced immediately.
    notify_settled_invoices(config, &mut summary).await;

    Ok(summary)
}

/// Deliver outstanding payment confirmations, updating `summary`.
///
/// Delivery failures are counted, never propagated: one unreachable channel
/// must not abort the rest of the batch, and the row stays unnotified for the
/// next pass to retry.
async fn notify_settled_invoices(config: &Config, summary: &mut SettlementSummary) {
    let pending = match store::due_for_notification(config, config.charge.max_checks_per_run) {
        Ok(rows) => rows,
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{e:#}")})),
                "could not list invoices awaiting payment confirmation"
            );
            return;
        }
    };

    for invoice in pending {
        match notify_paid(config, &invoice).await {
            Ok(true) => summary.notified += 1,
            Ok(false) => {}
            Err(e) => {
                summary.notify_failures += 1;
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "invoice_id": invoice.id,
                            "channel": invoice.channel,
                            "error": format!("{e:#}"),
                        })),
                    "payment confirmation delivery failed; will retry next pass"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charge::types::InvoiceStatus;

    const MERCHANT: &str = "MerchantWallet1111111111111111111111111111";
    const USDC: &str = "UsdcMint1111111111111111111111111111111111";
    const OTHER_MINT: &str = "FakeMint1111111111111111111111111111111111";

    fn invoice(currency: &str, base_units: i64) -> Invoice {
        Invoice {
            id: "inv-1".to_string(),
            agent_alias: "main".to_string(),
            table_number: Some(7),
            customer: None,
            amount_base_units: base_units,
            currency: currency.to_string(),
            reference: "Ref111".to_string(),
            recipient: MERCHANT.to_string(),
            memo: "Table 7".to_string(),
            status: InvoiceStatus::Pending,
            tx_signature: None,
            created_at: Utc::now(),
            paid_at: None,
            last_checked_at: None,
            channel: "telegram".to_string(),
            reply_target: Some("chat-1".to_string()),
            thread_id: None,
            notified_at: None,
        }
    }

    /// An SPL transfer crediting `owner` `post - pre` base units of `mint`.
    fn spl_tx(owner: &str, mint: &str, pre: Option<&str>, post: &str) -> serde_json::Value {
        let mut meta = serde_json::json!({
            "err": null,
            "postTokenBalances": [{
                "accountIndex": 3,
                "mint": mint,
                "owner": owner,
                "uiTokenAmount": {"amount": post, "decimals": 6}
            }]
        });
        if let Some(pre) = pre {
            meta["preTokenBalances"] = serde_json::json!([{
                "accountIndex": 3,
                "mint": mint,
                "owner": owner,
                "uiTokenAmount": {"amount": pre, "decimals": 6}
            }]);
        }
        serde_json::json!({"meta": meta, "transaction": {"message": {"accountKeys": []}}})
    }

    fn sol_tx(recipient: &str, pre: i64, post: i64) -> serde_json::Value {
        serde_json::json!({
            "meta": {"err": null, "preBalances": [0, pre], "postBalances": [0, post]},
            "transaction": {"message": {"accountKeys": [
                {"pubkey": "Payer1111"}, {"pubkey": recipient}
            ]}}
        })
    }

    #[test]
    fn exact_payment_settles() {
        let inv = invoice("USDC", 10_150_000);
        let tx = spl_tx(MERCHANT, USDC, None, "10150000");
        assert_eq!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::Settles {
                credited_base_units: 10_150_000
            }
        );
    }

    #[test]
    fn overpayment_settles() {
        let inv = invoice("USDC", 10_000_000);
        let tx = spl_tx(MERCHANT, USDC, None, "12000000");
        assert!(verify_transaction(&tx, &inv, USDC).settles());
    }

    #[test]
    fn one_base_unit_short_does_not_settle() {
        // The precise reason amounts are integers: off-by-one-base-unit must be
        // detectable, and it is invisible through float comparison.
        let inv = invoice("USDC", 10_150_000);
        let tx = spl_tx(MERCHANT, USDC, None, "10149999");
        assert_eq!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::Underpaid {
                credited: 10_149_999,
                required: 10_150_000
            }
        );
    }

    #[test]
    fn zero_value_reference_spam_does_not_settle() {
        // The attack this whole module exists to stop: a 0-value transfer that
        // carries the reference would clear the bill under a naive
        // "a signature exists" check.
        let inv = invoice("USDC", 10_000_000);
        let tx = spl_tx(MERCHANT, USDC, Some("5000000"), "5000000");
        assert_eq!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::NoQualifyingCredit
        );
    }

    #[test]
    fn credit_is_the_delta_not_the_closing_balance() {
        // A merchant account that already held 100 USDC and received 1 must
        // not read as a 101 USDC payment.
        let inv = invoice("USDC", 10_000_000);
        let tx = spl_tx(MERCHANT, USDC, Some("100000000"), "101000000");
        assert_eq!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::Underpaid {
                credited: 1_000_000,
                required: 10_000_000
            }
        );
    }

    #[test]
    fn payment_in_a_different_mint_does_not_settle() {
        // A worthless look-alike token must never clear a USDC bill.
        let inv = invoice("USDC", 10_000_000);
        let tx = spl_tx(MERCHANT, OTHER_MINT, None, "999999999");
        assert_eq!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::NoQualifyingCredit
        );
    }

    #[test]
    fn payment_to_a_different_wallet_does_not_settle() {
        let inv = invoice("USDC", 10_000_000);
        let tx = spl_tx("SomeoneElse111", USDC, None, "10000000");
        assert_eq!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::NoQualifyingCredit
        );
    }

    #[test]
    fn a_failed_transaction_never_settles() {
        let inv = invoice("USDC", 1);
        let mut tx = spl_tx(MERCHANT, USDC, None, "999999999");
        tx["meta"]["err"] = serde_json::json!({"InstructionError": [0, "Custom"]});
        assert_eq!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::ChainError
        );
    }

    #[test]
    fn native_sol_settles_on_lamport_delta() {
        let inv = invoice("SOL", 1_000_000_000);
        assert!(verify_transaction(&sol_tx(MERCHANT, 0, 1_000_000_000), &inv, USDC).settles());
        assert_eq!(
            verify_transaction(&sol_tx(MERCHANT, 0, 999_999_999), &inv, USDC),
            VerifyResult::Underpaid {
                credited: 999_999_999,
                required: 1_000_000_000
            }
        );
    }

    #[test]
    fn native_sol_accepts_bare_string_account_keys() {
        // Non-jsonParsed encodings yield bare strings; the verdict must not
        // depend on which encoding a node returned.
        let inv = invoice("SOL", 500);
        let tx = serde_json::json!({
            "meta": {"err": null, "preBalances": [0, 0], "postBalances": [0, 500]},
            "transaction": {"message": {"accountKeys": ["Payer1111", MERCHANT]}}
        });
        assert!(verify_transaction(&tx, &inv, USDC).settles());
    }

    #[test]
    fn a_sol_invoice_is_not_settled_by_an_spl_transfer() {
        let inv = invoice("SOL", 1_000);
        let tx = spl_tx(MERCHANT, USDC, None, "999999999");
        assert_eq!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::NoQualifyingCredit
        );
    }

    #[test]
    fn malformed_transactions_do_not_settle() {
        let inv = invoice("USDC", 1);
        for tx in [
            serde_json::json!({}),
            serde_json::json!({"meta": {"err": null}}),
            serde_json::json!({"meta": {"err": null, "postTokenBalances": []}}),
            // Non-numeric amount must not be coerced into a credit.
            serde_json::json!({"meta": {"err": null, "postTokenBalances": [{
                "accountIndex": 1, "mint": USDC, "owner": MERCHANT,
                "uiTokenAmount": {"amount": "not-a-number"}
            }]}}),
        ] {
            assert_eq!(
                verify_transaction(&tx, &inv, USDC),
                VerifyResult::NoQualifyingCredit,
                "malformed tx must never settle: {tx}"
            );
        }
    }

    #[test]
    fn ui_amount_float_is_ignored_in_favour_of_the_base_unit_string() {
        // `uiAmount` is a float and rounds; `amount` is the exact base-unit
        // string. A tx whose float looks sufficient but whose base units are
        // short must be rejected.
        let inv = invoice("USDC", 10_150_000);
        let mut tx = spl_tx(MERCHANT, USDC, None, "10149999");
        tx["meta"]["postTokenBalances"][0]["uiTokenAmount"]["uiAmount"] =
            serde_json::json!(10.15_f64);
        assert!(matches!(
            verify_transaction(&tx, &inv, USDC),
            VerifyResult::Underpaid { .. }
        ));
    }

    // ── End-to-end against a mock RPC ────────────────────────────────────

    use crate::charge::store as invoice_store;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config_for(dir: &std::path::Path, rpc_url: &str) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            charge: zeroclaw_config::schema::ChargeConfig {
                merchant_wallet: MERCHANT.to_string(),
                usdc_mint: USDC.to_string(),
                rpc_url: rpc_url.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Mock node: one signature for any address, and `tx` for any lookup.
    /// Responds by method name so a single server serves both calls.
    async fn mock_node(signature: &str, tx: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        let sig = signature.to_string();
        Mock::given(method("POST"))
            .respond_with(move |req: &wiremock::Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
                let m = body.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let result = if m == "getSignaturesForAddress" {
                    serde_json::json!([{ "signature": sig, "err": null }])
                } else {
                    tx.clone()
                };
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"jsonrpc":"2.0","id":1,"result":result}))
            })
            .mount(&server)
            .await;
        server
    }

    async fn seed(config: &Config, inv: &Invoice) {
        invoice_store::insert_invoice(config, inv).expect("seed invoice");
    }

    #[tokio::test]
    async fn a_qualifying_payment_settles_the_ledger_row_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let tx = spl_tx(MERCHANT, USDC, None, "10150000");
        let server = mock_node("sig-abc", tx).await;
        let config = config_for(dir.path(), &server.uri());

        let inv = invoice("USDC", 10_150_000);
        seed(&config, &inv).await;

        let summary = run_settlement_pass(&config).await.expect("pass runs");
        assert_eq!(summary.settled, 1, "{summary:?}");
        assert_eq!(summary.still_pending, 0);

        let stored = invoice_store::get_invoice(&config, "main", &inv.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, InvoiceStatus::Paid);
        assert_eq!(stored.tx_signature.as_deref(), Some("sig-abc"));
        assert!(stored.paid_at.is_some());
    }

    #[tokio::test]
    async fn an_underpayment_leaves_the_invoice_pending_end_to_end() {
        // The headline guarantee: a real transaction against the right
        // reference, for too little money, must NOT clear the bill.
        let dir = tempfile::tempdir().unwrap();
        let tx = spl_tx(MERCHANT, USDC, None, "9999999");
        let server = mock_node("sig-short", tx).await;
        let config = config_for(dir.path(), &server.uri());

        let inv = invoice("USDC", 10_000_000);
        seed(&config, &inv).await;

        let summary = run_settlement_pass(&config).await.unwrap();
        assert_eq!(summary.settled, 0, "underpayment must not settle");
        assert_eq!(summary.still_pending, 1);

        let stored = invoice_store::get_invoice(&config, "main", &inv.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, InvoiceStatus::Pending);
        assert!(stored.tx_signature.is_none());
        assert!(
            stored.last_checked_at.is_some(),
            "a completed check must stamp the invoice so the queue rotates"
        );
    }

    #[tokio::test]
    async fn zero_value_reference_spam_leaves_the_invoice_pending_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let tx = spl_tx(MERCHANT, USDC, Some("5000000"), "5000000");
        let server = mock_node("sig-spam", tx).await;
        let config = config_for(dir.path(), &server.uri());

        let inv = invoice("USDC", 10_000_000);
        seed(&config, &inv).await;

        assert_eq!(run_settlement_pass(&config).await.unwrap().settled, 0);
        assert_eq!(
            invoice_store::get_invoice(&config, "main", &inv.id)
                .unwrap()
                .unwrap()
                .status,
            InvoiceStatus::Pending
        );
    }

    #[tokio::test]
    async fn an_rpc_failure_never_settles_or_expires_an_invoice() {
        // A rate-limited or unreachable node must not be read as "unpaid" in a
        // way that loses data — the row stays exactly as it was, to retry.
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path(), "http://127.0.0.1:1");
        let inv = invoice("USDC", 10_000_000);
        seed(&config, &inv).await;

        let summary = run_settlement_pass(&config).await.expect("pass survives");
        assert_eq!(summary.errors, 1, "{summary:?}");
        assert_eq!(summary.settled, 0);

        let stored = invoice_store::get_invoice(&config, "main", &inv.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, InvoiceStatus::Pending);
        assert!(
            stored.last_checked_at.is_none(),
            "a failed check must not stamp the invoice as checked"
        );
    }

    #[tokio::test]
    async fn a_second_pass_does_not_resettle_an_already_paid_invoice() {
        let dir = tempfile::tempdir().unwrap();
        let tx = spl_tx(MERCHANT, USDC, None, "10000000");
        let server = mock_node("sig-1", tx).await;
        let config = config_for(dir.path(), &server.uri());
        seed(&config, &invoice("USDC", 10_000_000)).await;

        assert_eq!(run_settlement_pass(&config).await.unwrap().settled, 1);
        let second = run_settlement_pass(&config).await.unwrap();
        assert_eq!(second.checked, 0, "a paid invoice leaves the queue");
        assert_eq!(second.settled, 0);
    }

    #[tokio::test]
    async fn expired_invoices_are_dropped_before_any_rpc_call() {
        let dir = tempfile::tempdir().unwrap();
        // Unreachable node: if expiry needed RPC, this would error instead.
        let config = config_for(dir.path(), "http://127.0.0.1:1");
        let mut old = invoice("USDC", 1);
        old.created_at = Utc::now() - chrono::Duration::hours(48);
        seed(&config, &old).await;

        let summary = run_settlement_pass(&config).await.unwrap();
        assert_eq!(summary.expired, 1);
        assert_eq!(summary.checked, 0, "an expired invoice costs no RPC call");
        assert_eq!(summary.errors, 0);
    }

    #[tokio::test]
    async fn an_empty_ledger_makes_no_rpc_call() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for(dir.path(), "http://127.0.0.1:1");
        let summary = run_settlement_pass(&config).await.expect("no-op pass");
        assert_eq!(summary, SettlementSummary::default());
    }

    // ── Payment confirmation delivery ────────────────────────────────────

    // Delivery goes through a process-wide `OnceLock`, so every test module in
    // this binary shares one recorder rather than racing to install its own.
    use crate::cron::scheduler::test_delivery::{
        recorded_containing, register_recording_delivery_fn as install_recorder,
    };

    /// (channel, target, thread_id, body) of one recorded delivery.
    type Sent = (String, String, Option<String>, String);

    fn delivered_for(invoice_id: &str) -> Vec<Sent> {
        recorded_containing(invoice_id)
    }

    #[test]
    fn the_confirmation_states_table_amount_and_invoice() {
        // Exactly what was asked for: table, charge, invoice — built from the
        // ledger row, never paraphrased by a model.
        let mut inv = invoice("USDC", 10_150_000);
        inv.id = "inv-confirm".to_string();
        inv.table_number = Some(7);
        let msg = inv.confirmation_message();
        assert!(msg.contains("Table 7"), "{msg}");
        assert!(msg.contains("10.15 USDC"), "{msg}");
        assert!(msg.contains("inv-confirm"), "{msg}");
    }

    #[tokio::test]
    async fn a_settled_payment_is_announced_to_the_originating_channel() {
        install_recorder();
        let dir = tempfile::tempdir().unwrap();
        let tx = spl_tx(MERCHANT, USDC, None, "10000000");
        let server = mock_node("sig-notify", tx).await;
        let config = config_for(dir.path(), &server.uri());

        let mut inv = invoice("USDC", 10_000_000);
        inv.id = "inv-notify-1".to_string();
        inv.channel = "telegram".to_string();
        inv.reply_target = Some("chat-99".to_string());
        inv.thread_id = Some("thread-5".to_string());
        seed(&config, &inv).await;

        let summary = run_settlement_pass(&config).await.unwrap();
        assert_eq!(summary.settled, 1, "{summary:?}");
        assert_eq!(summary.notified, 1, "{summary:?}");

        let sent = delivered_for("inv-notify-1");
        assert_eq!(sent.len(), 1, "exactly one confirmation: {sent:?}");
        let (channel, target, thread, body) = &sent[0];
        assert_eq!(channel, "telegram", "must go back to the origin channel");
        assert_eq!(target, "chat-99");
        assert_eq!(thread.as_deref(), Some("thread-5"));
        assert!(body.contains("Table 7"), "{body}");
        assert!(body.contains("10 USDC"), "{body}");
        assert!(body.contains("inv-notify-1"), "{body}");

        let stored = invoice_store::get_invoice(&config, "main", "inv-notify-1")
            .unwrap()
            .unwrap();
        assert!(stored.notified_at.is_some());
    }

    #[tokio::test]
    async fn a_payment_is_announced_only_once_across_repeated_passes() {
        install_recorder();
        let dir = tempfile::tempdir().unwrap();
        let tx = spl_tx(MERCHANT, USDC, None, "10000000");
        let server = mock_node("sig-once", tx).await;
        let config = config_for(dir.path(), &server.uri());

        let mut inv = invoice("USDC", 10_000_000);
        inv.id = "inv-once".to_string();
        inv.channel = "telegram".to_string();
        inv.reply_target = Some("chat-1".to_string());
        seed(&config, &inv).await;

        run_settlement_pass(&config).await.unwrap();
        let second = run_settlement_pass(&config).await.unwrap();
        assert_eq!(second.notified, 0, "a later pass must not re-announce");
        assert_eq!(
            delivered_for("inv-once").len(),
            1,
            "the customer must not be told twice that they paid"
        );
    }

    #[tokio::test]
    async fn an_invoice_with_no_origin_channel_settles_without_announcing() {
        // A charge created from the CLI has nowhere to reply; settlement must
        // still work, silently.
        install_recorder();
        let dir = tempfile::tempdir().unwrap();
        let tx = spl_tx(MERCHANT, USDC, None, "10000000");
        let server = mock_node("sig-silent", tx).await;
        let config = config_for(dir.path(), &server.uri());

        let mut inv = invoice("USDC", 10_000_000);
        inv.id = "inv-silent".to_string();
        inv.channel = String::new();
        inv.reply_target = None;
        seed(&config, &inv).await;

        let summary = run_settlement_pass(&config).await.unwrap();
        assert_eq!(summary.settled, 1);
        assert_eq!(summary.notified, 0);
        assert!(delivered_for("inv-silent").is_empty());
        assert_eq!(
            invoice_store::get_invoice(&config, "main", "inv-silent")
                .unwrap()
                .unwrap()
                .status,
            InvoiceStatus::Paid
        );
    }

    #[tokio::test]
    async fn an_unpaid_invoice_is_never_announced() {
        install_recorder();
        let dir = tempfile::tempdir().unwrap();
        let tx = spl_tx(MERCHANT, USDC, None, "1");
        let server = mock_node("sig-underpaid", tx).await;
        let config = config_for(dir.path(), &server.uri());

        let mut inv = invoice("USDC", 10_000_000);
        inv.id = "inv-unpaid".to_string();
        inv.channel = "telegram".to_string();
        inv.reply_target = Some("chat-1".to_string());
        seed(&config, &inv).await;

        let summary = run_settlement_pass(&config).await.unwrap();
        assert_eq!(summary.settled, 0);
        assert_eq!(summary.notified, 0);
        assert!(
            delivered_for("inv-unpaid").is_empty(),
            "an underpaid invoice must never produce a payment confirmation"
        );
    }
}
