//! Where the current turn came from, readable by any tool.
//!
//! A tool is constructed once per agent, but the channel a turn arrives on
//! varies per message — so origin cannot be a constructor argument. The turn
//! loop already carries `channel_name` / `channel_reply_target`; this exposes
//! them to tools through a task-local, the same way
//! [`crate::agent::attachments`] carries media out.
//!
//! Tools that schedule deferred work need this: the `charge` tool records the
//! originating channel on its invoice so a settlement check running minutes
//! later — in a different process, with no turn around it — can deliver the
//! payment confirmation back to the conversation that asked for it.

use zeroclaw_api::TOOL_LOOP_THREAD_ID;

/// The conversation a turn is executing for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnChannelContext {
    /// Channel alias, e.g. `telegram`. Empty for non-channel entrypoints.
    pub channel: String,
    /// Where a reply goes on that channel (chat id, room id, address).
    /// `None` when the turn has no addressable reply target — a CLI run, say.
    pub reply_target: Option<String>,
    /// Platform thread id, when the channel threads replies.
    pub thread_id: Option<String>,
}

/// Join a channel type and alias into the dotted `<type>.<alias>` ref that
/// out-of-band delivery requires.
///
/// Mirrors `composite_channel_key` in `zeroclaw-channels`, which this crate
/// cannot call: channels depends on runtime, not the reverse. Kept trivial and
/// tested so the duplication stays honest.
#[must_use]
pub fn composite_channel_ref(channel: &str, alias: Option<&str>) -> String {
    match alias.map(str::trim).filter(|a| !a.is_empty()) {
        Some(alias) => format!("{channel}.{alias}"),
        None => channel.to_string(),
    }
}

impl TurnChannelContext {
    /// Build a context from a bare channel type plus its alias.
    #[must_use]
    pub fn with_alias(channel: &str, alias: Option<&str>, reply_target: Option<&str>) -> Self {
        Self {
            channel: composite_channel_ref(channel, alias),
            reply_target: reply_target
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            thread_id: TOOL_LOOP_THREAD_ID.try_with(Clone::clone).ok().flatten(),
        }
    }

    #[must_use]
    pub fn new(channel: &str, reply_target: Option<&str>) -> Self {
        Self {
            channel: channel.to_string(),
            reply_target: reply_target
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            // Threading is already scoped by the orchestrator; read it here so
            // a confirmation lands in the same thread as the request rather
            // than at the top of the room.
            thread_id: TOOL_LOOP_THREAD_ID.try_with(Clone::clone).ok().flatten(),
        }
    }

    /// The ambient context, or `None` outside any turn.
    ///
    /// `None` and "present but unaddressable" stay distinct: the former means
    /// no turn is running, the latter that this turn has nowhere to reply.
    #[must_use]
    pub fn current() -> Option<Self> {
        TURN_CHANNEL_CONTEXT.try_with(Clone::clone).ok().flatten()
    }

    /// Whether a deferred message could actually be delivered here.
    #[must_use]
    pub fn is_deliverable(&self) -> bool {
        !self.channel.trim().is_empty() && self.reply_target.is_some()
    }
}

tokio::task_local! {
    /// Set around each tool execution by the turn loop.
    pub static TURN_CHANNEL_CONTEXT: Option<TurnChannelContext>;
}

/// Scope `TURN_CHANNEL_CONTEXT` around `fut`. A `None` context is inert.
pub async fn scope_channel_context<F>(ctx: Option<TurnChannelContext>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    TURN_CHANNEL_CONTEXT.scope(ctx, fut).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_is_none_outside_a_turn() {
        assert!(TurnChannelContext::current().is_none());
    }

    #[tokio::test]
    async fn context_is_visible_to_the_scoped_future() {
        let ctx = TurnChannelContext::new("telegram", Some("12345"));
        scope_channel_context(Some(ctx.clone()), async {
            let seen = TurnChannelContext::current().expect("scoped");
            assert_eq!(seen.channel, "telegram");
            assert_eq!(seen.reply_target.as_deref(), Some("12345"));
            assert!(seen.is_deliverable());
        })
        .await;
        assert!(
            TurnChannelContext::current().is_none(),
            "the task-local must not outlive its scoped future"
        );
    }

    #[tokio::test]
    async fn blank_reply_targets_are_not_deliverable() {
        // A whitespace-only target would produce a send to nowhere; treat it
        // as absent so the caller can skip delivery deliberately.
        for target in [Some("   "), Some(""), None] {
            let ctx = TurnChannelContext::new("telegram", target);
            assert!(ctx.reply_target.is_none());
            assert!(!ctx.is_deliverable());
        }
    }

    #[test]
    fn a_channel_ref_is_dotted_only_when_aliased() {
        // Out-of-band delivery resolves `<type>.<alias>` from config; a bare
        // "telegram" is rejected outright once no live channel registry is
        // available — which is exactly the case in a short-lived
        // `charge check` process.
        assert_eq!(composite_channel_ref("telegram", Some("tg")), "telegram.tg");
        assert_eq!(composite_channel_ref("telegram", None), "telegram");
        assert_eq!(composite_channel_ref("telegram", Some("")), "telegram");
        assert_eq!(composite_channel_ref("telegram", Some("  ")), "telegram");
    }

    #[tokio::test]
    async fn with_alias_records_the_dotted_ref() {
        let ctx = TurnChannelContext::with_alias("telegram", Some("tg"), Some("chat-1"));
        assert_eq!(ctx.channel, "telegram.tg");
        assert!(ctx.is_deliverable());
    }

    #[tokio::test]
    async fn a_channelless_turn_is_not_deliverable() {
        let ctx = TurnChannelContext::new("", Some("12345"));
        assert!(!ctx.is_deliverable(), "no channel means nowhere to send");
    }
}
