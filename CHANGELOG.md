# Changelog

Notable changes, newest first. Versions follow crates.io; every crate in the
workspace shares one number.

## Unreleased

### Security

- **Request smuggling.** Duplicate `Content-Length` was accepted and the first
  won; `Content-Length` and `Transfer-Encoding` together were accepted and the
  length quietly ignored; `Transfer-Encoding` was matched by substring, so
  `xchunked` passed and `identity, chunked` did not; and `Content-Length : 5`
  parsed as a header. The connection buffer lives for the whole connection and
  the body reader drains exactly its length, so whatever a front-end thought was
  the body and this server did not is read as the start of the next request.
  Behind any proxy that resolves the ambiguity differently, an attacker prefixes
  a request onto a pooled connection and captures or rewrites the next victim's
  — and none of the session or CSRF work above helps, because the request the
  router sees was never the one the visitor sent. All four shapes are refused
  now, per RFC 7230 §3.3.3, and a second test holds that ordinary messages still
  parse: a security fix that refuses real traffic is a denial of service wearing
  a patch's clothes.

- **The CSRF token survived signing in**, so session fixation was only half
  defended. `regenerate` rotated the id and left the data, and `token()` returns
  the value already there — so an attacker who plants a session cookie knows the
  token the *authenticated* session then answers to, and can drive
  state-changing requests from a page of his own for its lifetime. Rotating the
  id stopped him riding the session and not much else. The token goes with the
  id now, as Laravel's `Store::regenerate` has always done.

- **Nothing throttled the second factor.** The password form checks the address
  lockout in three places; the two-factor and recovery forms checked it in none.
  Somebody who already holds a password had an unlimited-rate oracle against six
  digits — three of which are accepted at any moment, because of the step window
  either side — and a second factor guessed in minutes is not a second factor.
  Worse for the recovery form, where a miss runs one argon2 verification per
  stored code, so an unthrottled attempt is also half a second of this server's
  CPU that anybody can spend.

  Both forms check it now, and — this is the half the audit did not name —
  `refuse` counts the failure. Adding the check alone would have read a counter
  nothing ever incremented: a throttle that reads like a throttle and never
  fires. A test holds both halves for every form that checks a secret.

- **`javascript:` in a menu link, and the Content-Security-Policy that was never
  sent.** The menu URL was free text rendered straight into an `href`, so a
  holder of `menus.manage` could plant a link every signed-in person — super
  administrators included — sees in the sidebar and runs on click. It has to be
  a path inside the application now, `//host` refused with the rest, which is
  the rule the dashboard field always had and this one never did.

  The other half is worth saying plainly: **five places in the kit explain that
  they avoid an inline `<style>` or an `onclick` because the pages are served
  under a policy with no `unsafe-inline`, and no policy was ever set.** The
  discipline was real and held — no `style=` attributes, no `on*` handlers, no
  inline script, no external origin — and it bought nothing, because a browser
  enforces the policy it is given. It is given one now, and that discipline is
  exactly what makes turning it on safe: this is a policy the pages already
  satisfy. `nosniff`, a referrer policy and `X-Frame-Options` come with it.

- **Response splitting.** Header values were never checked for CR or LF, on
  insertion or on the way out. A newline ends the header and two end the head,
  so a value an attacker reaches becomes headers of their choosing and then a
  body of theirs — a `Set-Cookie` that fixes a session, or a second response a
  cache in front will serve to somebody else. The shape it arrives in is
  ordinary: a redirect built from a form field where `%0d%0a` survived being
  decoded. Control characters are stripped from names and values now; a tab,
  which is legal, survives.

- **Every secret fell back to a clock-seeded mixer when `/dev/urandom` could not
  be opened.** It was the only entropy source in the framework, and the fallback
  — a xorshift seeded from the nanosecond clock and a heap address — sat behind
  a log line. It fed session ids, CSRF tokens, API token secrets, argon2 salts,
  AES-GCM nonces, WebAuthn challenges and `APP_KEY`. A session id drawn from 64
  bits of clock is one that somebody who knows roughly when you signed in can
  enumerate; an `APP_KEY` drawn from it makes every signed cookie forgeable. The
  trigger was not hypothetical — file-descriptor exhaustion is something an
  attacker can induce. There is no fallback now: a process that cannot obtain
  randomness stops rather than issue a secret that can be guessed.

- **A response that changed nothing rewrote the session and re-sent its cookie,
  which made signing in a race.** Static files are the router's fallback and the
  fallback runs the global middleware, so every CSS, JavaScript and font request
  reached the session layer — and for a signed-in visitor the guard `!dirty &&
  !existed` was always false. `Guard::login` rotates the session id and destroys
  the old record; the asset requests a browser fires alongside the sign-in are
  still holding the *old* id, and each one wrote that record back and handed the
  browser a cookie pointing at it. Whichever response landed last decided which
  session the visitor kept. When an asset won, they were returned to a pre-login
  session and the next protected request answered 401 — signed out seconds after
  signing in, intermittently. The resurrected record was also a live credential
  after the rotation meant to destroy it.

- **"Other devices have been signed out" was not true.** Three controllers wrote
  `users.session_epoch` and two also wrote the matching session key, and nothing
  anywhere read either. Every other session stayed exactly as valid as it had
  been — after a password reset, which is what somebody does when they believe
  their account has been taken. `support::epoch::SessionEpoch` now compares it,
  in all four authenticated groups, and a test refuses any group that installs
  `Authenticate` without it.

- **A password reset signed somebody in without checking whether they may sign
  in, or asking for their second factor.** The password form and the magic link
  both check; this did not. So an account an administrator had deactivated
  signed itself back in through "forgot password", and anybody holding the
  victim's mailbox walked past the victim's enrolled authenticator. Activation
  had the same gap. Clearing a lockout on reset stays — owning the mailbox is a
  legitimate way out of one.

- **`users.update` alone was enough to become a super administrator.** The form
  offers every role, `super-admin` included, and nothing stopped somebody
  posting it at their own id; `destroy` had refused to act on your own account
  since it was written. Editing your own name is still fine — editing your own
  grants is not, and a super role holder is no longer editable by somebody who
  is not one. `roles.update` had the matching hole: `destroy` refused a super
  role, `update` did not, so the role could be renamed to `super-admin` or the
  real one renamed out of existence.

- **The SQL Server driver ignored `sslmode` entirely.** It was parsed and
  validated for `sqlserver://` URLs and then read by nobody: every connection
  used the default, "encrypt and trust whatever certificate turns up", so
  `verify-full` got no verification at all and `sslrootcert=` was discarded.
  Anyone on the path could present a self-signed certificate and read the
  session, including the LOGIN7 packet whose password is only obfuscated.
  `prefer` keeps the documented compromise, because SQL Server's own startup
  certificate cannot be verified; asking for verification now gets it.

- **The PostgreSQL driver sent the password in the clear whenever asked.** With
  `sslmode=prefer`, an attacker answers "no TLS" to the SSLRequest, then asks
  for a cleartext password, and receives it. The MySQL driver in the same crate
  already refused the equivalent with a comment explaining why; one crate should
  not hold two policies on one threat. It is refused on an unencrypted socket
  now, and still allowed inside TLS, which is how PostgreSQL authenticates
  against LDAP and PAM.

### Added

