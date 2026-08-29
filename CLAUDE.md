# Rustlavel — panduan untuk AI/coding agent

Rustlavel adalah framework web Rust full-stack terinspirasi **Laravel 13**, dibangun **from scratch** (tanpa Axum/SeaORM; async runtime satu-satunya dependency besar: Tokio). Visi lengkap dan urutan pengerjaan ada di `ROADMAP.md` — baca itu dulu sebelum mengerjakan fitur.

## Struktur repo

- `ROADMAP.md` — visi, prinsip desain, fase pengerjaan, keputusan yang sudah diambil.
- `framework/` — **semua kode** (Cargo workspace). Jangan menaruh kode di luar folder ini.
- File AI/dokumentasi proses (seperti file ini) tinggal di root, di luar `framework/`.

## Workspace (`framework/crates/`)

- `rustlavel` — meta-crate yang di-import aplikasi; package opsional diaktifkan via feature flags. Yang tidak di-enable tidak dikompilasi. `App` builder ada di sini.
- `rustlavel-core` — config, loader `.env` sendiri, `Json`, `Context` bertipe, event bus instrumentasi, dispatcher event aplikasi.
- `rustlavel-http` — server HTTP/1.1 di atas Tokio TCP (bukan hyper), `Router`, middleware, error page, `TestClient`, trait `Plugin`.
- `rustlavel-cli` — binary `rustlavel`: `new` (dengan `--with`), `serve`, `make:*`, `doctor`, `build`, `key:generate`. Perintah app-bound (`route:list`, `migrate`, `db:seed`, `queue:work`) di-forward ke binary project user (pola Loco).
- `rustlavel-macros` — `#[derive(Model)]`, proc-macro tulis tangan (tanpa syn/quote).
- Package opsional: `-db`, `-view`, `-validation`, `-auth`, `-cache`, `-client`, `-storage`, `-i18n`, `-ai`, `-mcp`, `-telescope`, `-queue`, `-mail`. Satu crate per package, jangan digabung.

## Aturan desain (wajib)

1. **Jangan tambah dependency pihak ketiga** untuk hal yang bisa dibangun sendiri — from scratch adalah keputusan produk. Tokio boleh; Axum/hyper/SeaORM/dotenv/serde/regex/syn tidak. **Pengecualian: kriptografi.** argon2, sha2, hmac, aes-gcm, rustls/tokio-rustls/webpki-roots boleh dan harus dipakai — menulis cipher, KDF, MAC atau TLS sendiri itu kerentanan, bukan prestasi. Tulis komentar di Cargo.toml yang menjelaskan kenapa.
2. **Tanpa magic runtime.** Tidak ada reflection/auto-discovery; semua eksplisit di `main.rs` user atau di-generate CLI (registry migration, dsb). Gantinya facade Laravel: extractor/DI compile-time.
3. **DX adalah fitur.** Pesan error macro dan compiler harus manusiawi; API dirancang meniru *rasa* Laravel (`r.get(...)`, `Schema::create(...)`, `make:controller`).
4. **Package opt-in.** Fitur baru = crate baru + feature flag di meta-crate `rustlavel`, bukan penambahan di core.
5. **Urutan pembuatan crate baru:** buat `src/lib.rs` di langkah yang sama dengan `Cargo.toml`, jangan lebih dulu. Direktori crate ber-Cargo.toml tanpa target membuat seluruh workspace gagal dimuat.
6. **Test berjalan paralel.** Jangan berbagi direktori fixture, port tetap, atau state global antar test; beri nama unik per test, atau serialisasi dengan `static Mutex` bila state-nya proses-wide.
7. Verifikasi dengan `cargo test --workspace --all-features` dan `cargo clippy --workspace --all-features --all-targets` (harus nol warning) dari dalam `framework/` sebelum menyatakan selesai.

## Konteks project

- GitHub target publish: akun `advancedynamic` (bukan `galihlasahido`); gh multi-account via HTTPS.
- Referensi framework: Laravel 13 (skeleton slim, AI SDK first-party, passkeys) dan Loco.rs sebagai pembanding kompetitor.
