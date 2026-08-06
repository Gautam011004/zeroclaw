//! SQLite invoice ledger for the `charge` tool.
//!
//! Modelled on [`crate::cron::store`]: free functions over `&Config`, database
//! path derived from `data_dir`, schema ensured on every write connection.
//! Read paths deliberately do **not** create the database, so a status query
//! before the first charge reports "no invoices" instead of leaving an empty
//! file behind.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use zeroclaw_config::schema::Config;

use super::types::{Invoice, InvoiceStatus};

/// Where the ledger lives: `[charge] store_path`, else `<data_dir>/charge/invoices.db`.
#[must_use]
pub fn invoice_db_path(config: &Config) -> std::path::PathBuf {
    let configured = config.charge.store_path.trim();
    if configured.is_empty() {
        config.data_dir.join("charge").join("invoices.db")
    } else {
        std::path::PathBuf::from(configured)
    }
}

fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous  = NORMAL;
         CREATE TABLE IF NOT EXISTS invoices (
            id                TEXT PRIMARY KEY,
            agent_alias       TEXT NOT NULL,
            table_number      INTEGER,
            customer          TEXT,
            amount_base_units INTEGER NOT NULL,
            currency          TEXT NOT NULL,
            reference         TEXT NOT NULL UNIQUE,
            recipient         TEXT NOT NULL,
            memo              TEXT NOT NULL DEFAULT '',
            status            TEXT NOT NULL DEFAULT 'pending',
            tx_signature      TEXT,
            created_at        TEXT NOT NULL,
            paid_at           TEXT,
            last_checked_at   TEXT,
            channel           TEXT NOT NULL DEFAULT '',
            reply_target      TEXT,
            thread_id         TEXT,
            notified_at       TEXT
         );
         -- Settlement polls pending invoices oldest-checked first.
         CREATE INDEX IF NOT EXISTS idx_invoices_status_checked
            ON invoices(status, last_checked_at);
         -- 'what is open for table 7' is the read the agent makes most.
         CREATE INDEX IF NOT EXISTS idx_invoices_agent_status
            ON invoices(agent_alias, status, table_number);
         -- The settlement lookup key.
         CREATE UNIQUE INDEX IF NOT EXISTS idx_invoices_reference
            ON invoices(reference);
         -- Paid-but-unannounced invoices, for confirmation retry.
         CREATE INDEX IF NOT EXISTS idx_invoices_notify
            ON invoices(status, notified_at);",
    )
    .context("Failed to initialize charge invoice schema")?;
    migrate_schema(conn)?;
    Ok(())
}

/// Add columns introduced after a ledger was first created.
///
/// `CREATE TABLE IF NOT EXISTS` is a no-op on an existing database, so a
/// ledger written by an earlier build keeps its old column set. SQLite has no
/// `ADD COLUMN IF NOT EXISTS`, so probe `table_info` and add what is missing.
fn migrate_schema(conn: &Connection) -> Result<()> {
    let existing: std::collections::HashSet<String> = {
        let mut stmt = conn
            .prepare("PRAGMA table_info(invoices)")
            .context("Failed to read invoice table info")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>("name"))
            .context("Failed to enumerate invoice columns")?;
        rows.collect::<rusqlite::Result<_>>()
            .context("Failed to collect invoice columns")?
    };

    for (name, ddl) in [
        (
            "channel",
            "ALTER TABLE invoices ADD COLUMN channel TEXT NOT NULL DEFAULT ''",
        ),
        (
            "reply_target",
            "ALTER TABLE invoices ADD COLUMN reply_target TEXT",
        ),
        (
            "thread_id",
            "ALTER TABLE invoices ADD COLUMN thread_id TEXT",
        ),
        (
            "notified_at",
            "ALTER TABLE invoices ADD COLUMN notified_at TEXT",
        ),
    ] {
        if !existing.contains(name) {
            conn.execute(ddl, [])
                .with_context(|| format!("Failed to add invoice column {name}"))?;
        }
    }
    Ok(())
}

fn with_initialized_connection<T>(
    config: &Config,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let db_path = invoice_db_path(config);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create charge directory: {}", parent.display()))?;
    }
    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open charge DB: {}", db_path.display()))?;
    initialize_schema(&conn)?;
    f(&conn)
}