- **Named database connections and a budget across them**, for
  database-per-tenant applications. `rustlavel::Connections` holds a connection
  per name — central and tenant coexisting, where `state::<Database>()` keyed by
  type could only ever hold one — and `get_or_open` is the call a tenancy
  middleware makes per request, reusing the pool the last request opened.

  The budget is the part that is easy to leave out, and the first design of it
  was wrong in a way worth recording. A shared semaphore across pools is the
  obvious answer and it fixes nothing: a permit is released the moment a
  connection is handed back, while the socket stays open in the idle queue. The
  semaphore counts connections *in use*; the server counts sockets *open*.
  Fifty resting pools hold five hundred sockets while every semaphore reads
  zero, and PostgreSQL ships with `max_connections = 100`.

  So the registry asks each pool `open_count()` — idle plus borrowed — and calls
  `close_idle()`, least recently used first, before opening more. Never a
  borrowed one: that connection is in the middle of somebody's query, and taking
  it away turns a capacity problem into a failed request. When everything is
  borrowed there is nothing to free and the pools' own semaphores make callers
  wait, which is the right answer to "genuinely busy". The default budget is 80
  against a default server allowance of 100, because a client that claims the
  whole allowance leaves nothing for psql, a backup, or the next deploy.

  Worth knowing for anybody who hits this wall: the refusal comes from the
  server, and it does not land on the tenant that caused it. The busy tenant
  reuses connections it already holds; the one refused is whichever happened to
  need a *new* one, very likely a quiet subsidiary. The company that suffers is
  not the company responsible, and the error points at the database rather than
  the code.

- **Migrations against any database, from application code** — which turned out
  to already be true. `Migrator::new(&db, migrations)` takes any database and
  any list and is public and re-exported; it was looked for in `App::migrations`
  and the CLI, where it is not. An API nobody can find is an API nobody has, so
  it now carries a worked example of provisioning a tenant. In the kit,
  `modules::migrations_for(&["sales", "accounting"])` answers "only the modules
  this tenant enabled", with `modules::names()` for a screen that offers them.

- **A service can be overridden for one request**, which is what makes
  database-per-tenant multi-tenancy a middleware rather than a rewrite.
  `Request::state<T>` reads the request's own extensions first and falls back to
  the application's, so a tenancy middleware resolves the tenant, opens or
  reuses its `Database` and calls `req.extend(db)` — and every controller
  underneath goes on saying `req.state::<Database>()`, talking to the right
  database without knowing tenants exist.

  Asked for by an application porting a Laravel ERP: a holding company and its
  subsidiaries with a database each, and about 550 controllers once the port is
  done. Threading `tenant::db(&req)` through every handler is the same program
  written five hundred more times, and one missed call site reads another
  company's data.

  It is a lookup order, not discovery — the rule against runtime magic is about
  values appearing with no line you can find, and the middleware that overrides
  a service is an ordinary explicit line in `main.rs`. A request given nothing
  gets what `main.rs` registered, which is every request in an application with
  no such middleware. A test holds the property that matters most: an override
  does not outlive its request, because a tenant connection leaking into the
  next visitor's would be the worst failure this could have.

  The rest of the tenancy work — a named connection registry, migrations
  runnable against an arbitrary database from application code, and a cap across
  tenant connections — is planned in `ROADMAP.md` rather than guessed at here.

- **`@route("name")` in templates, and a home that is a route name.** The kit
  declared thirty named routes and hard-coded fifty-four paths in its templates:
  `Router::url_for` existed and was tested, and nothing outside its own tests
  ever called it. The names were used by `route:list` and nothing else, so
  moving a route meant a search-and-replace whose misses announced themselves as
  404s. `@route` resolves at render time, like `@lang`, and the filling-in still
  happens in one function that `url_for` and the directive share. A name nobody
  registered is an error rather than an empty string — `href=""` is a link that
  looks like a link and reloads the page it is on.

  Where the application opens is `menus.home`, and it holds a route name rather
  than a path. It used to be `menus.dashboard_url` and it drove the logo only:
  `GET /` and the landing after sign-in both hard-coded `/dashboard`, so an
  administrator could point "Dashboard opens" at `/reports` and still arrive at
  the dashboard every morning. All three read one function now. A name that no
  route carries is refused when it is saved, which a path could never be.

- **Sessions in Redis**, behind `redis-sessions`, for an application running
  more than one process — the file store keeps a session on local disk, which is
  right for one machine and wrong for two. Redis owns the expiry, so a session
  that stops being touched disappears on its own and there is no directory
  quietly filling with one dead file per visitor. The client is the one in
  `rustlavel-cache`; a second Redis implementation would have drifted from it.

- **A changelog on the documentation site**, generated from this file by
  `docs/build-changelog.py` so the two cannot disagree. It refuses to render a
  construct it does not understand, and refuses to write a page with unpaired
  markup — both of which it caught while it was being written.


- **A taken port no longer stops a run.** `Address already in use` is the most
  common way starting an application fails, and the operating system's answer
  to it — `Io(Os { code: 48, kind: AddrInUse })` — names the struct holding the
  problem rather than the problem. The server now walks up from the port it was
  asked for and says which one it landed on:

  ```
  WARN  port 8399 is in use, so this is serving on 8400 instead
  INFO  Rustlavel serving on http://127.0.0.1:8400
  ```

  **Not in production.** There the default is a single attempt, because there
  something does depend on the number — a load balancer, a health check, a
  firewall rule — and a server that quietly moved to 8301 while traffic still
  went to 8300 would look like an outage with no cause in the logs. It refuses
  to start and says why instead. `server.port_attempts` overrides the default
  in either direction, and only `AddrInUse` walks on: a privileged port or an
  address that is not on this machine is still that error.

  `doctor` now reports a busy port as a warning rather than a failure, and names
  the port the server will most likely use.

### Fixed

- **`rustlavel serve` leaked a process on every reload, and that is what made a
  port stay busy.** It ran the application through `cargo run`, so the
  application was a *grandchild*: the handle `serve` held was cargo's. Killing
  it on reload killed cargo and left the application running — still holding the
  port — so the restart failed with `Address already in use`. Changing the port
  did not help, because the process holding the new port was the orphan started
  with that same new port a moment earlier.

  `serve` now runs `cargo build` and then executes the binary, so the handle is
  the application. `kill` kills it, Ctrl-C still reaches it because it stays in
  this process group, and there is one less process in the tree. Where cargo put
  the binary is asked of `cargo metadata` rather than assumed, since `target/`
  moves for a workspace member and `CARGO_TARGET_DIR` moves it for everybody.

  Measured across three reloads: one application process throughout, no
  `AddrInUse`, and no restart when nothing is touched.

- **The error a failed `main` prints named the variant, not the problem.** Rust
  formats it with `Debug`, so the derived form is what people actually saw:
  `Error: Io(Os { code: 48, kind: AddrInUse, message: "Address already in use" })`.
  `Debug` now reads exactly like `Display`, so it is the sentence that was
  written for them. Test output gains the same thing: `unwrap_err()` on a bad
  config says which file and line rather than which variant.

### Fixed

- **`rustlavel upgrade` left a project that could not build.** It merged every
  kit file and then left `Cargo.toml` naming the version the project started
  from, so the first thing anybody saw after a clean merge was a wall of
  compiler errors about items that did not exist — for a reason the command
  could have fixed itself. It now bumps the dependency and *unions* the feature
  list with what the kit needs, which matters because the kit gains flags over
  time: a 0.5.0 project has no `i18n`, and `@lang` does not compile without it.
  Features a project added of its own accord are never removed, and a `path =`
  dependency is left alone — that is somebody working against a checkout, and a
  version number would break them.

  Measured: a project scaffolded by the published 0.5.0 CLI upgrades, reports
  `Cargo.toml: 0.5.0 → 0.7.2, and gained i18n`, and — after resolving the one
  six-line `src/lib.rs` conflict the report explains — builds.

