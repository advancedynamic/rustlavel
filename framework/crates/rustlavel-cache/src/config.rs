//! Cache configuration and the driver factory.
//!
//! An application picks a driver in `config/cache.json` (or `.env`), never in
//! code, so the same binary runs on a laptop with the memory driver and in
//! production against Redis:
//!
//! ```json
//! {
//!   "driver": "${CACHE_DRIVER:memory}",
//!   "path":   "storage/cache",
//!   "url":    "${REDIS_URL}",
//!   "prefix": "${APP_NAME:rustlavel}:"
//! }
//! ```

use crate::file::FileStore;
use crate::memory::MemoryStore;
use crate::redis::{RedisConfig, RedisStore};
use crate::store::Cache;
use rustlavel_core::{Config, Error, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// Which backend to build, and what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Driver {
    /// Process-local. Fast, and lost on restart — and *not* shared between
    /// workers, which matters for rate limiting.
    Memory,
    /// One file per key under [`CacheConfig::path`].
    File,
    /// A Redis server reached over RESP.
    Redis,
}

impl Driver {
    /// Parse a driver name, listing the alternatives when it is not one.
    ///
    /// A typo in `.env` is one of the most common ways to lose an afternoon, so
    /// this refuses rather than silently falling back to memory.
    pub fn parse(name: &str) -> Result<Driver> {
        match name.trim().to_ascii_lowercase().as_str() {
            "memory" | "array" => Ok(Driver::Memory),
            "file" => Ok(Driver::File),
            "redis" => Ok(Driver::Redis),
            other => Err(Error::msg(format!(
                "`{other}` is not a cache driver. Set cache.driver to one of: memory, file, redis."
            ))),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Driver::Memory => "memory",
            Driver::File => "file",
            Driver::Redis => "redis",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub driver: Driver,
    /// Where the file driver keeps its entries.
    pub path: PathBuf,
    /// The Redis URL, empty to fall back to `REDIS_URL`.
    pub url: String,
    /// Prepended to every key. Two applications sharing one Redis need this;
    /// note that [`Cache::flush`] on Redis still empties the whole database.
    pub prefix: String,
    /// How often the memory driver sweeps expired entries.
    pub sweep_interval: std::time::Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            driver: Driver::Memory,
            // `storage/cache` is the directory `rustlavel new --with cache`
            // creates. The default used to be `storage/framework/cache`, which
            // is Laravel's path and not this scaffold's, so the file driver
            // wrote into a directory nothing had made.
            path: PathBuf::from("storage/cache"),
            url: String::new(),
            prefix: String::new(),
            sweep_interval: std::time::Duration::from_secs(60),
        }
    }
}

impl CacheConfig {
    /// Read `cache.driver`, `cache.path`, `cache.url` and `cache.prefix`.
    pub fn from_app_config(config: &Config) -> Result<Self> {
        Ok(CacheConfig {
            driver: Driver::parse(&config.string("cache.driver", "memory"))?,
            path: PathBuf::from(config.string("cache.path", "storage/cache")),
            url: config.string("cache.url", ""),
            prefix: config.string("cache.prefix", ""),
            ..CacheConfig::default()
        })
    }
}

/// The application's handle on the cache.
///
/// Holds an `Arc<dyn Cache>` so the driver is a boot-time decision, and is
/// itself a [`Cache`], so a handler can call `cache.remember(...)` on it
/// directly without unwrapping anything.
#[derive(Clone)]
pub struct CacheStore {
    inner: Arc<dyn Cache>,
}

impl CacheStore {
    /// Build the driver named in the application configuration.
    ///
    /// Deliberately synchronous and non-connecting: an application must boot
    /// even when Redis is momentarily down. Call [`CacheStore::verify`] when
    /// failing fast is what you want instead.
    pub fn from_config(config: &Config) -> Result<Self> {
        CacheStore::build(&CacheConfig::from_app_config(config)?)
    }

    pub fn build(settings: &CacheConfig) -> Result<Self> {
        let store: Arc<dyn Cache> = match settings.driver {
            Driver::Memory => Arc::new(MemoryStore::with_options(
                settings.prefix.clone(),
                settings.sweep_interval,
            )),
            Driver::File => {
                Arc::new(FileStore::with_prefix(&settings.path, settings.prefix.clone())?)
            }
            Driver::Redis => {
                let redis = if settings.url.is_empty() {
                    RedisConfig::from_app_config(&Config::new())?
                } else {
                    RedisConfig::from_url(&settings.url)?
                };
                Arc::new(RedisStore::new(redis, settings.prefix.clone()))
            }
        };

        Ok(CacheStore { inner: store })
    }

    /// Wrap a driver that was built by hand.
    pub fn from_driver(store: impl Cache) -> Self {
        CacheStore { inner: Arc::new(store) }
    }

    /// The underlying driver, for a caller that needs `Arc<dyn Cache>`.
    pub fn driver_handle(&self) -> Arc<dyn Cache> {
        Arc::clone(&self.inner)
    }

    /// Prove the store actually works, for a `doctor` command or a boot check.
    pub async fn verify(&self) -> Result<()> {
        let key = "__rustlavel_cache_probe";
        self.put(key, rustlavel_core::Json::from(1), std::time::Duration::from_secs(5)).await?;
        self.forget(key).await?;
        Ok(())
    }
}

