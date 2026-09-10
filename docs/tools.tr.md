# Araç ve Yapılandırma Referansı

Bu belge `agz-rust-coder` `0.2.0` sürümünün açık araç ve yapılandırma yüzeyini
tanımlar.

İstek zaman aşımı ve iptal denetimleri, Cargo öncesi ve sonrasındaki Git
sorgularını ve girdi kimliği hesaplamasını da kapsar. Git alt süreçleri ortak
süreç yöneticisini kullanır; NUL ile ayrılmış yollar temizlenmiş gösterim
metninden değil, boyutu sınırlanmış ham standart çıktıdan okunur. İptal veya zaman aşımı sonrasında yeni Git sorgusu başlatılmaz. Başarısız
derlemeler öneri/bağlam dönüşünden önce güncellik denetiminden geçer.
Kısaltılmış araç yanıtları özgün `status`, hata bayrağı ve `untrustedData`
işaretini korur.

## Araç Kataloğu

| Tool | Authority | Side effects | Sonuç |
| --- | --- | --- | --- |
| `check` | Cargo/rustc | Sınırlı target dizininde derleme yapabilir | Doğrulama durumu, komut kanıtı, tanılar ve zamanlama verisi. |
| `profile` | Cargo/rustc | Tek bir sınırlı Cargo hedefini `--timings` ile çalıştırır ve sınırlı HTML raporunu sunucuya ait kanıt altında saklar | Gözlenen yeniden derleme raporu, ayrıştırılmış admission/preflight/Cargo aşamaları, gözlenen/gerekçeli hipotez/bilinmiyor açıklamaları ve baz/aday karşılaştırması. |
| `audit` | Advisory scanner | Yetkili Rust dosyalarını okur | Sınırlı bulgular ve atlanan dosya nedenleri. |
| `crate_lookup` | crates.io | Sınırlı HTTPS isteği | `FOUND`, `NOT_FOUND`, `VERSION_MISMATCH` veya `UNAVAILABLE`. |
| `docs` | rustdoc/docs.rs | Cache, ağ veya yerel `cargo doc` kullanabilir | Tam sürüm alıntısı ve kaynak bilgisi ya da tipli erişilememe. |
| `context` | Rust Analyzer, workspace kaynağı, cargo metadata | Kaynağa asla yazmaz | Tanım, tüketici, test, imza, bağımlılık/feature kanıtı ve sınırlı alıntıları öğe başına nedeniyle birlikte revizyona bağlı kapsül olarak döndürür. |
| `api` | `probe` için Rust Analyzer + workspace kaynağı ve yalıtılmış Cargo adayı | Workspace'e asla yazmaz; `probe` geçici harness'i atılan aday kopyaya yerleştirir | İmza/import/trait/feature çözümü veya tam harness, assertion, yapılandırma ve input hash ile birlikte yalnız derleme kanıtı olan `COMPILES_IN_CONFIGURATION` sonucu. |
| `explain` | rustc/Cargo ve advisory Rust Analyzer | `macro`/`trait` için sınırlı Cargo check çalıştırır; `cfg` yalnız metadata kullanır | Kaynak nitelikli parçalar; eksik açılım eşlemesi `unknown` kalır, tahmin edilmez. |
| `verify` | Cargo/rustc | Sınırlı matris hücrelerinde derleme yapabilir | Hücre planı veya hücre başına tipli durum, kanıt ve kapsam sınırları içeren sınırlı matris çalıştırması. |
| `symbol` | Rust Analyzer | Workspace-code politikasına bağlı | Hover metni ve seçilen konum. |
| `references` | Rust Analyzer | Workspace-code politikasına bağlı | Sınırlı referans konumları. |
| `definition` | Rust Analyzer | Workspace-code politikasına bağlı | Seçilen tanım konumu. |
| `symbols` | Rust Analyzer | Workspace-code politikasına bağlı | Sınırlı dosya sembolleri. |
| `implementations` | Rust Analyzer | Workspace-code politikasına bağlı | Sınırlı uygulama konumları. |
| `hierarchy` | Rust Analyzer | Workspace-code politikasına bağlı | Sınırlı gelen/giden çağrı grafiği. |
| `rename` | Rust Analyzer | Kaynağa asla yazmaz | Doğrulanmış `old_string`/`new_string` edit paketi. |
| `refactor` | Rust Analyzer | Kaynağa asla yazmaz | Doğrulanmış, yazmasız refactor paketi. |
| `change` | Sunucuya ait scratch + aday doğrulaması için Cargo/rustc | Workspace'e asla yazmaz; yalnız aday kopyayı derler | Aday hash'leri, doğrulama kanıtı (taze `FAIL` için sınırlı tanılar ve yazmasız öneriler) ve doğrulanmış/doğrulanmamış export paketi içeren revizyona bağlı change kaydı. |
| `repair` | Sunucuya ait scratch + aday doğrulaması ve sınırlı küçültme için Cargo/rustc | Workspace'e asla yazmaz; yalnız geçici aday kopyaları oluşturup derler | Gerekçeli kök-neden hipotezleri ve kaynak alıntılı ownership kanıtıyla gruplanmış tanılar, aday başına ölçülmüş derleme/test sonucu, davranış/performans koruyucuları, kalan risklerle ölçülmüş seçim ve sabitlenmiş yapılandırmayla dışa aktarımı doğrulanmış küçültülmüş üretici. |
| `work` | change/validate üzerinde sunucuya ait work kaydı | Workspace'e yazmaz; yalnız bağlı aday kopyasını derler | Açık kapılar ve bütçelerle tipli intent yürütme: dürüst `READY` istenen-kapı kanıtı, tek kullanımlık revizyona bağlı token'lı sınırlı `NEEDS_MODEL` handoff veya tipli `BLOCKED`/`FAILED`/`CANCELLED` durma nedeni. |

`check` hedefleri `check`, `clippy`, `test`, `doc`, `fmt` ve `all` değerleridir.
Biçimlendirme yalnız kontrol kipinde çalışır. Tamamlanmış açık bir doğrulama daha
sonraki istek için yetki kanıtı olarak yeniden kullanılmaz; yalnız aynı anda
çalışan özdeş işe katılım mümkündür.

