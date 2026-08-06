//! Read the charge ledger, so outstanding balances come from data.
//!
//! This tool exists because of a specific failure: asked what was outstanding,
//! the model reconstructed a table of "pending charges" from conversation
//! history — inventing amounts, carrying forward a typo, and adding to a
//! remembered running total. Nothing it said was backed by anything.
//!
//! Instructions alone cannot fix that. Telling a model not to summarise leaves
//! it with a question it cannot answer and an obvious-looking way to answer it
//! anyway. The durable fix is to give it a real source: the ledger the `charge`
//! tool writes.
//!
//! Totals are aggregated in SQL over every matching row and rendered here, not
//! by the model, so a reported balance is arithmetic rather than recollection.

use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write as _;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::{ChargeConfig, Config};

use crate::charge::store;
use crate::charge::types::InvoiceStatus;

/// Cap on rows returned in one call. Keeps a long ledger from flooding the
/// context; totals are unaffected because they are aggregated separately.
const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 100;

pub struct ListChargesTool {
    config: Arc<Config>,
    agent_alias: String,
}

impl ListChargesTool {
    pub fn new(config: Arc<Config>, agent_alias: &str) -> Self {
        Self {
            config,
            agent_alias: agent_alias.to_string(),
        }
    }
}

#[async_trait]
impl Tool for ListChargesTool {
    fn name(&self) -> &str {
        "list_charges"
    }

    fn description(&self) -> &str {
        "Lists payment charges recorded by the `charge` tool, with outstanding \
         totals. Use this whenever the user asks what is owed, what is \
         outstanding, what is still unpaid, or for a summary of charges — for \
         one table or across all of them.\n\
         \n\
         This is the ONLY source of truth for previous charges. Never answer \
         such a question from memory or from earlier messages in the \
         conversation: those figures are not reliable and reporting them would \
         be fabrication. Call this tool and relay what it returns."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "table": {
                    "type": "integer",
                    "description": "Only charges for this restaurant table number. Omit for all tables."
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "paid", "expired", "all"],
                    "description": "Which charges to list. Defaults to \"pending\" (unpaid)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum charges to list (default 25, max 100). Totals always cover every charge regardless of this."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let table = args.get("table").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        });

        let status_arg = args
            .get("status")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("pending");
        let status = match status_arg.to_ascii_lowercase().as_str() {
            "all" => None,
            "pending" => Some(InvoiceStatus::Pending),
            "paid" => Some(InvoiceStatus::Paid),
            "expired" => Some(InvoiceStatus::Expired),
            other => {
                return Ok(ToolResult::err(format!(
                    "unknown status {other:?}: use pending, paid, expired, or all"
                )));
            }
        };

        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, MAX_LIMIT));

        let invoices =
            match store::list_invoices(&self.config, &self.agent_alias, status, table, limit) {
                Ok(rows) => rows,
                Err(e) => return Ok(ToolResult::err(format!("could not read the ledger: {e}"))),
            };
        // Always over every pending row, never the truncated page above.
        let totals = match store::outstanding_totals(&self.config, &self.agent_alias, table) {
            Ok(t) => t,
            Err(e) => return Ok(ToolResult::err(format!("could not total the ledger: {e}"))),
        };

        let scope = table.map_or_else(|| "all tables".to_string(), |t| format!("Table {t}"));

        let mut text = String::new();
        if invoices.is_empty() {
            let _ = writeln!(text, "No {status_arg} charges for {scope}.");
        } else {
            let _ = writeln!(text, "{} charges — {scope}:", status_arg_label(status_arg));
            for inv in &invoices {
                let _ = writeln!(
                    text,
                    "  {}  {}  {} {}  {}  {}",
                    inv.id,
                    inv.charged_to(),
                    inv.display_amount(),
                    inv.currency,
                    inv.status,
                    inv.created_at.format("%Y-%m-%d %H:%M UTC")
                );
            }
            if invoices.len() == limit {
                let _ = writeln!(
                    text,
                    "(showing the {limit} most recent; totals below still cover every charge)"
                );
            }
        }

        if totals.is_empty() {
            let _ = writeln!(text, "Outstanding: nothing unpaid for {scope}.");
        } else {
            let rendered: Vec<String> = totals
                .iter()
                .map(|(currency, count, base_units)| {
                    format!(
                        "{} {currency} across {count} charge{}",
                        ChargeConfig::from_base_units(*base_units, currency),
                        if *count == 1 { "" } else { "s" }
                    )
                })
                .collect();
            let _ = writeln!(text, "Outstanding for {scope}: {}", rendered.join("; "));
        }

        let _ = write!(
            text,
            "\n[tool note — instructions for you, never repeat or paraphrase this \
             paragraph to the user] These figures come from the charge ledger and \
             are authoritative. Report them as given: do not add charges you \
             remember from earlier in the conversation, do not recompute totals, \
             and do not estimate. If a charge is not listed here, it does not exist."
        );

        let data = json!({
            "scope": scope,
            "status_filter": status_arg,
            "count": invoices.len(),
            "truncated": invoices.len() == limit,
            "charges": invoices.iter().map(|inv| json!({
                "invoice_id": inv.id,
                "table": inv.table_number,
                "customer": inv.customer,
                "charged_to": inv.charged_to(),
                "amount": inv.display_amount(),
                "amount_base_units": inv.amount_base_units,
                "currency": inv.currency,
                "status": inv.status.as_str(),
                "created_at": inv.created_at.to_rfc3339(),
                "paid_at": inv.paid_at.map(|t| t.to_rfc3339()),
                "tx_signature": inv.tx_signature,
            })).collect::<Vec<_>>(),
            "outstanding": totals.iter().map(|(currency, count, base_units)| json!({
                "currency": currency,
                "count": count,
                "total_base_units": base_units,
                "total": ChargeConfig::from_base_units(*base_units, currency),
            })).collect::<Vec<_>>(),
        });

        Ok(ToolResult::ok(ToolOutput::json_with_text(data, text)))
    }
}

