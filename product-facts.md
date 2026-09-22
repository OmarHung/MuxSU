# MuxSU Product Facts

最後查證：2026-09-22

本文件記錄會直接影響產品設計與實作的外部事實。尚未在實機驗證的項目不得寫成已支援功能。

## 桌面框架

- 使用 Tauri 2 作為 Windows 與 macOS 的桌面殼層；前端採 Vite 靜態 SPA，透過 Tauri command 與 Rust 後端溝通。
- Tauri 2 官方提供 Windows 與 macOS 的 autostart plugin，可用於讓常駐代理程式在登入後啟動。
- Tauri CLI 目前尚未加入本專案；應以專案本地開發依賴加入，避免要求使用者安裝全域 CLI。

來源：

- https://v2.tauri.app/start/
- https://v2.tauri.app/start/frontend/
- https://v2.tauri.app/plugin/autostart/

## 網路喚醒

- Wake-on-LAN 的 magic packet 可以協助喚醒支援且已正確設定的網路介面。
- Windows 官方支援範圍主要是睡眠 S3 與休眠 S4；Fast Startup 或完整關機 S5 不應承諾一定能喚醒，實際結果仍受主機板、韌體、網卡與供電設定影響。
- macOS 的「Wake for network access」必須由使用者在系統設定中啟用；實際喚醒能力仍取決於 Mac 型號、電源狀態與網路連線方式。
- MuxSU 不得宣稱能喚醒已斷電、拔除電源，或硬體不支援網路喚醒的電腦。

來源：

- https://learn.microsoft.com/en-us/windows/win32/power/system-power-states
- https://learn.microsoft.com/en-us/troubleshoot/windows-client/setup-upgrade-and-drivers/wake-on-lan-feature
- https://learn.microsoft.com/en-us/windows-hardware/drivers/network/standardized-inf-keywords-for-power-management
- https://support.apple.com/en-gb/guide/mac-help/mh27905/mac

## 區域網路主機探索

- MuxSU 使用 DNS-SD over mDNS 廣告 `_muxsu._tcp.local.` 服務，讓 Windows 與 macOS 在不啟用 SMB 檔案分享的情況下互相找到主機名稱與 Agent endpoint。
- 配對密碼至少 15 個字元，並以 PBKDF2-HMAC-SHA256（600,000 次）衍生網路驗證金鑰；Agent 回應的完整內容與協議版本都包含在簽章內。
- `mdns-sd` 可自動追蹤主機網路介面的 IP 變更，並透過 TXT properties 傳遞 MuxSU 主機識別資料。
- 探索僅在主機醒著且 MuxSU 執行時有效；配對後必須保存 endpoint 與 MAC，才能在對方睡眠時嘗試 Wake-on-LAN。
- macOS 15+ 的 Local Network Privacy 要求 app 說明區域網路用途，使用 Bonjour 時也應在 `Info.plist` 宣告瀏覽的 service type；macOS 不要求 iOS 的 multicast entitlement。

來源：

- https://docs.rs/mdns-sd/latest/mdns_sd/
- https://docs.rs/mdns-sd/latest/mdns_sd/struct.ServiceInfo.html
- https://docs.rs/mac_address/latest/mac_address/
- https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy
- https://developer.apple.com/documentation/bundleresources/information-property-list/nsbonjourservices

## macOS DDC/CI

- Rust 生態有 `ddc-macos` 實作，可作為 macOS DDC/CI 後端的候選方案。
- macOS 的外接螢幕 DDC/CI 支援不是所有 Mac、連接埠與轉接器的穩定公開 API；Apple Silicon、內建 HDMI、USB-C/Thunderbolt 轉接器的能力可能不同。
- `m1ddc` 專案明確記錄部分 Apple Silicon 內建 HDMI 的限制。因此，在知道 Mac 精確型號、晶片與 HDMI-1 的實際連接路徑前，只能建立抽象層與測試工具，不能承諾 VG35VQ 的輸入切換一定能由 macOS 發起。
- 目前 Windows 實機已驗證 ASUS VG35VQ 的 VCP `0x60`，Windows DP 為 `0x0F`、Mac HDMI-1 為 `0x11`；Lenovo Q27q-10 必須維持非目標螢幕。

來源：

- https://github.com/haimgel/ddc-macos-rs
- https://docs.rs/ddc/latest/ddc/
- https://github.com/waydabber/m1ddc

## MCCS 輸入來源值

- MuxSU 以 VCP feature `0x60` 控制輸入來源；`0x0F`、`0x10`、`0x11`、`0x12` 可分別標示為 DisplayPort 1、DisplayPort 2、HDMI 1、HDMI 2。
- 顯示器的 capabilities string 可宣告其接受的 `0x60` 值，但實際資料可能不完整或錯誤，不能取代切換時的安全檢查。
- MCCS 沒有跨廠商一致的 USB-C 輸入值。未列於標準對照的值必須顯示為廠商自訂值，不得猜測接頭名稱。

