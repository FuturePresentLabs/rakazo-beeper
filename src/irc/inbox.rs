//! Durable IRC inbox.
//!
//! The inbox is authoritative for Rakazo: unread/acked/replied/reactions come
//! from this table, never from IRC or the bouncer. Inbound rows are written
//! *before* the caller prints or hands them to the UI. Outbound rows are written
//! `pending` before the socket write and become `sent` only on an echo; an
//! ambiguous send becomes `unknown` and is never automatically resent.
//!
//! `source_key` is the dedupe identity. With IRCv3 tags it is `msgid:<id>`;
//! without tags the client passes `src:<sha256(...)>` (see [`super::proto`]).
//! Replayed JOIN history therefore inserts nothing and notifies nobody.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::{path::Path, sync::Mutex};

use super::proto::{msgid_key, parse_numbered_chunk};

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
CREATE TABLE IF NOT EXISTS irc_messages (
  source_key TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  msg_id TEXT NOT NULL,
  direction TEXT NOT NULL,
  body TEXT NOT NULL,
  ts TEXT NOT NULL,
  unread INTEGER NOT NULL DEFAULT 0,
  acked INTEGER NOT NULL DEFAULT 0,
  replied INTEGER NOT NULL DEFAULT 0,
  reaction TEXT,
  raw_tags TEXT NOT NULL DEFAULT '{}',
  status TEXT NOT NULL,
  part_index INTEGER,
  part_total INTEGER,
  reply_to TEXT
);
CREATE INDEX IF NOT EXISTS irc_messages_thread ON irc_messages(thread_id);
CREATE INDEX IF NOT EXISTS irc_messages_msgid ON irc_messages(msg_id);
CREATE INDEX IF NOT EXISTS irc_messages_pending ON irc_messages(direction,status);
";

pub struct Inbox {
    db: Mutex<Connection>,
}