`profile` yalnızca `check`, `clippy`, `test` veya `doc` hedeflerinden birini
çalıştırır. Protokol admission, scheduler kuyruğu, metadata/identity preflight,
Cargo süreci ve finalizasyon sürelerini ayırır; paralel unit sürelerinin toplamı
duvar saati olarak sunulmaz. Stable Cargo `--timings` HTML raporu sınırlı ve
sunucuya ait bir artifact olarak saklanır; gömülü unit verisi yalnızca tam
sürüme bağlı şekille çıkarılır. Eksik, aşırı büyük veya bozuk rapor tahmin
yerine tipli `unavailable` üretir ve eksik Cargo kanıtı cache-hit olarak
sayılmaz. `buildAnalyze` tek örnek döndürür; `buildCompare` toolchain, donanım
sınıfı, yapılandırma, cache durumu, örnek sayıları ve kaynak değişim bağını
kaydeder; tek koşu, gürültülü, karışık warm/cold veya yetersiz örneklerde
`INCONCLUSIVE` döner. Karşılaştırmalar tek bir kanonik workspace köküne, yetki
kök epoch'una ve `changeId` değerine bağlanır; baz kanıt kimlikleri sırayı
koruyarak tekilleştirilir, örnek sayılarına yalnızca `COMPLETE` örnekler girer ve
bir taraf içinde birden çok `inputHash` değeri varsa sonuç reddedilir. Terminal
olmayan örnekler sayılmak yerine görünür uyarıyla hariç tutulur. Warm ve cold
deneyler birleştirilmez, gözlem olmadan CPU/I/O darboğaz türü kesinleştirilmez ve
önerilen feature/dependency/profile değişiklikleri otomatik uygulanmaz. Debug
assertion'ları ve test kapsamı gizli hızlandırma olarak kapatılmaz. Saklanan
zamanlama artifact'ları saklama politikasını bildirir; 24 saat sonra veya 64
dosyayı aşınca temizlenir ve en güncel 64 kanıt kaydı bellekte kalır.

`profile(action=runtime_compare)`, sahnelenmiş bir değişikliğin doğrulanmış iki
sunucu-sahipli anlık görüntüsünde (kaydedilmiş aday ve yakalanan bazın bayt-tam
yeniden kurulumu) tek bir belirlenmiş workload'u ölçer. Önce iki tarafta da aynı
doğruluk kapısı çalışır; böylece hızlı fakat yanlış veya işin bir bölümünü
atlayan aday, herhangi bir zamanlama ölçümünden önce reddedilir ve hash'ler
farklıysa sonuç `INCOMPARABLE` olur. Kabul eşiği ölçümden önce pozitif olarak
bildirilmelidir ve runner yalnız `[profile.runtime]` içinde operatör tarafından
yetkilendirilmiş adaptörü, dengelenmiş baseline/candidate çiftleri, warmup
koşuları, ham örnekler, örnek sayısı ve belirsizlikle çalıştırır. Tek koşu,
yetersiz veya gürültülü ölçüm `INCONCLUSIVE` döner; tek bir en iyi örnek asla
kazanç gösteremez. Derleme/hazırlık süresi workload süresinden ayrı kaydedilir;
allocation, peak-RSS ve donanım sayaçları MVP'de bunları gerçekten ölçen adaptör
olmadığı için `unavailable` kalır. Sonuç; kaynak digest'lerini, patch hash'ini,
capture manifest'ini, adaptör digest'ini, yapılandırmayı, toolchain'i, donanımı,
ham örnekleri ve kullanılan tam komutları bağlar; model yorumu ile ölçülen
bulgular ayrı alanlardır. Araç kaynağa asla yazmaz veya uygulamaz; export
edilebilir aday yine ayrı change export/apply yetkisi gerektirir.

`explain` eylemleri `macro`, `trait` ve `cfg` değerleridir. Her parça
`observedCompiler`, `advisoryAnalyzer`, `inferred` veya `unknown` niteliği
taşır. `macro`, tanılarda tutulan rustc açılım kaynağını, yetenek varsa
uzlaşılmış `rust-analyzer/expandMacro` cevabıyla birleştirir; desteklenmeyen
yetenek `UNSUPPORTED_CAPABILITY` döner ve derinlik sınırına ulaşan açılım
yürüyüşleri `truncated` olarak işaretlenir. `trait`, compiler'ın bildirdiği
beklenen/mevcut metnini ve başarısız bound'ları sınırlı kaynak seçkisiyle
raporlar; Rust Analyzer yükümlülükleri advisory kalır ve çelişkide compiler
tarafı otoritedir. `cfg`, kaynak `#[cfg]` koşulunu anchor'a sahip workspace
üyesine kapsanmış Cargo metadata feature'larıyla değerlendirir; böylece yalnız
bağımlılık crate'lerinde etkinleşen feature'lar anchor cfg'sini enabled
göstermez ve denenmemiş target koşulları `unknown` kalır. Rust Analyzer
istekleri workspace-code politikasına ve istek/epoch iptaline uyar; başarısızlık
tipli erişilememe parçalarına düşer. Çalıştırılmayan konfigürasyon doğrulanmış
gibi sunulmaz ve proc-macro/build-script politikası yükseltilmez.

Tüm araçlar `limits.tool_output_bytes` içinde eşdeğer belirli yapıdaki veri ve
metin döndürür. Uzak gövdeler ve alıntılar ayrıştırmadan önce sınırlandırılır.
Dış içerik `untrustedData` altında verilir ve sunucu talimatına eklenmez.

## Changeset Scratch Alanı

