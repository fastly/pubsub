use fastly::kv_store::{InsertMode, KVStoreError, LookupResponse};
use fastly::KVStore;
use std::time::Duration;

// the amount of time to wait before deleting an item after its expiration
// is reached. keeping expired items around allows their sequencing
// information to be reused if they are later updated prior to deletion.
// this helps reduce the chance of sequences restarting, which can be
// disruptive to message delivery.
const LINGER: Duration = Duration::from_secs(60 * 60 * 24);

const WRITE_TRIES_MAX: usize = 5;

#[derive(Debug)]
pub enum StorageError {
    StoreNotFound,
    TooManyRequests,
    InvalidMetadata,
    KVStore(KVStoreError),
}

#[derive(Copy, Clone)]
pub struct RetainedVersion {
    pub generation: u64,
    pub seq: u64,
}

pub struct RetainedMessage {
    pub ttl: Option<Duration>,
    pub data: Vec<u8>,
}

pub struct RetainedSlot {
    pub version: RetainedVersion,
    pub message: Option<RetainedMessage>,
}

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
struct Metadata {
    generation: u64,
    seq: u64,

    #[serde(rename = "expires-at", skip_serializing_if = "Option::is_none")]
    expires_at: Option<time::UtcDateTime>,
}

fn lookup(
    store: &KVStore,
    key_name: &str,
) -> Result<Option<(LookupResponse, Metadata)>, StorageError> {
    let lookup = match store.lookup(key_name) {
        Ok(l) => l,
        Err(KVStoreError::ItemNotFound) => return Ok(None),
        Err(e) => return Err(StorageError::KVStore(e)),
    };

    let meta = match lookup.metadata() {
        Some(data) => match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => return Err(StorageError::InvalidMetadata),
        },
        None => return Err(StorageError::InvalidMetadata),
    };

    Ok(Some((lookup, meta)))
}

pub trait Storage {
    fn write_retained(
        &self,
        topic: &str,
        message: &[u8],
        ttl: Option<Duration>,
    ) -> Result<RetainedVersion, StorageError>;

    fn read_retained(
        &self,
        topic: &str,
        after: Option<RetainedVersion>,
    ) -> Result<Option<RetainedSlot>, StorageError>;
}

pub struct KVStoreStorage {
    store_name: String,
}

impl KVStoreStorage {
    pub fn new(store_name: &str) -> Self {
        Self {
            store_name: store_name.to_string(),
        }
    }
}

impl Storage for KVStoreStorage {
    fn write_retained(
        &self,
        topic: &str,
        message: &[u8],
        ttl: Option<Duration>,
    ) -> Result<RetainedVersion, StorageError> {
        let store = match KVStore::open(&self.store_name) {
            Ok(Some(store)) => store,
            Ok(None) => return Err(StorageError::StoreNotFound),
            Err(e) => return Err(StorageError::KVStore(e)),
        };

        let key_name = format!("r:{topic}");

        let expires_at = ttl.map(|ttl| time::UtcDateTime::now() + ttl);

        let mut tries = 0;

        let version = loop {
            let (mut meta, generation) = match lookup(&store, &key_name)? {
                Some((lookup, meta)) => (meta, Some(lookup.current_generation())),
                None => (Metadata::default(), None),
            };

            let insert = store.build_insert();

            let insert = if let Some(generation) = generation {
                meta.seq += 1;

                insert.if_generation_match(generation)
            } else {
                meta.generation = rand::random();
                meta.seq = 1;

                insert.mode(InsertMode::Add)
            };

            meta.expires_at = expires_at;

            let meta_json =
                serde_json::to_string(&meta).expect("metadata should always be serializable");

            let insert = insert.metadata(&meta_json);

            let insert = if let Some(ttl) = ttl {
                // we set a TTL longer than the item's expiration time, to
                // allow the opportunity to reuse the item after expiration
                insert.time_to_live(ttl + LINGER)
            } else {
                insert
            };

            match insert.execute(&key_name, message.to_vec()) {
                Ok(()) => {
                    break RetainedVersion {
                        generation: meta.generation,
                        seq: meta.seq,
                    }
                }
                Err(KVStoreError::ItemPreconditionFailed) => {}
                Err(KVStoreError::TooManyRequests) => {}
                Err(e) => return Err(StorageError::KVStore(e)),
            }

            tries += 1;

            if tries >= WRITE_TRIES_MAX {
                // getting conflicts or rate limit errors after several tries
                return Err(StorageError::TooManyRequests);
            }
        };

        Ok(version)
    }

    fn read_retained(
        &self,
        topic: &str,
        after: Option<RetainedVersion>,
    ) -> Result<Option<RetainedSlot>, StorageError> {
        let store = match KVStore::open(&self.store_name) {
            Ok(Some(store)) => store,
            Ok(None) => return Err(StorageError::StoreNotFound),
            Err(e) => return Err(StorageError::KVStore(e)),
        };

        let key_name = format!("r:{topic}");

        let (mut lookup, meta) = match lookup(&store, &key_name)? {
            Some(ret) => ret,
            None => return Ok(None),
        };

        if let Some(after) = after {
            if meta.generation == after.generation && meta.seq <= after.seq {
                return Ok(None);
            }
        }

        let version = RetainedVersion {
            generation: meta.generation,
            seq: meta.seq,
        };

        let ttl = meta.expires_at.map(|expires_at| {
            let now = time::UtcDateTime::now();

            if now < expires_at {
                (expires_at - now).unsigned_abs()
            } else {
                Duration::from_millis(0)
            }
        });

        let message = if ttl != Some(Duration::from_millis(0)) {
            let value = lookup.take_body_bytes();

            Some(RetainedMessage { ttl, data: value })
        } else {
            None
        };

        Ok(Some(RetainedSlot { version, message }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str;

    #[test]
    fn retained() {
        let storage = KVStoreStorage::new("messages");

        assert!(storage
            .read_retained("storage-test", None)
            .unwrap()
            .is_none());

        let v1 = storage
            .write_retained("storage-test", "hello".as_bytes(), None)
            .unwrap();
        assert_eq!(v1.seq, 1);

        let s = storage
            .read_retained("storage-test", None)
            .unwrap()
            .unwrap();
        assert_eq!(s.version.generation, v1.generation);
        assert_eq!(s.version.seq, 1);
        let m = s.message.unwrap();
        assert!(m.ttl.is_none());
        assert_eq!(str::from_utf8(&m.data).unwrap(), "hello");

        let v2 = storage
            .write_retained(
                "storage-test",
                "world".as_bytes(),
                Some(Duration::from_secs(60)),
            )
            .unwrap();
        assert_eq!(v2.generation, v1.generation);
        assert_eq!(v2.seq, 2);

        let s = storage
            .read_retained("storage-test", None)
            .unwrap()
            .unwrap();
        assert_eq!(s.version.generation, v2.generation);
        assert_eq!(s.version.seq, 2);
        let m = s.message.unwrap();
        let ttl = m.ttl.unwrap();
        assert!(ttl <= Duration::from_secs(60));
        assert_eq!(str::from_utf8(&m.data).unwrap(), "world");

        // none after
        assert!(storage
            .read_retained("storage-test", Some(s.version))
            .unwrap()
            .is_none());

        // delete item so next write gets a new generation
        KVStore::open(&storage.store_name)
            .unwrap()
            .unwrap()
            .delete("r:storage-test")
            .unwrap();

        let new_v1 = storage
            .write_retained("storage-test", "hello".as_bytes(), None)
            .unwrap();
        assert!(new_v1.generation != v1.generation);
        assert_eq!(new_v1.seq, 1);
    }
}