來源：

- https://vesa.org/vesa-standards/
- https://github.com/microsoft/PowerToys/blob/main/doc/devdocs/modules/powerdisplay/design.md
- https://www.ddcutil.com/faq/

## MCCS 螢幕電源與訊號重送

- **「重啟螢幕」功能已於 2026-09-22 移除。** 它在手上兩台螢幕的行為無法預測，而且無法事先判斷會是哪一種：MSI MPG 274U 面板會黑掉再亮起，Acer VG252Q 黑掉之後回不來、需要手動復原。它也從來不是原始需求的解法——恢復螢幕內建 KVM 的是「重送訊號」。以下對 `0xD6` 的記錄保留，作為日後若要重新評估時的依據。

- 功能狀態：「重啟螢幕」與「重送訊號」對外標示為實驗性。截至 2026-09-22，重送訊號只在 MSI MPG 274U、Mac ↔ ITX-PC 這一組上驗證成功；VCP `0xD6` 是否真的讓面板關閉再開啟從未被驗證，只知道它不會重建 KVM 綁定。兩者都會寫入螢幕，失敗的復原手段是螢幕實體按鍵或另一台主機。「重新偵測」只做讀取，不標實驗性。

- MCCS 以 VCP `0xD6` 表示螢幕電源模式：`0x01` 開啟、`0x02` 待機、`0x03` 暫停、`0x04` 關閉、`0x05` 切斷電源。
- 2026-09-22 實機讀取（capabilities string，純讀取取得）：
  - MSI MPG 274U E16M：`...C8 C9 D6(05) DC(00 02 03 05) DF FD)mccs_ver(2.1)`——VCP `0xD6` 只宣告 `0x05`。
  - Acer VG252Q：`...C8 C9 CC(...) D6(01 04 05) DF E0(...)mccs_ver(2.2)`——三個值都宣告。
  - 同一次讀取也顯示 MPG 274U 的 `60( 11 12 0F 10)` 與它實際使用的輸入值（Mac=8、Windows=7）完全不相干，再次印證它用的是私有索引。
