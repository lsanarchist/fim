# FIM – monitorovanie integrity statického webu

Školský projekt (Softvérová bezpečnosť, Zadanie 1). Program `fim` sleduje súbory
webovej stránky, porovnáva ich so základným stavom (baseline) v databáze SQLite,
zapisuje udalosti do logu a pri incidente pošle správu na Telegram.

Na `index.html` je denný blok s dnešným dátumom a menami, ktoré majú meniny.
Blok sa mení každý deň o polnoci a **nie je** to incident.

## Ako to funguje

1. `init` uloží do SQLite pre každý súbor: cestu, SHA-256, veľkosť, čas
   modifikácie (mtime), prístupové práva a vlastníka (uid:gid). Pre adresáre
   len práva a vlastníka. Súbory sa hashujú po blokoch, takže aj veľmi veľký
   súbor zaberie len pár MB pamäte (`index.html` má limit 1 MiB).
2. `check` / `watch` súbory znovu prečíta a porovná s baseline. Hlási:
   `ADDED`, `DELETED`, `MODIFIED` (iný obsah), `METADATA` (iná veľkosť,
   mtime, práva alebo vlastník), `DAILY_BLOCK` (zlý denný blok), `UNREADABLE`.
3. Denný blok `<!-- FIM:daily -->…<!-- /FIM:daily -->` sa pred hashovaním
   nahradí značkou `{{DAILY}}`, ale **len ak** text v bloku presne zodpovedá
   dnešnému dátumu a menám z kalendára. Zvyšok `index.html` je chránený SHA-256.
   Včerajší blok sa akceptuje len prvé 2 minúty po polnoci **a len v súbore
   zapísanom včera** (časovač ho ešte nestihol prepísať). Čokoľvek zapísané
   dnes musí mať dnešný dátum.
4. Každý nový incident sa zapíše do logu, do tabuľky `incidents` a pošle sa
   jedna správa na Telegram. Keď sa stav vráti do poriadku, príde jedna správa
   „obnovené“. Rovnaký incident sa v ďalších cykloch neopakuje.
5. Ak Telegram nie je dostupný, správa ostane v tabuľke `outbox`, do logu sa
   zapíše `ALERT_FAILED` a v ďalšom cykle sa pošle znova. Monitorovanie beží ďalej.

## Zostavenie a testy

```sh
cargo build --release
cargo test
cargo fmt --check && cargo clippy -- -D warnings
```

Vyžaduje Linux a Rust ≥ 1.85.

## Konfigurácia

`config.json` (vzor: `config.example.json`). Neznáme polia sú chyba.
Relatívne cesty sa berú voči adresáru konfigurácie.

| Pole | Význam |
|---|---|
| `site_dir` | adresár webu, ktorý sa sleduje |
| `database` | SQLite databáza (mimo `site_dir`) |
| `log_file` | textový log (mimo `site_dir`) |
| `template` | šablóna `index.html` so značkou `{{DAILY}}` v dennom bloku |
| `namedays` | kalendár mien, 366 dní (`"MM-DD": "mená"`) |
| `timezone` | časové pásmo, napr. `Europe/Bratislava` |
| `interval_seconds` | ako často `watch` kontroluje (1–86400 s) |
| `telegram_bot_token` | token bota od @BotFather |
| `telegram_chat_id` | číselné ID chatu, kam idú správy |

Skutočný `config.json` obsahuje token, preto je v `.gitignore`.
Token sa nikdy nevypisuje do logu ani do chybových hlášok.

## Príkazy

```
fim <config.json> init        vytvorí baseline (existujúcu nikdy neprepíše)
fim <config.json> check       jedna kontrola; exit 0 = čisté, 1 = incident, 2 = chyba
fim <config.json> watch       kontrola každých interval_seconds
fim <config.json> page        zapíše dnešný dátum a meniny do index.html
fim <config.json> alert-test  pošle testovaciu správu na Telegram
```

`page` prepíše `index.html` len vtedy, ak sa stránka mimo denného bloku zhoduje
s baseline. Upravenú stránku neopraví potichu, aby sa nestratili dôkazy.
Zápis: dočasný súbor vedľa adresára webu a atomický `rename`.

Log má riadky `<čas> <UDALOSŤ> <správa>`, napr.:

```
2026-11-22T23:40:12+01:00 INCIDENT MODIFIED style.css content (SHA-256), size 22 -> 47
```

## Nasadenie (Debian/Kali)

```sh
cargo build --release
sudo deploy/install.sh   # 1. beh: skopíruje vzor do /etc/fim/config.json
sudo nano /etc/fim/config.json   # doplniť token a chat_id
sudo deploy/install.sh   # 2. beh: page, init, nginx, systemd
```

| Čo | Kde | Vlastník |
|---|---|---|
| web | `/var/www/fim` | root, 0755/0644 – monitor ho nemôže meniť |
| program | `/usr/local/bin/fim` | root |
| šablóna, kalendár | `/usr/local/share/fim` | root |
| konfigurácia | `/etc/fim/config.json` | root:fim 0640 |
| baseline | `/var/lib/fim/baseline.db` | fim |
| log | `/var/log/fim/fim.log` | fim |

- `fim.service` – `watch` ako používateľ `fim` (`ProtectSystem=strict`,
  zapisovať môže len do `/var/lib/fim` a `/var/log/fim`). Pri štarte servera
  sa spustí až po `fim-page.service`, takže nikdy nevidí včerajší dátum.
- `fim-page.timer` – `OnCalendar=*-*-* 00:00:00 Europe/Bratislava`,
  `Persistent=true`; spúšťa `fim-page.service` (`page` ako root).
- nginx (`deploy/nginx-fim.conf`) servíruje len `/var/www/fim`.

## Demo

`scripts/demo.sh` na nasadenom serveri ukáže: stránku cez nginx, čistú kontrolu,
zmenu `style.css`, pridanie a zmazanie súboru, sfalšovaný denný blok, obnovu
a aktualizáciu denného bloku. Výstup je v `docs/demo-output.txt`.

```sh
sudo scripts/demo.sh 2>&1 | tee docs/demo-output.txt
```

## Kalendár mien

`namedays.json` je slovenský občiansky kalendár mien. Zdroj: kalendár zostavuje
kalendárová komisia Ministerstva kultúry SR, zásady:
<https://www.culture.gov.sk/wp-content/uploads/2020/02/Zasady_zaradovania_mien_do_kalendara.pdf>,
prehľad: <https://en.wikipedia.org/wiki/Name_days_in_Slovakia>.
Skontrolované: všetkých 366 dní, žiadne duplicity. Dni bez mena (1. 1., 1. 5.,
2. 11., 25. 12.) majú prázdny reťazec a stránka ukáže „Dnes nemá meniny nikto.“
Novšie pridané mená (napr. Kiara, Timur) neboli overené voči oficiálnemu zoznamu.

## Obmedzenia

- Kontrola je periodická (každých `interval_seconds`), nie okamžitá. Zmena,
  ktorá sa vráti späť medzi dvoma kontrolami, sa nezistí.
- Útočník s právami root môže zmeniť aj baseline, log a program.
- Pridaný prázdny adresár sa zistí, ale mtime adresárov sa nesleduje.
- Mená súborov, ktoré nie sú UTF-8, sa ukladajú v stratovom tvare.
- `index.html` väčší ako 1 MiB sa hlási ako `UNREADABLE`.
- Ak správu nedoručí Telegram, opakuje sa každý cyklus; bez siete nepríde nič.
