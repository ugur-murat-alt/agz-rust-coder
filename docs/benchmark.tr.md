# Doğrulama ve Benchmark Protokolü

**AGZ Yazılım ürünüdür.** Bu belge
[belge okuma yolunun](README.tr.md) 4. adımıdır: release iddialarının belirli
yerel kapılar ve isteğe bağlı canlı benchmark'larla nasıl doğrulandığını
tanımlar. Sonraki adım [güvenlik politikası](../SECURITY.md) ve
[katkı kılavuzudur](../CONTRIBUTING.md).

Release iddiaları önce belirli yerel kapıları kullanır. Canlı model
benchmark'ları isteğe bağlı ölçümlerdir; derleyici, protokol, paket veya güvenlik
kontrollerinin yerine geçmez.

## Provider-Free Smoke'lar

```bash
cargo run -p xtask -- protocol-smoke
cargo run -p xtask -- opencode-smoke
cargo run -p xtask -- benchmark-smoke
cargo run -p xtask -- task-benchmark-smoke
```

`protocol-smoke` gerçek stdio binary'sini başlatır; initialize, araç keşfi,
belirli yapı/metin eşitliği, `2026-07-28` task oluşturma, ilerleme, iptal,
terminal durum, eşzamanlı fallback ve fixture temizliğini denetler.

`opencode-smoke`, pinli OpenCode host'u doğrudan ve gruplanmış yerel MCP
yapılandırmalarıyla çalıştırır. Loopback sahte provider belirli araç çağrıları
döndürür; ücretli veya dış model endpoint'i kullanılmaz.

`benchmark-smoke`, dondurulmuş temiz ve bozuk Rust fixture'larını oracle'a göre
çalıştırır. Durum ile `passed` alanının uyumunu ve güncel davranışın korunmuş
benchmark sözleşmesine eşitliğini doğrular.

`task-benchmark-smoke`, aynı benchmark altyapısını dondurulmuş
`rust-agent-tasks-v1` korpusuyla genişletir. Sekiz Rust görev sınıfı, üç farklı
sıra seed'iyle üç eşleştirilmiş ve sıra-konumu dengeli tekrar üzerinden oynatılır:
A) olağan shell/dosya çalışması, B) korunmuş `0.2.0` MCP yüzeyi ve C) Rust Change
Engine sözleşmesi. Görev isteği yalnız herkese açık görev verisini içerir;
başarıyı bağımsız oracle gözlemleri ve mutation guard belirler. Replay başarısız,
timeout ve iptal edilmiş denemeleri rapordan atmaz.

Provider-free replay bir harness fixture'ıdır; ölçülmüş model performansı
değildir. Süre, Cargo çağrısı, yeniden derleme ve host turu değerleri toplama ve
kapı mantığını sınamak için bulunur. Input/output/cache/schema token sayıları ile
maliyet bilinmediğinde sıfır değil `unknown` olarak kaydedilir. Gerçek
provider/model ölçümleri açık opt-in gerektirir.

Dondurulmuş v1 korpusu; eksik trait implementasyonu, borrow/move onarımı, exact
crate API kullanımı, çok-crate imza göçü, yalnız feature altında bozulan kod,
regresyon testi ekleme, bağımlılık yükseltme ve davranışı koruyan performans
çalışmasını kapsar. Normal workspace CI test komutuna dahil olan
`xtask/tests/task_benchmark.rs`; korpus hash'lerini, replay bütünlüğünü, bağımsız
puanlamayı, unknown kullanım semantiğini, negatif kontrolleri ve dengeli sıralamayı
doğrular. CI ayrıca platform matrisi üzerinde `task-benchmark-smoke` komutunu
doğrudan çalıştırarak transcript replay ve kanıt yayınlama yolunu ürün akışı
olarak sınar.

## Önceden Belirlenmiş Görev Benchmark Kapıları

Change Engine uygulanmadan önce korpus şu karşılaştırma kurallarını sabitler:

- kalite: C, B'nin altına düşemez; v1 için `non_inferiority_margin` 0 yüzde
  puandır;