`change`, izinli çalışma ağacının tamamını (değiştirilmiş takipli dosyalar ve
izlenmeyen dosyalar dahil) sunucuya ait scratch dizinine kopyalar; Git gerekmez.
Özgün workspace'e asla yazılmaz. Yakalama çıktısı sınırlı bir `excluded` listesi
(örneğin `.git`, Cargo target dizini ve sunucu scratch alanı) bildirir; böylece
dışlanan bir dizine bağlı girdi sessizce tam sayılmaz. `stage`, her yamayı ve
yeni dosyayı uygulamadan önce doğrular; ilk aday yazımından önce kalıcı bir
"applying" işareti yayınlar ve uygulanan her dosyanın geri okunan hash'lerini
kaydeder (tam tek eşleşme, UTF-8, CRLF'e duyarlı byte karşılaştırması, aday
içinde göreli yol, çakışma reddi). `validate`, `expectedRevision` ve
`baseIdentity` alanlarını zorunlu tutar (revizyon 0 geçerlidir), hiçbir Cargo
süreci başlamadan önce kayıtlı aday dosyalarını yeniden hash'ler ve `check` ile
aynı Cargo hedeflerini aday kopya üzerinde ayrı bir root guard ve izole target
diziniyle çalıştırır; yalnız güncel revizyona ait, iptal edilmemiş ve kimliği
eşleşen PASS/FAIL taze kanıt sayılır. `export` güncel yetkilendirme epoch'unu ve
aynı hash kontrolünü zorunlu tutar, revizyona bağlı paketi dürüst bir `verified`
işaretiyle döndürür; `discard` scratch alanını symlink izlemeden kaldırır.
Symlink, özel dosya türü veya sınır aşımı yakalamayı `INCOMPLETE_INPUTS` olarak
kapalı biçimde başarısız kılar; yakalanan ağaç dışındaki göreli path
bağımlılıkları için de `create`, aday kopya `path = "..."` referanslarını
yeniden üretemeyeceğinden bu yolları listeleyerek `INCOMPLETE_INPUTS` ile kapalı
başarısız olur. Uygulama sırasında G/Ç hatası, "applying" işareti ile son yayın
arasında çökme veya kayıtlı revizyonla eşleşmeyen aday byte'ları change'i
`FAILED_INCONSISTENT` işaretler ve sonraki stage/validate/export istekleri
reddedilir.

`validate` ve taze `inspect`/`export` kanıt satırı ayrıca sınırlı aday
derleyici geri bildirimi döndürür: `compact`, `standard` veya `full` detayına
göre en çok beş, on iki veya yirmi dört tanı; her tanıda `code`, `level`, adaya
göreli `file`, `line` ve kırpılmış `message` ile birlikte
`diagnosticsTotal`/`diagnosticsOmitted` sayaçları ve ayrıştırılan Cargo/test
`stats` verisi (`testsExecuted`, `buildSuccess`). Taze bir `FAIL` satırı ek
olarak, `check` ile aynı yardımcıdan üretilen ve aday byte'larına karşı
doğrulanan yazmasız bir `suggestionPackage` taşır; hiçbir workspace'e yazılmaz
ve `skipped`, `unsupported`, `*Total` ile `truncated` alanları eksiltmeleri
görünür kılar. Tanı ve öneriler yalnız güncel revizyona ait, iptal edilmemiş ve
byte doğrulaması geçmiş satırlarda bulunur: sonraki bir `stage` önceki satırı
geçersiz kılar ve bu geri bildirimi kaldırır. Derleyici metni güvenilmez kanıt
olarak kalır ve tüm sonuç `limits.tool_output_bytes` içinde görünür kırpmayla
sınırlanır.

## Onarım Adayları

`repair`, mevcut bir `change` kaydının güncel revizyonundaki taze `FAIL`
kanıtıyla çalışır. `analyze`, sınırlı tanıları hata kodu ve dosyaya göre
primary/secondary span'larla gruplar, kök-neden bağlarını kanıtlanmış gerçek
değil gerekçeli hipotez olarak etiketler ve ownership ile trait
yükümlülüklerini revizyona bağlı aday kopyadan okunan kaynak alıntılarıyla
açıklar. Aday kaynakları machine-applicable öneri paketi (düzleştirilmiş paket
öneri gruplamasını korumadığı için tek atomik aday olarak yeniden oynatılır),
doğrulanabilen Rust Analyzer quick fix'leri ve birkaç açık mekanik dönüşümdür
(move noktasında clone, yerel tipe derive, bilinen standart kütüphane importu).
Her aday yazmasız bir `oldString`/`newString` paketidir.

`try`, her aday için kendi change kaydını yeniden oluşturur (`create`, önceki
yamaların yeniden oynatılması, ardından aday yamalarının tek atomik stage
olarak uygulanması) ve `change` ile aynı izole Cargo yolundan doğrular. Stale
veya örtüşen bir parça adayın tamamını kısmi uygulama olmadan reddeder, aynı
aday hash'i tekrar denenmez ve `maxCandidates`, `maxCompiles` ile `wallTimeMs`
açık durma nedeniyle durur. `compare`; ölçülmüş derleme durumunu, tanı farkını,
değişen satırları, public API farkını ve verildiyse `constraints.testTarget`
kapı sonucunu ekler. Derlenemeyen, davranış/performans etkisi ekleyen (test
silme veya `#[ignore]`, assert kaldırma, lint kapatma, yeni
`todo!`/`unimplemented!`/`panic!`/`unwrap`, gereksiz clone, `unsafe`, hatayı
yutan dönüşüm, blok silme veya public API değişimi) ya da istenen test kapısını
geçemeyen aday asla seçilmez; test kapısı verilmediyse karşılaştırma yalnız
`compileVerified` etiketlidir. Mevcut kodda zaten bulunan etkiler, adayın
eklediği etkilerden ayrı raporlanır. İptal edilen, zaman aşımına uğrayan, eksik
veya cleanup hatası olan koşular kullanılabilir onarım kanıtı yayınlamaz.

## Work Yürütücüsü

`work`, tek bir sınırlı intent'i mevcut doğrulanmış domain API'leri üzerinden
yürütür; plan düğümü olarak keyfi bir kabuk komutu asla eklenmez. Eylemler
`start`, `resume`, `inspect` ve `cancel`'dır. Yaşam döngüsü
`PLANNED → COLLECTING → STAGING → VALIDATING → NEEDS_MODEL / READY / BLOCKED /
FAILED / CANCELLED` şeklindedir. Her `intent` tipli ve açıktır: bir `template`
(`implement_with_contract`, `repair_compile_failure` veya `refactor_and_verify`),
`scopePaths`, sınırlı `contract` ve `stopCondition`, tipli `acceptanceGates`
(`check` ile aynı Cargo hedefleri) ve aday başına `changeBudget`. `scopePaths`
dışındaki aday yamaları plan sessizce genişletilmeden `BLOCKED` olarak reddedilir.

`start`, change'i oluşturur (`change(action=create)`) veya devralır, host adayını
stage eder (`change(action=stage)`) ve istenen her kapıyı
`change(action=validate)` olarak çalıştırır. `READY` yalnız güncel revizyonda
istenen tüm kapılar taze ve otoriter biçimde geçtiğinde döner; bu, o revizyonda
istenen kapıların kanıtıdır; davranış sözleşmesinin genel olarak sağlandığının
veya kaynağın uygulandığının kanıtı değildir.

