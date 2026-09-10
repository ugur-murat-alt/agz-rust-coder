# Kurulum Ve İstemci Ayarı

**AGZ Yazılım ürünüdür.** Bu belge
[belge okuma yolunun](README.tr.md) 1. adımıdır. Tüm kurulum yöntemlerini,
işletim sistemi notlarını, MCP istemci yapılandırmasını, doğrulamayı ve sorun
gidermeyi kapsar. Sonraki adım
[araç ve yapılandırma referansıdır](tools.tr.md).

`agz-rust-mcp` bağımsız bir stdio MCP sunucusudur: tek bir çalıştırılabilir
dosya, MCP istemciniz tarafından başlatılır ve stdin/stdout üzerinde Model
Context Protocol konuşur. Aşağıdaki yöntemlerin tümü aynı `agz-rust-mcp`
çalıştırılabilir dosyasını kurar.

## Gereksinimler

- Linux, macOS veya Windows (x86_64) ya da macOS (arm64).
- MCP destekli bir istemci (OpenCode2, Codex veya başka bir stdio istemcisi).
- Rust `1.88.0` veya üzeri yalnızca `cargo install` ve kaynak yöntemleri için.
- Node.js yalnızca npm wrapper için.

## 1. npm Wrapper

```bash
npx -y @agz-yazilim/agz-rust-mcp --version
```

Wrapper yönetilen seçenektir: platformunuza uyan sürüm artifact'ını bulur ve
önbelleğe alır, stdio'yu ona iletir; sunucu kendi makinenizde derlenmez.
İstemci yapılandırmasında sabit bir wrapper sürümü belirtin; böylece
güncellemeler bilinçli olur. İlk kullanımda Node.js ve ağ erişimi gerekir.

## 2. Kurulum Betiği (`install.sh`)

Kurulum betiği bir sürüm arşivini indirir, yayımlanmış SHA-256 sağlama
toplamını çıkarma öncesinde doğrular, symlink hedeflerini reddeder, hazırlanan
binary'yi doğrular ve atomik olarak kurar. Şu anda yalnızca **Linux x86_64**
destekler; diğer platformlar açık bir hata alır ve 3, 4 veya 5. yöntemi
kullanmalıdır.

Kurulum betiğini ve sağlama toplamı manifestini sürüm sayfasından indirin,
betiğin kendisini doğrulayın ve çalıştırın:

```bash
curl -fsSL -O \
  https://github.com/ugur-murat-alt/agz-rust-mcp/releases/latest/download/install.sh
curl -fsSL -O \
  https://github.com/ugur-murat-alt/agz-rust-mcp/releases/latest/download/SHA256SUMS
grep ' install.sh$' SHA256SUMS | sha256sum --check
# macOS: grep ' install.sh$' SHA256SUMS | shasum -a 256 -c -

bash install.sh
```

Betik sağlama toplamı doğrulaması başarısızsa çalıştırmayın; sürüm sayfasından
yeniden indirin. Ortam değişkenleri:

| Değişken | Varsayılan | Anlam |
| --- | --- | --- |
| `AGZ_RUST_MCP_VERSION` | en güncel sürüm | Kurulacak tam sürüm. |
| `AGZ_RUST_MCP_INSTALL_DIR` | `$HOME/.local/bin` | Mutlak kurulum dizini. |

```bash
AGZ_RUST_MCP_VERSION=0.2.0 AGZ_RUST_MCP_INSTALL_DIR="$HOME/.local/bin" bash install.sh
```

Betik ayrıca arşiv düzenini ve çıkarılan binary'nin `--version` çıktısını
mevcut dosyayı değiştirmeden önce doğrular. `SHA256SUMS` dosyası crate, kaynak
paketi ve kurulum betiğini kapsar; her arşivin kendi `.tar.gz.sha256` dosyası
vardır.

## 3. Hazır Derlenmiş Arşivler

Sürüm sayfaları `.tar.gz` arşivlerini eşleşen `.sha256` dosyasıyla sunar:

| Platform | Arşiv (0.3.0 ve sonrası) |
| --- | --- |
| Linux x86_64 | `agz-rust-mcp-linux-x86_64.tar.gz` |
| macOS arm64 | `agz-rust-mcp-macos-arm64.tar.gz` |
| Windows x86_64 | `agz-rust-mcp-windows-x86_64.tar.gz` |

