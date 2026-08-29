# Rustlavel — Roadmap

Framework web Rust full-stack yang terinspirasi Laravel (referensi: **Laravel 13** — skeleton slim, package opt-in, AI first-party), dibangun **from scratch** (tanpa Axum/SeaORM). Kemudahan Laravel, jaminan compile-time Rust.

## Prinsip desain

1. **Batteries included, tapi opt-in.** Scaffold `rustlavel new` hanya berisi yang standar (routing, config, CLI). Fitur lain ditambah per package: `cargo add rustlavel-db` ≈ `composer require` — tidak di-add berarti tidak dikompilasi sama sekali. Bisa juga dipilih saat scaffold: `rustlavel new blog --with db,view,auth`.
2. **Ambil rasanya Laravel, bukan mekanismenya.** Konvensi, struktur folder, Artisan-style CLI, ergonomi API — ya. Facade statis dan service container runtime — tidak; diganti dependency injection compile-time yang error-nya ketahuan saat kompilasi.
3. **Eksplisit di satu tempat.** Tidak ada auto-discovery runtime; package diaktifkan satu baris di `main.rs` (`.plugin(Telescope::default())`). Registry (migrations, dsb.) di-generate otomatis oleh CLI.
4. **Pesan error yang manusiawi** — dari macro sampai halaman error dev. DX adalah fitur, bukan polesan.
5. **Stabilitas API adalah fitur** (pelajaran Laravel 13: zero breaking changes).
6. **Kripto tidak ditulis sendiri.** Semua protokol ditulis dari nol (HTTP, PostgreSQL, RESP, SMTP, JSON-RPC), tetapi primitif kriptografi memakai crate (argon2, sha2, hmac, aes-gcm, rustls). Menulis cipher sendiri itu kerentanan, bukan prestasi.

## Struktur repo

```
rustlavel/
├── README.md           # perkenalan proyek
├── ROADMAP.md          # file ini
├── CLAUDE.md           # keperluan AI/coding agent (di luar kode)
├── examples/blog/      # aplikasi contoh (workspace sendiri)
└── framework/          # kode framework: Cargo workspace
    └── crates/
        ├── rustlavel/            # meta-crate + feature flags
        ├── rustlavel-core/       # config, .env, JSON, context, event bus, dispatcher
        ├── rustlavel-http/       # server HTTP, routing, middleware, error page, test client
        ├── rustlavel-cli/        # binary `rustlavel`
        ├── rustlavel-macros/     # #[derive(Model)]
        ├── rustlavel-db/         # driver PostgreSQL, query builder, migrations, ORM
        ├── rustlavel-view/       # template engine gaya Blade
        ├── rustlavel-validation/ # validasi gaya Laravel
        ├── rustlavel-auth/       # session, hashing, CSRF, signed URL, guard
        ├── rustlavel-cache/      # memory / file / Redis + rate limiting
        ├── rustlavel-client/     # HTTP client keluar + Http::fake()
        ├── rustlavel-storage/    # disk lokal + S3-compatible
        ├── rustlavel-i18n/       # terjemahan + deteksi locale
        ├── rustlavel-ai/         # Anthropic / OpenAI / Ollama
        ├── rustlavel-mcp/        # MCP server + client
        ├── rustlavel-telescope/  # dashboard debugging
        ├── rustlavel-metrics/    # Prometheus, dari event bus
        ├── rustlavel-openapi/    # dokumentasi API dari route
        ├── rustlavel-queue/      # background jobs + scheduler
        ├── rustlavel-ws/         # WebSocket + broadcasting
        └── rustlavel-mail/       # SMTP + notifications
```

Di luar `framework/` ada `examples/blog/` — aplikasi contoh lengkap (model, migration, controller, validasi, template, test) yang jalan melawan PostgreSQL sungguhan. Workspace-nya sendiri.

Model publish: satu repo banyak crate (seperti `laravel/framework` berisi `illuminate/*`), tiap crate dipublish terpisah ke crates.io.

---

## Fase 0.1 — MVP ✅ selesai

- [x] `rustlavel new <app>` — scaffold slim ala Laravel 13, plus `--with` untuk memilih package
- [x] `rustlavel serve` — dev server + hot reload
- [x] HTTP/1.1 server from scratch di atas Tokio TCP: keep-alive, HEAD, chunked, limits, graceful shutdown
- [x] Routing: param, group + prefix, named routes, wildcard, `resource()`, `url_for`
- [x] Middleware pipeline (global, per-group, per-route) dengan short-circuit
- [x] Request/Response ergonomis; config + loader `.env` sendiri; JSON sendiri (tanpa serde)
- [x] Context bertipe (pengganti service container); plugin hook untuk package opsional
- [x] `make:controller`, `make:middleware`, `route:list`
- [x] Error page dev ala Ignition; panic di handler jadi 500 (berlaku juga di test)
- [x] Test client tanpa socket, dengan cookie jar
- [x] Static files, health check `/up`, structured logging, event bus instrumentation

## Fase 0.2 — Database ✅ selesai