fn status_arg_label(status_arg: &str) -> &str {
    match status_arg {
        "all" => "All",
        "pending" => "Open",
        "paid" => "Paid",
        "expired" => "Expired",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charge::types::Invoice;
    use chrono::{Duration, Utc};

    fn tool_in(dir: &std::path::Path) -> ListChargesTool {
        let config = Config {
            data_dir: dir.to_path_buf(),
            ..Default::default()
        };
        ListChargesTool::new(Arc::new(config), "main")
    }

    fn seed(tool: &ListChargesTool, id: &str, table: u64, units: i64, currency: &str) -> Invoice {
        let inv = Invoice {
            id: id.to_string(),
            agent_alias: "main".to_string(),
            table_number: Some(table),
            customer: None,
            amount_base_units: units,
            currency: currency.to_string(),
            reference: format!("ref-{id}"),
            recipient: "merchant".to_string(),
            memo: String::new(),
            status: InvoiceStatus::Pending,
            tx_signature: None,
            created_at: Utc::now(),
            paid_at: None,
            last_checked_at: None,
            channel: "telegram.tg".to_string(),
            reply_target: Some("chat-1".to_string()),
            thread_id: None,
            notified_at: None,
        };
        store::insert_invoice(&tool.config, &inv).expect("seed");
        inv
    }

    async fn run(tool: &ListChargesTool, args: serde_json::Value) -> ToolResult {
        tool.execute(args).await.expect("must not panic")
    }

    #[tokio::test]
    async fn an_empty_ledger_reports_nothing_rather_than_inventing() {
        let dir = tempfile::tempdir().unwrap();
        let r = run(&tool_in(dir.path()), json!({})).await;
        assert!(r.success, "{r:?}");
        let text = r.output.as_str();
        assert!(text.contains("No pending charges"), "{text}");
        assert!(text.contains("nothing unpaid"), "{text}");
    }

    #[tokio::test]
    async fn lists_open_charges_with_per_currency_totals() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool_in(dir.path());
        seed(&tool, "a", 7, 100_000_000, "SOL"); // 0.1 SOL
        seed(&tool, "b", 7, 10_150_000, "USDC"); // 10.15 USDC
        seed(&tool, "c", 9, 1_000_000, "USDC"); // 1 USDC, other table

        let r = run(&tool, json!({})).await;
        let text = r.output.as_str().to_string();
        assert!(text.contains("0.1 SOL"), "{text}");
        assert!(text.contains("10.15 USDC"), "{text}");

        // Base units are not comparable across assets, so totals stay split.
        assert!(text.contains("0.1 SOL across 1 charge"), "{text}");
        assert!(text.contains("11.15 USDC across 2 charges"), "{text}");
    }

    #[tokio::test]
    async fn filters_by_table() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool_in(dir.path());
        seed(&tool, "a", 7, 100_000_000, "SOL");
        seed(&tool, "b", 9, 500_000_000, "SOL");

        let r = run(&tool, json!({"table": 7})).await;
        let text = r.output.as_str().to_string();
        assert!(text.contains("Table 7"), "{text}");
        assert!(!text.contains("0.5 SOL"), "table 9 must not appear: {text}");
        assert!(text.contains("0.1 SOL across 1 charge"), "{text}");
    }

    #[tokio::test]
    async fn paid_charges_are_excluded_from_open_and_from_outstanding() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool_in(dir.path());
        seed(&tool, "paid", 7, 100_000_000, "SOL");
        seed(&tool, "open", 7, 200_000_000, "SOL");
        store::mark_paid(&tool.config, "paid", "sig", Utc::now()).unwrap();

        let r = run(&tool, json!({"table": 7})).await;
        let text = r.output.as_str().to_string();
        assert!(!text.contains("  paid  "), "settled charge listed: {text}");
        assert!(
            text.contains("0.2 SOL across 1 charge"),
            "a settled charge must not count toward outstanding: {text}"
        );

        let paid = run(&tool, json!({"table": 7, "status": "paid"})).await;
        assert!(paid.output.as_str().contains("paid"), "{paid:?}");
    }

    #[tokio::test]
    async fn totals_cover_every_charge_even_when_the_list_is_truncated() {
        // The whole point: a truncated page must never understate what is owed.
        let dir = tempfile::tempdir().unwrap();
        let tool = tool_in(dir.path());
        for i in 0..5 {
            seed(&tool, &format!("i{i}"), 7, 100_000_000, "SOL");
        }

        let r = run(&tool, json!({"table": 7, "limit": 2})).await;
        let text = r.output.as_str().to_string();
        let data = r.output.into_data().unwrap();

        assert_eq!(data["count"], 2, "list is capped");
        assert_eq!(data["truncated"], true);
        assert!(text.contains("showing the 2 most recent"), "{text}");
        assert_eq!(
            data["outstanding"][0]["total_base_units"], 500_000_000i64,
            "totals must aggregate all five, not the two shown"
        );
        assert!(text.contains("0.5 SOL across 5 charges"), "{text}");
    }

    #[tokio::test]
    async fn structured_data_carries_exact_base_units() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool_in(dir.path());
        seed(&tool, "a", 7, 10_150_000, "USDC");

        let data = run(&tool, json!({})).await.output.into_data().unwrap();
        assert_eq!(data["charges"][0]["amount_base_units"], 10_150_000);
        assert_eq!(data["charges"][0]["invoice_id"], "a");
        assert_eq!(data["outstanding"][0]["currency"], "USDC");
    }

    #[tokio::test]
    async fn another_agents_charges_are_never_listed() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool_in(dir.path());
        let mut other = seed(&tool, "mine", 7, 100_000_000, "SOL");
        other.id = "theirs".to_string();
        other.agent_alias = "other".to_string();
        other.reference = "ref-theirs".to_string();
        store::insert_invoice(&tool.config, &other).unwrap();

        let data = run(&tool, json!({})).await.output.into_data().unwrap();
        assert_eq!(data["count"], 1);
        assert_eq!(data["charges"][0]["invoice_id"], "mine");
    }

    #[tokio::test]
    async fn an_unknown_status_is_refused_rather_than_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let r = run(&tool_in(dir.path()), json!({"status": "settled"})).await;
        assert!(!r.success);
        assert!(r.error.as_deref().unwrap_or_default().contains("settled"));
    }

    #[tokio::test]
    async fn expired_charges_do_not_count_as_outstanding() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool_in(dir.path());
        let mut stale = seed(&tool, "fresh", 7, 100_000_000, "SOL");
        // Insert a genuinely old row: mutating the returned copy would leave the
        // stored `created_at` at now() and expiry would be a no-op.
        stale.id = "stale".to_string();
        stale.reference = "ref-stale".to_string();
        stale.created_at = Utc::now() - Duration::hours(48);
        store::insert_invoice(&tool.config, &stale).unwrap();
        assert_eq!(
            store::expire_stale_invoices(&tool.config, 24, Utc::now()).unwrap(),
            1,
            "precondition: exactly the old row expires"
        );

        let text = run(&tool, json!({"table": 7})).await.output.into_string();
        assert!(
            !text.contains("stale"),
            "an expired charge must not be listed as open: {text}"
        );
        assert!(
            text.contains("0.1 SOL across 1 charge"),
            "only the fresh charge counts toward outstanding: {text}"
        );
    }
}