**0.1.0-0.2.0 eski varlıkları.** `0.2.0` dahil o sürüme kadarki release'ler
önceki ürün adıyla yayımlandı ve eski arşiv/çalıştırılabilir adlarını kullanır.
Dosyalar erişilebilir kalır ve `install.sh` bunları otomatik eşler:

| Sürüm hattı | Arşiv adı | Arşivdeki çalıştırılabilir |
| --- | --- | --- |
| `0.1.0`-`0.2.0` | `agz-rust-coder-<platform>-<arch>.tar.gz` | `agz-rust-coder` |
| `0.3.0` ve sonrası | `agz-rust-mcp-<platform>-<arch>.tar.gz` | `agz-rust-mcp` |

Arşiv adı ne olursa olsun kurulan çalıştırılabilir her zaman `agz-rust-mcp`
olur; `install.sh` eski arşivdeki çalıştırılabilir dosyayı kurulum sırasında
yeniden adlandırır.

Linux ve macOS:

```bash
sha256sum --check agz-rust-mcp-macos-arm64.tar.gz.sha256   # Linux
shasum -a 256 -c agz-rust-mcp-macos-arm64.tar.gz.sha256    # macOS
tar -xzf agz-rust-mcp-macos-arm64.tar.gz
install -m 0755 agz-rust-mcp "$HOME/.local/bin/agz-rust-mcp"
```

Windows PowerShell:

```powershell
Get-FileHash .\agz-rust-mcp-windows-x86_64.tar.gz -Algorithm SHA256
tar -xzf .\agz-rust-mcp-windows-x86_64.tar.gz
# Get-FileHash çıktısını .sha256 dosyasıyla karşılaştırın, sonra klasörü PATH'e ekleyin.
```

Çıkarma öncesinde yazdırılan hash'i `.sha256` dosyasıyla karşılaştırın. Sağlama
toplamı eşleşmeyen arşivi asla çalıştırmayın.

## 4. crates.io (`cargo install`)

Rust `1.88.0` veya üzeri gerekir:

```bash
rustup toolchain install 1.88.0
cargo install agz-rust-mcp --locked
agz-rust-mcp --version
```

`--locked`, `Cargo.lock` içinde kayıtlı tam bağımlılık sürümleriyle derler.
Belirli bir sürüm için `cargo install agz-rust-mcp --version 0.2.0 --locked`
kullanın. Cargo `$HOME/.cargo/bin` (Windows'ta `%USERPROFILE%\.cargo\bin`)
dizinine kurar; bu dizinin `PATH` içinde olduğundan emin olun.

## 5. Kaynaktan Derleme

```bash
git clone https://github.com/ugur-murat-alt/agz-rust-mcp
cd agz-rust-mcp
cargo +1.88.0 build --release -p agz-rust-mcp --locked
./target/release/agz-rust-mcp --version
```

MSRV, edition 2024 ile Rust `1.88.0` sürümüdür; `rust-toolchain.toml` CI'ın
kullandığı toolchain'i sabitler. `target/release/agz-rust-mcp` (Windows'ta
`.exe`) dosyasını `PATH` içindeki bir dizine kopyalayın veya istemciyi mutlak
yola yönlendirin. Tam geliştirme kapısı için
[CONTRIBUTING.md](../CONTRIBUTING.md) dosyasına bakın.

## İşletim Sistemi Notları

- **Linux.** En kısa doğrulanmış yol `install.sh` (yalnızca x86_64). Betik
  eksik olduğunu bildirirse `$HOME/.local/bin` dizinini `PATH`'e ekleyin.
- **macOS.** `macos-arm64` arşivini veya `cargo install` kullanın. Intel
  Mac'lerde bu sürümde hazır arşiv yoktur; kaynaktan derleyin. İşletim sistemi
  imzasız binary'yi engellerse System Settings -> Privacy & Security
  bölümünden izin verin.
- **Windows.** `windows-x86_64` arşivini veya `cargo install` kullanın;
  `install.sh` Windows'u desteklemez. Çıkarma dizinini `PATH`'e ekleyin.

## MCP İstemci Ayarı

`agz-rust-mcp` bağımsız bir stdio sunucusudur; MCP destekli her istemci
çalıştırabilir. Kanonik çalışma dizini varsayılan yetkili köktür. İstemci başka
yerde başlatılıyorsa tekrarlanan `--allow-root <yol>` argümanlarıyla açık
kökler ekleyin. İstemcinin MCP kökleri yapılandırılmış erişimi daraltabilir,
genişletemez.