Kapı hatası, sınırlı bir karar paketiyle `NEEDS_MODEL` döndürür: `changeId`,
`revision`, `patchHash`, sınırlı `diagnostics`, machine-applicable
`suggestionPackage`, çözülmemiş yükümlülükler ve tipli aday girdi şeması ile
birlikte **work, revizyon ve patch hash'ine bağlı tek kullanımlık bir devam
token'ı**. `resume`, `workId` ve `continuationToken` alanlarını zorunlu tutar;
token'ın kullanılmadığını ve süresinin dolmadığını, bağlı change'in hâlâ aynı
revizyon ve patch hash'ini bildirdiğini doğrular, token'ı tüketir, host adayını
stage eder ve istenen kapıları yineler. Bayat, tekrar kullanılmış, süresi dolmuş
veya yanlış revizyona ait token `STALE` olarak reddedilir; daha önce denenmiş
özdeş aday `BLOCKED` ("ilerleme yok") olarak reddedilir; tanıları önceki handoff
ile birebir aynı olan aday da aynı şekilde durdurulur. Bütçeler (`maxCompiles`,
`maxCandidates`, `maxHandoffs`, `wallTimeMs`) stage öncesinde ve her derleme
öncesinde denetlenir; tükenme, tam bütçe adı ve bildirilen durma koşuluyla
`BLOCKED` döner. `inspect`, sınırlı kaydı döndürür (`workId` ile veya ona bağlı
en güncel work için `changeId` ile). `cancel`, work'ü `CANCELLED` işaretler ve
kayıtlı iptal token'ı üzerinden bağlı change doğrulamasını iptal eder; change
scratch alanı kendi TTL temizliğine bırakılır.

Gerekli bir araç kapalıysa, offline kısıt ağ tabanlı bir gerekli araçla
çelişiyorsa veya change servisi yoksa da `BLOCKED` döner; yürütücü sessizce
başka bir plana geçmez. Work kayıtları bu sürümde bellek içidir ve sunucu
yeniden başlatıldığında korunmaz; bağlı change scratch alanı kendi TTL
temizliğini kullanır. Derleyici metni ve host aday metni,
`limits.tool_output_bytes` içinde görünür kırpmayla güvenilmez kanıt olarak
kalır.

## Hata Küçültme

`repair(action=minimize)`, hatalı bir change revizyonunu küçük ve taşınabilir
bir yeniden üreticiye indirir. Önce güncel revizyonun taze `FAIL` kanıtındaki
gerçek bir tanıdan failure predicate üretilir: hata kodu artı trait ve tip
adlarını içeren normalleştirilmiş mesaj yapısı. Hata kodu tek başına predicate
değildir; `failurePredicate` yalnız kimliği daraltabilir (kod,
`messageContains`, `file`). Değişmemiş aday anlık görüntüsü predicate'i ilk
taze Cargo koşusunda ve ikinci bir değişmemiş doğrulama koşusunda üretmelidir;
eşleşmeyen, zaman aşımına uğrayan veya doğrulamadan önce derleme bütçesini
tüketen koşu küçültülmek yerine görünür biçimde `NOT_REPRODUCED` olarak durur.

Ardından küçültme araması, revizyona bağlı aday kopyada izinli
`reductionScope` eksenlerini (`files`, `items`, `modules` veya tümü) dener:
başvurulmayan dosyalar, sözdizimsel olarak bütün item'lar ve `use` item'ları,
modül dosyalarıyla birlikte `mod` bildirimleri ve satır içi `mod` blokları.
Başka yerde hâlâ başvurulan adlar ve hâlâ kullanılan modül yolları (`name::`)
aday gösterilmez; böylece bir küçültme hatanın bağlı olduğu kodu sessizce
silemez. Her deneme gerçek bir Cargo koşusudur; bir küçültme yalnız aynı
predicate üretildiğinde ve deneme yeni bir hata imzası eklemediğinde kabul
edilir. Aynı hata kodlu ilgisiz tanı, yanlış syntax hatası, yeni eksik
bağımlılık ve zaman aşımı reddedilir ve failure eşleşmesi ile nedeni kaydedilir.
Kabul edilen küçültmeler seçilme nedenleriyle kaydedilir; `remainingRisks`
aranmamış kapsamı belirtir.

Sonuç bir kanıt paketidir: SHA-256 içerik hash'leri ve satır içi içerikle en
küçük kaynak, sabitlenmiş toolchain dosyası, edition, target, feature'lar,
Cargo.toml ve Cargo.lock hash'leri, yeniden üretim komutu ve doğrulama kanıtı.
`REPRODUCED` iddiasından önce tam küçültülmüş kaynak temiz bir geçici dizinde
yeniden oluşturulur ve tekrar derlenir; yalnız orada yeni imza olmadan
eşleşen predicate `exportVerified: true` yapar. Bütçe durması ise
`verification: trialVerified` ile `BEST_KNOWN_REPRODUCER` ve kalan kapsamı
yayınlar. Global minimalite iddia edilmez, otomatik yükleme veya issue açma
yapılmaz ve orijinal workspace'e asla yazılmaz. Dışa aktarma Cargo home,
kimlik bilgileri, ortam sırları, sürüm kontrolü verisi veya ilgisiz depo
dosyalarını asla kopyalamaz; yalnız Rust kaynakları, manifestler, lockfile,
toolchain sabitlemeleri, `.cargo` yapılandırması ve provenance dosyaları
alınır ve dışarıda kalan her şey `omitted` içinde listelenir. Aday item'lar
ayrıca dosya, plan, anlık görüntü toplam bayt ve duvar saati ile sınırlıdır ve
iptal her Cargo alt sürecine iletilir.

## Bağlam Kapsülleri

`context` yalnız tipli çıpalarla çalışır: `{kind:"file",file,range?}` ve
`{kind:"symbol",symbol,file?,line?}`. Serbest metin veya doğal dil yorumu
yapılmaz. `prepare`; tanımlar, uygulamalar, workspace tüketicileri, ilgili test
adayları, hover imzaları, cargo metadata bağımlılık/feature kanıtı ve kaynak
alıntılarından oluşan sınırlı bir kapsül seçer. Her öğe seçim nedeni ve kaynak
bilgisi taşır; erişilemeyen, belirsiz veya bütçeyle çıkarılan öğeler görünür bir
`omitted` listesinde kalır.

