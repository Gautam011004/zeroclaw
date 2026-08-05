use std::sync::{Arc, Mutex};
use crate::media::MediaAttachment;

#[derive(Clone, Default)]
pub struct TurnAttachmentContext {
    pub attachments: Arc<Mutex<Vec<MediaAttachment>>>,
}

tokio::task_local! {
    pub static TURN_ATTACHMENT_CONTEXT: Option<TurnAttachmentContext>;
}

pub fn push_attachment(att: MediaAttachment) {
    let _ = TURN_ATTACHMENT_CONTEXT.try_with(|ctx| {
        if let Some(ctx) = ctx {
            if let Ok(mut attachments) = ctx.attachments.lock() {
                attachments.push(att);
            }
        }
    });
}

pub fn take_attachments() -> Vec<MediaAttachment> {
    TURN_ATTACHMENT_CONTEXT
        .try_with(|ctx| {
            if let Some(ctx) = ctx {
                if let Ok(mut attachments) = ctx.attachments.lock() {
                    return std::mem::take(&mut *attachments);
                }
            }
            Vec::new()
        })
        .unwrap_or_default()
}