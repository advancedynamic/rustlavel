# Rustlavel — Roadmap

Framework web Rust full-stack yang terinspirasi Laravel (referensi: **Laravel 13** — skeleton slim, package opt-in, AI first-party), dibangun **from scratch** (tanpa Axum/SeaORM). Kemudahan Laravel, jaminan compile-time Rust.

## Prinsip desain

1. **Batteries included, tapi opt-in.** Scaffold `rustlavel new` hanya berisi yang standar (routing, config, CLI). Fitur lain ditambah per package: `cargo add rustlavel-db` ≈ `composer require` — tidak di-add berarti tidak dikompilasi sama sekali.
2. **Ambil rasanya Laravel, bukan mekanismenya.** Konvensi, struktur folder, Artisan-style CLI, ergonomi API — ya. Facade statis dan service container runtime — tidak; diganti dependency injection compile-time (extractor pattern) yang error-nya ketahuan saat kompilasi.
3. **Eksplisit di satu tempat.** Tidak ada auto-discovery runtime; package diaktifkan satu baris di `main.rs` (`.plugin(Telescope::default())`). Registry (migrations, dsb.) di-generate otomatis oleh CLI.
4. **Pesan error yang manusiawi** — dari macro sampai halaman error dev. DX adalah fitur, bukan polesan.
5. **Stabilitas API adalah fitur** (pelajaran Laravel 13: zero breaking changes).

## Struktur repo

```
rustlavel/
├── ROADMAP.md          # file ini
├── CLAUDE.md           # keperluan AI/coding agent (di luar kode)
└── framework/          # semua kode: Cargo workspace
    ├── Cargo.toml
    └── crates/
        ├── rustlavel/            # meta-crate (yang di-import user, feature flags)
        ├── rustlavel-core/       # app lifecycle, config, .env, error, event/instrumentation internal
        ├── rustlavel-http/       # server HTTP, routing, middleware, request/response
        ├── rustlavel-cli/        # binary `rustlavel`: new, serve, make:*
        └── ...                   # crate lain menyusul per fase
```

Model publish: satu repo banyak crate (seperti `laravel/framework` berisi `illuminate/*`), tiap crate dipublish terpisah ke crates.io.

---

## Fase 0.1 — MVP ✅ selesai

- [x] `rustlavel new <app>` — scaffold slim ala Laravel 13: `main.rs`, `routes/web.rs`, `config/`, `.env`
- [x] `rustlavel serve` — dev server + hot reload (watch mtime, restart otomatis)
- [x] HTTP/1.1 server from scratch di atas Tokio TCP: keep-alive, HEAD, chunked, limits, graceful shutdown
- [x] Routing: `r.get("/users/{id}", handler)`, group + prefix, named routes, wildcard, `resource()`, `url_for`
- [x] Middleware pipeline (global, per-group, per-route) dengan short-circuit
- [x] Request/Response ergonomis: `request.input()`, `response().json()`, redirect, cookie, form/JSON body
- [x] Config + loader `.env` sendiri: `config("app.name")`, interpolasi `${VAR}`
- [x] JSON sendiri (parser + serializer) di core — tanpa serde
- [x] Context bertipe (pengganti service container): `req.state::<T>()`
- [x] Plugin hook untuk package opsional: `.plugin(...)`
- [x] `rustlavel make:controller`, `make:middleware`, `route:list`
- [x] Error page dev ala Ignition: stack location, potongan kode, panel request, saran perbaikan, kredensial disensor
- [x] Panic di handler jadi 500 (bukan crash), berlaku juga di test
- [x] Test client tanpa socket: `client.get("/x").await.assert_ok().assert_json(...)`
- [x] Static file serving `public/` dengan proteksi path traversal
- [x] Health check `/up`, structured logging JSON untuk production, event bus instrumentation

## Fase 0.2 — Database (`rustlavel-db`)

- [ ] Connection pool + driver wire protocol sendiri (mulai dari Postgres ATAU MySQL, satu dulu)
- [ ] Query builder: `DB::table("users").where(...).get()`
- [ ] Migrations + schema builder: `Schema::create("users", |t| { t.id(); t.string("name"); })`
- [ ] `rustlavel make:migration`, `migrate`, `migrate:rollback`, `migrate:fresh` (registry di-generate CLI)
- [ ] Seeder & factory: `rustlavel db:seed`, `User::factory().count(3).create()`
- [ ] ORM gaya Eloquent via `#[derive(Model)]`: relasi (has_many, belongs_to), eager loading anti N+1