- verim: C, B'ye göre en az %20 daha az host turu ve %15 daha az Cargo çağrısı
  hedefler;
- duvar saati: C, B'ye göre en fazla %10 gerileyebilir.

Her kol örnek sayısını, Wilson %95 aralığıyla başarı oranını, duvar saati
dağılımını, CPU süresini, snapshot hazırlığını, Cargo/yeniden derleme sayılarını,
host turlarını, cold/warm cache katmanlarını, token alanlarını, maliyeti, timeout
ve iptal sonuçlarını raporlar. Provider-free replay bu kapıların harness
tarafından doğru değerlendirildiğini kanıtlayabilir; Change Engine'i default-on
yapamaz. Bu karar için karşılaştırılabilir açık opt-in canlı çalışmalar gerekir.

Ham replay gözlemleri `benchmark/task-corpus/provider-free-replay.json`
dosyasındadır. Dondurulmuş görev manifesti, gömülü `fixtures.json` workspace
snapshot'ları ve bağımsız oracle kataloğu aynı dizindedir. Fixture ve ayar
hash'leri sonucu tam korpusa bağlar. Kaydedilen provenance; provider, model,
harness sürümü, MCP SHA, fixture hash, toolchain, OS/donanım, cache durumu ve
ayar hash'ini içerir.

## Kanıt Düzeni

Her çalışma `benchmark/results/stage7/` altında atomik yayımlanır:

- `run.json`: `run_id`, kip, fixture, protokol ve adapter metadata'sı;
- `results.json`: gözlemler ve pass/fail doğrulamaları;
- `report.md`: insan tarafından okunabilen sınırlı özet;
- `provenance.json`: `source_commit`, `source_checksum`, kirli durum ve komut
  kimliği.

Raporlarda prompt, mutlak workspace yolu, session ID, kimlik bilgisi veya özel
kaynak bulunmaz. Eşzamanlı yayıncılar lock ve benzersiz final dizini kullanır.

## Canlı Kip

Canlı kip, ücret oluşturabileceği için açıkça incelenmiş adapter ve açık operatör
kararı gerektirir:

```bash
AGZ_RUST_MCP_LIVE_ADAPTER=/absolute/path/to/reviewed-adapter \
  cargo run -p xtask -- benchmark-smoke --live
```

Manifest `provider` / `model` / `variant`, tekrar, fixture, varsa maliyet ve
`non_inferiority_margin` kaydını tutar. Boolean pass alanı tipli durumuyla
çelişen adapter çıktısı reddedilir. Farklı kaynak veya adapter checksum'larına
ait sonuçlar birleştirilmez.

## Release Kapısı

En küçük yerel release kapısı:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked --no-fail-fast
cargo +1.88.0 check --workspace --all-targets --all-features --locked
cargo build --release --locked
cargo package -p agz-rust-mcp --locked
cargo publish -p agz-rust-mcp --dry-run --locked
```

Provider-free smoke'lar, gerçek pinli Rust Analyzer/docs adapter'ları,
`cargo deny check`, workflow lint ve secret/vulnerability taramaları release
kanıtını tamamlar. Platform CI, Linux iş istasyonunda çalıştırılamayan macOS ve
Windows süreç/yol kapsamını sağlar.

## Girdi kimliği karşılaştırması (henüz yayımlanmadı)

`crates/agz-rust-mcp/examples/identity_measure.rs` örneğini aynı toolchain/profil
ile eski ve yeni kod üzerinde derleyin. İki binary ile
`python3 benchmark/identity_compare.py BASELINE CANDIDATE --output comparison.json`
komutunu çalıştırın. Betik özdeş örnekler üretir, çalışma sırasını dönüşümlü seçer,
üç ısınma sonrası varsayılan 15 örnek kaydeder. Tek kaynak ve manifest değişikliği
ayrı senaryolardır. Hash eşitsizliğinde karşılaştırma reddedilir.
`python3 -m unittest discover -s benchmark -p test_identity_compare.py` bu reddetme
kurallarını sınar. Ölçülen aşamada LLM, ağ veya Cargo çalıştırılmaz.
[Kaydedilmiş kanıt](rust-efficiency-evidence.md).