#[derive(Clone, Debug)]
pub struct InboundRecord {
    pub thread_id: String,
    pub msg_id: String,
    pub body: String,
    pub ts: String,
    pub raw_tags: String,
    /// Primary key to use for a whole message. For a tagged numbered chunk the
    /// inbox derives its own per-part key instead.
    pub source_key: String,
    /// True only when `msg_id` came from an IRCv3 `msgid` tag. Untagged
    /// numbered chunks cannot be safely grouped after a restart, so they are
    /// stored as individual rows rather than guessed together.
    pub tagged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredInbound {
    pub thread_id: String,
    pub msg_id: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOutcome {
    Inserted(StoredInbound),
    Duplicate,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub thread_id: String,
    pub msg_id: String,
    pub direction: String,
    pub body: String,
    pub status: String,
    pub reaction: Option<String>,
    pub reply_to: Option<String>,
}

impl Inbox {
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let db =
            Connection::open(path).with_context(|| format!("open IRC inbox {}", path.display()))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(SCHEMA)?;
        let inbox = Self { db: Mutex::new(db) };
        // A process that died between the pending insert and the outcome write
        // leaves an ambiguous row. It is never automatically resent.
        inbox.db.lock().unwrap().execute(
            "UPDATE irc_messages SET status='unknown' WHERE direction='outbound' AND status='pending'",
            [],
        )?;
        Ok(inbox)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }

    /// Store an inbound line. Numbered chunks carrying an IRCv3 `msgid` are
    /// persisted individually and assembled into one logical row when all parts
    /// are present. Returns `Duplicate` for replay or for an incomplete group.
    pub fn store_inbound(&self, record: InboundRecord) -> Result<StoreOutcome> {
        let db = self.db.lock().unwrap();
        let chunk = if record.tagged {
            parse_numbered_chunk(&record.body)
        } else {
            None
        };
        let Some((index, total, body)) = chunk else {
            let inserted = db.execute(
                "INSERT OR IGNORE INTO irc_messages
                 (source_key,thread_id,msg_id,direction,body,ts,unread,status,raw_tags)
                 VALUES (?1,?2,?3,'inbound',?4,?5,1,'received',?6)",
                params![
                    record.source_key,
                    record.thread_id,
                    record.msg_id,
                    record.body,
                    record.ts,
                    record.raw_tags,
                ],
            )?;
            return if inserted == 0 {
                Ok(StoreOutcome::Duplicate)
            } else {
                Ok(StoreOutcome::Inserted(StoredInbound {
                    thread_id: record.thread_id,
                    msg_id: record.msg_id,
                    body: record.body,
                }))
            };
        };

        // Tagged numbered chunk: durable per-part row, keyed by msgid + index.
        let part_key = format!("chunk:{}:{index}", record.msg_id);
        let inserted = db.execute(
            "INSERT OR IGNORE INTO irc_messages
             (source_key,thread_id,msg_id,direction,body,ts,unread,status,raw_tags,part_index,part_total)
             VALUES (?1,?2,?3,'inbound',?4,?5,0,'chunk',?6,?7,?8)",
            params![
                part_key,
                record.thread_id,
                record.msg_id,
                body,
                record.ts,
                record.raw_tags,
                index as i64,
                total as i64,
            ],
        )?;
        if inserted == 0 {
            return Ok(StoreOutcome::Duplicate);
        }

        let parts = {
            let mut statement = db.prepare(
                "SELECT part_index, body FROM irc_messages
                 WHERE msg_id=?1 AND direction='inbound' AND part_index IS NOT NULL
                 ORDER BY part_index",
            )?;
            let rows = statement.query_map(params![record.msg_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if parts.len() != total {
            return Ok(StoreOutcome::Duplicate);
        }
        for (expected, (actual, _)) in (1..=total).zip(parts.iter()) {
            if *actual != expected as i64 {
                return Ok(StoreOutcome::Duplicate);
            }
        }
        let assembled = parts.into_iter().map(|(_, body)| body).collect::<String>();
        let inserted = db.execute(
            "INSERT OR IGNORE INTO irc_messages
             (source_key,thread_id,msg_id,direction,body,ts,unread,status,raw_tags)
             VALUES (?1,?2,?3,'inbound',?4,?5,1,'received',?6)",
            params![
                msgid_key(&record.msg_id),
                record.thread_id,
                record.msg_id,
                assembled,
                record.ts,
                record.raw_tags,
            ],
        )?;
        if inserted == 0 {
            Ok(StoreOutcome::Duplicate)
        } else {
            Ok(StoreOutcome::Inserted(StoredInbound {
                thread_id: record.thread_id,
                msg_id: record.msg_id,
                body: assembled,
            }))
        }
    }

    /// Persist an outbound logical message (all chunks share `body`) as
    /// `pending` before any socket write.
    pub fn insert_outbound(
        &self,
        thread_id: &str,
        msgid: &str,
        body: &str,
        ts: &str,
        reply_to: Option<&str>,
    ) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO irc_messages
             (source_key,thread_id,msg_id,direction,body,ts,unread,status,raw_tags,reply_to)
             VALUES (?1,?2,?3,'outbound',?4,?5,0,'pending',?6,?7)",
            params![
                format!("out:{msgid}"),
                thread_id,
                msgid,
                body,
                ts,
                serde_json::json!({"kind":"message"}).to_string(),
                reply_to,
            ],
        )?;
        Ok(())
    }

    pub fn set_outbound_status(&self, msgid: &str, status: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE irc_messages SET status=?2 WHERE source_key=?1",
            params![format!("out:{msgid}"), status],
        )?;
        Ok(())
    }

    /// Mark a pending outbound row `sent` when the server echoes our `msgid`.
    pub fn mark_sent_by_msgid(&self, msgid: &str) -> Result<bool> {
        let changed = self.db.lock().unwrap().execute(
            "UPDATE irc_messages SET status='sent'
             WHERE source_key=?1 AND direction='outbound' AND status='pending'",
            params![format!("out:{msgid}")],
        )?;
        Ok(changed > 0)
    }

    /// Fallback echo detection for servers that do not relay tags (the current
    /// fleet ircd). Matches the newest pending outbound row in that thread.
    pub fn mark_sent_by_body(&self, thread_id: &str, body: &str) -> Result<bool> {
        let changed = self.db.lock().unwrap().execute(
            "UPDATE irc_messages SET status='sent'
             WHERE rowid = (SELECT rowid FROM irc_messages
                         WHERE thread_id=?1 AND direction='outbound'
                           AND status='pending' AND body=?2
                         ORDER BY rowid DESC LIMIT 1)",
            params![thread_id, body],
        )?;
        Ok(changed > 0)
    }

    /// Record a reaction locally first. The wire send is best-effort.
    pub fn apply_reaction(&self, thread_id: &str, msgid: &str, emoji: &str) -> Result<bool> {
        let changed = self.db.lock().unwrap().execute(
            "UPDATE irc_messages SET reaction=?3
             WHERE thread_id=?1 AND msg_id=?2 AND direction='inbound'",
            params![thread_id, msgid, emoji],
        )?;
        Ok(changed > 0)
    }

    pub fn mark_replied(&self, thread_id: &str, msgid: &str) -> Result<bool> {
        let changed = self.db.lock().unwrap().execute(
            "UPDATE irc_messages SET replied=1
             WHERE thread_id=?1 AND msg_id=?2 AND direction='inbound'",
            params![thread_id, msgid],
        )?;
        Ok(changed > 0)
    }

    pub fn row(&self, msgid: &str) -> Result<Option<Row>> {
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT thread_id,msg_id,direction,body,status,reaction,reply_to
                 FROM irc_messages WHERE msg_id=?1 AND part_index IS NULL
                 ORDER BY rowid LIMIT 1",
                params![msgid],
                |row| {
                    Ok(Row {
                        thread_id: row.get(0)?,
                        msg_id: row.get(1)?,
                        direction: row.get(2)?,
                        body: row.get(3)?,
                        status: row.get(4)?,
                        reaction: row.get(5)?,
                        reply_to: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Count logical rows (chunk parts excluded).
    pub fn count(&self) -> Result<i64> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM irc_messages WHERE part_index IS NULL",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn count_thread(&self, thread_id: &str) -> Result<i64> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM irc_messages
             WHERE thread_id=?1 AND part_index IS NULL",
            params![thread_id],
            |row| row.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(thread: &str, msg_id: &str, body: &str, tagged: bool) -> InboundRecord {
        InboundRecord {
            thread_id: thread.into(),
            msg_id: msg_id.into(),
            body: body.into(),
            ts: "2026-01-01T00:00:00Z".into(),
            raw_tags: "{}".into(),
            source_key: if tagged {
                msgid_key(msg_id)
            } else {
                format!("src:{msg_id}")
            },
            tagged,
        }
    }

    #[test]
    fn duplicate_source_key_inserts_nothing() {
        let inbox = Inbox::open_in_memory().unwrap();
        assert!(matches!(
            inbox
                .store_inbound(record("#orch", "a", "hello", false))
                .unwrap(),
            StoreOutcome::Inserted(_)
        ));
        assert_eq!(
            inbox
                .store_inbound(record("#orch", "a", "hello", false))
                .unwrap(),
            StoreOutcome::Duplicate
        );
        assert_eq!(inbox.count().unwrap(), 1);
    }

    #[test]
    fn tagged_numbered_chunks_reassemble_into_one_row() {
        let inbox = Inbox::open_in_memory().unwrap();
        assert_eq!(
            inbox
                .store_inbound(record("#orch", "m1", "[1/2] hello ", true))
                .unwrap(),
            StoreOutcome::Duplicate
        );
        assert_eq!(
            inbox
                .store_inbound(record("#orch", "m1", "[2/2] world", true))
                .unwrap(),
            StoreOutcome::Inserted(StoredInbound {
                thread_id: "#orch".into(),
                msg_id: "m1".into(),
                body: "hello world".into(),
            })
        );
        assert_eq!(inbox.count().unwrap(), 1);
        assert_eq!(inbox.row("m1").unwrap().unwrap().body, "hello world");
        // Replayed parts and the logical row all dedupe.
        assert_eq!(
            inbox
                .store_inbound(record("#orch", "m1", "[1/2] hello ", true))
                .unwrap(),
            StoreOutcome::Duplicate
        );
        assert_eq!(inbox.count().unwrap(), 1);
    }

    #[test]
    fn outbound_pending_becomes_unknown_across_restart() {
        let dir = std::env::temp_dir().join(format!("rakazo-irc-inbox-{}", std::process::id()));
        let path = dir.join("inbox.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let inbox = Inbox::open(&path).unwrap();
            inbox
                .insert_outbound("#orch", "out1", "hello", "now", None)
                .unwrap();
            assert_eq!(inbox.row("out1").unwrap().unwrap().status, "pending");
        }
        let inbox = Inbox::open(&path).unwrap();
        assert_eq!(inbox.row("out1").unwrap().unwrap().status, "unknown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reaction_and_reply_are_persisted_locally() {
        let inbox = Inbox::open_in_memory().unwrap();
        inbox
            .store_inbound(record("#orch", "a", "hello", false))
            .unwrap();
        assert!(inbox.apply_reaction("#orch", "a", "👍").unwrap());
        assert!(inbox.mark_replied("#orch", "a").unwrap());
        let row = inbox.row("a").unwrap().unwrap();
        assert_eq!(row.reaction.as_deref(), Some("👍"));
    }
}
