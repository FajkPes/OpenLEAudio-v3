# OpenLEAudio v4 — stabilita a ověření

## Zjištění
- Dodaný log obsahuje zastavení ISO potvrzení a následný supervision timeout ACL. Z logu nelze určit skutečné PHY řídicího spojení ani jednoznačně příčinu rozhození kanálů.
- ACL používalo ve výchozím nastavení auto, nezávisle na PHY audia.
- Vlákna přijímající HCI události a ACL neměla MMCSS prioritu, i když ji audio smyčka měla.
- WASAPI se vyprazdňovalo až po spotřebování lokální fronty. Při zátěži mohla narůstat stará data; periodické úplné vyprázdnění navíc zahazovalo rezervu čerstvého audia.
- Sekvenční synchronní USB zápisy L/R mohou při pozastavení CPU překročit jeden audio interval. Pouhé pokračování nezaručuje synchronizaci.
- Chyby plánování controlleru a nekompatibilní schopnosti zařízení měly stejný textový prefix. Reconnect proto mohl dočasnou chybu mylně označit za trvalou.

## Změny
- Nový výchozí profil Dosah / stabilita: automatický výběr kodeku podle schopností sluchátek, audio PHY 1M; řídicí ACL výchozí 1M. Google/Android a vlastní profil zůstávají dostupné.
- Migrace nastavení v12 → v13 zachovává osobní a vlastní audio nastavení. Dřívější automatický profil přechází na profil dosahu, ACL auto na 1M; explicitní ACL 2M zůstává zachováno.
- MMCSS Pro Audio i pro HCI a ACL čtecí vlákna.
- Průběžné čtení WASAPI; nejvýše přibližně 60 ms lokální fronty (nebo potřebná velikost rámce). Zahazují se celé vstupní rámce, takže zůstává pořadí kanálů. Nejde o přidání pevné 60ms latence.
- Nejvýše čtyři nevyřízené ISO pakety na kanál. Plný kanál zastaví přijetí celého stereo rámce.
- Číslování obou CIS zohlední zmeškané intervaly, místo vysílání dlouhých doháněcích dávek.
- Pozastavení audio smyčky alespoň 250 ms nebo stereo USB odeslání trvající celý SDU interval vyvolá obnovení obou kanálů.
- Obnova zahrnuje runtime chybu audio klienta. Tři neúspěšná sestavení při audio recovery předají řízení celkovému reconnectu podle jeho nastavení.
- Přechodné chyby vytváření CIG již nevypínají reconnect jako nekompatibilní schopnosti zařízení.
- Název aplikace, EXE, PRI, spouštěč, build vstup a verze balíčku jsou v4. Datový adresář pro párování zůstává zachován kvůli migraci.

## Ověření
- cargo test: 240 testů knihovny, 16 testů agenta, 6 integračních LC3 testů; vše prošlo.
- C# discovery-regression.py: PASS.
- Release build a dotnet publish: dokončeno, WinUI build bez chyb a upozornění. Rust linker pouze vypsal vytvoření importní knihovny.
- Nové testy pokrývají stereo ořez fronty, omezení pomalého kanálu, přetečení a společné posuny čísel rámců, migraci, 1M plán a retry klasifikaci.

Dosah, slyšitelná kvalita, radio recovery a chování při dlouhém 99% CPU zatížení vyžadují test s adaptérem a sluchátky. Úspěšné HCI potvrzení bufferu není důkaz, že sluchátka zvuk přehrála. 1M ani vyšší maximální počet retransmisí nezaručují konkrétní dosah.

## Doporučený ruční test
1. Spustit START OpenLEAudio 4.bat; pro nové výchozí hodnoty použít profil Dosah / stabilita a ACL 1M. Vlastní audio hodnoty migrace nepřepisuje.
2. Pustit známý stereo L/R záznam při nízké hlasitosti. Ověřit v logu skutečné ACL PHY a konfiguraci audio PHY.
3. Krátce zatížit CPU a po uvolnění ověřit pořadí a synchronizaci kanálů, případně záznam automatické obnovy.
4. Odejít mimo dosah a vrátit se. Ověřit přechod z audio recovery na reconnect a obnovení obou kanálů. Poté ukončit spojení ručně a ověřit, že se samo nevrací.

Implementační podklady: [MMCSS](https://learn.microsoft.com/windows/win32/procthread/multimedia-class-scheduler-service), [Bluetooth ISOAL — časování SDU](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-62/out/en/low-energy-controller/isochronous-adaptation-layer.html).