- [x] Driver PostgreSQL from scratch (protokol wire v3), SCRAM-SHA-256 terverifikasi vektor RFC 7677
- [x] Connection pool; transaksi bergaya guard + savepoint; rollback otomatis saat drop
- [x] Query builder aman injeksi (parameter terikat, identifier divalidasi, operator allowlist)
- [x] Penjaga: `update`/`delete` tanpa filter ditolak
- [x] Schema builder, migrations dengan batch + rollback + `fresh`, seeder, `Faker` deterministik
- [x] ORM `#[derive(Model)]` (proc-macro tulis tangan, tanpa syn/quote)
- [x] Relasi anti N+1: `has_many` / `belongs_to`
- [x] Pagination: nomor halaman dan cursor
- [x] CLI: `make:model`, `make:migration`, `make:seeder` + registry otomatis
- [x] 14 test integrasi melawan PostgreSQL 16 sungguhan

## Fase 0.3 — Web esensial ✅ selesai

- [x] Validation: 25 rule, sintaks string + builder bertipe, respons 422, `?` jalan di handler
- [x] Templating gaya Blade: `@extends/@section/@yield/@include/@if/@foreach`, escaping otomatis, reload saat dev
- [x] Auth: argon2, AES-GCM, signed URLs, session (memory/file), CSRF, guard, key derivation per-tujuan
- [x] Cache: memory / file / Redis (klien RESP from scratch) + rate limiting + throttle middleware
- [x] `IntoResponse` digeneralisasi: tiap tipe error menentukan bentuk responsnya sendiri
- [x] Pagination nyambung query builder
- [x] Testing: test client + cookie jar, `Http::fake()`
- [ ] Passkey/WebAuthn; starter kit `--auth` (login/register/reset) — belum
- [ ] Form HTML yang gagal validasi belum di-render ulang dengan pesan error; sekarang 422 teks polos (klien JSON sudah dapat bentuk Laravel)

## Fase 0.4 — AI & DX era agent ✅ selesai

- [x] `rustlavel-client`: HTTP client keluar dengan TLS, streaming, SSE, retry, `Http::fake()`
- [x] `rustlavel-ai`: Anthropic / OpenAI / Ollama lewat satu API; streaming, tool calling, structured output, `Ai::fake()`, API key tidak pernah bocor ke log
- [x] `rustlavel-mcp`: MCP server (stdio + HTTP) dan client; JSON-RPC from scratch; argumen tervalidasi sebelum handler; panic jadi `isError`
- [x] `make:mcp-tool`; `rustlavel new` menulis CLAUDE.md berisi konvensi untuk coding agent

## Fase 0.5 — Observability ✅ sebagian

- [x] `rustlavel-telescope`: rekam request, query, log, jobs, panggilan AI/MCP; dashboard `/telescope`; ditolak di production; redaksi field sensitif
- [x] Structured logging JSON untuk production
- [x] `rustlavel doctor` — diagnosa toolchain, .env, APP_KEY, port, database, layout, build
- [x] `rustlavel-metrics`: endpoint Prometheus dari event bus; request dilabeli pola route (bukan path) agar kardinalitas tidak meledak
- [ ] OpenTelemetry export — belum

## Fase 0.6 — Kelas berat ✅ sebagian

- [x] Storage: disk lokal + S3-compatible (SigV4 tulis sendiri, terverifikasi vektor AWS)
- [x] i18n: terjemahan, plural, deteksi locale
- [x] Events & listeners bertipe
- [x] Single binary deploy: `rustlavel build`, `make:docker`, cross-compile
- [x] `rustlavel-queue`: jobs, worker, retry, dead-letter, scheduler dengan parser cron sendiri
- [x] `rustlavel-mail`: SMTP from scratch, MIME, Mailable, `Mail::fake()`, notifications multi-channel
- [x] OpenAPI di-generate dari route: `.describe()/.tag()/.param()/.responds()` + halaman docs
- [x] Generator: `make:job`, `make:mail`, `make:notification`, `make:mcp-tool`
- [x] Perintah aplikasi: `migrate`, `migrate:rollback/fresh/status`, `db:seed`, `queue:work`, `queue:failed`, `schedule:run`
- [ ] Broadcasting/WebSocket ala Echo+Reverb — sedang dikerjakan
- [ ] Feature flags ala Pennant — belum
- [ ] `make:crud`, `tinker` — belum

## Fase 1.0+ — Ekosistem (sebagian)

- [x] Contoh app resmi: `examples/blog`
- [ ] Situs dokumentasi + cookbook
- [ ] `rustlavel make:package` — scaffold package pihak ketiga
- [ ] Komponen reaktif server-side ala Livewire

---

## Keputusan yang sudah diambil

| Keputusan | Pilihan |
|---|---|
| Referensi | Laravel 13 (struktur slim 11+, AI SDK, passkeys) |
| Fondasi | From scratch; hanya Tokio + crate kriptografi (argon2, sha2, hmac, aes-gcm, rustls) |
| Database pertama | PostgreSQL (protokol wire ditulis sendiri) |
| Distribusi | Meta-crate `rustlavel` + feature flags; crate terpisah per fitur |
| Aktivasi package | Eksplisit via `.plugin(...)`, tanpa auto-discovery runtime |
| Transaksi | Guard (`begin`/`commit`), bukan closure — lifetime future tidak bisa diekspresikan lewat closure |
| CLI | Binary global untuk `new/serve/make:*/doctor/build`; `migrate/seed/queue:work` di-forward ke binary project |
| Cerita marketing | "Senyaman Laravel saat nulis, setenang Rust saat deploy" |
| GitHub | `advancedynamic/rustlavel` (akun terpisah; gh multi-account + HTTPS) |
