# Rust doğruluğu ve verimliliği: altı başlık

Başlangıç: `feb7f1b5fe9022ea09870bc375cded075a27439e` (0.1.1).
Tek PR hedeflenir; main birleştirmesi, sürüm artırımı veya yayın bu görevin parçası değildir.
[Kullanım](tools.tr.md#explicit-validation-options) ve [kanıt raporu](rust-efficiency-evidence.md).

## 1. Akış sırasında derleyici kanıtı

Cargo stdout verisi terminal temizliğinden önce, satır bazında ve sınırlı JSON
olarak işlenir. Tanılar ham log kuyruğundan bağımsızdır. Sınırlar: 128 tanı,
1 MiB tanı verisi, satır başına 256 KiB. Yer dolduğunda hatalar uyarıların yerini
alabilir. Tekrar, atlanan kayıt, bozuk satır, büyük satır ve Cargo tamamlanması
ayrı gösterilir. İlk tanı geçici/güvenilmeyen ilerleme metni üretir; erken başarı
veya doğrulama yetkisi vermez.

## 2. Kimlik hesaplamasının gereksiz maliyeti

Her dosya eklenişinde kümenin tamamını saymak yerine workspace ve dış bağımlılık
sayaçları tutulur. Tekilleştirme, yetki, dosya/bayt sınırları ve içerik hash'leri
korunur. Tamamlanan kontrol yeni isteğin kanıtı olarak kullanılmaz. Salt okunur
`identity_measure` örneği ve `benchmark/identity_compare.py`, eski/yeni kodu aynı
yol, komut, kaynak ve hash üzerinde karşılaştırır.

## 3. Açık doğrulama profili ve kapsam

`check.options` ile feature seçimi, yerleşik hedef üçlüsü, test adı filtresi ve
test çalıştırıcısı belirtilir. Varsayılanlar değişmez. Affected/shadow kapsamı
check, Clippy, test ve belge aşamalarında kullanılabilir; tam doğrulama workspace
kapsamındadır. Boş/bilinmeyen değişiklik, Cargo girdileri, build script, dış path
bağımlılığı, procedural macro ve açık feature/platform seçimi kapsamı genişletir.
Paket grafiği kusursuz test etki analizi değildir. `all`, kaydedilen yapılandırmada
bütün aşamalardır; tüm platformlar veya feature kombinasyonları değildir.

## 4. Tanıya bağlı kaynak bağlamı

İsteğe bağlı bağlam; derleyicinin paket/hedef bilgisini, yetkili kaynak kesitini
ve çözümlenmiş doğrudan bağımlılık sürümlerini birleştirir. En fazla 24 bağlam,
her biri en fazla 1 MiB olan dört kaynak dosyası, yedi satır ve satır başına
240 karakter tutulur. Eksik, büyük veya yetkisiz kaynak açık gerekçeyle bildirilir.
`sourceHash` kesitin kaynağını bağlar. `input-identity-matched`, önce/sonra hash
uyuşmasıdır; atomik dosya sistemi fotoğrafı veya kaynak metnine güven talimatı
değildir. Düzeltme kararını kodlama ajanı verir; yeni LLM katmanı kurulmaz.
Mevcut gezinme ve sürüme bağlı belge araçları korunur.

## 5. Bağımsız doğruluk ve performans kanıtı

Özellik tabanlı testler Unicode ve rastgele çıktı parçalanmasını sınar. Küçük Loom
modeli, tamamlanma kontrolünden önce bildirim kaydını doğrular; negatif kontrol
eski sıralamadaki hatayı yakalar. Model bütün Tokio davranışlarını ispatlamaz;
gerçek süreç/protokol testleriyle tamamlanır. Benchmark aracı farklı hash,
yapılandırma, örnek sayısı, NaN veya sıfır süreleri reddeder. Ölçümler yalnız
girdi kimliği aşamasınındır; model kalitesi, token veya toplam derleme iddiası yoktur.

## 6. İsteğe bağlı hızlandırma

Nextest 0.9.143 açık seçimdir. Eksik/yanlış sürümde sessiz geri dönüş yoktur.
Filtreli test tam başarı sayılmaz; `all` ayrıca Cargo doctest çalıştırır.
Sccache 0.17.0 için açık ve mutlak `RUSTC_WRAPPER` gerekir. Unix soketli önbellek
sunucusu süreç yöneticisine aittir; derleme istemci tarafında gözetilen süreç
ağacında kalır. Yerel/sınırlı önbellek kullanılır, uzak/dağıtık ayarlar aktarılmaz,
incremental seçimi değiştirilmez. Gözetilen Sccache kipi şu anda Unix gerektirir;
varsayılan Cargo taşınabilirdir. Gerçek araç regresyonları ve checksum ile sabitlenmiş
Linux CI işi eklenmiştir.

## İncelemede giderilen ek sorunlar

Süreç kapanışı ve görev aboneliğinde bildirim kaçırma yarışı; zorla durdurmadan
sonra sınırsız bekleme ve süresi geçmiş zamanlayıcıyla hızlı döngü; iptal edilmiş
istekte süreç başlatma ve süre taşması düzeltildi. Öneri kaynakları yetkili,
sınırlı okunur; rustc sütunları bayt değil Unicode karakter sayılır. Başarısız
derlemelerin öneri/bağlamı da güncellik denetiminden geçer. İptal, bayat veri veya
eksik temizlikte öneriler kaldırılır. İlk tanı zamanı, zaman damgaları, tamamlanan
aşama sayısı, test hata kuyruğu, doctest hedef seçimi ve şema referansları düzeltildi.

## Teslim sınırı

Yerel doğrulama kanıt raporundadır. Son inceleme oturumunda GitHub bağlantısı
yalnız okuma eylemleri sundu; uzak PR ve CI tamamlanmış sayılamaz. Teslim paketi
tam kaynak, Git ağacı, yama, loglar ve kontrollü yayın betiğini korur. İki geçici
bootstrap iş akışı son kaynak ağacına dahil değildir.

Son tarama: süreç, iş ve LSP tamamlanma/kapasite bildirimleri durum kontrolünden önce kaydedilir. Süreç başlamadan oluşan iptal ve zaman aşımı; Git, metadata ve analizör denetimlerinde genel hata yerine özgün durumuyla aktarılır.
