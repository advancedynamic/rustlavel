//! The registry: defining flags, checking them, and overriding the answer.
//!
//! ```
//! use rustlavel_flags::{Flags, Scope};
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let flags = Flags::new()
//!     .define("new-checkout", |scope| async move { scope.id().ends_with('7') })
//!     .define("beta-search", |_| async { false })
//!     .rollout("dark-mode", 25);
//!
//! assert!(flags.active("new-checkout", &Scope::user_id(17)).await.unwrap());
//! assert!(flags.inactive("beta-search", &Scope::user_id(17)).await.unwrap());
//!
//! // ...except for this one customer, who asked.
//! flags.activate_for("beta-search", &Scope::user_id(17)).await.unwrap();
//! assert!(flags.active("beta-search", &Scope::user_id(17)).await.unwrap());
//! # });
//! ```

use crate::rollout;
use crate::scope::Scope;
use crate::store::{FlagStore, MemoryStore};
use rustlavel_core::{Config, Error, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// What a resolver hands back once it has been boxed.
type ResolverFuture = Pin<Box<dyn Future<Output = Result<bool>> + Send + 'static>>;

/// A defined flag's code, ready to be called with a scope.
type Resolver = Arc<dyn Fn(Scope) -> ResolverFuture + Send + Sync>;

/// What a resolver may return.
///
/// A resolver that just looks at the scope returns `bool`, and one that has to
/// go and ask a database returns `Result<bool>` — both are written the obvious
/// way and this trait is what lets [`Flags::define`] take either. There is no
/// third implementation and no reason for one: a flag is on or off, or the
/// question could not be answered.
pub trait FlagAnswer {
    fn into_answer(self) -> Result<bool>;
}

impl FlagAnswer for bool {
    fn into_answer(self) -> Result<bool> {
        Ok(self)
    }
}

impl FlagAnswer for Result<bool> {
    fn into_answer(self) -> Result<bool> {
        self
    }
}

/// Every flag the application knows about, and the store behind them.
///
/// Cheap to clone — the definitions and the store are behind `Arc` — so it goes
/// into application state once and every request gets a handle to the same one.
///
/// # Precedence
///
/// Five things can have an opinion about one flag, and the question this type
/// exists to answer is "why is this on for this user?". So the answer is one
/// short list, applied in order:
///
/// 1. **`flags.off` in configuration** — off, for everybody, full stop. This is
///    the incident switch and nothing below it can reopen the flag.
/// 2. **A stored override of `false`**, for this scope or globally — off.
/// 3. **A stored override of `true`**, for this scope or globally — on.
/// 4. **`flags.on` in configuration** — on.
/// 5. **The resolver**, if the flag was defined.
/// 6. Otherwise off. A flag nobody defined is not a flag.
///
/// Two of those deserve their reasons written down.
///
/// **Off beats on among stored overrides**, at every level, and a *global* off
/// therefore beats a per-scope on. This is the same rule
/// [`rustlavel_rbac`](https://docs.rs/rustlavel-rbac) applies to a deny, for
/// the same reason: an off switch that something else can quietly overrule is
/// not a switch, and the moment somebody reaches for one is the moment they
/// cannot afford to audit what else might be set. The cost is real and worth
/// knowing — you cannot exempt one customer from a global off, you have to
/// clear the global off first — and it is the cheaper half of the trade.
///
/// **Configuration outranks the store in both directions.** `flags.off` is read
/// from the environment, which means it survives a database being down, a
/// store being unreachable, and a cache that has not caught up. That is exactly
/// the situation you reach for it in.
#[derive(Clone)]
pub struct Flags {
    definitions: Arc<HashMap<String, Resolver>>,
    store: Arc<dyn FlagStore>,
    forced_on: Arc<BTreeSet<String>>,
    forced_off: Arc<BTreeSet<String>>,
}

impl Default for Flags {
    fn default() -> Self {
        Flags::new()
    }
}

impl Flags {
    /// An empty registry over an in-memory store.
    pub fn new() -> Self {
        Flags {
            definitions: Arc::new(HashMap::new()),
            store: Arc::new(MemoryStore::new()),
            forced_on: Arc::new(BTreeSet::new()),
            forced_off: Arc::new(BTreeSet::new()),
        }
    }

    /// An empty registry, with `flags.on` and `flags.off` already applied.
    ///
    /// Define the flags on top of it: `Flags::from_config(&config).define(...)`.
    pub fn from_config(config: &Config) -> Self {
        Flags::new().configured(config)
    }

    /// Read `flags.on` and `flags.off` and add them to what is already forced.
    ///
    /// Both are lists, which in `.env` means the comma-separated spelling —
    /// `FLAGS_OFF=new-checkout,beta-search` — and in a configuration file may
    /// be a JSON array. [`Config::list`] accepts either.
    ///
    /// It *adds*: calling it twice, or calling it after [`Flags::force_off`],
    /// leaves both sets of names forced. There is deliberately no way to
    /// un-force a name from configuration, because the only reason to want one
    /// is to undo an incident switch, and undoing an incident switch should
    /// take an edit to the thing that set it.
    ///
    /// A name in both lists is off. It has to be — see the precedence chain on
    /// [`Flags`] — and a deployment that has contradicted itself is one where
    /// the safe reading of the contradiction is the closed one.
    pub fn configured(self, config: &Config) -> Self {
        self.force_on(config.list("flags.on")).force_off(config.list("flags.off"))
    }

    /// Put a store behind the flags: a database table, Redis, anything that
    /// implements [`FlagStore`].
    pub fn store(mut self, store: impl FlagStore) -> Self {
        self.store = Arc::new(store);
        self
    }

    /// Use a store somebody else already built and holds a handle to.
    pub fn with_store(mut self, store: Arc<dyn FlagStore>) -> Self {
        self.store = store;
        self
    }

    /// Define a flag: a name, and code that decides it for a scope.
    ///
    /// The resolver is given the [`Scope`] by value and may do anything an
    /// async function may do, including go to the database. It is called at
    /// most once per scope inside a [`ScopedFlags`] batch, so a slow resolver
    /// costs its slowness once per request rather than once per question.
    ///
    /// ```
    /// # use rustlavel_flags::{Flags, Scope};
    /// let flags = Flags::new()
    ///     .define("new-checkout", |scope: Scope| async move { scope.id().ends_with('7') })
    ///     .define("beta-search", |_| async { false });
    /// # let _ = flags;
    /// ```
    ///
    /// Defining a name twice replaces the first definition rather than
    /// complaining. That is what makes a test able to say "this application,
    /// but with `new-checkout` on" in one line.
    pub fn define<F, Fut, A>(mut self, name: impl Into<String>, resolver: F) -> Self
    where
        F: Fn(Scope) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = A> + Send + 'static,
        A: FlagAnswer,
    {
        let boxed: Resolver = Arc::new(move |scope| {
            let future = resolver(scope);
            Box::pin(async move { future.await.into_answer() })
        });

        // `make_mut` clones the map only while somebody else is holding a
        // handle to it, which during the builder chain in `main.rs` nobody is.
        Arc::make_mut(&mut self.definitions).insert(name.into(), boxed);
        self
    }

    /// Define a flag as a stable percentage rollout.
    ///
    /// Shorthand for a resolver calling [`rollout::in_rollout`]; read that
    /// module for why the bucket is hashed rather than drawn, and for what
    /// widening a rollout does and does not do to the people already in it.
    ///
    /// Checked against [`Scope::none`] a rollout is not a percentage of
    /// anything — there is one scope, so it lands in one bucket and stays
    /// there, on or off for the whole installation. Percentages need a subject.
    pub fn rollout(self, name: impl Into<String>, percent: u8) -> Self {
        let name = name.into();
        let flag = name.clone();

        self.define(name, move |scope: Scope| {
            let flag = flag.clone();
            async move { rollout::in_rollout(&flag, &scope, percent) }
        })
    }

    /// Force these flags on, below any stored override. See [`Flags::configured`].
    pub fn force_on<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Arc::make_mut(&mut self.forced_on).extend(names.into_iter().map(Into::into));
        self
    }

    /// Force these flags off, above everything. See [`Flags::configured`].
    pub fn force_off<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Arc::make_mut(&mut self.forced_off).extend(names.into_iter().map(Into::into));
        self
    }

    /// Every defined flag's name, sorted. Does not include names that only
    /// exist as a forced value or a stored override.
    pub fn defined(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.definitions.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Whether a resolver was registered under this name.
    ///
    /// Worth checking in a boot-time assertion: an undefined flag is off, and a
    /// misspelled one is indistinguishable from a feature that never shipped.
    pub fn is_defined(&self, flag: &str) -> bool {
        self.definitions.contains_key(flag)
    }

    /// The store behind the flags, for an admin route that writes overrides.
    pub fn store_handle(&self) -> &Arc<dyn FlagStore> {
        &self.store
    }

    /// Is this flag on for this scope?
    ///
    /// `Result`, and not `bool`, deliberately. "I could not find out" is a
    /// third answer — a store that is down, a resolver whose query failed — and
    /// collapsing it into `false` would silently turn a feature off for
    /// everybody the moment something unrelated broke, which is exactly the
    /// kind of outage nobody thinks to look for.
    ///
    /// This call does not remember anything. Use [`Flags::for_scope`] when a
    /// request asks more than one question.
    pub async fn active(&self, flag: &str, scope: &Scope) -> Result<bool> {
        self.resolve(flag, scope).await
    }

    /// The negation, for the reading that comes out shorter.
    pub async fn inactive(&self, flag: &str, scope: &Scope) -> Result<bool> {
        Ok(!self.active(flag, scope).await?)
    }

    /// A view fixed on one scope, which remembers what it has already resolved.
    ///
    /// This is what a request should hold. Ten checks against one
    /// [`ScopedFlags`] run each resolver at most once; ten checks against
    /// [`Flags::active`] run each of them ten times.
    pub fn for_scope(&self, scope: Scope) -> ScopedFlags {
        ScopedFlags {
            flags: self.clone(),
            scope,
            resolved: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Turn a flag on for one scope, overriding whatever the resolver says.
    pub async fn activate_for(&self, flag: &str, scope: &Scope) -> Result<()> {
        self.store.set(flag, scope, true).await
    }

    /// Turn a flag off for one scope, overriding whatever the resolver says.
    pub async fn deactivate_for(&self, flag: &str, scope: &Scope) -> Result<()> {
        self.store.set(flag, scope, false).await
    }

    /// Drop one scope's override and let the resolver decide again.
    pub async fn forget_for(&self, flag: &str, scope: &Scope) -> Result<()> {
        self.store.forget(flag, scope).await
    }

    /// Turn a flag on for everybody who has no override of their own.
    pub async fn activate(&self, flag: &str) -> Result<()> {
        self.activate_for(flag, &Scope::none()).await
    }

    /// Turn a flag off for everybody, including scopes that have an override
    /// of their own saying otherwise — see the precedence chain on [`Flags`].
    pub async fn deactivate(&self, flag: &str) -> Result<()> {
        self.deactivate_for(flag, &Scope::none()).await
    }

    /// Drop the global override. Per-scope overrides are left alone.
    pub async fn forget(&self, flag: &str) -> Result<()> {
        self.forget_for(flag, &Scope::none()).await
    }

    /// Drop every override in the store, for every flag and every scope.
    pub async fn purge(&self) -> Result<()> {
        self.store.flush().await
    }

    /// The precedence chain, in the order the doc comment on [`Flags`] lists it.
    async fn resolve(&self, flag: &str, scope: &Scope) -> Result<bool> {
        // 1. The incident switch. Read before the store is touched, so it works
        //    when the store is the thing that is broken.
        if self.forced_off.contains(flag) {
            return Ok(false);
        }

        let scoped = self.store.get(flag, scope).await?;
        // A global scope is its own global override; asking twice would be the
        // same lookup and, on a store that charges for one, the same round trip.
        let global = if scope.is_none() {
            scoped
        } else {
            self.store.get(flag, &Scope::none()).await?
        };

        // 2 and 3. An override decides it, and among overrides, off wins.
        if scoped == Some(false) || global == Some(false) {
            return Ok(false);
        }
        if scoped == Some(true) || global == Some(true) {
            return Ok(true);
        }

        // 4. Forced on by configuration.
        if self.forced_on.contains(flag) {
            return Ok(true);
        }

        // 5 and 6. The resolver, or nothing.
        match self.definitions.get(flag) {
            Some(resolver) => resolver(scope.clone()).await,
            None => Ok(false),
        }
    }
}

impl std::fmt::Debug for Flags {
    /// The names and the forced lists. Resolvers are closures and have nothing
    /// to show, but which flags exist is the thing you actually want in a log
    /// line when a check came back with an answer you did not expect.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Flags")
            .field("defined", &self.defined())
            .field("forced_on", &self.forced_on)
            .field("forced_off", &self.forced_off)
            .field("store", &self.store.name())
            .finish()
    }
}

/// [`Flags`] fixed on one scope, remembering what it has already resolved.
///
/// Build one per request — [`Flags::for_scope`], or
/// [`FlagsExt::scoped_flags`](crate::FlagsExt) inside a handler — and ask it
/// everything. Each flag is resolved at most once for as long as the view
/// lives, so a resolver that costs a query costs it once however many times the
/// page asks.
///
/// The memory is a memo, not a lock: two tasks checking the same unresolved
/// flag on one clone may both run the resolver, and one of the two answers is
/// kept. Serialising them would mean holding a lock across the resolver's
/// `await` — turning every slow resolver into a queue — to save a duplicated
/// call that produces the same answer anyway.
///
/// It is also *not* a cache with a lifetime. An override written after a
/// [`ScopedFlags`] has resolved a flag is not seen by that view; call
/// [`ScopedFlags::refresh`] or, more usually, let the request end.
#[derive(Clone)]
pub struct ScopedFlags {
    flags: Flags,
    scope: Scope,
    resolved: Arc<Mutex<HashMap<String, bool>>>,
}

impl ScopedFlags {
    /// Who this view is answering for.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The registry behind the view.
    pub fn flags(&self) -> &Flags {
        &self.flags
    }

    /// Is this flag on for the view's scope?
    pub async fn active(&self, flag: &str) -> Result<bool> {
        if let Some(answer) = self.remembered(flag) {
            return Ok(answer);
        }

        let answer = self.flags.resolve(flag, &self.scope).await?;
        self.remember(flag, answer);
        Ok(answer)
    }

    /// The negation.
    pub async fn inactive(&self, flag: &str) -> Result<bool> {
        Ok(!self.active(flag).await?)
    }

    /// Several flags at once, in one pass.
    ///
    /// The shape a template wants: hand it a map and let it ask by name,
    /// rather than threading a `bool` per feature through the view model.
    ///
    /// ```
    /// # use rustlavel_flags::{Flags, Scope};
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// let flags = Flags::new().define("a", |_| async { true }).define("b", |_| async { false });
    /// let values = flags.for_scope(Scope::user_id(41)).values(["a", "b"]).await.unwrap();
    ///
    /// assert!(values["a"]);
    /// assert!(!values["b"]);
    /// # });
    /// ```
    ///
    /// Resolved one after another rather than concurrently: a resolver that
    /// takes a database connection would otherwise take as many as there are
    /// flags on the page, all at once, which is how a feature-flag call turns
    /// into a connection-pool outage.
    pub async fn values<I, S>(&self, flags: I) -> Result<BTreeMap<String, bool>>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let names: Vec<String> = flags.into_iter().map(Into::into).collect();
        let mut answers = BTreeMap::new();

        for name in names {
            let answer = self.active(&name).await?;
            answers.insert(name, answer);
        }

        Ok(answers)
    }

    /// Every *defined* flag, for this scope.
    ///
    /// Note the word: a flag that only exists as a stored override or a
    /// `flags.on` entry is not in the list, because nothing has told this
    /// process it exists. Use it for a debugging endpoint, not as the truth
    /// about what the installation has switched on.
    pub async fn all(&self) -> Result<BTreeMap<String, bool>> {
        let names: Vec<String> = self.flags.defined().into_iter().map(str::to_string).collect();
        self.values(names).await
    }

    /// Whether any of these flags is on. Stops at the first one that is.
    pub async fn any<I, S>(&self, flags: I) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let names: Vec<String> = flags.into_iter().map(Into::into).collect();

        for name in names {
            if self.active(&name).await? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Forget what has been resolved, so the next check asks again.
    ///
    /// For the one case that needs it: code that writes an override and then
    /// wants to see its own change through the same view.
    pub fn refresh(&self) {
        self.lock().clear();
    }

    /// How many flags this view has resolved so far. Tests use it to prove the
    /// memo exists; a debugging endpoint can show it.
    pub fn resolved_count(&self) -> usize {
        self.lock().len()
    }

    fn remembered(&self, flag: &str) -> Option<bool> {
        self.lock().get(flag).copied()
    }

    fn remember(&self, flag: &str, answer: bool) {
        self.lock().insert(flag.to_string(), answer);
    }

    /// A poisoned lock means a thread panicked while holding it. What is inside
    /// is a map of answers already computed — nothing can be half-written — so
    /// it is recovered rather than turning one panic into a panic per request.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, bool>> {
        self.resolved.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl std::fmt::Debug for ScopedFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedFlags")
            .field("scope", &self.scope)
            .field("resolved", &*self.lock())
            .finish()
    }
}