`capsuleId`, root epoch, toolchain/analyzer kimliği, kaynak hash'leri, tipli çıpa
kümesi, amaç, değişiklik etiketi, feature seçimi ve byte bütçesi üzerinden
sha256'dır. Bu nedenle değişen kaynak yeni bir kimlik üretir ve `expand`, geçerli
dosya hash'i saklanan hash'ten farklıysa öğeleri `stale` olarak işaretler; eski
sembol tutamaçları yeni bir revizyona sessizce uygulanmaz. `expand` yetkili
dosyaları yeniden okur, yeniden hash'ler ve öğeleri `cursor`/`pageSize` ile
sayfalar. `delta`, saklanan önceki kapsüle göre yalnız eklenen, değişen ve
kaldırılan öğeleri döndürür; sahte boş delta yerine `NOT_FOUND` veya `EXPIRED`
yanıtı verir.

Bellek içi kapsül deposu `context.max_capsules` ve `context.capsule_ttl_ms` ile
sınırlıdır; root epoch değişimi saklanan kapsülleri geçersiz kılar. Analyzer,
workspace ve metadata metni güvenilmez, kaynak etiketli kanıt olarak kalır.
Kapsüller bu sürümde MCP kaynağı olarak sunulmaz; belgelenen fallback `expand`
sayfalamasıdır. Boyutlar yalnız kesin UTF-8 byte ve karakter sayılarıdır;
tokenizer yoktur ve token sayısı bildirilmez.

## API Çözümü ve Probe

`api` iki eylem sunar ve yeni bir semantik motor kullanmaz:

- `resolve`, tek bir `{path,symbol?,line?,character?}` çıpası için mevcut Rust
  Analyzer hover, definition, completion ve reference kanıtını cargo metadata
  feature durumuyla birleştirir. Sınırlı imzayı, import ipuçlarını, metinsel
  trait sınırı parçalarını, etkin feature'ları ve yerel kullanım örneklerini
  döndürür; analyzer kanıtı yoksa açık `omitted` kaydı üretir ve `changeId`
  yalnız bağlama etiketi olarak yazılır.
- `probe`, host'un aday kod parçasını sunucuya ait bir change adayı içindeki
  geçici modüle yerleştirir; aynı yakalanmış bağımlılık grafiği, `Cargo.lock`
  (`--locked` ile), toolchain dosyası ve tipli feature seçimiyle tip kontrolü
  yapar ve change'i atar. Harness yaması yalnız aday kopyaya uygulanır; orijinal
  workspace'e asla yazılmaz ve sonrasında çıpa, `Cargo.toml` ve `Cargo.lock`
  hash'leri yeniden doğrulanır. Çıpa dosyası, varsayılan `cargo check`'in
  seçtiği bir library veya binary hedefinin bildirilmiş modülü değilse boş bir
  başarı yerine `UNSUPPORTED_CONTEXT` döner.

Başarılı probe yalnız `COMPILES_IN_CONFIGURATION` bildirir; çalışma zamanı
doğruluğu iddia edilmez ve kod parçası hiç çalıştırılmaz. `expectedSignature`
verildiğinde kod parçası o tipte döndürülür ve derleyici birleştirmek zorundadır;
kod parçası `todo!`, `unimplemented!` veya başka bir ıraksayan yapı içeriyorsa
tip kontrolü geçse bile sonuç tamamlanmış uygulama değil
`INCOMPLETE_IMPLEMENTATION` olur. Eksik bağımlılık veya feature yalnız
`requiresExplicitHostChange=true` taşıyan danışma niteliğinde `proposals` olarak
döner; probe hiçbir manifest, bağımlılık veya lockfile düzenlemez. Kod parçası,
harness, imza, yapılandırma ve input hash birlikte raporlanır; byte'lar
`api.max_snippet_bytes`, `api.max_snippets` ve `api.compile_timeout_ms` ile
sınırlanır ve iptal baştan sona uygulanır.

## Sonuç Anlamları

Beklenen alan sonuçları tipli durum içeren başarılı MCP çağrılarıdır:

- derleyici veya test hatası: `FAIL`;
- crate yokluğu, sürüm uyuşmazlığı veya registry kesintisi: `NOT_FOUND`,
  `VERSION_MISMATCH` veya `UNAVAILABLE`;
- bulunamayan veya belirsiz sembol: `NOT_FOUND` veya `AMBIGUOUS`;
- belge fallback tükenmesi: tipli erişilememe verisi;
- bilinmeyen veya TTL/root-epoch ile geçersizleşen kapsül tutamaçları:
  `NOT_FOUND` veya `EXPIRED`.

Geçersiz argüman, yetkisiz yol, kaynak sınırı, timeout ve semantik altyapı
yokluğu `isError=true` kullanır. Metin ve belirli yapıdaki durum aynı olmalıdır.

## Task ve İptal

`check` ve `docs`, uzlaşıldığında MCP task'larını destekler. Sunucu ilerleme
bildirir, `tasks/cancel` kabul eder; istek, root-epoch ve kapanma iptallerini
aktarır; terminal task durumunu sınırlı saklama sonrasında kaldırır. Task
desteklemeyen istemciler için eşzamanlı fallback korunur.

