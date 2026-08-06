//! Invoice record types for the charge ledger.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle of an invoice.
///
/// `Pending` is the only state settlement polling looks at. `Expired` exists so
/// an unpaid invoice eventually stops costing an RPC call on every run — the
/// poll cost is O(pending), and without expiry a demo's worth of abandoned
/// charges would be re-checked forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    Pending,
    Paid,
    Expired,
}

impl InvoiceStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Paid => "paid",
            Self::Expired => "expired",
        }
    }
}

impl std::fmt::Display for InvoiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for InvoiceStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "paid" => Ok(Self::Paid),
            "expired" => Ok(Self::Expired),
            other => Err(format!("unknown invoice status: {other}")),
        }
    }
}

/// One payment request and its settlement state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invoice {
    /// UUID shown to the user and used as the attachment file name.
    pub id: String,
    /// Owning agent. Databases under `data_dir` are shared across agents and
    /// partition at the row level, so every query filters on this.
    pub agent_alias: String,
    pub table_number: Option<u64>,
    pub customer: Option<String>,
    /// Amount in integer base units of `currency` (USDC 1e6, SOL 1e9).
    /// Never a float: this value is compared directly against on-chain
    /// integer balances during settlement.
    pub amount_base_units: i64,
    pub currency: String,
    /// Base58 pubkey embedded in the payment URL as the Solana Pay reference.
    /// This is the settlement lookup key — `getSignaturesForAddress` on it
    /// finds the paying transaction. Unique per invoice.
    pub reference: String,
    /// Merchant wallet the funds were requested to.
    pub recipient: String,
    pub memo: String,
    pub status: InvoiceStatus,
    /// Signature of the verified paying transaction, once settled.
    pub tx_signature: Option<String>,
    pub created_at: DateTime<Utc>,
    pub paid_at: Option<DateTime<Utc>>,
    /// Last settlement-poll attempt, so a run can prefer least-recently-checked
    /// invoices and spread RPC load.
    pub last_checked_at: Option<DateTime<Utc>>,
    /// Channel the charge was requested on (`telegram`, `slack`, …), so the
    /// payment confirmation goes back to the same conversation. Empty when the
    /// charge came from a surface with nowhere to reply (a CLI run).
    pub channel: String,
    /// Reply target on `channel` — chat id, room id, address.
    pub reply_target: Option<String>,
    /// Platform thread id, so the confirmation lands in the same thread as the
    /// request rather than at the top of the room.
    pub thread_id: Option<String>,
    /// When the payment confirmation was successfully delivered.
    ///
    /// Deliberately separate from `paid_at`: if the channel is down at the
    /// moment payment lands, the invoice is still correctly paid, and the
    /// confirmation must be retryable without re-settling anything.
    pub notified_at: Option<DateTime<Utc>>,
}

impl Invoice {
    /// Display amount for human-facing text. Never use this for comparison.
    #[must_use]
    pub fn display_amount(&self) -> f64 {
        zeroclaw_config::schema::ChargeConfig::from_base_units(
            self.amount_base_units,
            &self.currency,
        )
    }

    /// Whether a payment confirmation can be delivered for this invoice.
    #[must_use]
    pub fn is_notifiable(&self) -> bool {
        !self.channel.trim().is_empty() && self.reply_target.is_some()
    }

    /// The payment confirmation sent to the originating channel.
    ///
    /// Built here, deterministically, rather than by the model: a payment
    /// receipt must state what the ledger says, and an LLM paraphrasing a row
    /// is exactly how wrong totals reach a customer.
    #[must_use]
    pub fn confirmation_message(&self) -> String {
        format!(
            "✅ Payment received\n{} · {} {}\nInvoice: {}",
            self.charged_to(),
            self.display_amount(),
            self.currency,
            self.id
        )
    }

    /// Who the charge is against, for display.
    #[must_use]
    pub fn charged_to(&self) -> String {
        match (self.table_number, self.customer.as_deref()) {
            (Some(t), _) => format!("Table {t}"),
            (None, Some(c)) => c.to_string(),
            (None, None) => "Unassigned".to_string(),
        }
    }
}