## Fase 0.3 — Web esensial

- [ ] Validation: `required|email` style + versi type-safe via derive
- [ ] Templating gaya Blade (`rustlavel-view`): `@extends/@section/@foreach`, compile-time check, XSS escaping otomatis, reload tanpa recompile saat dev
- [ ] Auth (`rustlavel-auth`): session, argon2, guard, middleware `auth`; starter kit `--auth` (login/register/reset); passkey/WebAuthn
- [ ] Session & Cache: driver file / in-memory / Redis
- [ ] CSRF otomatis, signed URLs, `encrypt()`/`decrypt()`
- [ ] Rate limiting & CORS default
- [ ] Pagination nyambung query builder + view
- [ ] Testing helpers: `get("/users").assert_ok()`, DB transaksi rollback per test, `Mail::fake()`, `Queue::fake()`, `Http::fake()`, time travel

## Fase 0.4 — AI & DX era agent

- [ ] `rustlavel-ai`: satu API lintas provider (Anthropic, OpenAI, Ollama), streaming SSE, tool calling via macro, structured output ke struct tervalidasi, `Ai::fake()` untuk testing, config `config/ai.toml`
- [ ] `rustlavel-mcp`: MCP server (expose fitur app sebagai tools, `make:mcp-tool`) + MCP client; transport stdio & HTTP
- [ ] Scaffold ramah AI: `rustlavel new` menulis `CLAUDE.md`/`AGENTS.md`; MCP dev server expose `route:list`, skema DB, docs ke coding agent (ala Laravel Boost)

## Fase 0.5 — Observability

- [ ] `rustlavel-telescope`: rekam request, query + durasi, jobs, exceptions, logs, panggilan AI (token count); dashboard `/telescope`, storage SQLite. (Hook instrumentation-nya sudah disiapkan di core sejak Fase 0.1)
- [ ] Production: structured logging JSON, OpenTelemetry/tracing, metrics Prometheus, integrasi error reporting via `.env`
- [ ] `rustlavel doctor` — diagnosa env: versi, .env, koneksi DB, port, migration pending

## Fase 0.6 — Kelas berat

- [ ] `rustlavel-queue`: `dispatch(Job)` + worker, `queue:work`, dashboard ala Horizon
- [ ] Scheduler: `schedule().daily().at("13:00")`
- [ ] Events & listeners
- [ ] Mail + notifications multi-channel (mail/Slack/Telegram/database)
- [ ] Storage abstraksi (local/S3), HTTP client dengan retry
- [ ] Feature flags ala Pennant
- [ ] Broadcasting/WebSocket ala Echo+Reverb
- [ ] i18n/localization: `t("welcome.title")`
- [ ] OpenAPI auto-generate dari route typed + halaman docs
- [ ] Single binary deploy: `rustlavel build` (app + template + asset + migration dalam 1 file), `make:docker`, cross-compile
- [ ] `rustlavel make:crud`, `rustlavel route:list`, `rustlavel tinker` (mode scratch file)

## Fase 1.0+ — Ekosistem

- [ ] Situs dokumentasi kualitas Laravel + cookbook, sejak 0.x
- [ ] `rustlavel make:package` — scaffold package pihak ketiga; package bisa menyumbang perintah CLI
- [ ] Contoh app resmi (blog / todo / mini e-commerce)
- [ ] Komponen reaktif server-side ala Livewire (pembeda terbesar, jangka panjang)

---

## Keputusan yang sudah diambil

| Keputusan | Pilihan |
|---|---|
| Referensi | Laravel 13 (struktur slim 11+, AI SDK, passkeys) |
| Fondasi HTTP/ORM | From scratch (tanpa Axum/SeaORM); async runtime pakai Tokio |
| Distribusi | Meta-crate `rustlavel` + feature flags; crate terpisah per fitur |
| Aktivasi package | Eksplisit via `.plugin(...)`, tanpa auto-discovery runtime |
| CLI | Binary global untuk `new/serve/make:*`; `migrate/seed/queue:work` di-forward ke binary project user (pola Loco) |
| Cerita marketing | "Senyaman Laravel saat nulis, setenang Rust saat deploy" — error page Ignition + testing fakes + single binary |
| GitHub | Publish di bawah akun `advancedynamic` (akun terpisah; gh multi-account + HTTPS) |