`work` bu sürümde `change` ve `context` gibi eşzamanlıdır: MCP task desteği olan
ve olmayan istemciler aynı tipli eylem sonucunu alır. İstek iptali ve kapanma
yine iptal köprüsü üzerinden bağlı change doğrulamasına aktarılır ve
`work(action=cancel)`, work kaydının iptal token'ı üzerinden çalışmakta olan
doğrulamayı iptal eder.

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
| `tools.profile` | `true` | `profile` kaydı. |
| `tools.audit` | `true` | `audit` kaydı. |
| `tools.crate_lookup` | `true` | `crate_lookup` kaydı. |
| `tools.docs` | `true` | `docs` kaydı. |
| `tools.context` | `true` | `context` kaydı. |
| `tools.api` | `true` | `api` kaydı. |
| `tools.explain` | `true` | `explain` kaydı. |
| `tools.verify` | `true` | `verify` kaydı. |
| `tools.lsp` | `true` | Semantik gezinme araçları kaydı. |
| `tools.rename` | `true` | LSP açıksa `rename` kaydı. |
| `tools.refactor` | `true` | LSP açıksa `refactor` kaydı. |
| `tools.change` | `true` | `change` kaydı. |
| `tools.repair` | `true` | `change` etkinken `repair` kaydı. |
| `tools.work` | `true` | `work` kaydı. |
| `cargo.path` | PATH `cargo` | İsteğe bağlı Cargo binary değişimi. |
| `gate.hard_timeout_ms` | `600000` | Tek Cargo işlemi son süresi. |
| `gate.debounce_ms` | `500` | Kararlı girdi bekleme süresi. |
| `gate.host_concurrency` | `1` | Host genelindeki Cargo izinleri. |
| `gate.scope` | `shadow` | `workspace`, `shadow` veya `affected`. |
| `gate.cache` | `auto` | `auto`, `project` veya `isolated`. |
| `gate.min_free_disk_mb` | `1024` | Ön kontrol disk tabanı. |
| `gate.min_available_memory_mb` | `512` | İşletim sistemi güvenilir kullanılabilir bellek ölçümü sağladığında uygulanan ön kontrol tabanı (şu anda Linux). |
| `gate.cache_dir` | platform `agz-rust-coder/state/gate` | Sunucuya ait Cargo cache. |
| `gate.lease_dir` | platform `agz-rust-coder/state/leases` | Host lease ve süreç journal'ı. |
| `profile.max_report_bytes` | `4194304` | Tek Cargo zamanlama artifact'ı için sınırlı okuma/saklama üst sınırı. |
| `profile.max_runs` | `4` | Bir `profile` çağrısında kullanılabilen taze Cargo koşusu. |
| `profile.compare_samples` | `3` | Hız iddiası öncesi her taraf için gereken örnek sayısı. |
| `profile.runtime_max_samples` | `8` | Her taraf için en çok ölçülen runtime örnek sayısı. |
| `profile.runtime_min_samples` | `3` | İddia öncesi her taraf için gereken en az ölçülmüş runtime örneği. |
| `profile.runtime_max_warmup` | `2` | Runtime ölçümünden önce her tarafta atılan warmup koşusu. |
| `profile.runtime_run_timeout_ms` | `120000` | Yetkili tek runtime adaptör süreci için kesin zaman aşımı. |
| `profile.runtime.adapters` | empty | Operatör tarafından yetkilendirilmiş runtime benchmark adaptörleri; boş liste `runtime_compare` aracını fail-closed tutar. |
| `verify.max_cells` | `8` | İstek başına planlanan veya çalıştırılan matris hücresi için kesin üst sınır. |
| `verify.max_wall_ms` | `120000` | Tek matris çalıştırması için kesin duvar saati üst sınırı. |
| `verify.max_tests` | `16` | `test_plan`/`test_run` için planlanan test kapsamı üst sınırı. |
| `verify.repeats` | `2` | `test_candidate` için yinelenen baseline/aday çalıştırma üst sınırı. |
| `rust_analyzer.path` | PATH or rustup | İsteğe bağlı binary değişimi. |
| `rust_analyzer.timeout_ms` | `30000` | Semantik istek son süresi. |
| `rust_analyzer.idle_ms` | `900000` | Boş süreç ömrü. |
| `rust_analyzer.max_instances` | `2` | Eşzamanlı workspace süreci. |
| `rust_analyzer.check_hint` | `false` | RA check ipuçlarına izin verir. |
| `rust_analyzer.workspace_code` | `deny` | `deny` veya açık `allow`. |
| `docs.timeout_ms` | `300000` | Belge çözümleme son süresi. |
| `docs.fallback` | `auto` | `auto`, `local`, `network` veya `off`. |
| `docs.cache_dir` | platform `agz-rust-coder/docs` | Sunucuya ait docs cache. |
| `change.scratch_dir` | platform `agz-rust-coder/state/change` | Yetkili köklerin dışındaki sunucuya ait changeset scratch alanı. |
| `change.max_active` | `4` | Sunucu başına eşzamanlı etkin change. |
| `change.max_files` | `20000` | Change başına yakalanan dosya. |
| `change.max_bytes` | `268435456` | Change başına yakalanan aday byte. |
| `change.ttl_ms` | `86400000` | Açılış taramasından önce orphan ve discarded scratch saklama süresi. |
| `change.max_revisions` | `32` | Change başına stage revizyonu. |
| `repair.max_candidates` | `4` | `repair` işlemi başına aday denemesi; istekler yalnız daraltabilir. |
| `repair.max_compiles` | `4` | `repair` işlemi başına Cargo doğrulaması; istekler yalnız daraltabilir. |
| `repair.wall_time_ms` | `120000` | `repair` işlemi başına duvar saati bütçesi; istekler yalnız daraltabilir. |
| `repair.minimize_max_candidates` | `32` | `repair(action=minimize)` başına derlemeyle değerlendirilen küçültme denemesi; istekler yalnız daraltabilir. |
| `repair.minimize_max_compiles` | `16` | `repair(action=minimize)` başına, yeniden üretim ve dışa aktarma doğrulaması dahil Cargo koşusu; istekler yalnız daraltabilir. |
| `work.max_active` | `4` | Sunucu başına eşzamanlı, terminal olmayan work öğesi. |
| `work.max_compiles` | `12` | Bir work öğesinin çalıştırabileceği kapı doğrulaması. |
| `work.max_candidates` | `4` | Bir work öğesinin stage edebileceği host aday revizyonu. |
| `work.max_handoffs` | `3` | Bir work öğesinin verebileceği sınırlı `NEEDS_MODEL` handoff sayısı. |
| `work.wall_time_ms` | `600000` | Bir work öğesi için duvar saati bütçesi. |
| `work.continuation_ttl_ms` | `900000` | Tek kullanımlık handoff token'ının geçerlilik süresi. |
| `context.max_capsules` | `32` | Bellek içi kapsül ring kapasitesi. |
| `context.capsule_ttl_ms` | `900000` | Kapsül TTL süresi; root epoch değişimi de geçersiz kılar. |
| `context.max_items` | `64` | Bir kapsüle seçilen öğe sayısı. |
| `api.max_snippets` | `4` | Bir `probe` için kabul edilen aday kod parçası sayısı. |
| `api.max_snippet_bytes` | `32768` | Bir `probe` için toplam kod parçası byte bütçesi. |
| `api.compile_timeout_ms` | `120000` | Temizlik dahil bir `probe` için duvar saati sınırı. |
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

