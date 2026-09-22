import { createIcons, RefreshCw, TriangleAlert } from "lucide";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { locale, t } from "./i18n";
import { initializeTheme } from "./theme";
import "./styles/tokens.css";
import "./styles/base.css";
import "./switch-notice.css";

/** What a switch made from the tray menu is doing, as the backend phrases it:
 *  both titles are already in the language the rest of the app is showing. */
type SwitchNotice =
  | { kind: "working"; title: string }
  | { kind: "failed"; title: string; reasons: string[] };

/** The panel is as wide as `tauri.conf.json` makes it; only its height follows
 *  the message, which is one line for one display and more for several. */
const PANEL_WIDTH = 380;

const root = document.querySelector<HTMLElement>("#switch-notice-app");

initializeTheme();
document.documentElement.lang = locale;

function render(notice: SwitchNotice): void {
  if (!root) return;
  const working = notice.kind === "working";
  // The message carries display and host names the user typed, so every part
  // of it is set as text rather than markup.
  root.innerHTML = `
    <main class="notice ${working ? "is-working" : "is-failed"}">
      <div class="notice-head">
        <span class="notice-mark">
          <i data-lucide="${working ? "refresh-cw" : "triangle-alert"}" ${working ? 'class="is-spinning"' : ""}></i>
        </span>
        <h1></h1>
      </div>
      <ul class="notice-reasons"></ul>
      <footer class="notice-actions">
        ${working
          ? `<span class="caption">${t("switcher.closeHint")}</span>`
          : `<button class="button primary" id="notice-dismiss" type="button"></button>`}
      </footer>
    </main>`;
  root.querySelector("h1")!.textContent = notice.title;
  const reasons = root.querySelector(".notice-reasons")!;
  for (const reason of working ? [] : notice.reasons) {
    const item = document.createElement("li");
    item.textContent = reason;
    reasons.appendChild(item);
  }
  const dismiss = root.querySelector("#notice-dismiss");
  if (dismiss) dismiss.textContent = t("switchNotice.dismiss");
  createIcons({ icons: { RefreshCw, TriangleAlert } });
}

/** Fits the window to the message: one display's reason is a line, several are
 *  several, and a panel sized for the worst case looks empty for the usual one. */
async function fitToContent(): Promise<void> {
  const panel = root?.querySelector<HTMLElement>(".notice");
  if (!panel) return;
  const height = Math.ceil(panel.getBoundingClientRect().height);
  const window = getCurrentWindow();
  try {
    await window.setSize(new LogicalSize(PANEL_WIDTH, height));
    await window.center();
  } catch (error) {
    // A panel of the configured height still reads; only the fit is lost.
    console.warn("Unable to fit the panel to its message", error);
  }
}

async function dismiss(): Promise<void> {
  try {
    await invoke("hide_switch_notice");
  } catch {
    window.close();
  }
}

async function show(notice: SwitchNotice): Promise<void> {
  render(notice);
  await fitToContent();
  // Taking focus while a switch is still running would put the panel in the
  // way of whatever the user moved on to.
  if (notice.kind === "failed") {
    document.querySelector<HTMLButtonElement>("#notice-dismiss")?.focus();
  }
}

document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape" && event.key !== "Enter") return;
  event.preventDefault();
  void dismiss();
});

root?.addEventListener("click", (event) => {
  if ((event.target as HTMLElement).closest("#notice-dismiss")) void dismiss();
});

// A panel still up from an earlier notice is handed the new one.
void listen<SwitchNotice>("switch-notice", (event) => void show(event.payload));

async function initialize(): Promise<void> {
  // The window is shown before it has finished loading, so the message it was
  // opened for is read rather than waited for.
  const notice = await invoke<SwitchNotice | null>("get_switch_notice").catch(() => null);
  if (!notice) {
    void dismiss();
    return;
  }
  await show(notice);
}

void initialize();