- The ten packages the kit's code imports were written out in `new`, and
  nowhere else, so nothing could check them. They are `auth_kit::REQUIRED_PACKAGES`
  now, read by both the scaffold that writes them and the upgrade that repairs
  them.

## 0.7.2 — 2026-09-05

### Added

- **`rustlavel upgrade`.** A starter kit is a hundred files copied into an
  application, and from the moment they land they are the application's. That
  is what makes them useful and it was also a dead end: a release that fixed
  one of them could reach nobody, because reaching them meant overwriting
  whatever had been written on top. Every project created with 0.5.0 was still
  running 0.5.0's kit, with no way forward but a manual diff.

  `upgrade` reconciles three versions of each file instead of two — what the
  version in `.rustlavel/manifest.json` wrote, what is in the project now, and
  what this CLI would write today. Where one side moved, that side wins and
  nobody is asked. Where both moved to the same place, so does the merge. Where
  both moved differently, the file is written with `<<<<<<<` markers and the
  project stops compiling until a person decides — an upgrade that could not
  decide must not be able to pass unnoticed.

  The base comes from crates.io: every release carries `templates/` inside its
  `.crate`, so any version a project was created with is a download away, and
  downloads are cached. Fetching and unpacking is `curl` and `tar` rather than
  an HTTP client, a TLS stack and a gzip decoder added to a CLI that otherwise
  depends on nothing; `doctor` now says so if either is missing.

  `--dry-run` reports without writing. `--from <version>` supplies the base for
  a project older than manifests. A dirty git tree is refused, because `git
  checkout` is the way back and a clean tree is what makes it available.

  Measured rather than assumed: a project scaffolded by the *published* 0.5.0
  CLI, edited in three places, upgrades with 60 files merged, 16 added, 36
  already current, and one conflict — in the one file that both the editor and
  the release had changed.

### Changed

- **Every file the kit writes is now a file.** Eight of them — `main.rs`,
  `lib.rs`, the two registries, the seeder and the three config files — were
  Rust constants written to a project without ever existing as templates. That
  split is what let 0.7.0 ship a seeder with `{{crate_name}}` still in it, and
  it also meant `upgrade` had nothing to use as a base for them. They are
  template files like the other ninety-nine now, in one manifest, written by one
  loop. The guard that used to check eight hand-listed constants now renders
  every file in the manifest, so a template added tomorrow is covered the day it
  is added.

  `upgrade` can still read those old constants out of a published crate's
  source, so a project created before this change gets a proper merge rather
  than a conflict over a file nobody touched.

### Fixed

- **The starter kit's own tests failed in a scaffolded project.** Translating
  the auth pages replaced the literal "Sign in" with `@lang(…)`, and `@lang`
  asks the *engine* for its translator — but the test built its application with
  `App::bare()` and no translator, so the page rendered the key. The wiring lives
  in `support::views` now, which `main.rs` and `tests/web.rs` both call: a test
  that renders a page stands in for a visitor, and a visitor never sees a key.

- **The TLS test containers could not read their own key on Linux.**
  `docker/certs.sh` left the PostgreSQL key at mode 600 owned by whoever ran it.
  On macOS that works by accident; on a CI runner the ownership is real, so
  postgres got `Permission denied` and the container exited before a single test
  ran.

## 0.7.1 — 2026-09-05

### Fixed

- **`rustlavel new --with auth-kit` did not compile on 0.7.0.** The seeder was
  written to the project straight from its constant while every other generated
  file went through the placeholder renderer, so it arrived carrying the literal
  text `{{crate_name}}`. It is `crate::modules::permissions()` now — the seeder
  compiles *inside* the crate it belongs to and cannot name it from outside —
  and a test refuses any constant that is written unrendered while still holding
  a placeholder, or any rendered one that has a placeholder left after.

  Worth saying plainly: the templates were tested and the *writing* of them was
  not, so nothing in the suite looked at what a real project ends up with. This
  was found by scaffolding a project from the published crates and building it,
  which is now the last step of a release rather than an afterthought.

- **Every crate now has a README on crates.io.** All thirty-three shipped
  without one, so each page said so. The root `README.md` could not serve: cargo
  refuses a readme outside the package it belongs to, and `[workspace.package]
  readme` resolves against the workspace root, which lands outside every crate.
  So each declares its own. The meta-crate and the CLI are written by hand
  because those are the pages people actually land on; the rest carry what the
  reader of a package page needs — what it is, that it belongs to Rustlavel, and
  the feature flag that turns it on.

## 0.7.0 — 2026-09-05

### Added

- **Modules: a feature owns its routes, permissions and settings.** The kit was
  laid out by technical layer — every controller together, every model together
  — and the cost is that changing one feature means opening six files in four
  directories with nothing in the tree saying those six belong together.
  `src/modules/` is the alternative, and `backup` is the first to move: its
  dump format, schedule arithmetic, controller, four permissions, three
  settings and four routes are one directory now, and `mod.rs` is the whole of
  what the application has to know about it.

  Nothing is discovered. `modules::all()` is a hand-written list, the way
  `main.rs` is. The seam is the existing `Plugin` trait, which already carried
  routes, middleware and state; `Module` extends it with the four things a
  plugin could not register because the application collected them centrally.

  **The middleware comes with the routes.** Those four routes lived inside
  `r.group("/admin", …)`, which applied `Authenticate` and `IdleTimeout` to
  everything in it. A module registers on the bare router and inherits neither,
  so the move states them — a version that forgot would have put four
  unauthenticated routes into the application, one of which restores the
  database.

- **`make:module` and `make:service`**, and `route:list` gained `--path`,
  `--method` and `--name` with a count under the table. Ninety-six routes do
  not fit a screen, and piping through `grep` loses the header row that says
  what the columns are.

- **`@lang` in the template engine, and the sign-in pages and sidebar
  translated.** Settings → Language documented a `lang/<code>.json` convention
  and, until this release, promised that "adding one is the whole of adding a
  language". It was untrue three times over: the kit did not enable
  `rustlavel-i18n`, created no `lang/` directory, and **no template anywhere
  asked a translator for a word**. A file could hold a thousand phrases and not
  one word on screen would change.

  `@lang("auth.sign_in")` now exists, with `@lang("key", "name", value)` for a
  phrase carrying `:name` placeholders. Two decisions in it are worth stating.
  The key is a literal fixed at parse time, so a template still cannot compute
  what it renders — this is not the "calls in templates" the engine rejects.
  And the words are looked up when the page is written, never when it is
  parsed: one `Engine` serves every request and a parsed template is cached and
  shared, so resolving early would have served the first reader's language to
  everybody after them. There is a test for exactly that.

  The translator sits on the `Engine`, which is application-wide; the locale
  travels in the view context under `app_locale`, which is per page and which
  `page::shell` already set. `rustlavel-view` defines a small `Translate` trait
  and `rustlavel-i18n` implements it, so the template engine keeps its single
  dependency and the HTTP stack stays out of it.

  Translated so far: the sidebar, the header, and the sign-in pages, in English
  and Indonesian. The settings tabs and administration screens are not, and the
  Language tab says so rather than implying otherwise. Two guards keep the rest
  honest: every `@lang` key a template writes must have an English phrase, and
  every language file must carry exactly the same keys.

