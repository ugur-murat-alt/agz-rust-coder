# Doğrulama ve Benchmark Protokolü

Release iddiaları önce belirli yerel kapıları kullanır. Canlı model
benchmark'ları isteğe bağlı ölçümlerdir; derleyici, protokol, paket veya güvenlik
kontrollerinin yerine geçmez.

## Provider-Free Smoke'lar

```bash
cargo run -p xtask -- protocol-smoke
cargo run -p xtask -- opencode-smoke
cargo run -p xtask -- benchmark-smoke
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
AGZ_RUST_CODER_LIVE_ADAPTER=/absolute/path/to/reviewed-adapter \
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
cargo package -p agz-rust-coder --locked
cargo publish -p agz-rust-coder --dry-run --locked
```

Üç provider-free smoke, gerçek pinli Rust Analyzer/docs adapter'ları,
`cargo deny check`, workflow lint ve secret/vulnerability taramaları release
kanıtını tamamlar. Platform CI, Linux iş istasyonunda çalıştırılamayan macOS ve
Windows süreç/yol kapsamını sağlar.

## Girdi kimliği karşılaştırması (henüz yayımlanmadı)

`crates/agz-rust-coder/examples/identity_measure.rs` örneğini aynı toolchain/profil
ile eski ve yeni kod üzerinde derleyin. İki binary ile
`python3 benchmark/identity_compare.py BASELINE CANDIDATE --output comparison.json`
komutunu çalıştırın. Betik özdeş örnekler üretir, çalışma sırasını dönüşümlü seçer,
üç ısınma sonrası varsayılan 15 örnek kaydeder. Tek kaynak ve manifest değişikliği
ayrı senaryolardır. Hash eşitsizliğinde karşılaştırma reddedilir.
`python3 -m unittest discover -s benchmark -p test_identity_compare.py` bu reddetme
kurallarını sınar. Ölçülen aşamada LLM, ağ veya Cargo çalıştırılmaz.
[Kaydedilmiş kanıt](rust-efficiency-evidence.md).