## Yapılandırma Matrisi (`verify`)

`verify`, feature, hedef, toolchain ve geliştirme aşamaları üzerinde sınırlı bir
matrisi planlar (`action=matrix_plan`) veya çalıştırır (`action=matrix_run`).
Her hücre paket kapsamını, feature/default-feature seçimini, hedef üçlüsünü,
toolchain/MSRV değerini, profili, aşamayı (`check`, `clippy`, `test`, `doc`) ve
runner'ı kaydeder.

Planlama adayları `cargo metadata` ile `[workspace.metadata.agz-verify]` (veya
ilk workspace üyesinin `[package.metadata.agz-verify]`) altındaki açık proje
politikasından türetir:

- `feature-groups`: açıkça desteklenen feature adı dizileri; düz adların yanı
  sıra `dep/feat` ve zayıf `dep?/feat` seçicileri kabul edilir;
- `mutually-exclusive-features`: birlikte etkinleştirilmemesi gereken gruplar;
- `targets`: desteklenen yerleşik hedef üçlüleri; yalnız kurulu olanlar planlanır;
- `msrv`: paket `rust-version` değerini geçersiz kılan rustup toolchain seçici;
- `stages`: istek aşama belirtmezse kullanılacak varsayılan aşamalar.

`.github/workflows` altındaki CI dosyaları yalnız düz metin yapılandırma girdisi
olarak okunur ve kabuk komutu olarak asla yürütülmez. Desteklenmeyen
kombinasyonlar `UNSUPPORTED_CONFIGURATION` olarak bildirilir; bunlar ürün
regresyonu sayılmaz ve `--all-features` kör biçimde birleştirilmez.

Çalıştırılabilir her hücre sınırlı `CheckService` kapısından geçer. Hücre
durumları `PASS`, `FAIL`, `NOT_INSTALLED`, `RUNNER_UNAVAILABLE`,
`SKIPPED_BUDGET`, `UNSUPPORTED_CONFIGURATION`, `TIMEOUT`, `CANCELLED`,
`INCONCLUSIVE` ve `RESOURCE_BLOCKED` değerleridir; atlanan veya tamamlanmayan
hücre asla yeşil değildir. `FULL_REQUESTED_MATRIX` yalnız istenen her hücre
tamamlandığında döner; bütçe aşımında tamamlanan ve eksik hücre kimlikleri ayrı
verilir. Yerel olmayan hedefler yalnız derleme amaçlıdır ve o platformda test
çalıştığını iddia etmez. Kurulu yabancı hedefler için derleme amaçlı `check`
hücresi yalnız `check` istenen aşamalar arasındayken planlanır; yabancı hedefte
istenen `test`, `clippy` veya `doc` aşaması `UNSUPPORTED_CONFIGURATION` olarak
işaretlenir. Toolchain/MSRV hücresi seçilen toolchain'i çalıştırır: doğrudan
toolchain `cargo` çağrısında `RUSTC`, `RUSTUP_TOOLCHAIN` ve toolchain önekli
`PATH` sabitlenir; rustup shim geri dönüşünde `+toolchain` uygulanır. Böylece
gerçekten seçilen derleyici çalışır ve komut ile ortam hash'lerine bağlanır.
Hiçbir toolchain veya hedef indirilmez; Cargo mevcut ağ politikasına göre crate
indirebilir.

Planlayıcı sınırlı bir numaralandırıcıdır ve çıktısını `NOT exhaustive` olarak
işaretler. Her sonuç kaynak/lock/yapılandırma/toolchain bağını kapı kimliği ve
komut hash'leri üzerinden kurar; tamamlanmış bir `PASS` yeni açık çalıştırma için
yeniden kullanılmaz.

### Test planlama, çalıştırma ve aday doğrulama

**0.2.0 sürümünden itibaren** `verify` ayrıca `action=test_plan`, `test_run` ve
`test_candidate` destekler. Bu eylemler aynı sınırlı `CheckService` yürütücüsünü
kullanır; keyfi kabuk komutu, otomatik bağımlılık kurulumu veya workspace
kaynağına yazma eklenmez.

- `test_plan`; `cargo metadata` test envanterini, workspace paket grafını (ters
  bağımlılıklar), çağıranın verdiği semantik referans ipuçlarını, açık kullanıcı
  eşlemelerini ve değişen kümeyi (`changeId` kaydı veya `changedPaths`) birleştirir.
  Kapsamlar en ucuz/en ilgili önce sıralanır: açık eşlemeler, doğrudan değişen
  paketler (unit, integration, doctest), ters bağımlılık tüketicileri, ardından
  workspace geneline genişleme. Manifest, lockfile, build script, toolchain
  dosyası, `.cargo` yapılandırması, proc-macro paketi veya kütüphane kökü
  değişikliği dar plana kör güvenmek yerine konservatif workspace genişlemesini
  zorunlu kılar; her dahil etme ve atlama gerekçesi `testPlan` içinde görünür.
- `test_run` planlanan kapsamları sırayla çalıştırır; tam paket, hedef/binary,
  filtre, feature seçimi, runner, komut/girdi/ortam hash'leri ve çalışan test
  adlarını kaydeder. Sıfır eşleşme, ignored-only, custom harness ve eksik sonuç
  asla `PASS` değildir; tam istenen test adını çalıştırmayan alt dizge filtresi
  `INCONCLUSIVE` olur. Doctest, integration ve feature'a bağlı hedefler ayrı
  kapsamlardır ve `nextest` runner'ı ayrı doctest kapısını ortadan kaldırmaz.
  `FULL_REQUESTED_SUITE` yalnız plan tüm workspace envanterini kapsıyor ve her
  kapsam geçtiyse döner; aksi halde `TESTED_SUBSET` döner ve bu geliştirme geri
  bildirimidir, final kapı değildir.