- **Twelve more icons for a menu item**, and the Dashboard entry is first in
  the sidebar and points where you say. The rail's first entry is the one people
  reach for without reading, so adding a custom menu no longer pushes it down —
  and `menus.dashboard_url`, edited on the Menus screen rather than in Settings
  because it is navigation, decides where it and the logo above it go. A path
  inside the application only: the home button is not a place to send everybody
  off-site.
- **Bahasa Melayu is off the language list.** Two languages with files, not
  three with one that would never have had one.

- **Notifications have a page and a table of their own.** The bell in the header
  was the audit trail wearing a different label: it read `/admin/notifications`,
  which was a filtered view of who-did-what, and "See the whole trail" took you
  to the audit log. So it showed nothing addressed to you, and it was hidden
  entirely from anybody without `audit.view` — which is most people, all of whom
  have notices of their own. There is now a `notifications` table where a null
  `user_id` means everybody: a notice for one person and an announcement to all
  of them are the same row, because they are the same thing to a reader. Read
  state is a separate table, per person, so the first reader of an announcement
  does not mark it read for the rest. `/notifications` lists yours with the
  unread ones marked, "Mark all as read" clears them, and anybody holding the
  new `notifications.send` permission can write one — to a person or to
  everybody — from the page itself, because the alternative is an announcement
  feature nothing can reach.

### Security

- **Removing a passkey now asks for one.** It was an ordinary form post with
  nothing behind it but the session cookie and a CSRF token, which travel
  together — so stripping every passkey off an account was the first and easiest
  thing to do with a stolen session, and what it left behind was a password the
  thief already had. The removal now carries a WebAuthn assertion, checked
  against this account's credentials and against the user handle in the
  assertion, before anything is deleted. It is refused when the proof is
  missing, blank, or not an assertion at all — the three shapes a forged request
  takes, each with a test.

  The assertion may come from **any** of the account's passkeys, not the one
  being removed. Requiring that one sounds stricter and is worse: the reason to
  remove a passkey is usually that its device is gone, and a rule only the lost
  device can satisfy leaves a dead credential on the account for good.

### Fixed

- **`declare_module` destroyed any `mod.rs` that held more than declarations.**
  It collected every line, dropped the blanks and sorted the lot — correct for
  a file that is nothing but `pub mod` lines, and fatal for one that also holds
  a trait and a function. The first `make:module` turned `src/modules/mod.rs`
  into sorted fragments and the project stopped compiling. It inserts now, in
  order, leaving every other line where it was.
- **Test fixtures shared a directory between runs.** Seven tests across four
  crates named a temp directory after the test and then deleted it on the way
  in — enough for one run, and not for two: a second `cargo test` on the same
  machine wiped the first one's tree half-way through, and both failed
  somewhere unrelated. The process id is in the name now. This is the rule the
  project already states, applied to the places that were missing it.

- **`App` replaced a view engine the application had built for itself.** It
  registered its own whenever `resources/views` existed — which is whenever an
  application has views — so `.views(...)` silently did nothing. An engine
  carrying a translator was swapped for one that could not translate, and
  `@lang` rendered its keys. It now defers to an engine already registered.
- **The Language tab listed translation files from a different directory than
  the translator reads.** The tab asked config for `view.lang`; `i18n` reads
  `app.lang_path`. A project that moved its language files would have had the
  two looking in different places.

- **The Language tab looked broken, and one third of it was.** Changing
  anything there appeared to do nothing, for three different reasons.
  `app.currency` genuinely did nothing: `money()` was called only by its own
  tests, and the one caller of `preferences()` wrote `let (number, _) = ...` —
  it fetched the currency and threw it away. The setting is gone and `money()`
  takes the symbol from its caller, since there is no money in an account and a
  role. `app.number_format` did work, invisibly: a fresh install counts 1 user
  and 2 roles, and `1` is `1` in every format there is — the tab now prints a
  worked example (`1.234.567`) beside the dropdown, and a test requires the
  three formats to render differently. `app.locale` also worked, invisibly: it
  sets `lang` on every page, which the tab now says, rather than implying it
  translates the interface.

  Worth recording about the guard rather than the bug: the catalogue test
  passed `app.currency` all along, because reading a key and discarding the
  value counts as reading it. That test proves a key is *fetched*, not that it
  is *used*.

- **The Menus screen wrote rows the sidebar never read.** It has always saved
  menu items — and told a person, in its own empty state, that "the application
  falls back to its built-in navigation until you add something". Nothing read
  the table, so adding something changed nothing: `partials/nav.rl.html` was a
  hard-coded list. The sidebar now draws the `sidebar` location when it has
  items, honouring order, nesting and each item's permission, and treating a
  parent that points at `#` as a heading rather than a dead link. If that leaves
  the viewer with nothing, the built-in navigation comes back — a custom menu is
  a convenience, not a way to edit yourself out of the application: the custom
  menu sits *above* the built-in list rather than replacing it. Replacing it was
  the first attempt and it was wrong — adding one item took Users, Roles,
  Settings and the audit trail off the rail for an administrator who never asked
  to lose them. Each built-in entry checks its own permission already, so an
  administrator keeps everything and everybody else keeps what they are granted.
  Which items appear is a pure function with tests; the viewer's permissions are
  fetched once rather than once per item.
- **A menu item naming a permission that does not exist saved silently and then
  never appeared.** The field is free text on purpose, since a menu may point at
  a feature whose permission has not been created yet. The silence was not on
  purpose: an item guarded by a permission nobody holds is drawn for nobody, so
  it looked saved, listed correctly, and vanished. Saving one now says which
  permission is missing and what to do about it.
- **Signing in with a passkey and no authenticator app answered 419.** The
  scripts read the CSRF token out of the hidden `_token` field, which exists
  only where a form rendered — and on the two-factor page that form sits behind
  `@if(has_totp)`. Somebody enrolled in a passkey and nothing else got a page
  with no token anywhere on it, so "Use a passkey" posted an empty one and was
  refused. It worked in the configuration it was built in (passkey *and*
  authenticator) and in no other. The token is now a `<meta>` in both layouts,
  so every page carries one whether or not it renders a form, and the scripts
  read that first. Measured: the empty token still answers 419, the meta token
  reaches the handler.
- **Settings → Appearance did not change the colours until a minute later.**
  `/css/theme.css` is generated from those settings and changes the moment
  somebody clicks Save — and it was served `Cache-Control: public, max-age=60`
  with no `ETag`, so the browser went on painting the old colours out of its own
  cache. The one file in the application that changes on every Save was the one
  told to be cached, while `app.css`, which changes only on a rebuild, already
  revalidated. It now sends `no-cache` and a weak `ETag`, and answers the
  revalidation it invites: unchanged colours come back `304` with no body, and
  a changed colour comes back `200`. Measured both ways, not assumed.

### Removed

- **The "Check against breached passwords" switch is gone from Settings →
  Security.** It was never implemented: the lookup needs an outbound HTTPS call
  behind a feature flag a project may not have enabled, so the honest thing at
  the time was to refuse every new password while the switch was on and say so
  on the tab. That is still worse than not offering it. A starter kit should not
  ship a control whose only working state is off.

## 0.6.0 — 2026-09-04

### Changed

