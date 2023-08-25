use std::io;
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::Future;
use rand::{thread_rng, Rng, RngCore};
use redis::Value::Okay;
use redis::{Client, IntoConnectionInfo, RedisResult, Value};

const DEFAULT_RETRY_COUNT: u32 = 3;
const DEFAULT_RETRY_DELAY: u32 = 200;
const CLOCK_DRIFT_FACTOR: f32 = 0.01;
const UNLOCK_SCRIPT: &str = r#"
if redis.call("GET", KEYS[1]) == ARGV[1] then
  return redis.call("DEL", KEYS[1])
else
  return 0
end
"#;
const EXTEND_SCRIPT: &str = r#"
if redis.call("get", KEYS[1]) ~= ARGV[1] then
  return 0
else
  if redis.call("set", KEYS[1], ARGV[1], "PX", ARGV[2]) ~= nil then
    return 1
  else
    return 0
  end
end
"#;

#[derive(Debug)]
pub enum LockError {
    Io(io::Error),
    Redis(redis::RedisError),
    Unavailable,
}

/// The lock manager.
///
/// Implements the necessary functionality to acquire and release locks
/// and handles the Redis connections.
#[derive(Debug, Clone)]
pub struct LockManager {
    /// List of all Redis clients
    pub servers: Vec<Client>,
    quorum: u32,
    retry_count: u32,
    retry_delay: u32,
}

#[derive(Debug, Clone)]
pub struct Lock<'a> {
    /// The resource to lock. Will be used as the key in Redis.
    pub resource: Vec<u8>,
    /// The value for this lock.
    pub val: Vec<u8>,
    /// Time the lock is still valid.
    /// Should only be slightly smaller than the requested TTL.
    pub validity_time: usize,
    /// Used to limit the lifetime of a lock to its lock manager.
    pub lock_manager: &'a LockManager,
}

#[cfg(not(feature = "tokio-comp"))]
#[derive(Debug, Clone)]
pub struct LockGuard<'a> {
    pub lock: Lock<'a>,
}


// Dropping this guard inside the context of a tokio runtime if tokio-comp is enabled 
// will block the tokio runtime. 
// Because of this, the guard is not compiled if tokio-comp is enabled. 
#[cfg(not(feature = "tokio-comp"))]
impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        futures::executor::block_on(self.lock.lock_manager.unlock(&self.lock));
    }
}

impl LockManager {
    /// Create a new lock manager instance, defined by the given Redis connection uris.
    /// Quorum is defined to be N/2+1, with N being the number of given Redis instances.
    ///
    /// Sample URI: `"redis://127.0.0.1:6379"`
    pub fn new<T: AsRef<str> + IntoConnectionInfo>(uris: Vec<T>) -> LockManager {
        let quorum = (uris.len() as u32) / 2 + 1;

        let servers: Vec<Client> = uris
            .into_iter()
            .map(|uri| Client::open(uri).unwrap())
            .collect();

        LockManager {
            servers,
            quorum,
            retry_count: DEFAULT_RETRY_COUNT,
            retry_delay: DEFAULT_RETRY_DELAY,
        }
    }

    /// Get 20 random bytes from the pseudorandom interface.
    pub fn get_unique_lock_id(&self) -> io::Result<Vec<u8>> {
        || -> Result<Vec<u8>, io::Error> {
            let mut buf = [0u8; 20];
            thread_rng().fill_bytes(&mut buf);
            Ok(buf.to_vec())
        }()
    }

    /// Set retry count and retry delay.
    ///
    /// Retry count defaults to `3`.
    /// Retry delay defaults to `200`.
    pub fn set_retry(&mut self, count: u32, delay: u32) {
        self.retry_count = count;
        self.retry_delay = delay;
    }

