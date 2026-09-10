# AGZ Rust MCP

[![CI](https://github.com/ugur-murat-alt/agz-rust-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/ugur-murat-alt/agz-rust-mcp/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/agz-rust-mcp.svg)](https://crates.io/crates/agz-rust-mcp)
[![Lisans: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) | Türkçe

**AGZ Yazılım ürünüdür.** `agz-rust-mcp`, Cargo ve rustc çıktısına dayanan
sınırlı Rust doğruluğu için bağımsız bir stdio MCP sunucusudur. Sınırlı Cargo
doğrulaması çalıştırır, kaynak kodu denetler, tam sürüme ait crate belgelerini
çözer, Rust Analyzer ile gezinme sağlar ve kaynağa yazmayan rename/refactor
paketleri döndürür.

## Kurulum

```bash
# npm wrapper (Node.js gerekir; platforma uyan sürüm binary'sini bulur)
npx -y @agz-yazilim/agz-rust-mcp --version

# kurulum betiği (Linux x86_64): install.sh ve SHA256SUMS dosyalarını en güncel
# sürümden indirin, betiği doğrulayın, sonra çalıştırın
bash install.sh

# crates.io (Rust 1.88.0 veya üzeri gerekir)
cargo install agz-rust-mcp --locked
agz-rust-mcp --version
```

Linux x86_64, macOS arm64 ve Windows x86_64 için `.sha256` dosyalı hazır
`.tar.gz` arşivleri vardır. `0.2.0` dahil o sürüme kadarki release'ler eski
`agz-rust-coder-*` varlık adlarını kullanır; `0.3.0` sürümünden itibaren adlar
`agz-rust-mcp-*` olur.

**Adım adım yönergeler, işletim sistemi notları, sağlama toplamı doğrulaması ve
sorun giderme: [docs/install.tr.md](docs/install.tr.md) ·
[English](docs/install.md).**

## MCP İstemci Ayarı

Minimal OpenCode2 yapılandırması (`opencode.jsonc`):

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "rust": {
        "type": "local",
        "command": ["agz-rust-mcp"],
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

Minimal Codex yapılandırması (`~/.codex/config.toml`):

```toml
[mcp_servers.rust]
command = "agz-rust-mcp"
args = []
```

Kanonik çalışma dizini varsayılan yetkili köktür. İstemci başka yerde
başlatılıyorsa tekrarlanan `--allow-root` argümanlarıyla açık kökler ekleyin;
istemcinin MCP kökleri yapılandırılmış erişimi daraltabilir, genişletemez.
Wrapper ile yönetilen istemci varyantları için
[docs/install.tr.md](docs/install.tr.md) dosyasına bakın.

## Belgeler

Belgeleri şu sırayla okuyun:

1. [Kurulum ve istemci ayarı](docs/install.tr.md) - tüm kurulum yöntemleri ve
   istemci yapılandırması.
2. [Araç ve yapılandırma referansı](docs/tools.tr.md) - araçlar, eylemler,
   sonuç anlamları ve tüm yapılandırma anahtarları.
3. [Mimari](docs/architecture.tr.md) - süreç modeli, protokol yaşam döngüsü ve
   yetki sınırları.
4. [Doğrulama ve benchmark protokolü](docs/benchmark.tr.md) - smoke'lar, kapılar
   ve kanıt düzeni.
5. [Güvenlik politikası](SECURITY.md) ve [Katkı](CONTRIBUTING.md).

Tam okuma yolunu içeren iki dilli dizin:
[docs/README.tr.md](docs/README.tr.md) / [docs/README.md](docs/README.md).

## Kimlik

| Sözleşme | Değer |
| --- | --- |
| Crate, binary, server | `agz-rust-mcp` |
| MCP Registry | `io.github.ugur-murat-alt/agz-rust-mcp` |
| Güncel sürüm | `0.2.0` |
| İlk sürüm | `0.1.0` |
| Release tag | `agz-rust-mcp-v<version>` |
| Rust edition / MSRV | `2024` / `1.88.0` |
| Rust MCP SDK | `rmcp` `3.1.4` |
| Varsayılan / keşfedilen protokol | `2025-11-25` / `2026-07-28` |

MCP paket sahipliği kaydı: `mcp-name: io.github.ugur-murat-alt/agz-rust-mcp`.

## Araçlar

OpenCode2 gruplanmış MCP araçlarını çoğunlukla `rust_*` adıyla gösterir.

| MCP tool | OpenCode2 direct name | Default | Amaç |
| --- | --- | --- | --- |
| `check` | `rust_check` | `enabled` | Sınırlı Cargo check, Clippy, test, docs veya tam kapıyı çalıştırır. |
| `profile` | `rust_profile` | `enabled` | Gözlenen Cargo yeniden derleme davranışını analiz eder ve ölçülmemiş hız iddiası kurmadan sınırlı derleme kanıtını karşılaştırır. |
| `audit` | `rust_audit` | `enabled` | Rust kaynağını sınırlı statik bulgular için tarar. |
| `crate_lookup` | `rust_crate_lookup` | `enabled` | Crate adını ve isteğe bağlı tam sürümü crates.io üzerinde doğrular. |
| `docs` | `rust_docs` | `enabled` | Tam sürüm belgesini cache, yerel kaynak veya docs.rs üzerinden çözer. |
| `context` | `rust_context` | `enabled` | Öğe başına nedeniyle revizyona bağlı anlamsal bağlam kapsülü hazırlar, genişletir veya farkını üretir. |
| `api` | `rust_api` | `enabled` | Sınırlı analiz kanıtıyla API imzasını çözer veya aday kod parçasını workspace yapılandırmasının yalıtılmış kopyasında tip kontrolünden geçirir. |
| `explain` | `rust_explain` | `enabled` | Makro açılım kaynağını, trait yükümlülüklerini veya cfg etkinliğini açıklar. |
| `verify` | `rust_verify` | `enabled` | Sınırlı feature, hedef, toolchain ve aşama matrisini planlar veya çalıştırır. |
| `symbol` | `rust_symbol` | `enabled` | Bir sembol için Rust Analyzer hover verisini okur. |
| `references` | `rust_references` | `enabled` | Sınırlı referansları bulur. |
| `definition` | `rust_definition` | `enabled` | Seçilen tanımı bulur. |
| `symbols` | `rust_symbols` | `enabled` | Bir Rust dosyasındaki sembolleri listeler. |
| `implementations` | `rust_implementations` | `enabled` | Uygulamaları bulur. |
| `hierarchy` | `rust_hierarchy` | `enabled` | Sınırlı çağrı hiyerarşisini izler. |
| `rename` | `rust_rename` | `enabled` | Uygulamadan doğrulanmış yeniden adlandırma paketi üretir. |
| `refactor` | `rust_refactor` | `enabled` | Uygulamadan doğrulanmış refactor paketi üretir. |
| `change` | `rust_change` | `enabled` | Workspace'e yazmadan sunucuya ait scratch alanında revizyona bağlı changeset oluşturur, uygular, göç ettirir ve doğrular. |
| `repair` | `rust_repair` | `enabled` | Başarısız bir change revizyonu için derleyici güdümlü onarım adaylarını analiz eder, dener, karşılaştırır ve küçültür; workspace'e yazmaz. |
| `work` | `rust_work` | `enabled` | Tipli bir intent'i açık kapılar ve bütçelerle change/validate üzerinden yürütür; dürüst kapı kanıtı veya sınırlı tek kullanımlık handoff döndürür. |

Her araç belirli yapıda veri ve ona eşdeğer, boyutu sınırlı metin döndürür. Dış
veri `untrustedData` altında tutulur. Derleme hatası, bulunamayan crate veya
erişilemeyen belge gibi beklenen sonuçlar tipli sonuçtur; geçersiz girdi, yetki
ihlali, kaynak tükenmesi ve semantik altyapı yokluğu protokol hatasıdır.

## Yapılandırma

Öncelik sırası CLI, `AGZ_RUST_MCP_*` ortam değişkenleri, açık `--config` TOML
dosyası ve varsayılanlardır. Ortam anahtarları bölümler arasında `__` kullanır;
örnek: `AGZ_RUST_MCP_GATE__HARD_TIMEOUT_MS=600000`.

| Key | Default | Anlam |
| --- | --- | --- |
| `server.allow_roots` | canonical CWD | Workspace okuma/komut sınırı. |
| `server.allow_dependency_roots` | empty | Açık dış path-dependency kökleri. |
| `gate.hard_timeout_ms` | `600000` | Cargo işlemi son süresi. |
| `gate.scope` | `shadow` | Doğrulama hedefi: `workspace`, `shadow` veya `affected`. |
| `gate.cache` | `auto` | Cache politikası: `auto`, `project` veya `isolated`. |
| `rust_analyzer.workspace_code` | `deny` | Workspace kodu kapatılamazsa RA başlatmayı reddeder. |
| `docs.fallback` | `auto` | Belge kaynağı politikası. |
| `profile.max_report_bytes` | `4194304` | Tek Cargo zamanlama artifact'ı için sınırlı okuma/saklama üst sınırı. |
| `profile.max_runs` | `4` | Bir `profile` çağrısında kullanılabilen taze Cargo koşusu. |
| `profile.compare_samples` | `3` | Hız iddiası öncesi her taraf için gereken örnek sayısı. |
| `limits.tool_output_bytes` | `49152` | Serileştirilmiş araç sonucu üst sınırı. |
| `change.max_bytes` | `268435456` | Changeset başına yakalanan aday byte üst sınırı. |
| `repair.max_candidates` | `4` | Bir `repair` işleminin deneyebileceği aday sayısı. |
| `repair.max_compiles` | `4` | Bir `repair` işleminin çalıştırabileceği Cargo doğrulaması. |
| `repair.wall_time_ms` | `120000` | Bir `repair` işlemi için duvar saati bütçesi. |
| `repair.minimize_max_candidates` | `32` | Bir `repair(action=minimize)` işleminin derlemeyle değerlendirebileceği küçültme denemesi. |
| `repair.minimize_max_compiles` | `16` | Bir `repair(action=minimize)` işleminin yeniden üretim ve dışa aktarma doğrulaması dahil çalıştırabileceği Cargo koşusu. |
| `work.max_candidates` | `4` | Bir work öğesinin stage edebileceği host aday revizyonu. |
| `work.max_compiles` | `12` | Bir work öğesinin çalıştırabileceği kapı doğrulaması. |
| `work.wall_time_ms` | `600000` | Bir work öğesi için duvar saati bütçesi. |
| `telemetry.enabled` | `true` | Prompt veya kaynak içermeyen sınırlı yerel etkinlik kaydı. |

`profile` kanıtı sınırlıdır: en güncel 64 kayıt bellekte tutulur ve sunucuya ait
`profile-evidence` dizinindeki zamanlama artifact'ları 24 saat sonra veya 64
dosyayı aşınca temizlenir. `profile` sonucu bu saklama politikasını görünür
kılar; süresi dolan kanıt sessizce yeniden kullanılmaz.

Tüm CLI alanları için `agz-rust-mcp --help` çalıştırın. Tam davranış ve
varsayılan tablosu [docs/tools.tr.md](docs/tools.tr.md) içindedir.

## Güvenlik

Sunucu workspace kaynağını değiştirmez, ancak işletim sistemi sandbox'ı
değildir. Cargo build script'leri, testler, procedural macro'lar, yerel rustdoc
ve açıkça etkinleştirilen Rust Analyzer workspace kodu sunucu kullanıcısının
yetkileriyle çalışır. Daha güçlü sınır gerektiğinde container veya OS sandbox
kullanın. Workspace, dependency, cache, lease, journal, docs ve telemetry
yolları kanonikleştirilir, sınırlandırılır ve şüphede kapalı kalır; stdout
yalnız MCP çerçevelerine ayrılmıştır. Güvenlik açıklarını [SECURITY.md](SECURITY.md)
uyarınca özel bildirin.

## Bağlantılar

- Repository: https://github.com/ugur-murat-alt/agz-rust-mcp
- Crate: https://crates.io/crates/agz-rust-mcp
- SDK docs: https://docs.rs/rmcp/3.1.4/rmcp/
- MCP `2025-11-25`: https://modelcontextprotocol.io/specification/2025-11-25
- MCP `2026-07-28`: https://modelcontextprotocol.io/specification/2026-07-28

## Lisans

[MIT](LICENSE), Copyright (c) 2026 Ugur Murat Altintas.