- **The starter kit looks like the dashboard it was drawn from.** 0.5.3 took
  the *behaviour* out of that mock-up — a Backup tab, a Language tab, a header
  search, notifications, toasts — and none of its look, which is why the pages
  still read as a default Tailwind scaffold. The rest of it is here:

  - **A blue-grey palette rather than a neutral one.** Every surface, border
    and label sits on a hue of about 250, which is what keeps a screen of
    tables and forms from reading as photocopier grey. The `ink-*` numbering
    keeps its meaning, so a template written against the old scale still says
    what it meant.
  - **A dark navy sidebar in both schemes**, 240px, with the application's mark
    and description at the top and the signed-in person at the foot. A pale
    rail beside a pale page was one border away from not being there.
  - **A header that says where you are**, breadcrumb over title, with the
    search box centred and tinted so it reads as a control rather than as a
    hole in the bar.
  - **Page titles sit on the page**, not inside a card. A page that opened with
    two nested boxes before any content now opens with its own name.
  - **Flatter cards, denser type, pill badges, quieter table headings**, and
    the primary action in the design's own blue — which is this palette's 700
    rather than its 600.
  - **A split sign-in screen**: a flat navy panel on the left, the form on the
    right on the same pale ground as every other page, and the panel dropped
    rather than stacked on a narrow screen, because a decorative half-page
    above a form is something a person scrolls past. A second way in gets a
    second button under a rule, not a line of small print under the first.
  - **Inter, self-hosted.** The face the design is set in ships with the kit
    in `public/fonts/` under its SIL Open Font License, rather than being
    fetched from a font CDN — which breaks behind a firewall, sends a request
    per visitor to a third party, and would need the policy this kit is written
    against opened up. One variable file per subset with `unicode-range`, so a
    page with no accented characters never asks for the 85K Latin Extended
    half; the Latin half is 48K.
  - **A mark inside each address and password field** — an envelope, a
    padlock — on every sign-in, activation and reset form, positioned rather
    than laid out beside the box so the field keeps its width, and inert to
    the pointer so clicking the icon still focuses the field.
  - **The menu button collapses the rail to its icons** on a wide screen and
    opens the drawer on a narrow one — one control, in one place, at every
    width. The choice is remembered, and applied before the first paint so a
    reload does not flash the wide sidebar. In the rail the group headings
    become the dividers they were standing in for, because a heading clipped
    to "ADMIN" is worse than no heading.

  What is *not* here is the fourteen domain screens in that mock-up. They
  belong to an application, not to a scaffold.

### Added

- **A second file manifest, for what is not text.** Everything the CLI writes
  went through the placeholder renderer, which is right for a template and
  fatal for a woff2: braces that happened to line up inside the compressed
  stream would be rewritten and the file would arrive the right size and
  unreadable. `BINARY_FILES` carries those byte for byte, and a test compares
  what is embedded against what is on disk, refuses a binary extension in the
  text manifest, and fails if a file under `templates/auth-kit/` is in neither
  — because a template nobody listed reaches no project, silently.

### Fixed

- **The stylesheet the kit shipped was stale, and 58 classes had no rules.**
  `public/css/app.css` is a Tailwind build committed so a project needs no Node
  toolchain, and that is the trap: adding a class to a template is free, and the
  build only learns about it when somebody remembers to rebuild. 0.5.3 added a
  search box, a notification list, toasts, pagination, tab pills and row
  avatars, and shipped every one of them unstyled. Rebuilt — and a test now
  fails if a class the templates write is missing from the stylesheet beside
  them.
- **Twelve colours on Settings → Appearance changed nothing.**
  `theme_controller` has always generated `--sidebar-bg`, `--sidebar-text`,
  `--sidebar-active-bg`, `--sidebar-active-text`, `--login-from` and
  `--login-to` into `/css/theme.css`, and no rule anywhere consumed one. The
  sidebar and the sign-in panel are drawn from them now, which is what makes
  that tab do what it says.
- **Three buttons labelled "(Default)" restored something else.** Settings →
  Appearance offers quick presets, one named for the value the catalogue
  declares — and it kept handing back the colours of the palette this release
  replaced. A test now derives each of those presets from the catalogue and
  fails when the two drift.
- **Checkboxes and radios rendered as a dark square.** `border-ink-300` and
  friends half-style a native control: the browser goes on painting its own
  box and the declared border turns it into something neither styled nor
  default. The classes are gone and `accent-color` in the base layer does the
  whole job, which is what the design does too.
- **The menu button did nothing above `lg`.** It was `lg:hidden`, so the only
  control for the sidebar disappeared exactly where there was room to want
  one.

- **Sixteen settings were declared, drawn on a tab, saved to the database and
  read by nothing.** The Email tab wrote six keys the mailer never consulted —
  it was built once at boot from `config/mail.json` — so a host, port,
  encryption, username or password typed there changed nothing at all. The
  General tab's timezone, date format and description, and the Language tab's
  locale, were the same. Each is now wired: `support::mail::mailer_for` builds
  the mailer per send from the store, `support::format::Dates` renders every
  timestamp on the audit and backup pages through the chosen timezone and
  formats, and the name, description and locale reach both layouts — the
  description as a `<meta>` and the locale as `lang` on `<html>`.
- **`backup.path` now decides where a backup is written, and where one is read
  back from.** It was a text field beside a destination dropdown, and the
  writer called a helper that ignored both. Every path in the controller goes
  through one place now, because wiring only the write half would have left
  backups that could be made and not downloaded — and a reader falls back to
  the built-in directory, so pointing the field somewhere new does not strand
  the backups already on disk. The traversal check that stops a name climbing
  out of the directory applies to whichever directory is configured.
- **The backup destination, S3 bucket, fallback locale and first day of week
  are gone from the tab.** Nothing uploads a backup, this kit does not enable
  `rustlavel-i18n`, and there is no calendar to start on a Monday. A control
  that cannot be honoured is worse than no control.
- **The date formatter matched the dropdown's labels, not the values it
  saves.** `DD/MM/YYYY` is what the tab shows; `d/m/Y` is what it stores, so
  three of the four date formats fell through to the fourth — and the test
  written beside it, against the same labels, agreed with the bug. The
  formatter now takes its cases from the stored values, and a test takes its
  inputs from the catalogue and fails if two formats render alike.
- **Half the timezone list was UTC.** The offset table knew Kuala Lumpur and
  Bangkok, which the tab does not offer, and not Europe/London or
  America/New_York, which it does. It also had one fixed offset per zone, which
  is wrong for those two for part of the year, so it now takes the instant and
  applies the British and US summer-time rules; a test asserts the exact
  Sundays the clocks move, and another fails on any zone the tab offers that
  the formatter answers UTC for all year.
- **Every date on every page goes through that formatter.** `tokens::humanise`
  was a second, absolute, UTC formatter that ignored all three settings, and it
  was still rendering the users list, the dashboard, the profile, search,
  passkeys and half the audit page. It is gone, and the line under a timestamp
  now says how long ago — which is what it is for and what it did not do.
- **The profile page's "Joined" was the literal `—` for everybody.**
- **The password minimum a form advertised and the one the server enforced were
  read from different places.** The form asked `Config`, the check asked the
  settings store, so raising it on the Security tab produced a field that
  accepted twelve characters and a server that refused them. One function reads
  that key now.
- **A test walks the catalogue and fails on a key nothing reads.** This is the
  bug that kept coming back and never showed up in a review — the code that
  declares a setting and the code that ignores it are in different files, and
  both look right. Enumerating the catalogue is what finds it.