### OpenCode2 (`opencode.jsonc`)

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

Wrapper ile yönetilen varyant:

```jsonc
"command": ["npx", "-y", "@agz-yazilim/agz-rust-mcp"],
```

### Codex (`~/.codex/config.toml`)

```toml
[mcp_servers.rust]
command = "agz-rust-mcp"
args = []
```

Wrapper ile yönetilen varyant:

```toml
[mcp_servers.rust]
command = "npx"
args = ["-y", "@agz-yazilim/agz-rust-mcp"]
```

Yönetilen wrapper notu: npm wrapper ile alttaki binary güncellendiğinde istemci
yapılandırması değişmez; tekrarlanabilirlik için wrapper sürümünü sabitleyin.
Doğrudan binary kullanıyorsanız istemcinin `PATH` değeri kabuğunuzdan farklıysa
mutlak yol verin. Yapılandırmayı düzenledikten sonra istemciyi yeniden
başlatın.

Yapılandırma önceliği CLI bayrakları, `AGZ_RUST_MCP_*` ortam değişkenleri,
`--config` TOML ve varsayılanlardır; eksiksiz anahtar referansı
[docs/tools.tr.md](tools.tr.md) içindedir.

## Doğrulama

1. Çalıştırılabilir dosyayı denetleyin:

   ```bash
   agz-rust-mcp --version
   agz-rust-mcp --help
   ```

2. Stdio üzerinden MCP el sıkışmasını ve araç kataloğunu denetleyin (satır
   sonlu JSON-RPC):

   ```bash
   {
     printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"verify","version":"1.0.0"}}}'
     printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
     printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
   } | agz-rust-mcp
   ```

   `initialize` sonucunda uzlaşılan protokol adını, `tools/list` sonucunda
   `check`, `profile`, `audit`, `crate_lookup`, `docs`, `context`, `api`,
   `explain`, `verify`, `symbol`, `references`, `definition`, `symbols`,
   `implementations`, `hierarchy`, `rename`, `refactor`, `change`, `repair` ve
   `work` araçlarını bekleyin (yapılandırmayla kapatılan araçlar listelenmez).
   Stdout yalnız MCP çerçevelerine aittir; tanılar stderr'e gider.

3. Kaynak kopyada depo protokol smoke'unu çalıştırın:

   ```bash
   cargo run -p xtask -- protocol-smoke
   ```

## Sorun Giderme

| Belirti | Denetim ve çözüm |
| --- | --- |
| `agz-rust-mcp: command not found` | Kurulum dizinini (`$HOME/.local/bin` veya `$HOME/.cargo/bin`) `PATH`'e ekleyin ya da istemci yapılandırmasında mutlak yol kullanın. |
| Sağlama toplamı uyuşmuyor | Arşivi ve `.sha256` dosyasını sürüm sayfasından yeniden indirin; doğrulamayı atlamayın. |
| Kurulum betiği işletim sistemini/arch'i reddediyor | `install.sh` yalnız Linux x86_64 destekler; hazır arşiv, `cargo install` veya kaynak derleme kullanın. |
| `npx` beklenmeyen sürümü başlatıyor | İstemci yapılandırmasında wrapper sürümünü sabitleyin ve eski npm önbellek kayıtlarını temizleyin. |
| İstemci araç göstermiyor | İstemcinin `PATH` değerini doğrulayın, mutlak binary yolu kullanın ve başlangıç zaman aşımını yeterli tutun. |
| Yol veya kök yetki hatası | İstemciyi workspace içinde başlatın veya tekrarlanan `--allow-root <yol>` bayrakları verin. |
| Semantik araçlar erişilememe döndürüyor | Sabitlenmiş Rust Analyzer'ı kurun (`rustup component add rust-analyzer --toolchain 1.88.0`) ve workspace-code politikasını [docs/tools.tr.md](tools.tr.md) içinden inceleyin. |
| Eski toolchain'de `cargo install` başarısız | `rustup toolchain install 1.88.0` ile Rust `1.88.0` veya üzerini kurun. |

İlgili: [belge dizini](README.tr.md) - [araç referansı](tools.tr.md) -
[mimari](architecture.tr.md) - [güvenlik politikası](../SECURITY.md).
