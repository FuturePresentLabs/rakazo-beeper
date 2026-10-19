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
#[derive(Serialize, Deserialize)]
pub struct Inbound {
    pub event_id: String,
    pub room: String,
    pub text: String,
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
        Ok(store)
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
            let aad = crate::stable_id(&[tenant, &event.event_id]);
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