## 0.5.3 — 2026-09-03

One line per crate, and it is the line that makes 0.5.2's headline fix real.

### Fixed

- **`rust-version` reached no published manifest.** 0.5.2 declared
  `rust-version = "1.88"` in `[workspace.package]` and none of the thirty-three
  crates inherited it: workspace package fields are opt-in, and a member takes
  one only by asking with `rust-version.workspace = true`. crates.io reported
  `rust_version: null` for every crate in that release, and a consumer on Rust
  1.87 still got ``let` expressions in this position are unstable` from inside
  `rustlavel-core` — the exact error the field exists to replace.

  Found by exercising the published artifact rather than the working tree:
  `cargo add rustlavel` on 1.87, which still locked 0.5.2 and still failed the
  old way. With the inheritance in place cargo stops at resolution instead:

  ```text
  error: rustc 1.87.0 is not supported by the following package:
    rustlavel-core@0.5.3 requires rustc 1.88
  ```

  Nothing else changed. If you are on Rust 1.88 or newer, 0.5.2 and 0.5.3 are
  the same code.

## 0.5.2 — 2026-09-03

An interactive `rustlavel new`, a Backup tab that does what it says, and a
minimum toolchain that is measured rather than assumed.

### Added

- **`rustlavel new` asks.** With no `--with`, it now asks what kind of
  application, which database, and what else to put in. It answers itself when
  nobody is there — every prompt falls back to its default when stdin is not a
  terminal, and `--with`, `--all` and `--yes` skip it — so a scaffold in CI
  produces the same project it always did rather than hanging on a question.
- **The Backup tab has a schedule, a retention window and a destination.** The
  retention is live: after a successful backup the ones past the window go, row
  and file together. The schedule is a statement of intent — this application
  has no clock, so something outside it has to act on the answer, and **the
  panel says so, loudly, when a schedule is set and nothing has ever run one**.
  The destination offered local disk and S3-compatible — **and that was wrong;
  see Unreleased.** Nothing here uploads a backup anywhere, so the dropdown was
  the very failure its own note described, and it is gone.
- **The Language tab's number format and currency are read by something.**
  Every count on every administration page and the size column on the Backup
  tab go through one formatter. (The first day of the week was listed here too
  and read nothing; see Unreleased.)
- **A search box and a notification list in the header.** Search covers people,
  roles, permissions, menu items and settings, and a group is included only
  when the person may open what it links to. The notifications are the audit
  trail rather than a second table kept in step by hand, filtered to what is
  worth interrupting somebody for — sign-ins are left out, because a list that
  reports every sign-in is a list nobody reads. Flash messages become toasts,
  server-rendered first so they survive with scripting off.

### Fixed

- **`rust-version` was wrong, and wrong in the direction that matters.** 0.5.1
  did not declare one; the first attempt at this said 1.85, reasoned from
  edition 2024. The code uses let-chains (`&& let`) in twenty-one places across
  eighteen crates, and those stabilised in 1.88. Measured by installing the
  toolchains: 1.87 refuses, 1.88 builds the whole workspace with every feature
  and every test target. A declared floor that is too low is worse than none —
  cargo tells somebody on 1.86 the crate is compatible and rustc then refuses
  it.
- **The plugin lines never reached an auth-kit project.** `auth-kit` writes its
  own `main.rs` and that template had no `{{plugins}}` in it, so the fix
  shipped in 0.5.1 did nothing for the shape most people scaffold. The test
  passed because it called the helper directly; it renders all three templates
  and reads the output now.
- **Every `MAIL_*` variable was inert.** There was no `config/mail.json`, and
  `Config` has no automatic `MAIL_HOST` to `mail.host` mapping, so the mailer
  used its defaults while the Email tab — which reads the environment directly
  — reported that the environment was in charge.
- **Three ways the Backup screen produced something that was not a backup**:
  `Create` could not succeed on a stock scaffold, because the exporter
  paginated with `order by "id"` and the RBAC pivots have no surrogate key; the
  dump covered a hardcoded list that three tables added in one afternoon never
  reached; and `Restore` left the settings cache alone, so an administrator
  restoring a backup to undo a bad change saw the bad change still there.
- `QueueDashboard::new(db.clone())` in the scaffold's own comment did not
  compile — the signature takes an `Arc<dyn Queue>`.
- `rustlavel-openapi`'s header example called an `OpenApi` type that has never
  existed. The replacement is a compiled doctest, which caught the first
  attempt at the fix.
- `cache.path` defaulted to `storage/framework/cache` while the scaffold
  creates `storage/cache`.

## 0.5.1 — 2026-09-03

Three fixes, two of them to things 0.5.0 claimed were already done.

### Fixed

- **The scaffold registered eight of the twelve packages that ship a plugin.**
  `otel` has a no-argument constructor and was left out of the list — and
  `OpenTelemetry` was missing from the prelude as well, so the line could not
  have compiled had it been written. That is the same bug 0.5.0 fixed for
  `Metrics`, still present for `otel`. `mcp`, `oauth` and `oauth-provider` were
  not even named in the comment.

  The lists are maintained by hand, so the test no longer trusts them: it reads
  the crates directory, finds every `impl Plugin for`, and fails on any package
  the scaffold offers but says nothing about.
- **A cache miss cost two queries.** `ModelCache::find` called `Model::find`
  and then read the same row again to cache it — because `Model::to_json`
  cannot be rehydrated, so the model in hand was not enough to store. Two round
  trips on every cold read made the cache worse than no cache on the path it
  exists to help. Fetching the row once and hydrating from it gets both from a
  single query.

  A cache hit whose stored row no longer parses was also counted as a hit *and*
  fell through to the database, which overstated the hit rate by exactly the
  entries that were costing a query. The hit is recorded once a model has
  actually come back.
- A test in `rustlavel-ai` failed on whichever runs interleaved: it collected
  every `ai.call` event on the process-global bus and asserted there was
  exactly one, while ten other tests in the same file make a call. Being the
  only test that subscribes is not the same as being the only test that emits.

### Added

- `OpenTelemetry` is re-exported from the meta-crate and its prelude, like
  `Telescope`, `DebugBar` and `Metrics`.

## 0.5.0 — 2026-09-03

A second-level cache for models, and three things that were switched on and
doing nothing.

### Added

- **`rustlavel-model-cache`** — Hibernate's second-level cache, in this
  framework's vocabulary. Entities kept by primary key and query results kept
  by the SQL that produced them, over the `Cache` trait that already exists, so
  memory, file and Redis all work with no new configuration.

  Caching an entity is the easy half: one key, and the write that changes it
  knows which key to drop. Caching a *query* is the hard one, because a write
  to one row invalidates an unknown number of cached result sets and there is
  no way to enumerate them — a cached `WHERE role = 'admin'` is invalidated by
  an insert whose shape the cache never sees. The answer is Hibernate's: a
  **generation counter per table**, recorded on every cached result and bumped
  by every write. One counter invalidates every stale result set at once.
  Blunt on purpose — a write to any row of `users` costs you every cached
  `users` query, which beats serving a list that is missing a row somebody
  just added.

  The query key is a 64-bit fingerprint *and* the statement, stored beside the
  rows and compared on the way out, so a collision is a miss rather than one
  query's rows served as another's. A model with no registered region is not
  cached at all: a package that quietly starts holding every table the moment
  it is registered is a cache nobody chose.

  Six integration tests run against PostgreSQL 16, MySQL 8.4 and Azure SQL
  Edge, and prove the two things unit tests cannot — that a second read does
  not reach the database, and that a write makes it reach the database again.

