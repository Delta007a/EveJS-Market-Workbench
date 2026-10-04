MARKET WORKBENCH — START HERE / НАЧНИТЕ ЗДЕСЬ

1) Extract the ENTIRE archive into your EveJS root folder, beside StartMarketServer.bat.
2) Run StartMarketWorkbench.bat.
3) Wait for this exact backend line:
   Market Workbench is ready: http://127.0.0.1:8765/
4) Open http://127.0.0.1:8765/ in your browser.
5) TQ-like Market is the Recommended starting preset. Edit a Copy to customize it.
6) Legacy Market is the older/legacy economy preset.
7) Import community JSON presets with Import Preset from:
   tools\marketworkbench\presets\community-v1\
8) Build Market Database creates a SEPARATE database. Verify Market Database checks it.
   Nothing is installed into gameplay without your explicit confirmation.
9) The EXE is unsigned; Windows SmartScreen may warn. The Release includes ZIP.sha256.
   Check the actual ZIP filename's .sha256 file. File hashes are in package-manifest.json.

Full guide: tools\README.md. No Rust/Python/Node installation is required.
Keep tools\marketworkbench\user-data when updating. Ctrl+C stops Workbench only.

1) Распакуйте ВЕСЬ архив в корень EveJS, рядом с StartMarketServer.bat.
2) Запустите StartMarketWorkbench.bat.
3) Дождитесь: Market Workbench is ready: http://127.0.0.1:8765/
4) Откройте этот адрес в браузере.
5) TQ-like Market — рекомендуемый старт. Для изменений выберите Edit a Copy.
6) Legacy Market — старый вариант экономики.
7) Community presets импортируйте через Import Preset из
   tools\marketworkbench\presets\community-v1\.
8) Сначала строится ОТДЕЛЬНАЯ база рынка. В игру она не устанавливается без подтверждения.
9) EXE не подписан: SmartScreen может предупредить. Рядом с ZIP в Release есть .sha256.

Подробная инструкция: tools\README.md.