    async fn lock_instance(
        client: &redis::Client,
        resource: &[u8],
        val: Vec<u8>,
        ttl: usize,
    ) -> bool {
        let mut con = match client.get_async_connection().await {
            Err(_) => return false,
            Ok(val) => val,
        };
        let result: RedisResult<Value> = redis::cmd("SET")
            .arg(resource)
            .arg(val)
            .arg("NX")
            .arg("PX")
            .arg(ttl)
            .query_async(&mut con)
            .await;

        match result {
            Ok(Okay) => true,
            Ok(_) | Err(_) => false,
        }
    }

    async fn extend_lock_instance(
        client: &redis::Client,
        resource: &[u8],
        val: &[u8],
        ttl: usize,
    ) -> bool {
        let mut con = match client.get_async_connection().await {
            Err(_) => return false,
            Ok(val) => val,
        };
        let script = redis::Script::new(EXTEND_SCRIPT);
        let result: RedisResult<i32> = script
            .key(resource)
            .arg(val)
            .arg(ttl)
            .invoke_async(&mut con)
            .await;
        match result {
            Ok(val) => val == 1,
            Err(_) => false,
        }
    }

    async fn unlock_instance(client: &redis::Client, resource: &[u8], val: &[u8]) -> bool {
        let mut con = match client.get_async_connection().await {
            Err(_) => return false,
            Ok(val) => val,
        };
        let script = redis::Script::new(UNLOCK_SCRIPT);
        let result: RedisResult<i32> = script.key(resource).arg(val).invoke_async(&mut con).await;
        match result {
            Ok(val) => val == 1,
            Err(_) => false,
        }
    }

