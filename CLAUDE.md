# Rustlavel — panduan untuk AI/coding agent

Rustlavel adalah framework web Rust full-stack terinspirasi **Laravel 13**, dibangun **from scratch** (tanpa Axum/SeaORM; async runtime satu-satunya dependency besar: Tokio). Visi lengkap dan urutan pengerjaan ada di `ROADMAP.md` — baca itu dulu sebelum mengerjakan fitur.

## Struktur repo

- `ROADMAP.md` — visi, prinsip desain, fase pengerjaan, keputusan yang sudah diambil.
- `framework/` — **semua kode** (Cargo workspace). Jangan menaruh kode di luar folder ini.
- File AI/dokumentasi proses (seperti file ini) tinggal di root, di luar `framework/`.

## Workspace (`framework/crates/`)

- `rustlavel` — meta-crate yang di-import aplikasi; package opsional diaktifkan via feature flags. Yang tidak di-enable tidak dikompilasi.
- `rustlavel-core` — app lifecycle (`App`), config, loader `.env` sendiri, event/instrumentation internal (fondasi telescope & tracing).
- `rustlavel-http` — server HTTP/1.1 di atas Tokio TCP (bukan hyper), `Router`, middleware pipeline, Request/Response.
- `rustlavel-cli` — binary `rustlavel` (padanan artisan): `new`, `serve`, `make:*`. Perintah app-bound (`migrate`, `db:seed`, `queue:work`) di-forward ke binary project user (pola Loco).
- Crate baru per fitur (db, view, auth, ai, mcp, telescope, queue, ...) — satu crate per package, jangan digabung.

## Aturan desain (wajib)

1. **Jangan tambah dependency pihak ketiga** untuk hal yang bisa dibangun sendiri — from scratch adalah keputusan produk. Tokio boleh; Axum/hyper/SeaORM/dotenv tidak.
2. **Tanpa magic runtime.** Tidak ada reflection/auto-discovery; semua eksplisit di `main.rs` user atau di-generate CLI (registry migration, dsb). Gantinya facade Laravel: extractor/DI compile-time.
3. **DX adalah fitur.** Pesan error macro dan compiler harus manusiawi; API dirancang meniru *rasa* Laravel (`r.get(...)`, `Schema::create(...)`, `make:controller`).
4. **Package opt-in.** Fitur baru = crate baru + feature flag di meta-crate `rustlavel`, bukan penambahan di core.
5. Verifikasi dengan `cargo check` / `cargo test` dari dalam `framework/` sebelum menyatakan selesai.

## Konteks project

- GitHub target publish: akun `advancedynamic` (bukan `galihlasahido`); gh multi-account via HTTPS.
- Referensi framework: Laravel 13 (skeleton slim, AI SDK first-party, passkeys) dan Loco.rs sebagai pembanding kompetitor.