/// Read path: `Ok(None)` when no ledger exists yet, without creating one.
fn with_read_connection<T>(
    config: &Config,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<Option<T>> {
    let db_path = invoice_db_path(config);
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open charge DB: {}", db_path.display()))?;
    initialize_schema(&conn)?;
    f(&conn).map(Some)
}

fn parse_ts(raw: Option<String>) -> Option<DateTime<Utc>> {
    raw.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn row_to_invoice(row: &rusqlite::Row<'_>) -> rusqlite::Result<Invoice> {
    let status: String = row.get("status")?;
    let created_at: String = row.get("created_at")?;
    Ok(Invoice {
        id: row.get("id")?,
        agent_alias: row.get("agent_alias")?,
        table_number: row.get::<_, Option<i64>>("table_number")?.map(|v| v as u64),
        customer: row.get("customer")?,
        amount_base_units: row.get("amount_base_units")?,
        currency: row.get("currency")?,
        reference: row.get("reference")?,
        recipient: row.get("recipient")?,
        memo: row.get("memo")?,
        status: status.parse().unwrap_or(InvoiceStatus::Pending),
        tx_signature: row.get("tx_signature")?,
        created_at: parse_ts(Some(created_at)).unwrap_or_else(Utc::now),
        paid_at: parse_ts(row.get("paid_at")?),
        last_checked_at: parse_ts(row.get("last_checked_at")?),
        channel: row.get("channel").unwrap_or_default(),
        reply_target: row.get("reply_target")?,
        thread_id: row.get("thread_id")?,
        notified_at: parse_ts(row.get("notified_at")?),
    })
}

const SELECT_COLUMNS: &str = "id, agent_alias, table_number, customer, amount_base_units, \
     currency, reference, recipient, memo, status, tx_signature, created_at, paid_at, \
     last_checked_at, channel, reply_target, thread_id, notified_at";

/// Record a newly created payment request. Called by the `charge` tool before
/// it returns, so the QR the customer scans always has a ledger row behind it.
pub fn insert_invoice(config: &Config, invoice: &Invoice) -> Result<()> {
    with_initialized_connection(config, |conn| {
        conn.execute(
            "INSERT INTO invoices (
                id, agent_alias, table_number, customer, amount_base_units, currency,
                reference, recipient, memo, status, tx_signature, created_at, paid_at,
                last_checked_at, channel, reply_target, thread_id, notified_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, NULL, NULL,
                       ?12, ?13, ?14, NULL)",
            params![
                invoice.id,
                invoice.agent_alias,
                invoice.table_number.map(|v| v as i64),
                invoice.customer,
                invoice.amount_base_units,
                invoice.currency,
                invoice.reference,
                invoice.recipient,
                invoice.memo,
                invoice.status.as_str(),
                invoice.created_at.to_rfc3339(),
                invoice.channel,
                invoice.reply_target,
                invoice.thread_id,
            ],
        )
        .context("Failed to insert invoice")?;
        Ok(())
    })
}

/// One invoice by id, scoped to its owning agent.
pub fn get_invoice(config: &Config, agent_alias: &str, id: &str) -> Result<Option<Invoice>> {
    let found = with_read_connection(config, |conn| {
        let sql =
            format!("SELECT {SELECT_COLUMNS} FROM invoices WHERE id = ?1 AND agent_alias = ?2");
        conn.query_row(&sql, params![id, agent_alias], row_to_invoice)
            .optional()
            .context("Failed to query invoice")
    })?;
    Ok(found.flatten())
}

/// Open (pending) invoices for an agent, newest first, optionally for one table.
pub fn list_open_invoices(
    config: &Config,
    agent_alias: &str,
    table_number: Option<u64>,
) -> Result<Vec<Invoice>> {
    let rows = with_read_connection(config, |conn| {
        let mut sql = format!(
            "SELECT {SELECT_COLUMNS} FROM invoices WHERE agent_alias = ?1 AND status = 'pending'"
        );
        if table_number.is_some() {
            sql.push_str(" AND table_number = ?2");
        }
        sql.push_str(" ORDER BY created_at DESC");

        let mut stmt = conn.prepare(&sql)?;
        let mapped = match table_number {
            Some(t) => stmt
                .query_map(params![agent_alias, t as i64], row_to_invoice)?
                .collect::<rusqlite::Result<Vec<_>>>(),
            None => stmt
                .query_map(params![agent_alias], row_to_invoice)?
                .collect::<rusqlite::Result<Vec<_>>>(),
        };
        mapped.context("Failed to list open invoices")
    })?;
    Ok(rows.unwrap_or_default())
}

/// Invoices for an agent, newest first, optionally filtered by status and table.
///
/// `limit` bounds the rows returned; use [`outstanding_totals`] for figures
/// that must cover everything, since a truncated list would understate them.
pub fn list_invoices(
    config: &Config,
    agent_alias: &str,
    status: Option<InvoiceStatus>,
    table_number: Option<u64>,
    limit: usize,
) -> Result<Vec<Invoice>> {
    let rows = with_read_connection(config, |conn| {
        // Status is an enum rendered to a fixed string, never caller text.
        let mut sql = format!("SELECT {SELECT_COLUMNS} FROM invoices WHERE agent_alias = ?1");
        if let Some(status) = status {
            sql.push_str(&format!(" AND status = '{}'", status.as_str()));
        }
        if table_number.is_some() {
            sql.push_str(" AND table_number = ?3");
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?2");

        let mut stmt = conn.prepare(&sql)?;
        let mapped = match table_number {
            Some(t) => stmt
                .query_map(params![agent_alias, limit as i64, t as i64], row_to_invoice)?
                .collect::<rusqlite::Result<Vec<_>>>(),
            None => stmt
                .query_map(params![agent_alias, limit as i64], row_to_invoice)?
                .collect::<rusqlite::Result<Vec<_>>>(),
        };
        mapped.context("Failed to list invoices")
    })?;
    Ok(rows.unwrap_or_default())
}

/// Outstanding (pending) totals per currency: `(currency, count, base_units)`.
///
/// Aggregated in SQL over **every** matching row, not over a truncated page —
/// an outstanding balance that silently omits rows is worse than no balance.
/// Per-currency because base units are not comparable across assets: summing
/// lamports and USDC micro-units would be meaningless.
pub fn outstanding_totals(
    config: &Config,
    agent_alias: &str,
    table_number: Option<u64>,
) -> Result<Vec<(String, i64, i64)>> {
    let rows = with_read_connection(config, |conn| {
        let mut sql = String::from(
            "SELECT currency, COUNT(*), COALESCE(SUM(amount_base_units), 0) \
             FROM invoices WHERE agent_alias = ?1 AND status = 'pending'",
        );
        if table_number.is_some() {
            sql.push_str(" AND table_number = ?2");
        }
        sql.push_str(" GROUP BY currency ORDER BY currency");

        let mut stmt = conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        };
        let mapped = match table_number {
            Some(t) => stmt
                .query_map(params![agent_alias, t as i64], map)?
                .collect::<rusqlite::Result<Vec<_>>>(),
            None => stmt
                .query_map(params![agent_alias], map)?
                .collect::<rusqlite::Result<Vec<_>>>(),
        };
        mapped.context("Failed to total outstanding invoices")
    })?;
    Ok(rows.unwrap_or_default())
}

/// Pending invoices due for a settlement check, least-recently-checked first so
/// a capped run rotates through the backlog instead of starving the tail.
///
/// `limit` bounds RPC calls per run; expired invoices are excluded by
/// [`expire_stale_invoices`] having already moved them out of `pending`.
pub fn due_for_settlement_check(config: &Config, limit: usize) -> Result<Vec<Invoice>> {
    let rows = with_read_connection(config, |conn| {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM invoices WHERE status = 'pending' \
             ORDER BY last_checked_at IS NOT NULL, last_checked_at ASC, created_at ASC \
             LIMIT ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        stmt.query_map(params![limit as i64], row_to_invoice)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to list invoices due for settlement")
    })?;
    Ok(rows.unwrap_or_default())
}

/// Stamp a settlement attempt that did not find a confirmed payment, so the
/// next run prefers invoices it has looked at least recently.
pub fn mark_checked(config: &Config, id: &str, now: DateTime<Utc>) -> Result<()> {
    with_initialized_connection(config, |conn| {
        conn.execute(
            "UPDATE invoices SET last_checked_at = ?2 WHERE id = ?1",
            params![id, now.to_rfc3339()],
        )
        .context("Failed to stamp invoice check")?;
        Ok(())
    })
}

/// Settle an invoice against a verified transaction.
///
/// Only transitions `pending -> paid`, and returns whether this call was the
/// one that did it. The `status = 'pending'` guard makes concurrent settlement
/// runs idempotent: a second run observing the same transaction updates nothing
/// and reports `false`, so a payment is never double-counted.
pub fn mark_paid(
    config: &Config,
    id: &str,
    tx_signature: &str,
    paid_at: DateTime<Utc>,
) -> Result<bool> {
    with_initialized_connection(config, |conn| {
        let changed = conn
            .execute(
                "UPDATE invoices
                    SET status = 'paid', tx_signature = ?2, paid_at = ?3, last_checked_at = ?3
                  WHERE id = ?1 AND status = 'pending'",
                params![id, tx_signature, paid_at.to_rfc3339()],
            )
            .context("Failed to mark invoice paid")?;
        Ok(changed > 0)
    })
}

/// Paid invoices whose confirmation has not been delivered yet.
///
/// Separate from settlement so a channel outage at the moment of payment does
/// not lose the confirmation: the row stays `paid` with `notified_at` unset and
/// the next pass retries delivery without touching settlement state.
pub fn due_for_notification(config: &Config, limit: usize) -> Result<Vec<Invoice>> {
    let rows = with_read_connection(config, |conn| {
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM invoices \
             WHERE status = 'paid' AND notified_at IS NULL \
               AND channel <> '' AND reply_target IS NOT NULL \
             ORDER BY paid_at ASC LIMIT ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        stmt.query_map(params![limit as i64], row_to_invoice)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("Failed to list invoices due for notification")
    })?;
    Ok(rows.unwrap_or_default())
}

/// Record that the payment confirmation reached the channel.
///
/// Guarded on `notified_at IS NULL` and reports whether this call was the one
/// that claimed it, so two concurrent runs cannot both announce the same
/// payment to the customer.
pub fn mark_notified(config: &Config, id: &str, now: DateTime<Utc>) -> Result<bool> {
    with_initialized_connection(config, |conn| {
        let changed = conn
            .execute(
                "UPDATE invoices SET notified_at = ?2 WHERE id = ?1 AND notified_at IS NULL",
                params![id, now.to_rfc3339()],
            )
            .context("Failed to stamp invoice notification")?;
        Ok(changed > 0)
    })
}

/// Move unpaid invoices older than `expiry_hours` out of `pending` so they stop
/// consuming an RPC call on every settlement run. Returns how many expired.
///
/// Never touches `paid` rows — a late-expiring invoice that was already settled
/// must keep its paid state.
pub fn expire_stale_invoices(
    config: &Config,
    expiry_hours: u64,
    now: DateTime<Utc>,
) -> Result<usize> {
    let cutoff = now - Duration::hours(expiry_hours as i64);
    with_initialized_connection(config, |conn| {
        let changed = conn
            .execute(
                "UPDATE invoices SET status = 'expired'
                  WHERE status = 'pending' AND created_at < ?1",
                params![cutoff.to_rfc3339()],
            )
            .context("Failed to expire stale invoices")?;
        Ok(changed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &std::path::Path) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            ..Default::default()
        }
    }

    fn invoice(id: &str, table: u64, base_units: i64) -> Invoice {
        Invoice {
            id: id.to_string(),
            agent_alias: "main".to_string(),
            table_number: Some(table),
            customer: None,
            amount_base_units: base_units,
            currency: "USDC".to_string(),
            reference: format!("ref-{id}"),
            recipient: "merchant".to_string(),
            memo: format!("Table {table}"),
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

    #[test]
    fn reads_do_not_create_the_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());

        assert!(
            list_open_invoices(&config, "main", None)
                .unwrap()
                .is_empty()
        );
        assert!(get_invoice(&config, "main", "nope").unwrap().is_none());
        assert!(
            !invoice_db_path(&config).exists(),
            "a read before the first charge must not leave an empty ledger behind"
        );
    }

    #[test]
    fn insert_then_read_round_trips_base_units_exactly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());
        // 10.15 USDC — the value that truncates under float maths.
        insert_invoice(&config, &invoice("a", 7, 10_150_000)).unwrap();

        let got = get_invoice(&config, "main", "a").unwrap().expect("stored");
        assert_eq!(got.amount_base_units, 10_150_000, "no rounding in storage");
        assert!((got.display_amount() - 10.15).abs() < 1e-9);
        assert_eq!(got.charged_to(), "Table 7");
        assert_eq!(got.status, InvoiceStatus::Pending);
    }

    #[test]
    fn invoices_are_scoped_to_their_agent() {
        // Shared DB under data_dir; separation is per-row, so a query for one
        // agent must never surface another's charges.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());
        let mut other = invoice("b", 1, 1_000_000);
        other.agent_alias = "other".to_string();
        insert_invoice(&config, &invoice("a", 1, 1_000_000)).unwrap();
        insert_invoice(&config, &other).unwrap();

        let mine = list_open_invoices(&config, "main", None).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, "a");
        assert!(get_invoice(&config, "main", "b").unwrap().is_none());
    }

    #[test]
    fn duplicate_reference_is_rejected() {
        // The reference is the settlement lookup key; two invoices sharing one
        // would both match the same transaction and double-settle.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());
        insert_invoice(&config, &invoice("a", 1, 1)).unwrap();
        let mut clash = invoice("c", 2, 2);
        clash.reference = "ref-a".to_string();
        assert!(insert_invoice(&config, &clash).is_err());
    }

    #[test]
    fn filters_open_invoices_by_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());
        insert_invoice(&config, &invoice("a", 7, 1)).unwrap();
        insert_invoice(&config, &invoice("b", 7, 2)).unwrap();
        insert_invoice(&config, &invoice("c", 9, 3)).unwrap();

        assert_eq!(
            list_open_invoices(&config, "main", Some(7)).unwrap().len(),
            2
        );
        assert_eq!(
            list_open_invoices(&config, "main", Some(9)).unwrap().len(),
            1
        );
        assert_eq!(list_open_invoices(&config, "main", None).unwrap().len(), 3);
    }

    #[test]
    fn mark_paid_is_idempotent_across_concurrent_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());
        insert_invoice(&config, &invoice("a", 1, 1_000_000)).unwrap();

        let now = Utc::now();
        assert!(
            mark_paid(&config, "a", "sig-1", now).unwrap(),
            "first settles"
        );
        assert!(
            !mark_paid(&config, "a", "sig-2", now).unwrap(),
            "a second run seeing the same payment must not re-settle"
        );

        let got = get_invoice(&config, "main", "a").unwrap().unwrap();
        assert_eq!(got.status, InvoiceStatus::Paid);
        assert_eq!(got.tx_signature.as_deref(), Some("sig-1"));
        assert!(got.paid_at.is_some());
        assert!(
            list_open_invoices(&config, "main", None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn expiry_drops_stale_pending_but_never_paid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());
        let now = Utc::now();

        let mut old = invoice("old", 1, 1);
        old.created_at = now - Duration::hours(48);
        let mut old_paid = invoice("old_paid", 2, 1);
        old_paid.created_at = now - Duration::hours(48);
        insert_invoice(&config, &old).unwrap();
        insert_invoice(&config, &old_paid).unwrap();
        insert_invoice(&config, &invoice("fresh", 3, 1)).unwrap();
        mark_paid(&config, "old_paid", "sig", now).unwrap();

        assert_eq!(expire_stale_invoices(&config, 24, now).unwrap(), 1);
        assert_eq!(
            get_invoice(&config, "main", "old").unwrap().unwrap().status,
            InvoiceStatus::Expired
        );
        assert_eq!(
            get_invoice(&config, "main", "old_paid")
                .unwrap()
                .unwrap()
                .status,
            InvoiceStatus::Paid,
            "an already-settled invoice must never be expired"
        );
        assert_eq!(
            get_invoice(&config, "main", "fresh")
                .unwrap()
                .unwrap()
                .status,
            InvoiceStatus::Pending
        );
    }

    #[test]
    fn settlement_queue_is_capped_and_prefers_unchecked_then_oldest_checked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            let mut inv = invoice(id, i as u64, 1);
            inv.created_at = Utc::now() - Duration::minutes(i as i64);
            insert_invoice(&config, &inv).unwrap();
        }
        // 'a' has been checked; the never-checked ones must come first.
        mark_checked(&config, "a", Utc::now()).unwrap();

        let due = due_for_settlement_check(&config, 10).unwrap();
        assert_eq!(due.len(), 3);
        assert_eq!(
            due.last().map(|i| i.id.as_str()),
            Some("a"),
            "a recently-checked invoice sorts last so the backlog rotates"
        );

        assert_eq!(
            due_for_settlement_check(&config, 2).unwrap().len(),
            2,
            "the cap bounds RPC calls per run"
        );
    }

    #[test]
    fn settled_invoices_leave_the_settlement_queue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = test_config(dir.path());
        insert_invoice(&config, &invoice("a", 1, 1)).unwrap();
        mark_paid(&config, "a", "sig", Utc::now()).unwrap();
        assert!(due_for_settlement_check(&config, 10).unwrap().is_empty());
    }

    #[test]
    fn store_path_override_is_honoured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let custom = dir.path().join("nested").join("ledger.db");
        let mut config = test_config(dir.path());
        config.charge.store_path = custom.to_string_lossy().to_string();

        insert_invoice(&config, &invoice("a", 1, 1)).unwrap();
        assert!(custom.exists(), "override path must be used verbatim");
        assert!(!dir.path().join("charge").join("invoices.db").exists());
    }
}