- 2026-09-22 實機測試（MSI MPG 274U）：按下重啟螢幕後面板確實黑掉再亮起，電源循環在這台上可用；但螢幕內建的 KVM 沒有跟著回到本機。使用者回報之後按「重新偵測」時 KVM 回來了，機制未明（重新偵測只做 DDC 讀取，不改變輸入），尚待重現。
- 2026-09-22 實機測試（Acer VG252Q，宣告 `D6(01 04 05)`）：`0x04` 與 `0x01` 都被接受（沒有回報寫入失敗），但**面板沒有回來，停在黑畫面**，之後 DDC/CI 也壞掉（macOS 回報 `DDC/CI checksum mismatch`），必須手動處理才能復原。推測是 DPMS 關閉後，重新驅動訊號的是主機端而非螢幕端，`0x01` 只喚醒螢幕控制器，無法讓 macOS 重新輸出。
- 因此截至 2026-09-22，VCP `0xD6` 電源循環在手上兩台螢幕一台可用、一台會把面板留在黑畫面，而且無法事先從螢幕的宣告判斷是哪一種。文件與介面必須說出這個風險，不得寫成「部分螢幕可用」了事。
- **`0xD6` 的宣告值對實際行為毫無預測力，不得用來決定送不送指令。** 2026-09-22 實機對照：MPG 274U 只宣告 `0x05`（照字面完全無法開機），實際卻接受 `0x04` 且面板黑掉後正常亮起；VG252Q 宣告 `01 04 05`（照字面完全支援），實際卻黑掉不回來。曾經實作過「沒宣告 `0x01` 就拒絕」的保護，但那會擋掉唯一真的能用的那台，已移除。
- 仍然永不送出 `0x05`：它是單向寫入，接受它的螢幕可能沒有任何指令能把它叫回來。
- MuxSU 的「重啟螢幕」永遠不送 `0x05`。它在許多螢幕上是單向寫入，送出後就不再回應 DDC/CI，會把使用者推回螢幕的實體電源鍵。
- 螢幕接不接受 `0xD6` 由韌體決定，capabilities string 的宣告也不保證正確。因此不得宣稱所有螢幕都能重啟；失敗時必須回報，並說明螢幕停在哪個狀態、怎麼手動復原。
- 截至 2026-09-22，完整電源循環只在 MSI MPG 274U 上驗證成功（面板黑掉再亮起），且它不會恢復螢幕內建的 KVM。Acer VG252Q 失敗且需要手動復原。
- 2026-09-22 實機觀測（MSI MPG 274U）：DDC/CI 電源循環後畫面會回來，但螢幕內建的 USB／KVM 沒有跟著回來，USB 裝置維持斷線。螢幕的 USB hub／KVM 綁定的是「目前作用中的輸入」，不是面板電源，所以把面板關掉再開不會重建這個綁定。
- 因此電源循環在喚醒後會再寫一次 VCP `0x60`，把原本的輸入值寫回去要求重新綁定。這一步失敗不代表重啟失敗，只記錄不回報。韌體若忽略「寫入與目前相同的值」，就只有真正換過一次輸入（重送訊號）才會重建 USB 綁定。
- 螢幕只對「當下正在顯示的那個輸入」回應 DDC/CI。因此一台主機把畫面切走之後，就再也無法對這台螢幕寫入任何東西；回程只能由當下在畫面上的那台主機執行。這正是 MuxSU 的 agent 與 remote fallback 存在的理由，螢幕維護功能必須遵守同一條規則，不能自己再發明一套本機來回。
- 2026-09-22 實機事故（MSI MPG 274U）：Mac 端把螢幕切到 Windows 的輸入後，Mac 的回程寫入連同重試全部失敗，螢幕停在 Windows，只能用螢幕按鍵或從 Windows 端切回。任何「切出去再自己切回來」的設計在這台螢幕上都會把畫面留在對面。
- 因此「重送訊號」的形狀是：只繞行**已配對主機**的輸入；出發前必須即時 Ping 確認那台主機的 agent 有回應，沒有回應就完全不動螢幕；回程先試本機（少數螢幕確實會回應非作用中的輸入），失敗就請那台主機的 agent 切回來；兩者都失敗時，訊息必須說出螢幕停在哪台主機的哪個輸入，以及螢幕按鍵與對方主機這兩條復原路徑。
- 2026-09-22 實機驗證（MSI MPG 274U）：螢幕內建的 USB／KVM 綁定跟著作用中的輸入走。真正換過一次輸入再換回來（重送訊號）之後，USB 裝置會跟著回到本機；而 DDC/CI 電源循環（0xD6）不會，因為它沒有改變輸入。
- 2026-09-22 實機驗證（MSI MPG 274U，Mac ↔ ITX-PC）：加上兩端重試與 2.5 秒沉澱之後，「切到對方主機的輸入再由對方切回來」的完整來回可以成功。Windows 端補上 DDC/CI 重試是其中一環。
- 2026-09-22 實機事故第三次（MSI MPG 274U）：Mac 請 ITX-PC 代切時，Windows 端在**讀取**目前輸入這一步就失敗，回報 `ERROR_GRAPHICS_DDCCI_INVALID_MESSAGE_COMMAND`（os error -1071241847），整個切換因此中止。時間點是螢幕剛換完輸入、還在重新同步的那幾秒。
- Windows 的 DDC/CI 路徑原本完全沒有重試，而 macOS 端有（`with_ddc_retry`）。剛換過輸入的螢幕會短暫回覆格式錯誤的封包，因此 Windows 端同樣需要重試；跨主機的代切要求本身也必須重試，因為那是畫面唯一的回程。
- 切換服務在寫入前會先讀取目前輸入以判斷「是否已在該輸入」。讀取失敗會讓整個切換中止——即使那次寫入原本會成功。對回程而言這是單點失效，重試無法解決時應考慮讓寫入不依賴前置讀取。
- 平台 controller 的輸入寫入在「驗證讀不回來」時會回報成功（macOS 端明文如此：讀不到不代表寫入失敗）。而螢幕顯示其他主機時，本機正好就讀不到。因此「寫入回報成功」不能當作畫面已經回到本機的證據；判斷畫面是否回來只能靠一次乾淨的讀取，讀不到就必須當作沒回來。2026-09-22 實機事故第二次：回程因為這個假成功而跳過了「請對方主機代切」，畫面留在對面卻回報成功。
- 2026-09-22 實機觀測（MSI MPG 274U）：VCP `0x60` 寫入 `0x01` 被拒絕、讀回仍停在原輸入，而主機之間的正常切換一直可用。`0x01` 來自 capabilities 讀不全時退回的私有索引 `1..=max`；要寫入螢幕的輸入值只能取自「某台主機實際在用的值」，不能取自那串索引裡沒人用的一端。

來源：

- https://vesa.org/vesa-standards/
- https://www.ddcutil.com/faq/
- https://docs.rs/ddc/latest/ddc/
- https://learn.microsoft.com/en-us/windows/win32/api/lowlevelmonitorconfigurationapi/nf-lowlevelmonitorconfigurationapi-setvcpfeature

## 產品安全邊界

