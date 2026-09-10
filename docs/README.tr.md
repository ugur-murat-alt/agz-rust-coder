# AGZ Rust MCP Belgeleri

[English](README.md) | Türkçe

**AGZ Yazılım ürünüdür.** `agz-rust-mcp`, Cargo ve rustc çıktısına dayanan
sınırlı Rust doğruluğu için bağımsız bir stdio MCP sunucusudur.

Bu dizin okuma yolunu tanımlar. İlk kez okurken sırayı izleyin; her adım
Türkçe eşine ve önceki/sonraki belgeye bağlantı verir.

## Okuma Yolu

| Adım | Belge | Kapsam |
| --- | --- | --- |
| 1 | [Kurulum ve istemci ayarı](install.tr.md) | Tüm kurulum yöntemleri, işletim sistemi notları, OpenCode2 ve Codex yapılandırması, doğrulama ve sorun giderme. |
| 2 | [Araçlar ve yapılandırma](tools.tr.md) | Araç kataloğu, eylemler, sonuç anlamları ve eksiksiz yapılandırma referansı. |
| 3 | [Mimari](architecture.tr.md) | Süreç modeli, veri akışı, protokol yaşam döngüsü, yetki modeli ve kalan risk. |
| 4 | [Doğrulama ve benchmark protokolü](benchmark.tr.md) | Provider-free smoke'lar, görev benchmark kapıları, kanıt düzeni ve release kapısı. |
| 5 | [Güvenlik politikası](../SECURITY.md) ve [Katkı](../CONTRIBUTING.md) | Güvenlik sınırı ve özel bildirim; değişiklik kuralları ve doğrulama komutları. |

## Referans Ve Arka Plan

- [CHANGELOG.md](../CHANGELOG.md) - sürüm geçmişi.
- [CODE_OF_CONDUCT.tr.md](../CODE_OF_CONDUCT.tr.md) - proje davranış politikası.
- [Rust doğruluğu ve verimlilik planı](rust-efficiency-plan.tr.md) - altı başlık
  çalışması ve [doğrulama kanıtı](rust-efficiency-evidence.md).
- [ADR 0001: kaynağa uygulama yeteneği](adr/0001-source-apply-capability.md) -
  sunucunun değişikliği uygulamak yerine edit paketi döndürmesinin nedeni.
- [Kök README](../README.tr.md) - ürün özeti ile makine tarafından okunabilen
  araç ve yapılandırma tabloları.

Okuma yolundaki her İngilizce belgenin eşleştirilmiş Türkçe dosyası vardır:
kurulum, araç, mimari ve benchmark kılavuzları birbirine doğrudan bağlantı verir.
