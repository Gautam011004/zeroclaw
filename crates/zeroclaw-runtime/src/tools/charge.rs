use async_trait::async_trait;
use chrono::Utc;
use image::Luma;
use image::{DynamicImage, ImageFormat};
use qrcode::QrCode;
use serde_json::json;
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer};
use std::io::Cursor;
use std::sync::Arc;
use url::Url;
use uuid::Uuid;
use zeroclaw_api::media::MediaAttachment;
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::schema::{ChargeConfig, Config};

use crate::agent::channel_context::TurnChannelContext;
use crate::charge::store;
use crate::charge::types::{Invoice, InvoiceStatus};

pub struct ChargeTool {
    config: Arc<Config>,
    /// Owning agent — stamped on every invoice row, since the ledger under
    /// `data_dir` is shared and partitions per agent at the row level.
    agent_alias: String,
}

impl ChargeTool {
    pub fn new(config: Arc<Config>, agent_alias: &str) -> Self {
        Self {
            config,
            agent_alias: agent_alias.to_string(),
        }
    }

    fn charge_config(&self) -> &ChargeConfig {
        &self.config.charge
    }
}

/// Build the Solana Pay URL that the QR encodes.
///
/// # Why this is assembled by hand
///
/// `Url::query_pairs_mut` serialises as `application/x-www-form-urlencoded`,
/// which encodes a space as `+`. The Solana Pay spec is RFC 3986: a space must
/// be `%20`. A wallet that parses strictly sees a malformed request and can
/// silently degrade to a plain address+amount transfer — dropping `reference`,
/// which makes the payment permanently unattributable to its invoice. That
/// failure is invisible: the customer pays, and the bill is never marked paid.
///
/// # Why there is no `memo`
///
/// The spec's `memo` obliges the wallet to add an SPL Memo instruction. Wallets
/// that don't support it can drop the whole structured request rather than just
/// that field. `message` and `label` are display-only and carry no such
/// requirement, so the human-readable text rides there instead. Nothing in
/// settlement reads the memo — `reference` is the key — so this costs nothing.
///
/// # Why `reference` matters
///
/// The spec requires a wallet to include each `reference` pubkey as a
/// **read-only, non-signer account key**. That is what makes
/// `getSignaturesForAddress(reference)` find the payment afterwards. The
/// reference never signs and never holds funds.
///
/// `amount` is the **display** amount, not base units: the spec defines it as a
/// decimal in user units and the wallet scales by the mint's decimals.
pub(crate) fn build_payment_url(
    recipient: &Pubkey,
    reference: &Pubkey,
    amount: f64,
    currency: &str,
    message: &str,
    usdc_mint: &str,
) -> anyhow::Result<Url> {
    use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

    /// RFC 3986 query component: percent-encode everything outside the
    /// unreserved set. Notably encodes space as `%20`, never `+`.
    const QUERY: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'&')
        .add(b'+')
        .add(b'/')
        .add(b'<')
        .add(b'=')
        .add(b'>')
        .add(b'?')
        .add(b'@')
        .add(b'[')
        .add(b'\\')
        .add(b']')
        .add(b'^')
        .add(b'`')
        .add(b'{')
        .add(b'|')
        .add(b'}');

    let mut raw = format!("solana:{recipient}?amount={amount}&reference={reference}");

    // Without `spl-token` the wallet pays in native SOL for a USDC invoice —
    // the right number of the wrong asset.
    if currency.eq_ignore_ascii_case("USDC") {
        raw.push_str("&spl-token=");
        raw.push_str(usdc_mint);
    }

    let message = message.trim();
    if !message.is_empty() {
        raw.push_str("&message=");
        raw.push_str(&utf8_percent_encode(message, QUERY).to_string());
    }

    Ok(Url::parse(&raw)?)
}

#[async_trait]
impl Tool for ChargeTool {
    fn name(&self) -> &str {
        "charge"
    }

