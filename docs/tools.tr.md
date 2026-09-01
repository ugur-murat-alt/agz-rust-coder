# Araç ve Yapılandırma Referansı

Bu belge `agz-rust-coder` `0.1.0` sürümünün açık araç ve yapılandırma yüzeyini
tanımlar.

## Araç Kataloğu

| Tool | Authority | Side effects | Sonuç |
| --- | --- | --- | --- |
| `check` | Cargo/rustc | Sınırlı target dizininde derleme yapabilir | Doğrulama durumu, komut kanıtı, tanılar ve zamanlama verisi. |
| `audit` | Advisory scanner | Yetkili Rust dosyalarını okur | Sınırlı bulgular ve atlanan dosya nedenleri. |
| `crate_lookup` | crates.io | Sınırlı HTTPS isteği | `FOUND`, `NOT_FOUND`, `VERSION_MISMATCH` veya `UNAVAILABLE`. |
| `docs` | rustdoc/docs.rs | Cache, ağ veya yerel `cargo doc` kullanabilir | Tam sürüm alıntısı ve kaynak bilgisi ya da tipli erişilememe. |
| `symbol` | Rust Analyzer | Workspace-code politikasına bağlı | Hover metni ve seçilen konum. |
| `references` | Rust Analyzer | Workspace-code politikasına bağlı | Sınırlı referans konumları. |
| `definition` | Rust Analyzer | Workspace-code politikasına bağlı | Seçilen tanım konumu. |
| `symbols` | Rust Analyzer | Workspace-code politikasına bağlı | Sınırlı dosya sembolleri. |
| `implementations` | Rust Analyzer | Workspace-code politikasına bağlı | Sınırlı uygulama konumları. |
| `hierarchy` | Rust Analyzer | Workspace-code politikasına bağlı | Sınırlı gelen/giden çağrı grafiği. |
| `rename` | Rust Analyzer | Kaynağa asla yazmaz | Doğrulanmış `old_string`/`new_string` edit paketi. |
| `refactor` | Rust Analyzer | Kaynağa asla yazmaz | Doğrulanmış, yazmasız refactor paketi. |

`check` hedefleri `check`, `clippy`, `test`, `doc`, `fmt` ve `all` değerleridir.
Biçimlendirme yalnız kontrol kipinde çalışır. Tamamlanmış açık bir doğrulama daha
sonraki istek için yetki kanıtı olarak yeniden kullanılmaz; yalnız aynı anda
çalışan özdeş işe katılım mümkündür.

Tüm araçlar `limits.tool_output_bytes` içinde eşdeğer belirli yapıdaki veri ve
metin döndürür. Uzak gövdeler ve alıntılar ayrıştırmadan önce sınırlandırılır.
Dış içerik `untrustedData` altında verilir ve sunucu talimatına eklenmez.

## Sonuç Anlamları

Beklenen alan sonuçları tipli durum içeren başarılı MCP çağrılarıdır:

- derleyici veya test hatası: `FAIL`;
- crate yokluğu, sürüm uyuşmazlığı veya registry kesintisi: `NOT_FOUND`,
  `VERSION_MISMATCH` veya `UNAVAILABLE`;
- bulunamayan veya belirsiz sembol: `NOT_FOUND` veya `AMBIGUOUS`;
- belge fallback tükenmesi: tipli erişilememe verisi.

Geçersiz argüman, yetkisiz yol, kaynak sınırı, timeout ve semantik altyapı
yokluğu `isError=true` kullanır. Metin ve belirli yapıdaki durum aynı olmalıdır.

## Task ve İptal

`check` ve `docs`, uzlaşıldığında MCP task'larını destekler. Sunucu ilerleme
bildirir, `tasks/cancel` kabul eder; istek, root-epoch ve kapanma iptallerini
aktarır; terminal task durumunu sınırlı saklama sonrasında kaldırır. Task
desteklemeyen istemciler için eşzamanlı fallback korunur.

## Yapılandırma Kaynakları

Öncelik CLI, `AGZ_RUST_CODER_*` ortamı, açık TOML ve varsayılanlardır. Listeler
alt öncelikli değerleri eklemek yerine değiştirir. Bilinmeyen TOML veya ortam
anahtarı başlangıcı reddeder.

Ortam değişkenleri alanı büyük harfe çevirir ve bölümler arasında `__` kullanır:
`gate.hard_timeout_ms`, `AGZ_RUST_CODER_GATE__HARD_TIMEOUT_MS` olur. Root listeleri
platformun path-list ayırıcısını kullanır.

## Yapılandırma Referansı