- `test_candidate`; `changeId` (aşamalanmış düzeltme), `testPatch` (regresyon
  testi yaması) ve `behaviorContract` (`testName` ve `expectedFailure`) alır.
  `ChangeService` üzerinden sunucuya ait bir deneme kopyası oluşturur, test
  yamasını baseline'a uygular, aynı temel kimlik üzerinde değişikliğin kayıtlı
  düzeltme yamalarını yeniden uygular ve aynı testi iki anlık görüntüde çalıştırır
  — hiçbir zaman orijinal workspace'te değil. Sözleşme yalnız baseline beklenen
  assertion metniyle fail ve aday pass olduğunda `SATISFIED` olur. Baseline
  API'sine karşı derlenemeyen test ayrı `BASELINE_INCOMPATIBLE` durumudur ve
  hatayı yakaladığı sayılmaz. Test silme, assertion boşaltma, `#[ignore]` ekleme
  ve kapsam daraltma karşılaştırma öncesinde veya sırasında reddedilir;
  `budget.repeats` (`verify.repeats` ile sınırlanır) gözlenen pass/fail sayılarını
  raporlar ve kararsızlık `INCONCLUSIVE` olarak bildirilir — başarısızlık sonrası
  bir retry gözlenen kararsızlığı silmez.

## Açık doğrulama seçenekleri

Aşağıdaki `check` ekleri **0.2.0 sürümünden itibaren kullanılabilir**. `options`
verilmezse mevcut Cargo davranışı korunur. Örnekler kabuk komutu değil MCP
argüman nesnesidir:

```json
{"target":"check","options":{"noDefaultFeatures":true,"features":["serde"],"context":true}}
```

```json
{"target":"test","options":{"runner":"nextest","testFilter":"parses_empty_input"}}
```

`options`: `features` (en fazla 64 ad, ad başına 128 bayt), `allFeatures`,
`noDefaultFeatures`, `targetTriple` (yerleşik hedef, JSON dosyası değil),
`testFilter` (sınırlı test adı alt dizgesi), `runner` (`cargo` / `nextest`),
`sccache`, `context`. Bilinmeyen seçenek, komut bayrağı, kontrol karakteri,
çelişkili feature seçimi ve `target=all` ile test filtresi reddedilir.
`allFeatures` tek birleşik seçimdir; bütün kombinasyonların testi değildir.
Farklı platform hedefi önceden kurulu olmalıdır. Çapraz hedefte test çalıştırmak
için operatörün Cargo runner yapılandırması gerekir; check başarısı çalıştırma
testi değildir. Otomatik toolchain indirilmez.

`gate.scope`, geliştirme sırasında check, Clippy, test ve doc için geçerlidir.
`all`, istenen yapılandırmada workspace aşamalarını çalıştırır. Genel/belirsiz
girdi değişiklikleri ve açık feature/platform seçimi kapsamı genişletir.
`FULL_PASS` yalnız kaydedilen aşamalar ve seçeneklerin başarısıdır. Filtreli
Cargo testinde en az bir çalıştırılmış libtest kanıtı yoksa Cargo sıfır koduyla
çıksa bile `INCONCLUSIVE` döner. Aynı kanıtı vermeyen özel test harness çıktısı
da başarılı sayılmaz. Nextest `--no-tests=fail` ile sıfır eşleşmeyi reddeder.

Aşama kanıtında `evidence`, `diagnosticsOmitted`, `contexts` ve mevcut çıktı/temizlik
bayrakları bulunur. Aşamadaki `firstDiagnosticMs` süreç başlangıcına göredir;
istek düzeyindeki değer ön kontrol ve kuyruğu da içerir. Log kesilmesi mutlaka
derleyici tanısının kaybolduğu anlamına gelmez. Bozuk/büyük kayıtlar ve atlanan
tanılar ayrı gösterilir. Geçici ilerleme metinleri güvenilmeyen derleyici verisidir;
nihai sonuç değildir.

Bağlam kesitleri kaynak hash'i ve çözümlenmiş doğrudan bağımlılık sürümlerini taşır;
tahmini düzeltme tavsiyesi üretmez. `input-identity-matched`, tam önce/sonra girdi
kimliklerinin aynı olduğunu belirtir. Atomik dosya fotoğrafı değildir; düzenleme
öncesi kaynak hash'i veya `old_string` tekrar doğrulanmalıdır. Başarısız derlemeler
de öneri/bağlam dönüşünden önce tekrar denetlenir. İptal, zaman aşımı veya eksik
temizlikte kullanılabilir öneri verilmez. Kaynak bütçeleri ve eksiklik gerekçeleri
gösterilir; MCP kaynak dosyalarını yine değiştirmez.

Nextest 0.9.143, yapılandırılmış tüm workspace/dependency köklerinin dışında,
güvenilen mutlak PATH dizininden bulunmalıdır. Sessiz çalıştırıcı geri dönüşü yoktur.
Gözetilen Sccache için 0.17.0 sürümüne işaret eden mutlak `RUSTC_WRAPPER` ve
`sccache=true` gerekir. Bu kip şu anda Unix gerektirir; özel ön plan yerel
önbellek sunucusu/soketi açar, derlemeyi istemci tarafında tutar ve dönüşten önce
süreç ağacını temizler. Uzak/dağıtık ayarlar aktarılmaz; incremental seçimi
korunur. `gate.lease_dir` altında yerel disk önbelleği 256 MiB ile sınırlıdır.
Unix soket yolu çok uzunsa daha kısa `gate.lease_dir` seçilmesini isteyen açık
hata döner. Her Sccache yapılandırmasını şeffafça destekleyen bir kip değildir.

Rust kütüphanesinin açık istek/kanıt struct'larına alanlar eklendi. Struct literal
kullanan istemcilerde uyarlama gerekebilir; `GateRequest::new(...).with_options(...)`
tercih edilmelidir. Önceki MCP alanları ve varsayılanlar şema testleriyle korunur.

[Altı başlık planı](rust-efficiency-plan.tr.md) ve
[doğrulama kanıtı](rust-efficiency-evidence.md).

macOS ve Windows üzerinde başka bir sürece ait olduğu görülen, fakat sahibinin
sonlandığı doğrulanamayan host lease dosyaları silinmez. Sahip sürecin çöktüğü
doğrulandıktan ve doğrulama işlemi çalışmadığından emin olunduktan sonra eski
lease dosyasını elle temizlemek gerekebilir. Linux üzerinde PID yokluğu
doğrulanarak kurtarma korunur. Sccache açıkken yalnız metadata sorgularında
RUSTC_WRAPPER devre dışıdır; derleme yönetilen ve doğrulanmış cache oturumunu kullanır.