    fn description(&self) -> &str {
        "Creates exactly one payment request and generates its payment QR code. \
         Use this tool once per payment. If the user says table 5, that refers to \
         restaurant table number 5.\n\
         \n\
         The QR code image is produced by this tool and delivered to the user \
         automatically as an image attachment — you do NOT need to create, describe, \
         or link one, and you must never tell the user that you cannot generate QR \
         codes. Report the invoice number, the amount charged, and the table, then \
         stop; the image arrives on its own.\n\
         \n\
         This tool creates and records exactly ONE charge and returns only that \
         charge. It cannot tell you about any other invoice. To report what a \
         table owes, what is outstanding, or any summary of previous charges, \
         call `list_charges` — never answer from memory or from earlier messages, \
         which would be fabrication."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "amount": {
                    "type": "number",
                    "description": "Amount to charge"
                },
                "currency": {
                    "type": "string",
                    "description": "Currency to receive: SOL or USDC."
                },
                "table": {
                    "type": "integer",
                    "description": "Restaurant table number."
                },
                "customer": {
                    "type": "string",
                    "description": "Customer identifier if not charging a table."
                },
                "memo": {
                    "type": "string",
                    "description": "Details about the transaction"
                }
            },
            "required": ["amount", "currency"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let charge_cfg = self.charge_config();

        // Fail closed on an unconfigured recipient. A default merchant address
        // would silently direct real funds at whoever shipped the binary.
        let merchant = charge_cfg.merchant_wallet.trim();
        if merchant.is_empty() {
            return Ok(ToolResult::err(
                "no merchant wallet configured — set `merchant_wallet` under [charge] \
                 in config.toml before issuing charges",
            ));
        }
        let recipient = match merchant.parse::<Pubkey>() {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::err(format!(
                    "[charge] merchant_wallet is not a valid Solana address: {e}"
                )));
            }
        };

        let reference = Keypair::new();
        let reference_pubkey = reference.pubkey();
        let invoice_id = Uuid::new_v4().to_string();

        // Accept a JSON number or a numeric string: models routinely emit
        // `"amount": "10"`. A bad value returns a tool error the model can act
        // on, rather than panicking and killing the whole turn.
        let Some(amount) = args.get("amount").and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        }) else {
            return Ok(ToolResult::err(
                "`amount` is required and must be a number, e.g. 10 or \"10.50\"",
            ));
        };
        let Some(currency) = args.get("currency").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::err(
                "`currency` is required, e.g. \"USDC\" or \"SOL\"",
            ));
        };
        // Single source of truth for validity AND the integer form the ledger
        // stores. Settlement compares this against on-chain integer balances,
        // so the float never survives past here.
        let amount_base_units = match ChargeConfig::to_base_units(amount, currency) {
            Ok(units) => units,
            Err(e) => return Ok(ToolResult::err(e)),
        };
        let table = args.get("table").and_then(|v| v.as_u64());
        let customer = args
            .get("customer")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let memo = args
            .get("memo")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                if let Some(table) = table {
                    format!("Table {} charged {} {}", table, amount, currency)
                } else {
                    format!("Invoice {}", invoice_id)
                }
            });

        let payment_url = build_payment_url(
            &recipient,
            &reference_pubkey,
            amount,
            currency,
            &memo,
            charge_cfg.usdc_mint.trim(),
        )?;

        let code = QrCode::new(payment_url.as_str())?;

        let image = code.render::<Luma<u8>>().build();

        let mut cursor = Cursor::new(Vec::new());

        DynamicImage::ImageLuma8(image).write_to(&mut cursor, ImageFormat::Png)?;

        let png_bytes = cursor.into_inner();

        let attachment = MediaAttachment {
            file_name: format!("{invoice_id}.png"),
            data: png_bytes,
            mime_type: Some("image/png".to_string()),
        };

        // Where this charge was requested, so settlement — running minutes
        // later in another process with no turn around it — can deliver the
        // payment confirmation back to the same conversation.
        let origin = TurnChannelContext::current().unwrap_or_default();

        let invoice = Invoice {
            id: invoice_id.clone(),
            agent_alias: self.agent_alias.clone(),
            table_number: table,
            customer: customer.map(str::to_string),
            amount_base_units,
            currency: currency.to_string(),
            reference: reference_pubkey.to_string(),
            recipient: recipient.to_string(),
            memo: memo.clone(),
            status: InvoiceStatus::Pending,
            tx_signature: None,
            created_at: Utc::now(),
            paid_at: None,
            last_checked_at: None,
            channel: origin.channel.clone(),
            reply_target: origin.reply_target.clone(),
            thread_id: origin.thread_id.clone(),
            notified_at: None,
            refunded_at: None,
            refund_to: None,
        };
        let charged_to = invoice.charged_to();

        // Record before returning: a QR the customer can scan must always have
        // a ledger row behind it, or settlement has nothing to reconcile
        // against and the payment silently goes unrecognised. A store failure
        // therefore fails the charge rather than issuing an untracked one.
        if let Err(e) = store::insert_invoice(&self.config, &invoice) {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "invoice_id": invoice_id,
                        "error": format!("{e:#}"),
                    })),
                "failed to record invoice; charge refused"
            );
            return Ok(ToolResult::err(format!(
                "could not record the invoice, so no charge was issued: {e}"
            )));
        }

        let data = json!({
            "invoice_id": invoice_id,
            "table": table,
            "customer": customer,
            "charged_to": charged_to,
            "memo": memo,
            "amount": amount,
            // The authoritative figure settlement compares on-chain; `amount`
            // above is display only.
            "amount_base_units": amount_base_units,
            "currency": currency,
            "reference": reference_pubkey.to_string(),
            // `.to_string()` — a bare `Pubkey` serialises as a 32-element byte
            // array, which is unreadable in the model's tool result.
            "recipient": recipient.to_string(),
            "status": "pending",
            "qr_code_attached": true,
            "notify_channel": origin.channel,
            "notify_on_payment": origin.is_deliverable(),
        });

        // What the model actually reads. The facts come first as a clean block
        // the model can relay; the guidance is fenced in a bracketed note so it
        // reads as a directive rather than prose to repeat — an unfenced
        // sentence here gets echoed verbatim to the end user.
        let text = format!(
            "Payment request created.\n\
             Invoice: {invoice_id}\n\
             Charged to: {charged_to}\n\
             Amount: {amount} {currency}\n\
             Status: pending\n\
             \n\
             [tool note — instructions for you, never repeat or paraphrase this \
             paragraph to the user] The QR code image is already attached to this \
             reply and is delivered automatically; do not describe it, apologise \
             for it, or offer another way to produce one. Report ONLY the invoice, \
             amount, and who it was charged to, from the block above. This tool is \
             stateless and returned exactly one charge — you have no record of any \
             other invoice, so do not list, total, or summarise previous charges."
        );

        Ok(ToolResult::ok_with_attachments(
            zeroclaw_api::tool::ToolOutput::json_with_text(data, text),
            vec![attachment],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WALLET: &str = "4StEu9VEXVjba77JrwsiVcWT734NE7uahRXjnfHkzbkr";
    const MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

    /// A config with a throwaway ledger under `dir` and the given merchant
    /// wallet (empty = unconfigured, which must fail closed).
    fn test_config(dir: &std::path::Path, merchant_wallet: &str) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            charge: zeroclaw_config::schema::ChargeConfig {
                merchant_wallet: merchant_wallet.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// A tool wired to a throwaway ledger under `dir`.
    fn tool_in(dir: &std::path::Path) -> ChargeTool {
        ChargeTool::new(Arc::new(test_config(dir, TEST_WALLET)), "main")
    }

    /// Run one charge against a fresh ledger, returning the result and the
    /// tempdir (kept alive so the DB outlives the call).
    async fn charge_in(args: serde_json::Value) -> (ToolResult, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = tool_in(dir.path())
            .execute(args)
            .await
            .expect("must not panic");
        (r, dir)
    }

    async fn charge(args: serde_json::Value) -> ToolResult {
        charge_in(args).await.0
    }

    #[tokio::test]
    async fn produces_a_png_attachment_and_keeps_bytes_out_of_the_text() {
        let r = charge(json!({"amount": 10, "currency": "USDC", "table": 5})).await;
        assert!(r.success, "{r:?}");
        assert_eq!(r.attachments.len(), 1, "exactly one QR image per charge");
        let att = &r.attachments[0];
        assert_eq!(att.mime_type.as_deref(), Some("image/png"));
        assert!(att.file_name.ends_with(".png"));
        assert!(
            att.data.starts_with(&[0x89, b'P', b'N', b'G']),
            "attachment must be real PNG bytes"
        );

        // The bytes travel out-of-band; the model must never see them.
        let text = r.output.as_str();
        assert!(
            !text.contains("PNG") && !text.contains("base64"),
            "image data leaked into the model-visible output: {text}"
        );
    }

    #[tokio::test]
    async fn model_visible_text_reports_invoice_amount_and_table() {
        // This is the reply content the user asked for: invoice number, amount,
        // and what it was charged to.
        let r = charge(json!({"amount": 10, "currency": "USDC", "table": 2})).await;
        let text = r.output.as_str().to_string();
        assert!(text.contains("Table 2"), "missing table: {text}");
        assert!(text.contains("10 USDC"), "missing amount: {text}");

        let data = r.output.into_data().expect("structured data");
        let invoice = data["invoice_id"].as_str().expect("invoice_id");
        assert!(text.contains(invoice), "invoice not in text: {text}");
        assert_eq!(data["table"], 2);
        assert_eq!(data["charged_to"], "Table 2");
        assert_eq!(data["qr_code_attached"], true);
    }

    #[tokio::test]
    async fn text_tells_the_model_the_qr_is_already_attached() {
        // Regression for the model replying "I don't generate QR codes": the
        // tool output is the only place it can learn otherwise.
        let r = charge(json!({"amount": 1, "currency": "SOL", "table": 1})).await;
        let text = r.output.as_str().to_lowercase();
        assert!(
            text.contains("qr code") && text.contains("attached"),
            "output must state the QR is attached: {text}"
        );
    }

    #[tokio::test]
    async fn recipient_is_a_readable_address_not_a_byte_array() {
        let r = charge(json!({"amount": 1, "currency": "USDC"})).await;
        let data = r.output.into_data().expect("structured data");
        assert_eq!(
            data["recipient"].as_str(),
            Some(TEST_WALLET),
            "a bare Pubkey serialises as 32 raw bytes, which is unreadable"
        );
    }

    #[tokio::test]
    async fn falls_back_to_customer_then_unassigned() {
        let r = charge(json!({"amount": 3, "currency": "USDC", "customer": "Ana"})).await;
        assert!(r.output.as_str().contains("Ana"));

        let r = charge(json!({"amount": 3, "currency": "USDC"})).await;
        assert!(r.output.as_str().contains("Unassigned"));
    }

    #[tokio::test]
    async fn numeric_string_amount_is_accepted() {
        // Models routinely emit `"amount": "10"`; this used to panic the turn.
        let r = charge(json!({"amount": "10.5", "currency": "USDC", "table": 4})).await;
        assert!(r.success, "{r:?}");
        assert!(r.output.as_str().contains("10.5 USDC"));
    }

    #[tokio::test]
    async fn bad_arguments_return_a_tool_error_instead_of_panicking() {
        for args in [
            json!({"currency": "USDC"}),                  // missing amount
            json!({"amount": "abc", "currency": "USDC"}), // unparseable
            json!({"amount": 0, "currency": "USDC"}),     // non-positive
            json!({"amount": -5, "currency": "USDC"}),    // negative
            json!({"amount": 10}),                        // missing currency
        ] {
            let r = charge(args.clone()).await;
            assert!(!r.success, "expected a tool error for {args}");
            assert!(r.error.is_some(), "error text must explain the problem");
            assert!(r.attachments.is_empty(), "no QR for a rejected charge");
        }
    }

    #[tokio::test]
    async fn a_successful_charge_is_recorded_in_the_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = tool_in(dir.path());
        let r = tool
            .execute(json!({"amount": 10.15, "currency": "USDC", "table": 7}))
            .await
            .expect("no panic");
        assert!(r.success, "{r:?}");

        let data = r.output.into_data().expect("structured data");
        let id = data["invoice_id"].as_str().expect("invoice_id");

        let stored = store::get_invoice(&tool.config, "main", id)
            .expect("ledger readable")
            .expect("charge must persist an invoice row");
        assert_eq!(stored.table_number, Some(7));
        assert_eq!(stored.currency, "USDC");
        assert_eq!(stored.status, InvoiceStatus::Pending);
        assert_eq!(
            stored.amount_base_units, 10_150_000,
            "10.15 USDC must store exactly, with no float truncation"
        );
        assert_eq!(
            stored.reference,
            data["reference"].as_str().unwrap(),
            "the ledger reference must match the one embedded in the QR, or \
             settlement can never find the payment"
        );
        assert!(stored.tx_signature.is_none());
    }

    #[tokio::test]
    async fn each_charge_gets_a_unique_reference() {
        // Two invoices sharing a reference would both match one transaction.
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = tool_in(dir.path());
        let mut refs = std::collections::HashSet::new();
        for _ in 0..5 {
            let r = tool
                .execute(json!({"amount": 1, "currency": "USDC", "table": 1}))
                .await
                .expect("no panic");
            assert!(r.success);
            let data = r.output.into_data().unwrap();
            assert!(
                refs.insert(data["reference"].as_str().unwrap().to_string()),
                "reference reused across charges"
            );
        }
        assert_eq!(
            store::list_open_invoices(&tool.config, "main", Some(1))
                .unwrap()
                .len(),
            5
        );
    }

    #[tokio::test]
    async fn refuses_to_charge_without_a_configured_merchant_wallet() {
        // Fail closed: no default recipient, or funds go to a stranger.
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = ChargeTool::new(Arc::new(test_config(dir.path(), "")), "main");

        let r = tool
            .execute(json!({"amount": 10, "currency": "USDC"}))
            .await
            .expect("no panic");
        assert!(!r.success);
        assert!(
            r.error
                .as_deref()
                .unwrap_or_default()
                .contains("merchant_wallet"),
            "error must name the missing setting: {r:?}"
        );
        assert!(r.attachments.is_empty(), "no QR for a refused charge");
    }

    #[tokio::test]
    async fn refuses_an_invalid_merchant_wallet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = ChargeTool::new(Arc::new(test_config(dir.path(), "not-an-address")), "main");

        let r = tool
            .execute(json!({"amount": 10, "currency": "USDC"}))
            .await
            .expect("no panic");
        assert!(!r.success);
        assert!(r.attachments.is_empty());
    }

    #[tokio::test]
    async fn rejected_charges_leave_no_ledger_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = tool_in(dir.path());
        for args in [
            json!({"currency": "USDC"}),
            json!({"amount": -1, "currency": "USDC"}),
            json!({"amount": 0.0000001, "currency": "USDC"}),
            json!({"amount": 1, "currency": "DOGE"}),
        ] {
            let r = tool.execute(args.clone()).await.expect("no panic");
            assert!(!r.success, "expected rejection for {args}");
        }
        assert!(
            store::list_open_invoices(&tool.config, "main", None)
                .unwrap()
                .is_empty(),
            "a rejected charge must not persist an invoice"
        );
    }

    #[tokio::test]
    async fn the_configured_usdc_mint_is_used_not_a_hardcoded_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = test_config(dir.path(), TEST_WALLET);
        config.charge.usdc_mint = "MintAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string();
        let tool = ChargeTool::new(Arc::new(config), "main");

        let r = tool
            .execute(json!({"amount": 1, "currency": "USDC"}))
            .await
            .expect("no panic");
        assert!(r.success, "{r:?}");
        // The mint is embedded in the QR payload, so decoding it is the only
        // way to see it; assert via the stored invoice + config instead.
        assert_eq!(
            tool.charge_config().usdc_mint,
            "MintAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        );
    }

    #[tokio::test]
    async fn invoices_are_stamped_with_the_owning_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = Arc::new(test_config(dir.path(), TEST_WALLET));
        let tool = ChargeTool::new(Arc::clone(&config), "kitchen");

        let r = tool
            .execute(json!({"amount": 1, "currency": "USDC", "table": 3}))
            .await
            .expect("no panic");
        assert!(r.success);
        assert_eq!(
            store::list_open_invoices(&config, "kitchen", None)
                .unwrap()
                .len(),
            1
        );
        assert!(
            store::list_open_invoices(&config, "bar", None)
                .unwrap()
                .is_empty(),
            "another agent must not see these charges"
        );
    }

    #[tokio::test]
    async fn a_charge_records_the_channel_it_was_requested_on() {
        // The link that lets a settlement pass — minutes later, in another
        // process, with no turn around it — reply to the right conversation.
        use crate::agent::channel_context::{TurnChannelContext, scope_channel_context};

        let dir = tempfile::tempdir().expect("tempdir");
        let tool = tool_in(dir.path());
        let ctx = TurnChannelContext::new("telegram", Some("chat-4242"));

        let r = scope_channel_context(Some(ctx), async {
            tool.execute(json!({"amount": 5, "currency": "USDC", "table": 3}))
                .await
                .expect("no panic")
        })
        .await;
        assert!(r.success, "{r:?}");

        let data = r.output.into_data().unwrap();
        let id = data["invoice_id"].as_str().unwrap();
        assert_eq!(data["notify_on_payment"], true);

        let stored = store::get_invoice(&tool.config, "main", id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.channel, "telegram");
        assert_eq!(stored.reply_target.as_deref(), Some("chat-4242"));
        assert!(stored.is_notifiable());
        assert!(stored.notified_at.is_none());
    }

    #[tokio::test]
    async fn a_charge_outside_any_channel_is_recorded_but_not_notifiable() {
        // A CLI-originated charge has nowhere to reply. It must still be
        // recorded and settleable — just silently.
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = tool_in(dir.path());
        let r = tool
            .execute(json!({"amount": 5, "currency": "USDC", "table": 3}))
            .await
            .expect("no panic");
        assert!(r.success);

        let data = r.output.into_data().unwrap();
        assert_eq!(data["notify_on_payment"], false);
        let stored = store::get_invoice(&tool.config, "main", data["invoice_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert!(!stored.is_notifiable());
    }

    #[tokio::test]
    async fn the_qr_encodes_the_same_reference_stored_in_the_ledger() {
        // Ties the two halves together: the reference the customer's wallet
        // will echo is the one settlement searches for.
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = tool_in(dir.path());
        let r = tool
            .execute(json!({"amount": 4, "currency": "USDC", "table": 2}))
            .await
            .expect("no panic");
        assert!(r.success);

        let data = r.output.into_data().unwrap();
        let stored = store::get_invoice(&tool.config, "main", data["invoice_id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.reference,
            data["reference"].as_str().unwrap(),
            "ledger and tool output must agree on the settlement key"
        );
    }

    #[test]
    fn spaces_are_percent_encoded_not_form_encoded() {
        // A `+` here is form encoding, not RFC 3986. Phantom degraded such a
        // URL to a plain transfer and silently dropped `reference`, leaving a
        // real payment permanently unattributable to its invoice.
        let recipient = TEST_WALLET.parse::<Pubkey>().unwrap();
        let url = build_payment_url(
            &recipient,
            &Keypair::new().pubkey(),
            1.0,
            "SOL",
            "Table 1 charged 1 SOL",
            MINT,
        )
        .unwrap();

        let raw = url.as_str();
        assert!(
            raw.contains("message=Table%201%20charged%201%20SOL"),
            "spaces must be %20: {raw}"
        );
        assert!(
            !raw.contains('+'),
            "form encoding leaked into the URL: {raw}"
        );
    }

    #[test]
    fn the_url_shape_matches_the_solana_pay_spec() {
        let recipient = TEST_WALLET.parse::<Pubkey>().unwrap();
        let reference = Keypair::new().pubkey();
        let url = build_payment_url(&recipient, &reference, 0.1, "SOL", "Table 1", MINT).unwrap();
        let raw = url.as_str();

        assert!(raw.starts_with(&format!("solana:{TEST_WALLET}?")), "{raw}");
        assert!(raw.contains("amount=0.1"), "{raw}");
        assert!(raw.contains(&format!("reference={reference}")), "{raw}");
        assert!(
            !raw.contains("spl-token"),
            "SOL must not name a mint: {raw}"
        );
    }

    #[test]
    fn a_message_with_reserved_characters_is_escaped() {
        let recipient = TEST_WALLET.parse::<Pubkey>().unwrap();
        let url = build_payment_url(
            &recipient,
            &Keypair::new().pubkey(),
            1.0,
            "SOL",
            "a&b=c?d#e",
            MINT,
        )
        .unwrap();
        // Unescaped `&`/`=` would inject bogus query parameters and could
        // truncate `reference` out of the request entirely.
        assert!(url.as_str().contains("message=a%26b%3Dc%3Fd%23e"), "{url}");
        assert_eq!(url.query_pairs().filter(|(k, _)| k == "message").count(), 1);
    }

    #[test]
    fn an_empty_message_is_omitted_entirely() {
        let recipient = TEST_WALLET.parse::<Pubkey>().unwrap();
        let url = build_payment_url(&recipient, &Keypair::new().pubkey(), 1.0, "SOL", "  ", MINT)
            .unwrap();
        assert!(!url.as_str().contains("message="), "{url}");
    }
}
