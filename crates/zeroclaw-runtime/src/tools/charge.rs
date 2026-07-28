use async_trait::async_trait;
use serde_json::json;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};

pub struct ChargeTool;

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
        "Generate a payment request for a restaurant table or customer. The table parameter is the table number, not the number of payment requests to create."
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
                "memo": {
                    "type": "string",
                    "description": "Optional payment memo"
                }
            },
            "required": ["amount", "currency"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "status": "stub",
                "message": "Charge tool invoked successfully.",
                "received_args": args,
            }))?
            .into(),
            error: None,
        })
    }
}