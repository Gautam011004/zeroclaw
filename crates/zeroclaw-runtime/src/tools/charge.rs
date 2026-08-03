use async_trait::async_trait;
use serde_json::json;
use uuid::Uuid;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use solana_sdk::{
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
};
use url::Url;
use qrcode::QrCode;
use image::Luma;
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use zeroclaw_api::media::MediaAttachment;

pub struct ChargeTool;

const MERCHANT_WALLET: &str =
    "4StEu9VEXVjba77JrwsiVcWT734NE7uahRXjnfHkzbkr";

const DEVNET_USDC_MINT: &str =
    "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

impl ChargeTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ChargeTool {
    fn name(&self) -> &str {
        "charge"
    }

    fn description(&self) -> &str {
        "Creates exactly one payment request. Use this tool once per payment. If the user says table 5, that refers to restaurant table number 5"
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
                    "description": "Currency to receive (SOL, USDC, etc.)"
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

    async fn execute(
        &self,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        let reference = Keypair::new();
        let reference_pubkey = reference.pubkey();
        let recipient = MERCHANT_WALLET.parse::<Pubkey>()?;

        
        let invoice_id = Uuid::new_v4().to_string();
        let amount = args["amount"].as_f64().unwrap();
        let currency = args["currency"].as_str().unwrap();
        let table = args.get("table").and_then(|v| v.as_u64());
        let customer = args.get("customer").and_then(|v| v.as_str());
        let memo = args
            .get("memo")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                if let Some(table) = args.get("table").and_then(|v| v.as_u64()) {
                    format!("Table {} charged {} {}", table, amount, currency)
                } else {
                    format!("Invoice {}", invoice_id)
                }
            });

        let mut payment_url = Url::parse(&format!("solana:{}", recipient))?;

        payment_url
        .query_pairs_mut()
        .append_pair("amount", &amount.to_string())
        .append_pair("reference", &reference_pubkey.to_string())
        .append_pair("memo", &memo);

        println!("{}", payment_url);

        let code = QrCode::new(payment_url.as_str())?;
        let image = code.render::<Luma<u8>>().build();

        if currency.eq_ignore_ascii_case("USDC") {
            payment_url
                .query_pairs_mut()
                .append_pair("spl-token", DEVNET_USDC_MINT);
        }

        let mut cursor = Cursor::new(Vec::new());

        DynamicImage::ImageLuma8(image).write_to(&mut cursor, ImageFormat::Png)?;

        let png_bytes = cursor.into_inner();

        let attachment = MediaAttachment {
            file_name: format!("{invoice_id}.png"),
            data: png_bytes,
            mime_type: Some("image/png".to_string()),
        };


        let output = json!({
            "invoice_id": invoice_id,
            "memo": memo,
            "amount": amount,
            "currency": currency,
            "reference": reference_pubkey.to_string(),
            "recipient": recipient,
            "status": "pending"
        });

        Ok(ToolResult::ok_with_attachments(
            output,
            vec![attachment],
        ))
    }
}