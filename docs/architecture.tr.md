# Mimari

`agz-rust-coder`; RMCP stdio adaptörü, sınırlı alan servisleri ve denetimli dış
süreçlerden oluşan tek bir Rust sürecidir. Uzak transport bağlantısı kabul etmez
ve workspace kaynağına yazmaz.

## Veri Akışı

1. `main` katı yapılandırmayı yükler ve RMCP'yi stdin/stdout üzerinde başlatır.
2. Sunucu varsayılan olarak `2025-11-25` protokolüyle uzlaşır ve
   `2026-07-28` desteğini keşfedebilir.
3. Client root'ları yapılandırılmış kök kümesini daraltır; her workspace ve
   dependency yolu root guard tarafından denetlenir.
4. Admission control eşzamanlı araçları ve task'ları sınırlar.
5. Alan servisleri Cargo, statik audit, belge veya Rust Analyzer işlemlerini
   ortak süreç denetimi üzerinden çalıştırır.
6. Yanıtlar tek wire-size sınırı içinde belirli yapıda veri ve eşdeğer metne
   dönüştürülür.
7. Kapanma yeni işi durdurur, task'ları iptal eder, Rust Analyzer'ı kapatır,
   denetimli süreç gruplarını/job'ları sonlandırır, telemetry'yi yazar ve eksik
   temizliği hata olarak bildirir.

## Bileşenler

| Component | Responsibility |
| --- | --- |
| `server` | MCP araçları, kaynaklar, prompt'lar, task'lar, ilerleme, yanıt eşitliği. |
| `workspace` | Root yetkisi, paket seçimi, metadata, girdi kimliği. |
| `gate` | Cargo hedefleri, ön kontrol, singleflight, cache, host lease'leri. |
| `process` | Sınırlı çıktı, son süre, süreç grubu/Job Object, kurtarma journal'ı. |
| `lsp` | Rust Analyzer yaşam döngüsü, belge eşleme, gezinme, yazmasız edit. |
| `docs` | Lockfile çözümü, cache, source/docs.rs/yerel fallback. |
| `tools` | Doğrulama, audit, crate lookup, semantik alan işlemleri. |
| `telemetry` | Sınırlı yerel etkinlik kaydı ve atomik döndürme. |

## Protokol Yaşam Döngüsü

Sunucu tools, resources, prompts, roots, progress, cancellation ve tasks
yüzeylerini sunar. Task destekli istekler `CreateTaskResult` döndürür; polling ve
`tasks/cancel`, uzlaşılan RMCP task modelini kullanır. Task desteği olmayan
istemci aynı alan işlemini eşzamanlı alır.

Root değişikliği epoch değerini artırır. Eski root nesline bağlı iş iptal edilir
ve root'a duyarlı cache yetkisi geçersiz olur. Client root'ları kimlik doğrulama
değildir ve yapılandırılmış köklerde olmayan yolu yetkilendiremez.

## Yetki Modeli

Doğrulamayı Cargo ve rustc çıktısı belirler. Rust Analyzer ve kaynak audit'i
tavsiye niteliğindedir. Başarılı sonuç gözlenen worktree/girdi kimliğine bağlıdır
ve sonraki açık doğrulama isteğinde yeniden kullanılmaz.

Semantik rename/refactor yanıtları sınırlı edit paketleri içerir. Sunucu yol
sınırını, dosya sürümünü, çakışmayı ve eski metni doğrular ama değişikliği
uygulamaz.

## Süreç ve Depolama Sınırları

Unix komutları process group, Windows komutları Job Object kullanır. Timeout ve
kapanma önce nazik durdurmayı, ardından sınırlı zorla sonlandırma ve reap
işlemini dener. Temizliğin tamamlandığı kanıtlanamazsa journal kaydı tutulur.

Sunucuya ait durum, yetkili köklerin dışında platformun `agz-rust-coder` ad
alanında yaşar. Cache yayını lock korumalı, aynı dizinde atomik değiştirme
kullanır. Symlink veya üst dizin kimliği belirsizliği şüphede kapalı kalır.

## Kalan Risk

Bu mimari sandbox değildir. Cargo build script'leri, testler, procedural
macro'lar, yerel rustdoc ve açıkça izin verilen Rust Analyzer workspace kodu
işletim sistemi hesabının dosya, süreç ve ağ haklarını kullanabilir. Unix alt
süreçleri kasıtlı olarak process group dışına çıkabilir. Daha güçlü güvence için
container veya OS sandbox gerekir.

## Dağıtım

Crate ve binary adı `agz-rust-coder` değeridir. Release tag'leri
`agz-rust-coder-v<version>` biçimindedir. Resmî MCP Registry kimliği
`io.github.ugur-murat-alt/agz-rust-coder` olup tam crates.io paket sürümü ve
repository'deki `server.json` tarafından desteklenir.