### Fixed

- **A model with a `bool` field could not be read on MySQL.** MySQL has no
  boolean: `BOOLEAN` is an alias for `tinyint(1)`, and nothing on the wire
  tells a flag from a small counter, so the driver hands back an integer. The
  model does know — it declared the field `bool` — and that is the layer where
  the two are now reconciled. Zero and one only: a counter column declared
  `bool` is a mistake, and guessing `true` for 7 would hide it.
- **A package the scaffold was asked for was never registered.** `rustlavel
  new app --with telescope` compiled the dependency in and left `main.rs`
  saying nothing about it — the application ran, `/telescope` answered 404, and
  the generated file gave no hint a line was missing. The scaffold writes the
  line now, for the plugins that can be built from nothing; the ones needing a
  database handle are named in a comment rather than emitted as code that
  would not compile.
- `DebugBar` was not re-exported from the meta-crate at all, and neither it,
  `Metrics` nor `Telescope` was in the prelude — which is the one thing a
  generated `main.rs` imports.
- One OTel test failed a few runs in a hundred for no reason. It asserts that
  a tag byte is absent from an encoded span, and reasoned carefully about the
  identifiers while forgetting that `Span::new` timestamps itself from the
  clock, so the nanoseconds supplied a stray `0x7a` whenever they contained
  one. The times are pinned.

## 0.4.0 — 2026-09-03

A new package, two bugs that made features look like they worked when they did
not, and the administration area the starter kit was missing.

### Added

- **`rustlavel-audit`** — who did what, to which record, from where. Different
  from the application log on purpose: a log line is for whoever is debugging,
  an audit entry is a record somebody may be asked about a year later.
  `req.audit("users.deleted").on("User", id).describe(...).record()`. The
  address and the user agent are read off the request rather than passed in,
  because an entry that says "someone updated the settings" answers nothing.
  The actor column is nullable and deliberately not a foreign key: an entry
  outlives the account it describes, and a cascade would delete the evidence
  along with the subject.
- **RS256 in `rustlavel-webauthn`.** TPM-backed Windows Hello and a number of
  older security keys sign with nothing else, so a relying party that does not
  offer it turns them away at registration — which is what Chrome's
  `pubKeyCredParams` warning is about. The arithmetic is written here, and the
  crypto rule survives intact: verifying an RSA signature involves no secret
  at all. The padding is checked by re-encoding the block a valid signature
  would produce, never by parsing the recovered one — a lenient parser is how
  Bleichenbacher's 2006 forgery worked.
- Menu management and an audit page in the starter kit, plus a brand colour on
  Settings → Appearance that reaches every page. The hue comes from the choice
  and the lightness ladder stays, which is what keeps white-on-brand legible
  when somebody picks an unusual colour.
- `QueryBuilder::or_filter_op` and `or_filter_like`: `or_filter` could only
  ever mean equality, so a search across three columns with `like` had no way
  to say so.
- `Permissions::users_in_role`, which answers "is anybody still holding this
  role" with a count rather than a list.

### Fixed

- **Passkeys could not be registered at all.** The starter kit's script sent an
  invention of its own — `raw_id`, `client_data_json`, flat rather than nested
  under `response` — where the parser reads what
  `PublicKeyCredential.toJSON()` produces, so every credential arrived with no
  `rawId`.
- **Settings → Appearance had never coloured anything.** `/css/theme.css` was
  served as `text/plain`, because `Response::with_text` sets its own content
  type and was chained after the header that set the right one. A browser
  refuses a stylesheet served as plain text and says so only in a console
  warning, so nothing in the application noticed.
- **`Files` sent `cache-control: public, max-age=3600` on everything, with no
  way to change it.** Nothing it serves is fingerprinted, so that is a promise
  the filename cannot keep: rebuild a stylesheet and the browser keeps showing
  the old one for the rest of the hour. The default is now `no-cache` — cache,
  but ask first — and `Files::cache_control` is there for URLs that do carry a
  content hash.
- **`BoxFuture` was not reachable outside `rustlavel-http`**, so a middleware
  written in an application could not spell its own signature. The trait was a
  documented extension point that could not be extended.
- `rustlavel-auth` called `GenericArray::from_slice`, deprecated in newer
  generic-array releases. The framework's lockfile pins an older one, so clippy
  here stayed clean while every freshly scaffolded project got three warnings
  out of a dependency it never asked for.
- The Roles page's Users column had been empty since the page was written: the
  handler filled it with a literal null because nothing in `rustlavel-rbac`
  could answer it.
- Sign-in tab order. "Forgot?" sat between the address and the password in the
  document, so everybody tabbing out of their email landed on it.

### Changed

- Five settings on the starter kit's Security tab were stored and read by
  nothing, which is worse than not having them. Magic-link sign-in, email
  verification, password-reuse prevention, the idle session timeout and the
  From address on the Email tab all do something now. `auth.require_mfa` was
  enforced by the login form and by nothing else, so an activation link, a
  password reset and a magic link were three ways around it.

## 0.3.2 — 2026-09-03

Three bugs that only appear when the thing is run rather than compiled.

### Fixed

- **`FLAGS_ON` and `FLAGS_OFF` did nothing.** The crate read `flags.on` from
  configuration and nothing mapped the environment into it, so the documented
  incident switch was inert. It now reads the environment when the
  configuration key is absent.
- **The starter kit's seeder logged the first administrator's activation link
  at `info`**, so `LOG_LEVEL=warn` hid the only way into a new application. It
  is printed now, with the full URL.
- The four OTLP collector tests share one container and one log, and could
  interleave under a full workspace run. They take turns.
- `docker/env.sh` printed `KEY=value` without `export`, so `eval` set a shell
  variable that `cargo` never saw and every integration suite skipped while
  reporting a pass. It also now sets `DATABASE_URL`, without which fifteen
  more did the same.

## 0.3.1 — 2026-09-03

### Fixed

- `rustlavel new --with rbac` and `--with flags` were refused: both are
  features of the `rustlavel` crate, and neither was in the list the scaffold
  accepts. `rbac` had been missing for a release. A test now reads the
  meta-crate's manifest and fails if the two lists disagree in either
  direction, so this cannot drift again.

## 0.3.0 — 2026-09-03

The minor number, because `Errors` gained a field and a `Validator` built from
a request now behaves differently for a browser: it redirects instead of
rendering text. Everything else is new.

### Changed

- **A failed HTML form now redirects back to itself** with the messages and the
  submitted input, instead of answering a plain `422` with the page gone. JSON
  clients are unaffected and still get Laravel's shape. An application with no
  session middleware falls back to the old plain-text `422`.

### Added

- `rustlavel-flags` — runtime feature switches resolved per user or tenant,
  with percentage rollouts that are stable across processes, a store for
  operator overrides, and `FLAGS_OFF` as an incident switch that is read before
  the store is touched.
