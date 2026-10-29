use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use fs2::FileExt;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::Mutex,
};

pub struct Store {
    db: Mutex<Connection>,
    cipher: XChaCha20Poly1305,
    _lock: DatabaseLock,
}

struct DatabaseLock(File);
impl Drop for DatabaseLock {
    fn drop(&mut self) {
        // Closing alone can leave flock held briefly by a concurrent fork
        // before exec closes inherited descriptors. Release the owned lease
        // explicitly, after SQLite (the first Store field) has closed.
        if let Err(error) = FileExt::unlock(&self.0) {
            tracing::error!(%error, "could not release database process lock");
        }
    }
}
#[derive(Clone, Debug)]
pub struct Binding {
    pub tenant: String,
    pub bot: String,
    pub room: String,
    pub ghost: String,
    pub cursor: i64,
}
pub struct Profile {
    pub avatar_hash: String,
    pub avatar_uri: String,
    pub applied_hash: String,
}
#[derive(Serialize, Deserialize)]
pub struct Inbound {
    pub event_id: String,
    pub room: String,
    pub text: String,
    #[serde(default)]
    pub content: serde_json::Value,
}

pub struct MessageLink {
    pub rakazo_id: String,
    pub matrix_id: String,
    pub part: String,
    pub role: String,
    pub kind: String,
    pub hash: String,
}
pub struct Pending {
    pub id: i64,
    pub event: Inbound,
    pub attempts: u32,
}