| Key | Default | Notlar |
| --- | --- | --- |
| `server.allow_roots` | canonical CWD | Birincil yetkili workspace kökleri. |
| `server.allow_dependency_roots` | empty | Dış path-dependency kökleri. |
| `tools.check` | `true` | `check` kaydı. |
| `tools.audit` | `true` | `audit` kaydı. |
| `tools.crate_lookup` | `true` | `crate_lookup` kaydı. |
| `tools.docs` | `true` | `docs` kaydı. |
| `tools.lsp` | `true` | Semantik gezinme araçları kaydı. |
| `tools.rename` | `true` | LSP açıksa `rename` kaydı. |
| `tools.refactor` | `true` | LSP açıksa `refactor` kaydı. |
| `cargo.path` | PATH `cargo` | İsteğe bağlı Cargo binary değişimi. |
| `gate.hard_timeout_ms` | `600000` | Tek Cargo işlemi son süresi. |
| `gate.debounce_ms` | `500` | Kararlı girdi bekleme süresi. |
| `gate.host_concurrency` | `1` | Host genelindeki Cargo izinleri. |
| `gate.scope` | `shadow` | `workspace`, `shadow` veya `affected`. |
| `gate.cache` | `auto` | `auto`, `project` veya `isolated`. |
| `gate.min_free_disk_mb` | `1024` | Ön kontrol disk tabanı. |
| `gate.min_available_memory_mb` | `512` | Ön kontrol bellek tabanı. |
| `gate.cache_dir` | platform `agz-rust-coder/state/gate` | Sunucuya ait Cargo cache. |
| `gate.lease_dir` | platform `agz-rust-coder/state/leases` | Host lease ve süreç journal'ı. |
| `rust_analyzer.path` | PATH or rustup | İsteğe bağlı binary değişimi. |
| `rust_analyzer.timeout_ms` | `30000` | Semantik istek son süresi. |
| `rust_analyzer.idle_ms` | `900000` | Boş süreç ömrü. |
| `rust_analyzer.max_instances` | `2` | Eşzamanlı workspace süreci. |
| `rust_analyzer.check_hint` | `false` | RA check ipuçlarına izin verir. |
| `rust_analyzer.workspace_code` | `deny` | `deny` veya açık `allow`. |
| `docs.timeout_ms` | `300000` | Belge çözümleme son süresi. |
| `docs.fallback` | `auto` | `auto`, `local`, `network` veya `off`. |
| `docs.cache_dir` | platform `agz-rust-coder/docs` | Sunucuya ait docs cache. |
| `limits.max_rename_edits` | `200` | Rename edit sınırı. |
| `limits.max_refactor_edits` | `200` | Refactor edit sınırı. |
| `limits.process_output_bytes` | `8388608` | Birleşik alt süreç çıktı sınırı. |
| `limits.tool_output_bytes` | `49152` | MCP araç sonucu sınırı. |
| `limits.max_in_flight_tools` | `32` | Eşzamanlı araç kabulü. |
| `limits.max_active_tasks` | `16` | Çalışan task sınırı. |
| `limits.max_retained_tasks` | `128` | Terminal task sınırı. |
| `limits.identity_files` | `20000` | Girdi kimliği dosya sınırı. |
| `limits.identity_file_bytes` | `33554432` | Kimlik dosyası başına sınır. |
| `limits.identity_total_bytes` | `268435456` | Toplam kimlik byte sınırı. |
| `limits.external_files` | `5000` | Dış dependency dosya sınırı. |
| `limits.external_bytes` | `67108864` | Dış dependency byte sınırı. |
| `limits.git_output_bytes` | `8388608` | Git kanıtı sınırı. |
| `limits.audit_files` | `10000` | Audit dosya sınırı. |
| `limits.audit_file_bytes` | `2097152` | Audit dosyası başına sınır. |
| `limits.audit_total_bytes` | `67108864` | Toplam audit byte sınırı. |
| `limits.audit_findings` | `200` | Audit bulgu sınırı. |
| `telemetry.enabled` | `true` | Yerel etkinlik kaydını açar. |
| `telemetry.path` | platform `agz-rust-coder/state/activity.jsonl` | Sunucuya ait JSONL yolu. |
| `telemetry.retention_bytes` | `8388608` | Döndürme eşiği. |
| `telemetry.retention_days` | `7` | Gün cinsinden saklama. |
| `telemetry.max_archives` | `3` | Arşiv sınırı. |

Sunucuya ait yollar yetkili workspace veya dependency köküyle çakışamaz.
Telemetry sınırlı işlem metadata'sı tutar; ham prompt, özel kaynak, araç argümanı,
ham yol veya session kimliği saklamaz.

## Rust Analyzer Politikası

Varsayılan `rust_analyzer.workspace_code=deny` profili çalışan sunucunun şemasını
denetler; build script, procedural macro ve check-on-save özelliklerini kapatır.
Bu doğrulanamazsa semantik araçlar süreci başlatmadan erişilememe döndürür.
`allow`, workspace kodu çalıştırmak için açık tercihtir.

## İlgili Belgeler

- [README](../README.tr.md)
- [Mimari](architecture.tr.md)
- [Benchmark protokolü](benchmark.tr.md)
- [Güvenlik politikası](../SECURITY.md)