    // Can be used for creating or extending a lock
    async fn exec_or_retry<'a, T, Fut>(
        &'a self,
        resource: &[u8],
        value: &[u8],
        ttl: usize,
        lock: T,
    ) -> Result<Lock<'a>, LockError>
    where
        T: Fn(&'a Client) -> Fut,
        Fut: Future<Output = bool>,
    {
        for _ in 0..self.retry_count {
            let start_time = Instant::now();
            let n = join_all(self.servers.iter().map(&lock))
                .await
                .into_iter()
                .fold(0, |count, locked| if locked { count + 1 } else { count });

            let drift = (ttl as f32 * CLOCK_DRIFT_FACTOR) as usize + 2;
            let elapsed = start_time.elapsed();
            let validity_time = ttl
                - drift
                - elapsed.as_secs() as usize * 1000
                - elapsed.subsec_nanos() as usize / 1_000_000;

            if n >= self.quorum && validity_time > 0 {
                return Ok(Lock {
                    lock_manager: self,
                    resource: resource.to_vec(),
                    val: value.to_vec(),
                    validity_time,
                });
            } else {
                join_all(
                    self.servers
                        .iter()
                        .map(|client| Self::unlock_instance(client, resource, value)),
                )
                .await;
            }

            let n = thread_rng().gen_range(0..self.retry_delay);
            tokio::time::sleep(Duration::from_millis(u64::from(n))).await
        }

        Err(LockError::Unavailable)
    }

    /// Unlock the given lock.
    ///
    /// Unlock is best effort. It will simply try to contact all instances
    /// and remove the key.
    pub async fn unlock(&self, lock: &Lock<'_>) {
        join_all(
            self.servers
                .iter()
                .map(|client| Self::unlock_instance(client, &lock.resource, &lock.val)),
        )
        .await;
    }

    /// Acquire the lock for the given resource and the requested TTL.
    ///
    /// If it succeeds, a `Lock` instance is returned,
    /// including the value and the validity time
    ///
    /// If it fails. `None` is returned.
    /// A user should retry after a short wait time.
    pub async fn lock<'a>(&'a self, resource: &[u8], ttl: usize) -> Result<Lock<'a>, LockError> {
        let val = self.get_unique_lock_id().unwrap();

        self.exec_or_retry(resource, &val.clone(), ttl, move |client| {
            Self::lock_instance(client, resource, val.clone(), ttl)
        })
        .await
    }

    /// Loops until the lock is acquired.
    ///
    /// The lock is placed in a guard that will unlock the lock when the guard is dropped.
    #[cfg(not(feature = "tokio-comp"))]
    pub async fn acquire<'a>(&'a self, resource: &[u8], ttl: usize) -> LockGuard<'a> {
        let lock = self.acquire_no_guard(resource, ttl).await;
        LockGuard{lock}
    }

    /// Loops until the lock is acquired.
    pub async fn acquire_no_guard<'a>(&'a self, resource: &[u8], ttl: usize) -> Lock<'a> {
        loop {
            if let Ok(lock) = self.lock(resource, ttl).await {
                return lock;
            }
        }
    }

    /// Extend the given lock by given time in milliseconds
    pub async fn extend<'a>(&'a self, lock: &Lock<'a>, ttl: usize) -> Result<Lock<'a>, LockError> {
        self.exec_or_retry(&lock.resource, &lock.val, ttl, move |client| {
            Self::extend_lock_instance(client, &lock.resource, &lock.val, ttl)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use once_cell::sync::Lazy;
    use testcontainers::clients::Cli;
    use testcontainers::images::redis::Redis;
    use testcontainers::{Container, RunnableImage};

    use super::*;

    type Containers = Vec<Container<'static, Redis>>;

    static DOCKER: Lazy<Cli> = Lazy::new(Cli::docker);

    fn is_normal<T: Sized + Send + Sync + Unpin>() {}

    fn create_clients() -> (Containers, Vec<String>) {
        let containers: Containers = (1..=3)
            .map(|_| {
                let image = RunnableImage::from(Redis::default()).with_tag("7-alpine");
                DOCKER.run(image)
            })
            .collect();

        let addresses = containers
            .iter()
            .map(|node| format!("redis://localhost:{}", node.get_host_port_ipv4(6379)))
            .collect();

        (containers, addresses)
    }

    // Test that the LockManager is Send + Sync
    #[test]
    fn test_is_normal() {
        is_normal::<LockManager>();
        is_normal::<LockError>();
        is_normal::<Lock>();
        #[cfg(not(feature = "tokio-comp"))]
        is_normal::<LockGuard>();
    }

    #[tokio::test]
    async fn test_lock_get_unique_id() -> Result<()> {
        let rl = LockManager::new(Vec::<String>::new());
        assert_eq!(rl.get_unique_lock_id()?.len(), 20);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_get_unique_id_uniqueness() -> Result<()> {
        let rl = LockManager::new(Vec::<String>::new());

        let id1 = rl.get_unique_lock_id()?;
        let id2 = rl.get_unique_lock_id()?;

        assert_eq!(20, id1.len());
        assert_eq!(20, id2.len());
        assert_ne!(id1, id2);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_valid_instance() {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());

        assert_eq!(3, rl.servers.len());
        assert_eq!(2, rl.quorum);
    }

    #[tokio::test]
    async fn test_lock_direct_unlock_fails() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        assert!(!rl.unlock_instance(&rl.servers[0], &key, &val).await);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_direct_unlock_succeeds() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;
        redis::cmd("SET").arg(&*key).arg(&*val).execute(&mut con);

        assert!(rl.unlock_instance(&rl.servers[0], &key, &val).await);

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_direct_lock_succeeds() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;

        redis::cmd("DEL").arg(&*key).execute(&mut con);
        assert!(
            rl.lock_instance(&rl.servers[0], &*key, val.clone(), 1000)
                .await
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_unlock() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        let val = rl.get_unique_lock_id()?;
        let mut con = rl.servers[0].get_connection()?;
        let _: () = redis::cmd("SET")
            .arg(&*key)
            .arg(&*val)
            .query(&mut con)
            .unwrap();

        let lock = Lock {
            lock_manager: &rl,
            resource: key,
            val,
            validity_time: 0,
        };

        rl.unlock(&lock).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_lock() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());

        let key = rl.get_unique_lock_id()?;
        match rl.lock(&key, 1000).await {
            Ok(lock) => {
                assert_eq!(key, lock.resource);
                assert_eq!(20, lock.val.len());
                assert!(lock.validity_time > 900);
                assert!(
                    lock.validity_time > 900,
                    "validity time: {}",
                    lock.validity_time
                );
            }
            Err(e) => panic!("{:?}", e),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_lock_lock_unlock() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());

        let key = rl.get_unique_lock_id()?;

        let lock = rl.lock(&key, 1000).await.unwrap();
        assert!(
            lock.validity_time > 900,
            "validity time: {}",
            lock.validity_time
        );

        if let Ok(_l) = rl2.lock(&key, 1000).await {
            panic!("Lock acquired, even though it should be locked")
        }

        rl.unlock(&lock).await;

        match rl2.lock(&key, 1000).await {
            Ok(l) => assert!(l.validity_time > 900),
            Err(_) => panic!("Lock couldn't be acquired"),
        }

        Ok(())
    }

    #[cfg(not(feature = "tokio-comp"))]
    #[tokio::test]
    async fn test_lock_lock_unlock_raii() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());
        let key = rl.get_unique_lock_id()?;

        async {
            let lock_guard = rl.acquire(&key, 1000).await;
            let lock = &lock_guard.lock;
            assert!(
                lock.validity_time > 900,
                "validity time: {}",
                lock.validity_time
            );

            if let Ok(_l) = rl2.lock(&key, 1000).await {
                panic!("Lock acquired, even though it should be locked")
            }
        }
        .await;

        match rl2.lock(&key, 1000).await {
            Ok(l) => assert!(l.validity_time > 900),
            Err(_) => panic!("Lock couldn't be acquired"),
        }

        Ok(())
    }

    #[cfg(not(feature = "tokio-comp"))]
    #[tokio::test]
    async fn test_lock_extend_lock() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl1 = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());

        let key = rl1.get_unique_lock_id()?;

        async {
            let lock1 = rl1.acquire(&key, 1000).await;

            // Wait half a second before locking again
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            rl1.extend(&lock1.lock, 1000).await.unwrap();

            // Wait another half a second to see if lock2 can unlock
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            // Assert lock2 can't access after extended lock
            match rl2.lock(&key, 1000).await {
                Ok(_) => panic!("Expected an error when extending the lock but didn't receive one"),
                Err(e) => match e {
                    LockError::Unavailable => (),
                    _ => panic!("Unexpected error when extending lock"),
                },
            }
        }
        .await;

        Ok(())
    }

    #[cfg(not(feature = "tokio-comp"))]
    #[tokio::test]
    async fn test_lock_extend_lock_releases() -> Result<()> {
        let (_containers, addresses) = create_clients();

        let rl1 = LockManager::new(addresses.clone());
        let rl2 = LockManager::new(addresses.clone());

        let key = rl1.get_unique_lock_id()?;

        async {
            // Create 500ms lock and immediately extend 500ms
            let lock1 = rl1.acquire(&key, 500).await;
            rl1.extend(&lock1.lock, 500).await.unwrap();

            // Wait one second for the lock to expire
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            // Assert rl2 can lock with the key now
            match rl2.lock(&key, 1000).await {
                Err(_) => {
                    panic!("Unexpected error when trying to claim free lock after extend expired")
                }
                _ => (),
            }

            // Also assert rl1 can't reuse lock1
            match rl1.extend(&lock1.lock, 1000).await {
                Ok(_) => panic!("Did not expect OK() when re-extending rl1"),
                Err(e) => match e {
                    LockError::Unavailable => (),
                    _ => panic!("Expected lockError::Unavailable when re-extending rl1"),
                },
            }
        }
        .await;

        Ok(())
    }
}