- `rustlavel make:crud`, `make:package` and `tinker`.
- A cookbook at
  [advancedynamic.github.io/rustlavel/cookbook.html](https://advancedynamic.github.io/rustlavel/cookbook.html).
- `Flash`, in `rustlavel-http`: somewhere to leave a value for exactly one
  further request. Implemented by the session, used by validation and the view
  layer, so neither has to depend on the other.
- `Request::errors`, `old`, `old_field`, `has_errors` and `previous_url`.
- A nightly CI workflow and a `docker/` set that stand up the eight servers the
  integration suites need — including the TLS certificates, which is why
  database TLS was untested even though CI already had the databases.

## 0.2.2 — 2026-09-02

Two bugs that met anybody who ran `rustlavel new --with auth-kit` on 0.2.1.
Nothing else changed.

### Fixed

- The starter kit shipped a `match` that clippy rewrites as `matches!`, so
  `cargo clippy -- -D warnings` failed on a freshly scaffolded project. A lint
  in the kit is a lint in an application's own build.
- The scaffold's default test asserted a public home page, which the kit
  replaces with a redirect to the sign-in page, so `cargo test` failed
  immediately after `rustlavel new --with auth-kit`. The kit now ships four
  tests of its own covering the guard and the CSRF check.

### Added

- A CI job that scaffolds a project with the kit, builds it, runs its tests and
  runs clippy over it. None of the kit's ~57 files are compiled by the
  workspace, so a rename in the framework used to break them silently.

## 0.2.1 — 2026-09-02

The patch number rather than the minor, and that is the right slot: for a
`0.x` version Cargo treats the minor as the major, so a breaking change would
have to be `0.3.0`. Nothing here breaks anything. Everything below is new, so
`rustlavel = "0.2"` picks it up on its own.

### Added

- **`rustlavel new app --with auth-kit`** — a starter kit, in the shape Laravel
  Breeze has: the controllers, views, routes and migrations are written into
  the project and belong to it, so the login page is a file you edit rather
  than a template you override. The library half stays in crates, so a
  security fix arrives with `cargo update`.
  - Sign-in with an authenticator app (TOTP), passkeys, and single-use recovery
    codes; a sign-in audit log; per-account and per-address lockout.
  - Self-registration (switchable off) and admin invitation, both ending at a
    link where the person chooses their own password — so an administrator
    never knows one.
  - Roles and permissions with a management area, direct per-user grants and
    denies, and "view as this user" for support.
  - Eleven pages of Tailwind v4, built to a committed stylesheet so no Node
    toolchain is needed, under a Content-Security-Policy with no
    `unsafe-inline` anywhere.
- **`rustlavel-rbac`** — roles, permissions, wildcard matching, a cached check
  and `Can::permission(...)` route guards. Tested against PostgreSQL, MySQL and
  SQL Server.
- **`rustlavel-auth::totp`** — RFC 6238, verified against the full RFC 4226 and
  RFC 6238 test tables, with recovery codes.
- **`rustlavel-auth::qr`** — a QR encoder, checked byte for byte against
  libqrencode across 291 payloads. It exists because posting a TOTP secret to a
  chart API is not acceptable and a JavaScript QR library would need a looser
  policy than the kit ships with.
- **`rustlavel-auth::impersonation`** — acting as another user, with the real
  operator kept for the audit trail.
- `CoseKey::to_bytes` and a CBOR encoder, without which a passkey could be
  verified and never stored.
- `RouteHandle::middleware`, for a resource whose verbs need different guards.
- `Request::inputs`, for the repeated fields a checkbox group sends.
- `Config::list`, `Request::body_bytes` in tests, `sha256_hex`.

## 0.2.0 — 2026-09-02

The minor number rather than the patch, and deliberately: for a `0.x`
version Cargo treats the minor as the major, so `rustlavel = "0.1"` would
have picked this up on its own. Two changes below need a person to read them
first.

### Breaking

- **`Request::ip()` no longer reads `X-Forwarded-For` by itself.** An
  application behind a load balancer must add the new `TrustProxies`
  middleware, or it will see the balancer's address for every request. See
  below for why this is worth the interruption.
- **`Error` has a new variant, `Unavailable`.** Code that matches on `Error`
  exhaustively will not compile until it handles it.

### Security

- **`Request::ip()` no longer trusts `X-Forwarded-For` on its own.** It did,
  unconditionally, which meant any client could choose its own address by
  sending a header — and the rate limiter and idempotency scoping both key on
  it, so both were escapable by varying one header per request. The address is
  now the peer that opened the socket unless the new `TrustProxies` middleware
  establishes that the connection came from a proxy named in advance.
  Applications behind a load balancer must add `TrustProxies::from_config` (or
  `::at([...])`) to keep seeing real client addresses.

### Added

- `TrustProxies`, with CIDR matching for IPv4 and IPv6, reading
  `trustedproxy.proxies`. Forwarded hops are stripped from the right, so a
  value the client wrote before the first proxy appended to it is never taken
  as the client address.
- `Request::scheme()`, `is_secure()`, `forwarded_host()`, `forwarded_port()`,
  from a trusted proxy's `X-Forwarded-Proto`/`-Host`/`-Port`.
- `Request::with_peer`, so a test can say where a request came from.
- **A circuit breaker** for outbound calls, in `rustlavel-client`:
  `Client::new().breaker(CircuitBreaker::new())`. It trips on a failure rate
  over a sliding window rather than a raw count, keeps one breaker per host,
  and probes with a few calls before resuming. A 5xx counts against the
  upstream; a 4xx does not.
- `Error::Unavailable`, raised when a call was refused rather than attempted,
  so a caller can safely fall back. It renders as a 503.

## 0.1.1 — 2026-09-02

The API release: everything an application that only serves JSON needed and
did not have, plus five packages that existed in the repository but had never
been published.

### Added

- **CORS** (`Cors`), configured with the keys of Laravel's `config/cors.php`;
  every list accepts a comma-separated string so it can come from `.env`.
- **Response compression** (`Compress`): gzip and deflate, with a DEFLATE
  codec written in the framework — fixed and dynamic Huffman blocks and a
  complete inflater, checked against zlib's own output.
- **ETag and conditional requests** (`ETag`): `If-None-Match` and
  `If-Modified-Since` answered with `304`. Static files now carry
  `Last-Modified`.
- **API resources** (`JsonResource`, `attributes()`): one JSON shape per type,
  with Laravel's `data`/`meta`/`links` for pagination.
- **Request identifiers** (`RequestId`), on the request, the response, a
  task-local, and the `http.request` instrumentation event.
- **Idempotency keys** (`Idempotency`, in `rustlavel-cache`): a write with an
  `Idempotency-Key` runs once and is replayed thereafter.
- **Health probes** (`Health`): `/up` for liveness, `/up/ready` for readiness
  with concurrent, time-limited checks.
- **`Timeout`** and **`BodyLimit`** middleware, per group.
- **API versioning**: `Router::version`, `VersionHeader`, and
  `Deprecation`/`Sunset` headers from `.deprecated_at()` and `.sunset()`.
- `#[rustlavel::main]` and `#[rustlavel::test]`, with `tokio` re-exported, so
  an application's dependency list is one line.
- `Config::list`, for values that are an array in JSON and a comma-separated
  string in `.env`.
- `SearchClient::from_config` and `RelyingParty::from_config`.
- `rustlavel-client` asks for and transparently decodes gzip and deflate.
- First publication of `rustlavel-debugbar`, `rustlavel-ldap`,
  `rustlavel-otel`, `rustlavel-search` and `rustlavel-webauthn`.

### Fixed

- A warning from the framework's own code no longer appears in an
  application's build when the `db` and `queue` packages are both off.

## 0.1.0 — 2026-08-30

First release.