impl Store {
    pub fn open(path: &Path, key: &str) -> Result<Self> {
        let decoded = STANDARD
            .decode(key)
            .context("database key must be base64")?;
        ensure!(decoded.len() == 32, "database key must decode to 32 bytes");
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(path.with_extension("lockfile"))?;
        lock.try_lock_exclusive()
            .context("another bridge process owns this database")?;
        let lock = DatabaseLock(lock);
        // Create with restrictive permissions before SQLite opens WAL files.
        let _ = options.open(path)?;
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS tenants (id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS bindings (
              tenant TEXT NOT NULL, bot TEXT NOT NULL, room TEXT NOT NULL, ghost TEXT NOT NULL, cursor INTEGER NOT NULL,
              PRIMARY KEY(tenant,bot), UNIQUE(tenant,room));
            CREATE TABLE IF NOT EXISTS provisions (tenant TEXT NOT NULL, bot TEXT NOT NULL, cursor INTEGER NOT NULL, PRIMARY KEY(tenant,bot));
            CREATE TABLE IF NOT EXISTS backfills (tenant TEXT NOT NULL, bot TEXT NOT NULL, before_seq INTEGER NOT NULL, done INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(tenant,bot));
            CREATE TABLE IF NOT EXISTS historical_events (tenant TEXT NOT NULL, event_id TEXT NOT NULL, PRIMARY KEY(tenant,event_id));
            CREATE TABLE IF NOT EXISTS profiles (tenant TEXT NOT NULL, bot TEXT NOT NULL, avatar_hash TEXT NOT NULL, avatar_uri TEXT NOT NULL, applied_hash TEXT NOT NULL DEFAULT '', PRIMARY KEY(tenant,bot));
            CREATE TABLE IF NOT EXISTS message_links (tenant TEXT NOT NULL,bot TEXT NOT NULL,rakazo_id TEXT NOT NULL,part TEXT NOT NULL,matrix_id TEXT NOT NULL,role TEXT NOT NULL,kind TEXT NOT NULL,hash TEXT NOT NULL,PRIMARY KEY(tenant,bot,rakazo_id,part));
            CREATE INDEX IF NOT EXISTS message_links_matrix ON message_links(tenant,bot,matrix_id);
            CREATE TABLE IF NOT EXISTS send_receipts (tenant TEXT NOT NULL,bot TEXT NOT NULL,event_id TEXT NOT NULL,seq INTEGER NOT NULL,PRIMARY KEY(tenant,bot,event_id));
            CREATE TABLE IF NOT EXISTS event_nonces (tenant TEXT NOT NULL,nonce TEXT NOT NULL,event_id TEXT NOT NULL,room TEXT NOT NULL,PRIMARY KEY(tenant,nonce));
            CREATE TABLE IF NOT EXISTS run_routes (tenant TEXT NOT NULL,bot TEXT NOT NULL,run_id TEXT NOT NULL,event_id TEXT NOT NULL,thread_root TEXT,PRIMARY KEY(tenant,bot,run_id));
            CREATE TABLE IF NOT EXISTS media_cache (tenant TEXT NOT NULL,bot TEXT NOT NULL,source_id TEXT NOT NULL,uri TEXT NOT NULL,PRIMARY KEY(tenant,bot,source_id));
            CREATE TABLE IF NOT EXISTS link_bootstraps (tenant TEXT NOT NULL,bot TEXT NOT NULL,PRIMARY KEY(tenant,bot));
            CREATE TABLE IF NOT EXISTS sync_cursors (tenant TEXT NOT NULL,bot TEXT NOT NULL,kind TEXT NOT NULL,cursor INTEGER NOT NULL,PRIMARY KEY(tenant,bot,kind));
            CREATE TABLE IF NOT EXISTS transactions (tenant TEXT NOT NULL, txn TEXT NOT NULL, PRIMARY KEY(tenant,txn));
            CREATE TABLE IF NOT EXISTS inbox (
              id INTEGER PRIMARY KEY, tenant TEXT NOT NULL, event_id TEXT NOT NULL, room TEXT NOT NULL,
              payload BLOB, state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
              next_at INTEGER NOT NULL DEFAULT 0, error TEXT,
              UNIQUE(tenant,event_id));
            CREATE INDEX IF NOT EXISTS inbox_pending ON inbox(tenant,state,id);")?;
        let store = Self {
            db: Mutex::new(db),
            cipher: XChaCha20Poly1305::new_from_slice(&decoded).unwrap(),
            _lock: lock,
        };
        let saved: Option<Vec<u8>> = store
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key='key-check'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(saved) = saved {
            ensure!(
                store.decrypt("key-check", &saved)? == b"rakazo-beeper-v1",
                "incorrect database key"
            );
        } else {
            store.db.lock().unwrap().execute(
                "INSERT INTO metadata VALUES ('key-check',?1)",
                [store.encrypt("key-check", b"rakazo-beeper-v1")?],
            )?;
        }
        // Upgrade old queue metadata without retaining any completed message body.
        {
            let mut db = store.db.lock().unwrap();
            let rows = {
                let mut query=db.prepare("SELECT tenant,event_id,room FROM inbox WHERE NOT EXISTS(SELECT 1 FROM event_nonces WHERE event_nonces.tenant=inbox.tenant AND event_nonces.event_id=inbox.event_id)")?;
                query
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let tx = db.transaction()?;
            for (tenant, id, room) in rows {
                tx.execute(
                    "INSERT OR IGNORE INTO event_nonces VALUES(?1,?2,?3,?4)",
                    params![tenant, crate::stable_id(&[&tenant, &id]), id, room],
                )?;
            }
            tx.commit()?;
        }
        Ok(store)
    }

    pub fn profile(&self, tenant: &str, bot: &str) -> Result<Option<Profile>> {
        Ok(self.db.lock().unwrap().query_row("SELECT avatar_hash,avatar_uri,applied_hash FROM profiles WHERE tenant=?1 AND bot=?2", params![tenant,bot], |row| Ok(Profile {avatar_hash:row.get(0)?,avatar_uri:row.get(1)?,applied_hash:row.get(2)?})).optional()?)
    }
    pub fn link(&self, binding: &Binding, link: &MessageLink) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO message_links VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(tenant,bot,rakazo_id,part) DO UPDATE SET matrix_id=excluded.matrix_id,role=excluded.role,kind=excluded.kind,hash=excluded.hash",params![binding.tenant,binding.bot,link.rakazo_id,link.part,link.matrix_id,link.role,link.kind,link.hash])?;
        Ok(())
    }
    pub fn links(&self, binding: &Binding, id: &str) -> Result<Vec<MessageLink>> {
        let db = self.db.lock().unwrap();
        let mut query=db.prepare("SELECT rakazo_id,matrix_id,part,role,kind,hash FROM message_links WHERE tenant=?1 AND bot=?2 AND rakazo_id=?3 ORDER BY part")?;
        Ok(query
            .query_map(params![binding.tenant, binding.bot, id], |row| {
                Ok(MessageLink {
                    rakazo_id: row.get(0)?,
                    matrix_id: row.get(1)?,
                    part: row.get(2)?,
                    role: row.get(3)?,
                    kind: row.get(4)?,
                    hash: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn all_links(&self, binding: &Binding) -> Result<Vec<MessageLink>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare("SELECT rakazo_id,matrix_id,part,role,kind,hash FROM message_links WHERE tenant=?1 AND bot=?2 ORDER BY rakazo_id,part")?;
        Ok(query
            .query_map(params![binding.tenant, binding.bot], |row| {
                Ok(MessageLink {
                    rakazo_id: row.get(0)?,
                    matrix_id: row.get(1)?,
                    part: row.get(2)?,
                    role: row.get(3)?,
                    kind: row.get(4)?,
                    hash: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn commit_thread_clear(
        &self,
        binding: &Binding,
        event_cursor: i64,
        preserved: &[MessageLink],
        preserved_runs: &std::collections::HashSet<String>,
    ) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "DELETE FROM message_links WHERE tenant=?1 AND bot=?2",
            params![binding.tenant, binding.bot],
        )?;
        for link in preserved {
            tx.execute(
                "INSERT INTO message_links VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    binding.tenant,
                    binding.bot,
                    link.rakazo_id,
                    link.part,
                    link.matrix_id,
                    link.role,
                    link.kind,
                    link.hash
                ],
            )?;
        }
        let run_ids = {
            let mut query =
                tx.prepare("SELECT run_id FROM run_routes WHERE tenant=?1 AND bot=?2")?;
            query
                .query_map(params![binding.tenant, binding.bot], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for run_id in run_ids {
            if !preserved_runs.contains(&run_id) {
                tx.execute(
                    "DELETE FROM run_routes WHERE tenant=?1 AND bot=?2 AND run_id=?3",
                    params![binding.tenant, binding.bot, run_id],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO sync_cursors VALUES(?1,?2,'thread-events',?3) ON CONFLICT(tenant,bot,kind) DO UPDATE SET cursor=excluded.cursor",
            params![binding.tenant, binding.bot, event_cursor],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn matrix_link(&self, binding: &Binding, id: &str) -> Result<Option<MessageLink>> {
        Ok(self.db.lock().unwrap().query_row("SELECT rakazo_id,matrix_id,part,role,kind,hash FROM message_links WHERE tenant=?1 AND bot=?2 AND matrix_id=?3 LIMIT 1",params![binding.tenant,binding.bot,id],|row|Ok(MessageLink {rakazo_id:row.get(0)?,matrix_id:row.get(1)?,part:row.get(2)?,role:row.get(3)?,kind:row.get(4)?,hash:row.get(5)?})).optional()?)
    }
    pub fn receipt(&self, binding: &Binding, id: &str) -> Result<Option<i64>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT seq FROM send_receipts WHERE tenant=?1 AND bot=?2 AND event_id=?3",
                params![binding.tenant, binding.bot, id],
                |row| row.get(0),
            )
            .optional()?)
    }
    pub fn run_route(
        &self,
        binding: &Binding,
        run: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        Ok(self.db.lock().unwrap().query_row("SELECT event_id,thread_root FROM run_routes WHERE tenant=?1 AND bot=?2 AND run_id=?3",params![binding.tenant,binding.bot,run],|row|Ok((row.get(0)?,row.get(1)?))).optional()?)
    }
    pub fn save_run_route(
        &self,
        binding: &Binding,
        run: &str,
        event: &str,
        thread: Option<&str>,
    ) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO run_routes VALUES(?1,?2,?3,?4,?5) ON CONFLICT(tenant,bot,run_id) DO UPDATE SET event_id=excluded.event_id,thread_root=excluded.thread_root",params![binding.tenant,binding.bot,run,event,thread])?;
        Ok(())
    }
    pub fn pending_room_count(&self, binding: &Binding) -> Result<i64> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM inbox WHERE tenant=?1 AND room=?2 AND state='pending'",
            params![binding.tenant, binding.room],
            |row| row.get(0),
        )?)
    }
    pub fn sent_event(&self, binding: &Binding, event: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM inbox WHERE tenant=?1 AND room=?2 AND event_id=?3 AND state='sent')",
            params![binding.tenant, binding.room, event],
            |row| row.get(0),
        )?)
    }
    pub fn event_for_seq(&self, binding: &Binding, seq: i64) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT event_id FROM send_receipts WHERE tenant=?1 AND bot=?2 AND seq=?3",
                params![binding.tenant, binding.bot, seq],
                |row| row.get(0),
            )
            .optional()?)
    }
    pub fn event_for_nonce(&self, binding: &Binding, nonce: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT event_id FROM event_nonces WHERE tenant=?1 AND nonce=?2 AND room=?3",
                params![binding.tenant, nonce, binding.room],
                |row| row.get(0),
            )
            .optional()?)
    }
    pub fn complete_send(&self, binding: &Binding, id: i64, event: &str, seq: i64) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO send_receipts VALUES(?1,?2,?3,?4)",
            params![binding.tenant, binding.bot, event, seq],
        )?;
        tx.execute(
            "UPDATE inbox SET state='sent',payload=NULL,error=NULL WHERE tenant=?1 AND id=?2",
            params![binding.tenant, id],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn cached_media(&self, binding: &Binding, source: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT uri FROM media_cache WHERE tenant=?1 AND bot=?2 AND source_id=?3",
                params![binding.tenant, binding.bot, source],
                |row| row.get(0),
            )
            .optional()?)
    }
    pub fn cache_media(&self, binding: &Binding, source: &str, uri: &str) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO media_cache VALUES(?1,?2,?3,?4) ON CONFLICT(tenant,bot,source_id) DO UPDATE SET uri=excluded.uri",params![binding.tenant,binding.bot,source,uri])?;
        Ok(())
    }
    pub fn links_initialized(&self, binding: &Binding) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM link_bootstraps WHERE tenant=?1 AND bot=?2)",
            params![binding.tenant, binding.bot],
            |row| row.get(0),
        )?)
    }
    pub fn initialize_links(&self, binding: &Binding) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO link_bootstraps VALUES(?1,?2)",
            params![binding.tenant, binding.bot],
        )?;
        Ok(())
    }
    pub fn sync_cursor(&self, binding: &Binding, kind: &str) -> Result<Option<i64>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT cursor FROM sync_cursors WHERE tenant=?1 AND bot=?2 AND kind=?3",
                params![binding.tenant, binding.bot, kind],
                |row| row.get(0),
            )
            .optional()?)
    }
    pub fn set_sync_cursor(&self, binding: &Binding, kind: &str, cursor: i64) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO sync_cursors VALUES(?1,?2,?3,?4) ON CONFLICT(tenant,bot,kind) DO UPDATE SET cursor=excluded.cursor",params![binding.tenant,binding.bot,kind,cursor])?;
        Ok(())
    }
    pub fn fail(&self, tenant: &str, id: i64) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE inbox SET state='failed',payload=NULL,error='unsupported event; owner notified' WHERE tenant=?1 AND id=?2",params![tenant,id])?;
        Ok(())
    }
    pub fn cache_avatar(&self, tenant: &str, bot: &str, hash: &str, uri: &str) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO profiles(tenant,bot,avatar_hash,avatar_uri) VALUES(?1,?2,?3,?4) ON CONFLICT(tenant,bot) DO UPDATE SET avatar_hash=excluded.avatar_hash,avatar_uri=excluded.avatar_uri",params![tenant,bot,hash,uri])?;
        Ok(())
    }
    pub fn applied_profile(&self, tenant: &str, bot: &str, hash: &str) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE profiles SET applied_hash=?3 WHERE tenant=?1 AND bot=?2",
            params![tenant, bot, hash],
        )?;
        Ok(())
    }
    fn encrypt(&self, aad: &str, bytes: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0; 24];
        rand::thread_rng().fill_bytes(&mut nonce);
        let ciphertext = self.cipher.encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: bytes,
                aad: aad.as_bytes(),
            },
        )?;
        Ok([nonce.to_vec(), ciphertext].concat())
    }
    fn decrypt(&self, aad: &str, bytes: &[u8]) -> Result<Vec<u8>> {
        ensure!(bytes.len() >= 40, "invalid encrypted payload");
        self.cipher
            .decrypt(
                XNonce::from_slice(&bytes[..24]),
                Payload {
                    msg: &bytes[24..],
                    aad: aad.as_bytes(),
                },
            )
            .context("database payload authentication failed")
    }

    pub fn bind_tenant(&self, tenant: &str, fingerprint: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO tenants VALUES (?1,?2)",
            params![tenant, fingerprint],
        )?;
        let saved: String = db.query_row(
            "SELECT fingerprint FROM tenants WHERE id=?1",
            [tenant],
            |r| r.get(0),
        )?;
        ensure!(
            saved == fingerprint,
            "tenant identity changed; use a new tenant id instead of reusing persisted state"
        );
        Ok(())
    }
    pub fn bindings(&self, tenant: &str) -> Result<Vec<Binding>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare(
            "SELECT tenant,bot,room,ghost,cursor FROM bindings WHERE tenant=?1 ORDER BY bot",
        )?;
        Ok(query
            .query_map([tenant], |r| {
                Ok(Binding {
                    tenant: r.get(0)?,
                    bot: r.get(1)?,
                    room: r.get(2)?,
                    ghost: r.get(3)?,
                    cursor: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn add_binding(&self, binding: &Binding) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO bindings VALUES (?1,?2,?3,?4,?5)",
            params![
                binding.tenant,
                binding.bot,
                binding.room,
                binding.ghost,
                binding.cursor
            ],
        )?;
        Ok(())
    }
    pub fn baseline(&self, tenant: &str, bot: &str, proposed: i64) -> Result<i64> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO provisions VALUES (?1,?2,?3)",
            params![tenant, bot, proposed],
        )?;
        Ok(db.query_row(
            "SELECT cursor FROM provisions WHERE tenant=?1 AND bot=?2",
            params![tenant, bot],
            |r| r.get(0),
        )?)
    }
    pub fn cursor(&self, binding: &Binding, cursor: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE bindings SET cursor=MAX(cursor,?3) WHERE tenant=?1 AND bot=?2",
            params![binding.tenant, binding.bot, cursor],
        )?;
        Ok(())
    }

    pub fn history_before(&self, binding: &Binding) -> Result<Option<i64>> {
        let baseline = self.baseline(&binding.tenant, &binding.bot, binding.cursor)?;
        let before = baseline
            .checked_add(1)
            .context("history baseline overflow")?;
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO backfills(tenant,bot,before_seq) VALUES(?1,?2,?3)",
            params![binding.tenant, binding.bot, before],
        )?;
        let (before, done): (i64, bool) = db.query_row(
            "SELECT before_seq,done FROM backfills WHERE tenant=?1 AND bot=?2",
            params![binding.tenant, binding.bot],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((!done).then_some(before))
    }
    pub fn history_progress(&self, binding: &Binding, before: Option<i64>) -> Result<()> {
        self.db.lock().unwrap().execute("UPDATE backfills SET before_seq=COALESCE(?3,before_seq),done=(?3 IS NULL) WHERE tenant=?1 AND bot=?2",params![binding.tenant,binding.bot,before])?;
        Ok(())
    }
    // Reserve before the HTTP request: its websocket echo may precede its response.
    pub fn reserve_history(&self, tenant: &str, ids: &[String]) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        for id in ids {
            tx.execute(
                "INSERT OR IGNORE INTO historical_events VALUES(?1,?2)",
                params![tenant, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Commit the dedupe marker and all messages before acknowledging the homeserver.
    pub fn ingest(&self, tenant: &str, txn: &str, events: &[Inbound]) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO transactions VALUES (?1,?2)",
            params![tenant, txn],
        )?;
        if inserted == 0 {
            return Ok(());
        }
        for event in events {
            let historical: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM historical_events WHERE tenant=?1 AND event_id=?2)",
                params![tenant, event.event_id],
                |row| row.get(0),
            )?;
            if historical {
                continue;
            }
            let aad = crate::stable_id(&[tenant, &event.event_id]);
            tx.execute(
                "INSERT OR IGNORE INTO event_nonces VALUES(?1,?2,?3,?4)",
                params![tenant, aad, event.event_id, event.room],
            )?;
            let payload = self.encrypt(&aad, &serde_json::to_vec(event)?)?;
            tx.execute(
                "INSERT OR IGNORE INTO inbox(tenant,event_id,room,payload) VALUES (?1,?2,?3,?4)",
                params![tenant, event.event_id, event.room, payload],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn next(&self, tenant: &str, now: i64) -> Result<Option<Pending>> {
        let db = self.db.lock().unwrap();
        let row = db.query_row("SELECT id,event_id,payload,attempts,next_at FROM inbox WHERE tenant=?1 AND state='pending' ORDER BY id LIMIT 1", [tenant], |r| {
            Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,u32>(3)?,r.get::<_,i64>(4)?))
        }).optional()?;
        let Some((id, event_id, payload, attempts, next_at)) = row else {
            return Ok(None);
        };
        if next_at > now {
            return Ok(None);
        }
        let event = serde_json::from_slice(
            &self.decrypt(&crate::stable_id(&[tenant, &event_id]), &payload)?,
        )?;
        Ok(Some(Pending {
            id,
            event,
            attempts,
        }))
    }
    pub fn complete(&self, tenant: &str, id: i64) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE inbox SET state='sent',payload=NULL,error=NULL WHERE tenant=?1 AND id=?2",
            params![tenant, id],
        )?;
        Ok(())
    }
    pub fn retry(&self, tenant: &str, id: i64, attempts: u32, now: i64) -> Result<()> {
        let delay = (1_i64 << attempts.min(8)).min(300);
        self.db.lock().unwrap().execute("UPDATE inbox SET attempts=attempts+1,next_at=?3,error='upstream delivery failed' WHERE tenant=?1 AND id=?2", params![tenant,id,now+delay])?;
        Ok(())
    }
    pub fn pending_count(&self) -> Result<i64> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT count(*) FROM inbox WHERE state='pending'",
            [],
            |r| r.get(0),
        )?)
    }
}
