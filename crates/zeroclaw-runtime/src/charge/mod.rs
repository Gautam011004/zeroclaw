//! Solana Pay charge ledger: invoices created by the `charge` tool and the
//! settlement state the daemon's `charge-settlement` worker updates.
//!
//! Amounts live here as **integer base units**, never floats — see
//! [`zeroclaw_config::schema::ChargeConfig::to_base_units`]. On-chain balances
//! are integers, so comparing a float invoice against them rounds, and a
//! correct payment can read as underpaid.

pub mod rpc;
pub mod settlement;
pub mod store;
pub mod types;

pub use settlement::{SettlementSummary, run_settlement_pass};
pub use types::{Invoice, InvoiceStatus};

/// Whether the daemon should run settlement passes for this config.
///
/// Requires the tool enabled, a merchant wallet configured (nothing can settle
/// without one), and a non-zero interval.
#[must_use]
pub fn settlement_worker_enabled(config: &zeroclaw_config::schema::Config) -> bool {
    config.charge.enabled
        && !config.charge.merchant_wallet.trim().is_empty()
        && config.charge.settlement_interval_secs > 0
}

#[cfg(test)]
mod tests {
    use zeroclaw_config::schema::{ChargeConfig, Config};

    fn config_with(charge: ChargeConfig) -> Config {
        Config {
            charge,
            ..Default::default()
        }
    }

    fn wired() -> ChargeConfig {
        ChargeConfig {
            merchant_wallet: "4StEu9VEXVjba77JrwsiVcWT734NE7uahRXjnfHkzbkr".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_worker_runs_when_charging_is_fully_configured() {
        assert!(super::settlement_worker_enabled(&config_with(wired())));
    }

    #[test]
    fn the_worker_stays_off_without_a_merchant_wallet() {
        // Nothing can settle without a recipient to verify against, so polling
        // would burn RPC calls to no purpose.
        assert!(!super::settlement_worker_enabled(&config_with(
            ChargeConfig::default()
        )));
    }

    #[test]
    fn the_worker_stays_off_when_disabled_or_interval_is_zero() {
        let mut off = wired();
        off.enabled = false;
        assert!(!super::settlement_worker_enabled(&config_with(off)));

        let mut zero = wired();
        zero.settlement_interval_secs = 0;
        assert!(
            !super::settlement_worker_enabled(&config_with(zero)),
            "interval 0 must disable the worker; `charge check` still works by hand"
        );
    }
}
