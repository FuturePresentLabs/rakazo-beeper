//! Interim reply surface: a messaging attachment, never an assistant append.
//!
//! Rakazo has no assistant-append RPC. `threads.send` always writes a **user**
//! message and queues a model run, so this module deliberately has no path to
//! it. The only outbound operation is `conversationAttachments.directSend` on a
//! linked attachment that belongs to the allowlisted attachment-backed bot.
//!
//! A normal Agent bot (like the Fleet Orchestrator today) has no linked
//! attachment, so `reply` fails closed with `unsupported: no assistant-append
//! RPC`. It never lies by posting user text under the orchestrator's name.

use crate::rakazo::{Bot, LinkedAttachment, Rakazo};
use anyhow::{Result, ensure};
use std::future::Future;

/// The only outbound capability reply needs. There is intentionally no
/// `threads.send`/user-message method here.
pub trait AttachmentSurface {
    fn linked(&self) -> impl Future<Output = Result<Vec<LinkedAttachment>>> + Send;
    fn direct_send(
        &self,
        attachment_id: &str,
        text: &str,
        nonce: &str,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl AttachmentSurface for Rakazo {
    fn linked(&self) -> impl Future<Output = Result<Vec<LinkedAttachment>>> + Send {
        Rakazo::linked_attachments(self)
    }

    fn direct_send(
        &self,
        attachment_id: &str,
        text: &str,
        nonce: &str,
    ) -> impl Future<Output = Result<()>> + Send {
        Rakazo::direct_send_attachment(self, attachment_id, text, nonce)
    }
}

/// What `reply` would do, computed before any side effect so `--dry-run` can
/// print the same decision the real call would take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Send text to this linked attachment.
    Attachment {
        attachment_id: String,
        label: String,
    },
    /// No assistant-append RPC and no attachment to fall back to.
    Unsupported(String),
}

impl Plan {
    pub fn surface(&self) -> &'static str {
        match self {
            Self::Attachment { .. } => "attachment",
            Self::Unsupported(_) => "unsupported",
        }
    }
}

/// Decide how a reply to `bot` would be delivered. Read-only.
pub async fn plan<S: AttachmentSurface>(surface: &S, bot: &Bot) -> Result<Plan> {
    if bot.capabilities.is_none() {
        return Ok(Plan::Unsupported(
            "unsupported: no assistant-append RPC".to_string(),
        ));
    }
    let matches: Vec<_> = surface
        .linked()
        .await?
        .into_iter()
        .filter(|attachment| attachment.label == bot.name)
        .collect();
    Ok(match matches.as_slice() {
        [attachment] => Plan::Attachment {
            attachment_id: attachment.id.clone(),
            label: attachment.label.clone(),
        },
        [] => Plan::Unsupported("unsupported: no assistant-append RPC".to_string()),
        _ => Plan::Unsupported(
            "unsupported: no assistant-append RPC (ambiguous linked attachment)".to_string(),
        ),
    })
}

/// Execute a reply. Empty text is refused before listing anything; an
/// unsupported surface performs no write.
pub async fn reply<S: AttachmentSurface>(
    surface: &S,
    bot: &Bot,
    text: &str,
    nonce: &str,
) -> Result<Plan> {
    ensure!(!text.trim().is_empty(), "refusing to send an empty reply");
    let plan = plan(surface, bot).await?;
    if let Plan::Attachment { attachment_id, .. } = &plan {
        surface.direct_send(attachment_id, text, nonce).await?;
    }
    Ok(plan)
}

/// A UUID-shaped idempotency key for `directSend`, which requires a UUID.
pub fn nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    // RFC 9562 version 4, variant 1.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn bot(name: &str, capabilities: Option<serde_json::Value>) -> Bot {
        Bot {
            id: "bot-1".into(),
            name: name.into(),
            color: String::new(),
            title: String::new(),
            description: String::new(),
            space_id: "space".into(),
            thread_id: Some("thread-1".into()),
            parent_bot_id: None,
            spawn_key: None,
            archived_at: None,
            capabilities,
            relay_only: None,
        }
    }

    #[derive(Default)]
    struct FakeSurface {
        linked: Vec<LinkedAttachment>,
        sent: Mutex<Vec<(String, String, String)>>,
    }

    impl AttachmentSurface for FakeSurface {
        fn linked(&self) -> impl Future<Output = Result<Vec<LinkedAttachment>>> + Send {
            let linked = self.linked.clone();
            async move { Ok(linked) }
        }

        fn direct_send(
            &self,
            attachment_id: &str,
            text: &str,
            nonce: &str,
        ) -> impl Future<Output = Result<()>> + Send {
            self.sent.lock().unwrap().push((
                attachment_id.to_string(),
                text.to_string(),
                nonce.to_string(),
            ));
            async move { Ok(()) }
        }
    }

    fn attachment(id: &str, label: &str) -> LinkedAttachment {
        LinkedAttachment {
            id: id.into(),
            binding_id: "binding".into(),
            label: label.into(),
            channel: "phone".into(),
            linked_at: None,
        }
    }

    #[tokio::test]
    async fn normal_agent_has_no_reply_surface_and_sends_nothing() {
        let surface = FakeSurface::default();
        let outcome = reply(&surface, &bot("Fleet Orchestrator", None), "hi", "n")
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Plan::Unsupported("unsupported: no assistant-append RPC".into())
        );
        assert_eq!(outcome.surface(), "unsupported");
        assert!(surface.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn attachment_reply_uses_only_the_attachment_path() {
        let surface = FakeSurface {
            linked: vec![attachment("att-1", "Fleet Orchestrator")],
            sent: Mutex::new(Vec::new()),
        };
        let plan = reply(
            &surface,
            &bot("Fleet Orchestrator", Some(serde_json::json!({}))),
            "status is green",
            "nonce-1",
        )
        .await
        .unwrap();
        assert_eq!(
            plan,
            Plan::Attachment {
                attachment_id: "att-1".into(),
                label: "Fleet Orchestrator".into()
            }
        );
        assert_eq!(
            *surface.sent.lock().unwrap(),
            vec![(
                "att-1".to_string(),
                "status is green".to_string(),
                "nonce-1".to_string()
            )]
        );
        assert_eq!(plan.surface(), "attachment");
    }

    #[tokio::test]
    async fn attachment_is_never_guessed_from_a_foreign_label() {
        let surface = FakeSurface {
            linked: vec![attachment("att-other", "Someone Else")],
            sent: Mutex::new(Vec::new()),
        };
        assert_eq!(
            plan(
                &surface,
                &bot("Fleet Orchestrator", Some(serde_json::json!({})))
            )
            .await
            .unwrap(),
            Plan::Unsupported("unsupported: no assistant-append RPC".into())
        );
        assert!(surface.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn generated_nonce_is_a_uuid_v4() {
        let value = nonce();
        let parts: Vec<_> = value.split('-').map(str::len).collect();
        assert_eq!(parts, vec![8, 4, 4, 4, 12]);
        assert_eq!(&value[14..15], "4");
    }
}
