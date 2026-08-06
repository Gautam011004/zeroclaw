//! Per-turn collection of binary media attachments produced by tools.
//!
//! Tools return attachments on [`ToolResult::attachments`], which
//! `execute_one_tool` forwards onto [`ToolExecutionOutcome::attachments`].
//! Those outcomes are consumed by value in `collect_tool_results`, whose job is
//! to build *model-visible text* — so the bytes need somewhere else to live for
//! the rest of the turn.
//!
//! That home is [`AttachmentScope`]: an `Arc`-shared collector owned by the turn
//! entrypoint (the channel orchestrator, gateway, ACP server, or CLI), scoped as
//! a task-local for the lifetime of the tool loop, and drained once at delivery
//! into `SendMessage::attachments`. Attachment bytes therefore never enter
//! `history`, never reach the provider, and are never rendered as text or base64
//! for the LLM.
//!
//! This mirrors [`crate::agent::tool_receipts::ReceiptScope`] exactly, which
//! solves the same shape of problem (per-turn side-channel accumulation from
//! tools at arbitrary depth, consumed once at the end). As with receipts, the
//! task-local exists so `DelegateTool` sub-loops can forward into the *same*
//! per-turn collector without changing the `Tool` trait signature.
//!
//! # Scope placement
//!
//! The collector must be created by, and drained by, the **same** stack frame —
//! the turn entrypoint. Opening the scope deeper (e.g. inside `Agent::turn`) and
//! draining it shallower silently yields nothing: by the time the outer frame
//! runs, the scoped future has completed and the task-local is unset. See
//! [`AttachmentScope::current`], which reports that case as `None` rather than
//! degrading it to an empty `Vec`.

use std::sync::{Arc, Mutex};

use zeroclaw_api::media::MediaAttachment;

/// Per-turn attachment forwarding scope. Cloning shares the same collector.
#[derive(Clone, Default)]
pub struct AttachmentScope {
    collector: Arc<Mutex<Vec<MediaAttachment>>>,
}

impl AttachmentScope {
    /// A fresh scope with an empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The collector reference for the explicit `ToolLoop.collected_attachments`
    /// parameter. Receives every tool-produced attachment for the turn.
    #[must_use]
    pub fn collector(&self) -> &Mutex<Vec<MediaAttachment>> {
        &self.collector
    }

    /// The ambient scope for the current task, or `None` when no turn
    /// entrypoint has opened one.
    ///
    /// `None` and `Some(empty)` are deliberately distinguishable: the former
    /// means "no collector is installed" (a wiring bug, if attachments were
    /// expected), the latter means "installed, and this turn produced none".
    /// Collapsing the two is what makes a misplaced scope look like a tool that
    /// simply returned nothing.
    #[must_use]
    pub fn current() -> Option<Self> {
        TURN_ATTACHMENT_SCOPE.try_with(Clone::clone).ok().flatten()
    }

    /// Append one attachment to this turn's collector.
    pub fn push(&self, attachment: MediaAttachment) {
        self.collector
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(attachment);
    }

    /// Append many attachments, preserving order.
    pub fn extend(&self, attachments: impl IntoIterator<Item = MediaAttachment>) {
        self.collector
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(attachments);
    }

    /// Take everything collected so far, leaving the collector empty.
    ///
    /// Call this exactly once per delivery. Draining twice on the same turn
    /// sends the attachments with whichever branch ran first and silently drops
    /// them from the second.
    #[must_use]
    pub fn drain(&self) -> Vec<MediaAttachment> {
        std::mem::take(
            &mut *self
                .collector
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

tokio::task_local! {
    /// Set by each turn entrypoint for the lifetime of one tool loop.
    /// `DelegateTool` reads this to forward sub-agent attachments into the same
    /// per-turn collector, so a QR code produced three levels deep still reaches
    /// the channel.
    pub static TURN_ATTACHMENT_SCOPE: Option<AttachmentScope>;
}

/// Scope `TURN_ATTACHMENT_SCOPE` around `fut` for the lifetime of one turn.
/// One seam shared by every entrypoint; a `None` scope is inert.
pub async fn scope_attachments<F>(scope: Option<AttachmentScope>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    TURN_ATTACHMENT_SCOPE.scope(scope, fut).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(name: &str) -> MediaAttachment {
        MediaAttachment {
            file_name: name.to_string(),
            data: vec![1, 2, 3],
            mime_type: Some("image/png".to_string()),
        }
    }

    #[tokio::test]
    async fn current_is_none_outside_any_scope() {
        assert!(
            AttachmentScope::current().is_none(),
            "an unscoped task must report None, not an empty collector — \
             collapsing the two hides a misplaced scope"
        );
    }

    #[tokio::test]
    async fn current_is_none_when_scoped_to_none() {
        scope_attachments(None, async {
            assert!(AttachmentScope::current().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn push_inside_scope_is_visible_on_the_outside_handle() {
        // The load-bearing property: the entrypoint keeps its own clone, so the
        // bytes survive the scoped future completing.
        let scope = AttachmentScope::new();
        scope_attachments(Some(scope.clone()), async {
            AttachmentScope::current()
                .expect("scope must be installed")
                .push(attachment("qr.png"));
        })
        .await;

        let drained = scope.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].file_name, "qr.png");
    }

    #[tokio::test]
    async fn drain_empties_the_collector() {
        let scope = AttachmentScope::new();
        scope.push(attachment("a.png"));
        assert_eq!(scope.drain().len(), 1);
        assert!(
            scope.drain().is_empty(),
            "a second drain must not re-deliver the same attachments"
        );
    }

    #[tokio::test]
    async fn extend_preserves_order() {
        let scope = AttachmentScope::new();
        scope.extend([attachment("1.png"), attachment("2.png")]);
        scope.push(attachment("3.png"));
        let names: Vec<_> = scope
            .drain()
            .into_iter()
            .map(|a| a.file_name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["1.png", "2.png", "3.png"]);
    }

    #[tokio::test]
    async fn clones_share_one_collector() {
        let scope = AttachmentScope::new();
        let clone = scope.clone();
        clone.push(attachment("shared.png"));
        assert_eq!(scope.drain().len(), 1);
    }

    #[tokio::test]
    async fn scope_does_not_leak_past_the_scoped_future() {
        // Regression guard for the original bug: scoping deep and draining
        // shallow. Once the scoped future completes, the task-local is gone.
        scope_attachments(Some(AttachmentScope::new()), async {
            assert!(AttachmentScope::current().is_some());
        })
        .await;
        assert!(
            AttachmentScope::current().is_none(),
            "the task-local must not outlive its scoped future"
        );
    }
}