/// Delegation, so `CacheStore` is usable everywhere a `Cache` is — including
/// picking up every default method and all of `CacheExt`.
impl Cache for CacheStore {
    fn driver(&self) -> &'static str {
        self.inner.driver()
    }

    fn get<'a>(&'a self, key: &'a str) -> crate::store::BoxFuture<'a, Result<Option<rustlavel_core::Json>>> {
        self.inner.get(key)
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        value: rustlavel_core::Json,
        ttl: std::time::Duration,
    ) -> crate::store::BoxFuture<'a, Result<()>> {
        self.inner.put(key, value, ttl)
    }

    fn forever<'a>(
        &'a self,
        key: &'a str,
        value: rustlavel_core::Json,
    ) -> crate::store::BoxFuture<'a, Result<()>> {
        self.inner.forever(key, value)
    }

    fn forget<'a>(&'a self, key: &'a str) -> crate::store::BoxFuture<'a, Result<bool>> {
        self.inner.forget(key)
    }

    fn flush(&self) -> crate::store::BoxFuture<'_, Result<()>> {
        self.inner.flush()
    }

    fn has<'a>(&'a self, key: &'a str) -> crate::store::BoxFuture<'a, Result<bool>> {
        self.inner.has(key)
    }

    fn increment<'a>(&'a self, key: &'a str, by: i64) -> crate::store::BoxFuture<'a, Result<i64>> {
        self.inner.increment(key, by)
    }

    fn decrement<'a>(&'a self, key: &'a str, by: i64) -> crate::store::BoxFuture<'a, Result<i64>> {
        self.inner.decrement(key, by)
    }

    fn increment_within<'a>(
        &'a self,
        key: &'a str,
        by: i64,
        ttl: std::time::Duration,
    ) -> crate::store::BoxFuture<'a, Result<i64>> {
        self.inner.increment_within(key, by, ttl)
    }

    fn ttl<'a>(&'a self, key: &'a str) -> crate::store::BoxFuture<'a, Result<Option<std::time::Duration>>> {
        self.inner.ttl(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CacheExt;
    use rustlavel_core::Json;
    use std::time::Duration;

    #[test]
    fn a_driver_typo_names_the_valid_choices() {
        let error = Driver::parse("redsi").unwrap_err().to_string();
        assert!(error.contains("memory, file, redis"), "got: {error}");
    }

    #[test]
    fn driver_names_are_case_insensitive_and_array_means_memory() {
        assert_eq!(Driver::parse("Redis").unwrap(), Driver::Redis);
        assert_eq!(Driver::parse(" file ").unwrap(), Driver::File);
        // Laravel calls the in-process driver `array`; accept both names.
        assert_eq!(Driver::parse("array").unwrap(), Driver::Memory);
    }

    #[tokio::test]
    async fn the_factory_defaults_to_the_memory_driver() {
        let store = CacheStore::from_config(&Config::new()).unwrap();

        assert_eq!(store.driver(), "memory");
        store.forever("k", Json::from(1)).await.unwrap();
        assert_eq!(store.get("k").await.unwrap(), Some(Json::from(1)));
    }

    #[tokio::test]
    async fn the_factory_builds_the_file_driver_at_the_configured_path() {
        let directory = std::env::temp_dir()
            .join(format!("rustlavel-cache-factory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);

        let config = Config::new();
        config.set("cache.driver", "file");
        config.set("cache.path", directory.to_string_lossy().to_string());

        let store = CacheStore::from_config(&config).unwrap();
        assert_eq!(store.driver(), "file");

        store.forever("on-disk", Json::from("yes")).await.unwrap();
        assert!(directory.exists());
        assert_eq!(store.get("on-disk").await.unwrap(), Some(Json::from("yes")));

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_factory_builds_the_redis_driver_without_connecting() {
        let config = Config::new();
        config.set("cache.driver", "redis");
        config.set("cache.url", "redis://127.0.0.1:1/0");

        // No await, no server: building must not touch the network, or an
        // application could not boot while Redis restarts.
        let store = CacheStore::from_config(&config).unwrap();
        assert_eq!(store.driver(), "redis");
    }

    #[test]
    fn a_malformed_redis_url_is_refused_at_boot() {
        let config = Config::new();
        config.set("cache.driver", "redis");
        config.set("cache.url", "http://not-redis");

        assert!(CacheStore::from_config(&config).is_err());
    }

    #[tokio::test]
    async fn the_configured_prefix_reaches_the_driver() {
        let config = Config::new();
        config.set("cache.prefix", "tenant-a:");
        let prefixed = CacheStore::from_config(&config).unwrap();

        let bare = CacheStore::from_driver(MemoryStore::new());

        prefixed.forever("who", Json::from("a")).await.unwrap();
        // A different store with no prefix must not see the prefixed key.
        assert_eq!(bare.get("who").await.unwrap(), None);
        assert_eq!(prefixed.get("who").await.unwrap(), Some(Json::from("a")));
    }

    #[tokio::test]
    async fn a_store_handle_supports_the_full_cache_api_including_remember() {
        let store = CacheStore::from_driver(MemoryStore::new());

        let value = store
            .remember("expensive", Duration::from_secs(60), || async { Ok(Json::from(99)) })
            .await
            .unwrap();

        assert_eq!(value, Json::from(99));
        assert_eq!(store.pull("expensive").await.unwrap(), Some(Json::from(99)));
        assert!(!store.has("expensive").await.unwrap());
        store.verify().await.unwrap();
    }
}