- 螢幕切換只允許作用於使用者明確選取的共用螢幕集合；集合中每一台螢幕仍必須符合下方的切換目標規則才能被個別切換，選取多台螢幕不會放寬任一台螢幕的比對規則。
- 完整 EDID 指紋精確符合是「能否切換」的前提，不是「是否仍屬於共用集合」的前提。螢幕在睡眠、顯示其他主機、或切換 mode 後重新列舉時，都可能讀不出與先前相同的序號（macOS EDID 無效時退回 CoreGraphics 身分，Windows WMI `SerialNumberID` 為空時讀為無序號）；這些都只代表當下無法辨識，不代表螢幕已移除。此時必須保留使用者的共用設定並標示為無法使用，不得移出共用清單，也不得清除其他主機對該螢幕的輸入設定。只有使用者能移除共用螢幕。
- 有些螢幕在不同顯示模式下回報不同的 EDID 產品碼（MSI MPG 274U 在 3840×2160 回報 `MSI:3CF0`、在 1920×1080 回報 `MSI:7CF0`；macOS 兩種模式都讀不到序號，Windows 兩種模式都讀得到），而且 macOS 自己的顯示器記錄回報的是同樣那兩組值。切換模式在每一台主機上都會讀成不同螢幕，因此「這兩個身分是同一台螢幕」無法由程式推導，只能由使用者宣告。
- 「是同一台螢幕」是等價關係，比對時**兩邊**都必須先解析成主身分。只解析觀測到的那一邊，會讓以別名儲存的選擇連自己都認不出來，進而每次加入共用都產生一筆重複。這條規則適用於所有「當下在線的螢幕 vs 已儲存選擇」的比對，前端與後端都一樣。
- 使用者宣告的身分等價只決定「哪一台螢幕是使用者要的」，不決定「能不能寫入」。切換時必須從當下在線的螢幕中挑出**恰好一個**目標再送出；有完整指紋精確符合的螢幕時就用它。
- EDID 帶著兩個彼此無關的序號欄位：bytes 12–15 的 32 位元數字序號，以及 tag `0xFF` 的序號文字描述符。螢幕可以只填其中一個，也可以兩個都填，而兩台主機各只讀其中一個。因此同一台螢幕在兩台主機上可能回報**兩個完全不同的序號**，這不代表它是兩台（2026-09-20 實機觀測：同一台 Acer VG252Q，Windows 讀到 `TH6TT0028525`、macOS 讀到 `576726074`；同一台 MSI MPG 274U 只填了文字欄位，所以 Windows 讀到 `CF0H246200009`、macOS 讀不到）。比對序號前必須先確認兩邊讀的是同一個欄位，否則會把一台螢幕斷定成兩台。
- 唯一的放寬是序號：Windows 讀的是 EDID 的序號文字（WMI `SerialNumberID`），macOS 讀的是 EDID 的 32 位元數字序號，兩者常常一邊有、一邊沒有。合併在讀得到序號的主機上建立時，會帶著這個序號，另一台主機永遠無法精確符合。因此找不到精確符合時，「只差在一邊讀不到序號」的在線螢幕也算目標，但這樣的候選必須恰好一台；超過一台就拒絕寫入，兩個不同的序號也永遠不算同一台。
- 跨主機切換 command 必須攜帶明確的目標螢幕指紋；Agent 回應的通訊協定版本必須與目前版本完全一致，否則在採用任何回應資料前直接拒絕並提示使用者更新，不得以猜測方式降級。
- 主機身分一旦決定就不得再衍生。配對主機以 `LocalHostIdentity::id` 記住這台電腦的配對、共用主機順序與自訂名稱，身分一變就全部失聯。MAC 位址不可直接充當身分：macOS 對 Wi-Fi 隱私位址、AWDL、bridge 與 Apple Silicon 的 `anpi` 裝置都發放 locally administered 位址，而系統列舉到哪一個並不穩定（實測同一台 Mac 曾以三個不同身分示人）。設了 locally-administered 位元或全零的位址一律不得用於身分；MAC 只用於 Wake-on-LAN。
- 網路上的遠端 command 必須經過配對與驗證；不得提供未驗證的區網切換端點。
- 切換流程應先確認目標主機代理程式已就緒；若離線，先送 Wake-on-LAN，再等待健康檢查。逾時時停止自動切換，讓使用者決定是否強制切換。
- 應用程式更新必須通過 Tauri updater 公鑰驗證；使用者確認前不得靜默下載或安裝。
- 更新簽章私鑰不得進入原始碼、安裝包、Release assets 或 CI log；release workflow 必須使用最小 `GITHUB_TOKEN` 權限並將 Actions 固定到完整 commit SHA。
- Tauri updater 簽章不等同 Windows Authenticode 或 macOS Developer ID/notarization；正式對外發佈仍應補齊兩個平台的作業系統層級程式碼簽署。

來源：

- https://v2.tauri.app/plugin/updater/
- https://v2.tauri.app/distribute/pipelines/github/
- https://docs.github.com/en/actions/reference/security/secure-use
