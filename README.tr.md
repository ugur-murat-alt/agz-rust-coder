# agz-rust-coder

[![CI](https://github.com/ugur-murat-alt/agz-rust-coder/actions/workflows/ci.yml/badge.svg)](https://github.com/ugur-murat-alt/agz-rust-coder/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/agz-rust-coder.svg)](https://crates.io/crates/agz-rust-coder)
[![Lisans: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) | Türkçe

`agz-rust-coder`, derleyici çıktısını temel alan Rust çalışmaları için bağımsız
bir stdio MCP sunucusudur. Sınırlı Cargo doğrulaması çalıştırır, kaynak kodu
denetler, tam sürüme ait crate belgelerini çözer, Rust Analyzer ile gezinme
sağlar ve kaynağa yazmayan rename/refactor paketleri döndürür.

## Kimlik

| Sözleşme | Değer |
| --- | --- |
| Crate, binary, server | `agz-rust-coder` |
| MCP Registry | `io.github.ugur-murat-alt/agz-rust-coder` |
| Güncel sürüm | `0.1.1` |
| İlk sürüm | `0.1.0` |
| Release tag | `agz-rust-coder-v<version>` |
| Rust edition / MSRV | `2024` / `1.88.0` |
| Rust MCP SDK | `rmcp` `3.1.4` |
| Varsayılan / keşfedilen protokol | `2025-11-25` / `2026-07-28` |

MCP paket sahipliği kaydı: `mcp-name: io.github.ugur-murat-alt/agz-rust-coder`.

## Kurulum

```bash
cargo install agz-rust-coder --locked
agz-rust-coder --version
```

Paket crates.io üzerinden kaynak olarak dağıtılır. Release sayfalarında ayrıca
hazır derlenmiş arşivler ve SHA-256 sağlama toplamları bulunur.

## OpenCode

Kurulu binary'yi `opencode.jsonc` dosyasına ekleyin:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "rust": {
        "type": "local",
        "command": ["agz-rust-coder"],
        "cwd": ".",
        "codemode": false,
        "timeout": {
          "startup": 30000,
          "catalog": 30000,
          "execution": 720000
        }
      }
    }
  }
}
```

Kanonik çalışma dizini varsayılan yetkili köktür. İstemci başka yerde
başlatıyorsa tekrarlanan `--allow-root` argümanlarıyla açık kökler ekleyin.
İstemcinin MCP kökleri yapılandırılmış erişimi daraltabilir, genişletemez.

## Araçlar

OpenCode gruplanmış MCP araçlarını çoğunlukla `rust_*` adıyla gösterir.

| MCP tool | OpenCode direct name | Default | Amaç |
| --- | --- | --- | --- |
| `check` | `rust_check` | `enabled` | Sınırlı Cargo check, Clippy, test, docs veya tam kapıyı çalıştırır. |
| `audit` | `rust_audit` | `enabled` | Rust kaynağını sınırlı statik bulgular için tarar. |
| `crate_lookup` | `rust_crate_lookup` | `enabled` | Crate adını ve isteğe bağlı tam sürümü crates.io üzerinde doğrular. |
| `docs` | `rust_docs` | `enabled` | Tam sürüm belgesini cache, yerel kaynak veya docs.rs üzerinden çözer. |
| `symbol` | `rust_symbol` | `enabled` | Bir sembol için Rust Analyzer hover verisini okur. |
| `references` | `rust_references` | `enabled` | Sınırlı referansları bulur. |
| `definition` | `rust_definition` | `enabled` | Seçilen tanımı bulur. |
| `symbols` | `rust_symbols` | `enabled` | Bir Rust dosyasındaki sembolleri listeler. |
| `implementations` | `rust_implementations` | `enabled` | Uygulamaları bulur. |
| `hierarchy` | `rust_hierarchy` | `enabled` | Sınırlı çağrı hiyerarşisini izler. |
| `rename` | `rust_rename` | `enabled` | Uygulamadan doğrulanmış yeniden adlandırma paketi üretir. |
| `refactor` | `rust_refactor` | `enabled` | Uygulamadan doğrulanmış refactor paketi üretir. |

Her araç belirli yapıda veri ve ona eşdeğer, boyutu sınırlı metin döndürür. Dış
veri `untrustedData` altında tutulur. Derleme hatası, bulunamayan crate veya
erişilemeyen belge gibi beklenen sonuçlar tipli sonuçtur; geçersiz girdi, yetki
ihlali, kaynak tükenmesi ve semantik altyapı yokluğu protokol hatasıdır.

## Yapılandırma

Öncelik sırası CLI, `AGZ_RUST_CODER_*` ortam değişkenleri, açık `--config` TOML
dosyası ve varsayılanlardır. Ortam anahtarları bölümler arasında `__` kullanır;
örnek: `AGZ_RUST_CODER_GATE__HARD_TIMEOUT_MS=600000`.

| Key | Default | Anlam |
| --- | --- | --- |
| `server.allow_roots` | canonical CWD | Workspace okuma/komut sınırı. |
| `server.allow_dependency_roots` | empty | Açık dış path-dependency kökleri. |
| `gate.hard_timeout_ms` | `600000` | Cargo işlemi son süresi. |
| `gate.scope` | `shadow` | Doğrulama hedefi: `workspace`, `shadow` veya `affected`. |
| `gate.cache` | `auto` | Cache politikası: `auto`, `project` veya `isolated`. |
| `rust_analyzer.workspace_code` | `deny` | Workspace kodu kapatılamazsa RA başlatmayı reddeder. |
| `docs.fallback` | `auto` | Belge kaynağı politikası. |
| `limits.tool_output_bytes` | `49152` | Serileştirilmiş araç sonucu üst sınırı. |
| `telemetry.enabled` | `true` | Prompt veya kaynak içermeyen sınırlı yerel etkinlik kaydı. |

Tüm CLI alanları için `agz-rust-coder --help` çalıştırın. Tam davranış ve
varsayılan tablosu [docs/tools.tr.md](docs/tools.tr.md) içindedir.

## Güvenlik

Sunucu workspace kaynağını değiştirmez, ancak işletim sistemi sandbox'ı
değildir. Cargo build script'leri, testler, procedural macro'lar, yerel rustdoc
ve açıkça etkinleştirilen Rust Analyzer workspace kodu sunucu kullanıcısının
yetkileriyle çalışır. Daha güçlü sınır gerektiğinde container veya OS sandbox
kullanın.

- Workspace ve dependency yolları kanonikleştirilir ve şüphede kapalı kalır.
- Cache, lease, journal, docs ve telemetry yolları yetkili köklerle çakışamaz.
- Alt süreç çıktısı, HTTP gövdeleri, dizin yürüyüşleri, editler, task'lar ve
  telemetry sınırlıdır.
- `rename`, `refactor` ve biçim kontrolü yalnız veri döndürür.
- Stdout MCP çerçevelerine ayrılmıştır; log ve panic çıktısı stderr kullanır.

Güvenlik açıklarını [SECURITY.md](SECURITY.md) uyarınca özel bildirin.

## Geliştirme

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked --no-fail-fast
cargo +1.88.0 check --workspace --all-targets --all-features --locked
cargo build --release --locked
cargo run -p xtask -- protocol-smoke
cargo run -p xtask -- opencode-smoke
cargo run -p xtask -- benchmark-smoke
```

[CONTRIBUTING.md](CONTRIBUTING.md), [mimari](docs/architecture.tr.md),
[araç referansı](docs/tools.tr.md), [benchmark protokolü](docs/benchmark.tr.md) ve
[CHANGELOG.md](CHANGELOG.md) ayrıntıları içerir.

## Kanonik Bağlantılar

- Repository: https://github.com/ugur-murat-alt/agz-rust-coder
- Crate: https://crates.io/crates/agz-rust-coder
- SDK docs: https://docs.rs/rmcp/3.1.4/rmcp/
- MCP `2025-11-25`: https://modelcontextprotocol.io/specification/2025-11-25
- MCP `2026-07-28`: https://modelcontextprotocol.io/specification/2026-07-28

## Lisans

[MIT](LICENSE), Copyright (c) 2026 Ugur Murat Altintas.

Henüz yayımlanmamış akış tanıları, açık doğrulama seçenekleri ve isteğe bağlı hızlandırma için [altı başlık çalışmasına](docs/rust-efficiency-plan.tr.md) bakınız.