/// The error a handler gets when the package was never registered.
///
/// Lives here so [`FlagsExt`](crate::FlagsExt) and the middleware say the same
/// thing, and so the sentence is written once.
pub(crate) fn missing_registry() -> Error {
    Error::msg(
        "feature flags were checked, but there is no `Flags` in application state. Register them \
         with `App::new().plugin(FeatureFlags::new(flags))`, or put one there yourself with \
         `Context::builder().state(flags)`. Refusing to guess whether the flag is on.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A registry where `new-checkout` is on for user ids ending in 7, and
    /// `beta-search` is off for everybody.
    fn flags() -> Flags {
        Flags::new()
            .define("new-checkout", |scope: Scope| async move { scope.id().ends_with('7') })
            .define("beta-search", |_| async { false })
    }

    fn config(on: &str, off: &str) -> Config {
        let config = Config::new();
        config.set("flags.on", on);
        config.set("flags.off", off);
        config
    }

    #[tokio::test]
    async fn a_resolver_decides_when_nothing_else_has_an_opinion() {
        let flags = flags();

        assert!(flags.active("new-checkout", &Scope::user_id(17)).await.unwrap());
        assert!(flags.inactive("new-checkout", &Scope::user_id(18)).await.unwrap());
        assert!(flags.inactive("beta-search", &Scope::user_id(17)).await.unwrap());
    }

    #[tokio::test]
    async fn a_flag_nobody_defined_is_off() {
        // And not an error. A check for a flag that was deleted in the last
        // release must not take the page down.
        assert!(flags().inactive("never-existed", &Scope::none()).await.unwrap());
        assert!(!flags().is_defined("never-existed"));
        assert_eq!(flags().defined(), ["beta-search", "new-checkout"]);
    }

    #[tokio::test]
    async fn a_resolver_may_fail_and_the_failure_is_not_a_no() {
        let flags = Flags::new()
            .define("flaky", |_| async { Result::<bool>::Err(Error::msg("the database is down")) });

        let error = flags.active("flaky", &Scope::none()).await.unwrap_err();

        assert!(error.to_string().contains("the database is down"), "{error}");
    }

    // --- The precedence chain, one rung at a time. ---

    #[tokio::test]
    async fn an_override_for_a_scope_beats_the_resolver() {
        let flags = flags();
        let seven = Scope::user_id(7);
        let eight = Scope::user_id(8);

        flags.deactivate_for("new-checkout", &seven).await.unwrap();
        flags.activate_for("beta-search", &eight).await.unwrap();

        assert!(flags.inactive("new-checkout", &seven).await.unwrap());
        assert!(flags.active("beta-search", &eight).await.unwrap());

        // And only for that scope.
        assert!(flags.active("new-checkout", &Scope::user_id(27)).await.unwrap());
        assert!(flags.inactive("beta-search", &Scope::user_id(9)).await.unwrap());
    }

    #[tokio::test]
    async fn forgetting_an_override_gives_the_flag_back_to_the_resolver() {
        let flags = flags();
        let seven = Scope::user_id(7);

        flags.deactivate_for("new-checkout", &seven).await.unwrap();
        assert!(flags.inactive("new-checkout", &seven).await.unwrap());

        flags.forget_for("new-checkout", &seven).await.unwrap();
        assert!(flags.active("new-checkout", &seven).await.unwrap());
    }

    #[tokio::test]
    async fn a_global_override_reaches_every_scope() {
        let flags = flags();

        flags.activate("beta-search").await.unwrap();

        assert!(flags.active("beta-search", &Scope::user_id(1)).await.unwrap());
        assert!(flags.active("beta-search", &Scope::tenant("acme")).await.unwrap());
        assert!(flags.active("beta-search", &Scope::none()).await.unwrap());
    }

    #[tokio::test]
    async fn a_scope_override_and_a_global_one_agreeing_is_uneventful() {
        let flags = flags();

        // Global on, one customer explicitly on as well: nothing surprising.
        flags.activate("beta-search").await.unwrap();
        flags.activate_for("beta-search", &Scope::user_id(1)).await.unwrap();
        assert!(flags.active("beta-search", &Scope::user_id(1)).await.unwrap());

        // Global off, one customer explicitly off: also nothing surprising.
        flags.deactivate("new-checkout").await.unwrap();
        flags.deactivate_for("new-checkout", &Scope::user_id(17)).await.unwrap();
        assert!(flags.inactive("new-checkout", &Scope::user_id(17)).await.unwrap());
    }

    #[tokio::test]
    async fn a_global_off_beats_a_scope_on() {
        // The rule that makes `deactivate` an actual switch: an operator
        // pulling a feature during an incident does not have to know which
        // customers somebody enabled it for by hand.
        let flags = flags();

        flags.activate_for("beta-search", &Scope::user_id(1)).await.unwrap();
        assert!(flags.active("beta-search", &Scope::user_id(1)).await.unwrap());

        flags.deactivate("beta-search").await.unwrap();
        assert!(flags.inactive("beta-search", &Scope::user_id(1)).await.unwrap());

        // ...and clearing the global off gives the per-customer one back.
        flags.forget("beta-search").await.unwrap();
        assert!(flags.active("beta-search", &Scope::user_id(1)).await.unwrap());
    }

    #[tokio::test]
    async fn a_scope_off_beats_a_global_on() {
        let flags = flags();

        flags.activate("beta-search").await.unwrap();
        flags.deactivate_for("beta-search", &Scope::user_id(1)).await.unwrap();

        assert!(flags.inactive("beta-search", &Scope::user_id(1)).await.unwrap());
        assert!(flags.active("beta-search", &Scope::user_id(2)).await.unwrap());
    }

    #[tokio::test]
    async fn config_off_beats_every_override_there_is() {
        let flags = flags().configured(&config("", "new-checkout,beta-search"));

        flags.activate("new-checkout").await.unwrap();
        flags.activate_for("new-checkout", &Scope::user_id(17)).await.unwrap();
        flags.activate_for("beta-search", &Scope::tenant("acme")).await.unwrap();

        // Nothing gets past it: not the resolver, not a global override, not a
        // per-scope one set specifically for this user.
        assert!(flags.inactive("new-checkout", &Scope::user_id(17)).await.unwrap());
        assert!(flags.inactive("new-checkout", &Scope::none()).await.unwrap());
        assert!(flags.inactive("beta-search", &Scope::tenant("acme")).await.unwrap());
    }

    #[tokio::test]
    async fn config_on_beats_the_resolver_but_not_an_override() {
        let flags = flags().configured(&config("beta-search", ""));

        assert!(flags.active("beta-search", &Scope::user_id(1)).await.unwrap());

        // An operator can still take it away from one customer without a
        // deploy, which is the whole reason the store outranks `flags.on`.
        flags.deactivate_for("beta-search", &Scope::user_id(1)).await.unwrap();
        assert!(flags.inactive("beta-search", &Scope::user_id(1)).await.unwrap());
        assert!(flags.active("beta-search", &Scope::user_id(2)).await.unwrap());
    }

    #[tokio::test]
    async fn a_name_in_both_config_lists_is_off() {
        let flags = flags().configured(&config("beta-search", "beta-search"));

        assert!(flags.inactive("beta-search", &Scope::user_id(1)).await.unwrap());
    }

    #[tokio::test]
    async fn config_can_force_a_flag_nobody_defined() {
        // Useful during a rename: the new name works from `.env` before the
        // code that defines it has shipped.
        let flags = flags().configured(&config("unshipped", ""));

        assert!(flags.active("unshipped", &Scope::none()).await.unwrap());
    }

    #[tokio::test]
    async fn config_lists_accept_a_json_array_as_well_as_a_comma_separated_string() {
        let config = Config::new();
        config.set(
            "flags.off",
            rustlavel_core::Json::Array(vec![
                rustlavel_core::Json::from("new-checkout"),
                rustlavel_core::Json::from("beta-search"),
            ]),
        );

        let flags = flags().configured(&config);

        assert!(flags.inactive("new-checkout", &Scope::user_id(17)).await.unwrap());
        assert!(flags.inactive("beta-search", &Scope::user_id(17)).await.unwrap());
    }

    #[tokio::test]
    async fn config_off_survives_a_store_that_cannot_answer() {
        // The point of reading it first: an incident switch that needs a
        // working store is no use during the incident that broke the store.
        let flags = Flags::new().store(BrokenStore).configured(&config("", "new-checkout"));

        assert!(flags.inactive("new-checkout", &Scope::user_id(17)).await.unwrap());
        // Any other flag does surface the failure rather than guessing.
        assert!(flags.active("beta-search", &Scope::user_id(17)).await.is_err());
    }

    /// A store that fails every read, standing in for one that is down.
    struct BrokenStore;

    impl FlagStore for BrokenStore {
        fn name(&self) -> &'static str {
            "broken"
        }

        fn get<'a>(
            &'a self,
            _flag: &'a str,
            _scope: &'a Scope,
        ) -> crate::store::BoxFuture<'a, Result<Option<bool>>> {
            Box::pin(async { Err(Error::msg("the flag store is unreachable")) })
        }

        fn set<'a>(
            &'a self,
            _flag: &'a str,
            _scope: &'a Scope,
            _value: bool,
        ) -> crate::store::BoxFuture<'a, Result<()>> {
            Box::pin(async { Err(Error::msg("the flag store is unreachable")) })
        }

        fn forget<'a>(
            &'a self,
            _flag: &'a str,
            _scope: &'a Scope,
        ) -> crate::store::BoxFuture<'a, Result<()>> {
            Box::pin(async { Err(Error::msg("the flag store is unreachable")) })
        }

        fn flush(&self) -> crate::store::BoxFuture<'_, Result<()>> {
            Box::pin(async { Err(Error::msg("the flag store is unreachable")) })
        }
    }

    // --- Rollouts. ---

    #[tokio::test]
    async fn a_rollout_puts_the_same_scope_in_the_same_bucket_across_instances() {
        // A fresh registry each time, as a new process would have. If the
        // bucket came from a random draw or from anything held in memory, the
        // two runs would disagree.
        let first: Vec<bool> = {
            let flags = Flags::new().rollout("dark-mode", 25);
            let mut answers = Vec::new();
            for id in 0..500 {
                answers.push(flags.active("dark-mode", &Scope::user_id(id)).await.unwrap());
            }
            answers
        };

        let second: Vec<bool> = {
            let flags = Flags::new().rollout("dark-mode", 25);
            let mut answers = Vec::new();
            for id in 0..500 {
                answers.push(flags.active("dark-mode", &Scope::user_id(id)).await.unwrap());
            }
            answers
        };

        assert_eq!(first, second);
        assert!(first.iter().any(|on| *on), "a 25% rollout reached nobody at all");
        assert!(first.iter().any(|on| !*on), "a 25% rollout reached everybody");
    }

    #[tokio::test]
    async fn a_rollout_is_roughly_the_proportion_it_says_over_ten_thousand_scopes() {
        let flags = Flags::new().rollout("dark-mode", 25);
        let mut inside = 0;

        for id in 0..10_000 {
            if flags.active("dark-mode", &Scope::user_id(id)).await.unwrap() {
                inside += 1;
            }
        }

        assert!((2_300..=2_700).contains(&inside), "25% of 10,000 scopes came out as {inside}");
    }

    #[tokio::test]
    async fn a_rollout_is_still_overridable() {
        let flags = Flags::new().rollout("dark-mode", 0);

        assert!(flags.inactive("dark-mode", &Scope::user_id(41)).await.unwrap());

        flags.activate_for("dark-mode", &Scope::user_id(41)).await.unwrap();
        assert!(flags.active("dark-mode", &Scope::user_id(41)).await.unwrap());
    }

    // --- The per-scope view. ---

    /// A registry whose resolver counts how many times it ran.
    fn counted() -> (Flags, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);

        let flags = Flags::new().define("slow", move |_| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                true
            }
        });

        (flags, calls)
    }

    #[tokio::test]
    async fn a_scoped_view_runs_a_resolver_once_however_often_it_is_asked() {
        let (flags, calls) = counted();
        let view = flags.for_scope(Scope::user_id(41));

        for _ in 0..10 {
            assert!(view.active("slow").await.unwrap());
        }
        assert!(!view.inactive("slow").await.unwrap());

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(view.resolved_count(), 1);
    }

    #[tokio::test]
    async fn asking_the_registry_directly_resolves_every_time() {
        // The other half of the promise: `Flags::active` is not a cache, and a
        // caller who wants the memo has to say so by taking a view.
        let (flags, calls) = counted();

        for _ in 0..10 {
            flags.active("slow", &Scope::user_id(41)).await.unwrap();
        }

        assert_eq!(calls.load(Ordering::SeqCst), 10);
    }

    #[tokio::test]
    async fn two_scopes_are_two_answers_and_two_calls() {
        let (flags, calls) = counted();

        flags.for_scope(Scope::user_id(1)).active("slow").await.unwrap();
        flags.for_scope(Scope::user_id(2)).active("slow").await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_batch_of_flags_costs_one_call_each() {
        let (flags, calls) = counted();
        let flags = flags.define("other", |_| async { false });
        let view = flags.for_scope(Scope::user_id(41));

        let values = view.values(["slow", "other", "slow", "other"]).await.unwrap();

        assert_eq!(values.len(), 2);
        assert!(values["slow"]);
        assert!(!values["other"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refreshing_a_view_makes_it_ask_again() {
        let (flags, calls) = counted();
        let view = flags.for_scope(Scope::user_id(41));

        view.active("slow").await.unwrap();
        view.refresh();
        view.active("slow").await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(view.scope(), &Scope::user_id(41));
    }

    #[tokio::test]
    async fn a_view_answers_for_every_defined_flag_at_once() {
        let view = flags().for_scope(Scope::user_id(17));
        let all = view.all().await.unwrap();

        assert_eq!(all.len(), 2);
        assert!(all["new-checkout"]);
        assert!(!all["beta-search"]);
        assert!(view.any(["beta-search", "new-checkout"]).await.unwrap());
        assert!(!view.any(["beta-search", "never-existed"]).await.unwrap());
    }

    #[tokio::test]
    async fn a_view_carries_the_failure_of_a_resolver_rather_than_swallowing_it() {
        let flags =
            Flags::new().define("flaky", |_| async { Result::<bool>::Err(Error::msg("nope")) });

        assert!(flags.for_scope(Scope::none()).values(["flaky"]).await.is_err());
    }

    // --- Housekeeping. ---

    #[tokio::test]
    async fn purge_drops_every_override() {
        let flags = flags();
        flags.deactivate_for("new-checkout", &Scope::user_id(17)).await.unwrap();
        flags.activate("beta-search").await.unwrap();

        flags.purge().await.unwrap();

        assert!(flags.active("new-checkout", &Scope::user_id(17)).await.unwrap());
        assert!(flags.inactive("beta-search", &Scope::user_id(17)).await.unwrap());
    }

    #[tokio::test]
    async fn a_clone_shares_the_store_and_the_definitions() {
        let flags = flags();
        let handle = flags.clone();

        handle.activate("beta-search").await.unwrap();

        assert!(flags.active("beta-search", &Scope::none()).await.unwrap());
    }

    #[tokio::test]
    async fn defining_a_name_twice_replaces_it() {
        let flags = flags().define("beta-search", |_| async { true });

        assert!(flags.active("beta-search", &Scope::none()).await.unwrap());
        assert_eq!(flags.defined().len(), 2);
    }

    #[tokio::test]
    async fn a_store_can_be_supplied_as_a_shared_handle() {
        let store = MemoryStore::new();
        let flags = Flags::new().with_store(Arc::new(store.clone()) as Arc<dyn FlagStore>);

        flags.activate("beta-search").await.unwrap();

        // Written through the registry, readable through the handle the
        // application kept — which is how an admin screen lists overrides.
        assert_eq!(store.get("beta-search", &Scope::none()).await.unwrap(), Some(true));
        assert_eq!(flags.store_handle().name(), "memory");
    }

    #[test]
    fn the_debug_output_names_the_flags() {
        let rendered = format!("{:?}", flags().configured(&config("a", "b")));

        assert!(rendered.contains("new-checkout"), "{rendered}");
        assert!(rendered.contains("forced_off"), "{rendered}");
        assert!(rendered.contains("memory"), "{rendered}");
    }

    #[test]
    fn the_missing_registry_error_says_how_to_fix_it() {
        let message = missing_registry().to_string();

        assert!(message.contains("FeatureFlags::new"), "{message}");
        assert!(message.contains("Refusing to guess"), "{message}");
    }
}
