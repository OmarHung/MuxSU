import { t, type MessageKey } from "./i18n";
import { diagnosticsDialogsHtml, diagnosticsSectionHtml } from "./diagnostics";

/** The settings sections, in tab-list order. */
export const SETTINGS_TABS = ["displays", "hosts", "groups", "startup", "appearance", "experimental", "help", "reset"] as const;
export type SettingsTab = typeof SETTINGS_TABS[number];

interface TabSpec { id: SettingsTab; icon: string; label: MessageKey; countId?: string; danger?: boolean }

const upperTabs: TabSpec[] = [
  { id: "displays", icon: "monitor", label: "settingsTab.displays", countId: "count-displays" },
  { id: "hosts", icon: "network", label: "settingsTab.hosts", countId: "count-hosts" },
  { id: "groups", icon: "layers", label: "settingsTab.groups", countId: "count-groups" },
  { id: "startup", icon: "keyboard", label: "settingsTab.startup" },
  { id: "appearance", icon: "sun-moon", label: "settingsTab.appearance" },
];
const lowerTabs: TabSpec[] = [
  { id: "experimental", icon: "flask-conical", label: "settingsTab.experimental" },
  { id: "help", icon: "circle-help", label: "settingsTab.help" },
  { id: "reset", icon: "triangle-alert", label: "settingsTab.reset", danger: true },
];

function tabButton(tab: TabSpec): string {
  return `<button type="button" class="settings-tab-button ${tab.danger ? "is-danger" : ""} ${tab.id === "displays" ? "is-active" : ""}" data-settings-tab="${tab.id}">
    <i data-lucide="${tab.icon}"></i><span>${t(tab.label)}</span>${tab.countId ? `<span class="count" id="${tab.countId}"></span>` : ""}
  </button>`;
}

function tabHead(title: MessageKey, body?: MessageKey): string {
  return `<header class="tab-head"><h2>${t(title)}</h2>${body ? `<p>${t(body)}</p>` : ""}</header>`;
}

function switchRow(id: string, title: MessageKey, hint: MessageKey): string {
  return `<label class="switch-row">
    <span class="switch-label"><strong>${t(title)}</strong><small>${t(hint)}</small></span>
    <input id="${id}" type="checkbox" class="toggle-checkbox" />
    <span class="switch-slider"></span>
  </label>`;
}

function displaysTab(): string {
  return `<section class="settings-tab is-active" id="settings-tab-displays" data-settings-panel="displays">
    ${tabHead("settingsTab.displays", "settingsTab.displaysBody")}
    <div class="form-section first">
      <h3 class="group-title">${t("settings.sharedDisplays")}</h3>
      <div class="list" id="monitor-picker"></div>
      <div class="note glass" id="maintenance-experimental-note" hidden><i data-lucide="flask-conical"></i><p><b>${t("settings.experimentalTitle")}</b>${t("settings.experimentalBody")}</p></div>
      <div class="monitor-merge" id="monitor-merge"></div>
    </div>
    <div class="form-section">
      <h3 class="group-title">${t("settings.localInput")}</h3>
      <div class="list" id="local-input-summary"></div>
    </div>
    <div class="form-section">
      <h3 class="group-title">${t("settings.inputLabels")}</h3>
      <p class="section-hint">${t("settings.inputLabelsHint")}</p>
      <div class="list" id="input-labels"></div>
    </div>
    <div class="note glass"><i data-lucide="info"></i><p><b>${t("settings.detectionTitle")}</b>${t("settings.detectionBody")}</p></div>
  </section>`;
}

function hostsTab(minSharedKeyLength: number): string {
  return `<section class="settings-tab" id="settings-tab-hosts" data-settings-panel="hosts">
    ${tabHead("settingsTab.hosts", "settingsTab.hostsBody")}
    <div class="form-section">
      <h3 class="group-title">${t("settings.hostsTitle")}</h3>
      <p class="section-hint">${t("settings.hostsHint")} ${t("settings.localComputerHint")}</p>
      <div class="list" id="host-list"></div>
    </div>
    <div class="form-section pairing-section">
      <div class="group-head">
        <h3 class="group-title">${t("settings.discoverTitle")}</h3>
        <button class="button small" id="scan-button" type="button"><i data-lucide="search"></i><span>${t("action.searchAgain")}</span></button>
      </div>
      <div class="list" id="peer-list"></div>
    </div>
    <div class="form-section pairing-key-section">
      <h3 class="group-title">${t("settings.pairingTitle")}</h3>
      <div class="list">
        <label class="row field-row">
          <span><span class="row-title">${t("settings.password")}</span><span class="row-hint">${t("settings.passwordHint")}</span></span>
          <span class="input-wrap"><i data-lucide="key-round"></i><input class="field-input" id="shared-key" type="password" minlength="${minSharedKeyLength}" placeholder="${t("settings.passwordPlaceholder")}" /></span>
        </label>
        <label class="row field-row">
          <span><span class="row-title">${t("settings.wait")}</span><span class="row-hint">${t("settings.waitHint")}</span></span>
          <input class="field-input" id="wait-seconds" type="number" min="5" max="120" />
        </label>
      </div>
    </div>
  </section>`;
}

function groupsTab(): string {
  return `<section class="settings-tab" id="settings-tab-groups" data-settings-panel="groups">
    ${tabHead("settingsTab.groups", "settingsTab.groupsBody")}
    <div class="form-section">
      <div class="group-head">
        <h3 class="group-title">${t("settings.groupsTitle")}</h3>
        <button class="button small" id="new-group-button" type="button"><i data-lucide="plus"></i><span>${t("action.newGroup")}</span></button>
      </div>
      <p class="section-hint">${t("settings.groupsHint")}</p>
      <div class="list" id="group-list"></div>
    </div>
  </section>`;
}

function startupTab(): string {
  return `<section class="settings-tab" id="settings-tab-startup" data-settings-panel="startup">
    ${tabHead("settingsTab.startup", "settingsTab.startupBody")}
    <div class="form-section shortcut-section">
      <h3 class="group-title">${t("settings.hostSwitcher")}</h3>
      <div class="list">
        ${switchRow("host-switcher-enabled", "settings.hostSwitcher", "settings.hostSwitcherHint")}
        <div class="row">
          <span><span class="row-title">${t("settings.shortcut")}</span><span class="row-hint">${t("settings.shortcutHint")}</span></span>
          <button id="shortcut-recorder" class="shortcut-recorder" type="button"><span id="shortcut-value"></span><em>${t("settings.recordShortcut")}</em></button>
        </div>
        <small id="shortcut-status" class="shortcut-status" aria-live="polite"></small>
      </div>
    </div>
    <div class="form-section">
      <h3 class="group-title">${t("settings.startupTitle")}</h3>
      <div class="list">
        ${switchRow("autostart", "settings.autostart", "settings.autostartHint")}
        ${switchRow("check-updates", "settings.autoUpdates", "settings.autoUpdatesHint")}
      </div>
    </div>
  </section>`;
}

function appearanceTab(): string {
  return `<section class="settings-tab" id="settings-tab-appearance" data-settings-panel="appearance">
    ${tabHead("settingsTab.appearance", "settingsTab.appearanceBody")}
    <div class="list">
      <label class="row has-icon">
        <span class="app-icon is-accent"><i data-lucide="languages"></i></span>
        <span class="row-title">${t("language.label")}</span>
        <select class="field-select plain-font" id="language-select">
          <option value="system">${t("language.system")}</option>
          <option value="en">${t("language.english")}</option>
          <option value="zh-TW">${t("language.traditionalChinese")}</option>
        </select>
      </label>
      <label class="row has-icon">
        <span class="app-icon is-accent"><i data-lucide="sun-moon"></i></span>
        <span class="row-title">${t("theme.label")}</span>
        <select class="field-select plain-font" id="theme-select">
          <option value="system">${t("theme.system")}</option>
          <option value="light">${t("theme.light")}</option>
          <option value="dark">${t("theme.dark")}</option>
        </select>
      </label>
    </div>
  </section>`;
}

function noteCard(title: MessageKey, body: MessageKey): string {
  return `<article class="note-card glass"><h4>${t(title)}</h4><p>${t(body)}</p></article>`;
}

function experimentalTab(): string {
  return `<section class="settings-tab" id="settings-tab-experimental" data-settings-panel="experimental">
    ${tabHead("settingsTab.experimental", "settingsTab.experimentalBody")}
    <div class="form-section first">
      <div class="list">
        ${switchRow("experimental-enabled", "settings.experimentalSwitch", "settings.experimentalSwitchHint")}
      </div>
    </div>
    <div class="note glass"><i data-lucide="flask-conical"></i><p><b>${t("settings.experimentalTitle")}</b>${t("settings.experimentalBody")}</p></div>
  </section>`;
}

function helpTab(releaseRows: string): string {
  return `<section class="settings-tab" id="settings-tab-help" data-settings-panel="help">
    ${tabHead("settingsTab.help", "settingsTab.helpBody")}
    <div class="form-section">
      <h3 class="group-title">${t("help.guideTitle")}</h3>
      <div class="note-list">
        ${noteCard("help.replaceTitle", "help.replaceBody")}
        ${noteCard("help.inputTitle", "help.inputBody")}
        ${noteCard("help.autoTitle", "help.autoBody")}
        ${noteCard("help.adapterTitle", "help.adapterBody")}
        ${noteCard("settings.mccsTitle", "settings.mccsBody")}
        ${noteCard("settings.ddcTitle", "settings.ddcBody")}
      </div>
    </div>
    ${diagnosticsSectionHtml()}
    <div class="form-section">
      <h3 class="group-title">${t("help.onboardingTitle")}</h3>
      <div class="list">
        <div class="row">
          <span class="row-sub">${t("help.onboardingBody")}</span>
          <button class="button small" id="onboarding-restart" type="button">${t("help.onboardingAction")}</button>
        </div>
      </div>
    </div>
    <div class="form-section">
      <h3 class="group-title" id="release-history-title">${t("help.releaseHistoryTitle")}</h3>
      <div class="list release-table-wrap">
        <table class="release-table" aria-labelledby="release-history-title">
          <thead><tr><th scope="col">${t("help.releaseDate")}</th><th scope="col">${t("help.releaseVersion")}</th><th scope="col">${t("help.releaseLink")}</th></tr></thead>
          <tbody id="release-history-body">${releaseRows}</tbody>
        </table>
      </div>
    </div>
    <div class="form-section">
      <h3 class="group-title">${t("about.title")}</h3>
      <dl class="list about-grid">
        <!-- MuxSU's author, as the LICENCE names them. DisplayMux and Henry Hsu
             are credited in the upstream note below this list. -->
        <div class="row about-item"><dt><i data-lucide="user-round"></i>${t("about.developer")}</dt><dd>Omar Hung</dd></div>
        <div class="row about-item"><dt><i data-lucide="github"></i>GitHub</dt><dd><a href="https://github.com/OmarHung/MuxSU" data-external-url>OmarHung/MuxSU<i data-lucide="external-link"></i></a></dd></div>
        <div class="row about-item"><dt><i data-lucide="activity"></i>${t("about.version")}</dt><dd id="app-version" aria-live="polite">${t("about.loading")}</dd></div>
      </dl>
      <p class="upstream-note">${t("help.upstream")}</p>
    </div>
  </section>`;
}

function resetTab(): string {
  return `<section class="settings-tab reset-section" id="settings-tab-reset" data-settings-panel="reset">
    <header class="tab-head"><h2>${t("settings.resetTitle")}</h2><p>${t("settings.resetIntro")}</p></header>
    <div class="reset-actions">
      <button type="button" class="reset-button" data-reset-scope="displays">
        <strong>${t("settings.resetDisplays")}</strong>
        <small>${t("settings.resetDisplaysHint")}</small>
      </button>
      <button type="button" class="reset-button is-danger" data-reset-scope="everything">
        <strong>${t("settings.resetEverything")}</strong>
        <small>${t("settings.resetEverythingHint")}</small>
      </button>
    </div>
  </section>`;
}

function overlaysHtml(): string {
  return `
  <div class="operation-overlay" id="operation-overlay" aria-live="polite" aria-hidden="true"><div class="operation-dialog"><div class="spinner"></div><h2 id="operation-title">${t("operation.running")}</h2><p id="operation-detail">${t("operation.preparingBody")}</p></div></div>
  <div class="update-overlay" id="update-overlay" aria-hidden="true">
    <div class="update-dialog" role="dialog" aria-modal="true" aria-labelledby="update-title" aria-describedby="update-version">
      <p class="section-kicker">SIGNED UPDATE</p>
      <h2 id="update-title">${t("update.available")}</h2>
      <p id="update-version"></p>
      <div class="update-notes" id="update-notes"></div>
      <div class="update-progress" id="update-progress" hidden><div id="update-progress-bar"></div></div>
      <p class="update-progress-label" id="update-progress-label"></p>
      <div class="update-actions"><button class="scan-button" id="update-cancel" type="button">${t("action.later")}</button><button class="save-button" id="update-install" type="button"><i data-lucide="download"></i>${t("action.downloadInstall")}</button></div>
    </div>
  </div>
  ${diagnosticsDialogsHtml()}
  <div class="onboarding-overlay" id="onboarding-overlay" aria-hidden="true"></div>
  <section class="onboarding-tooltip" id="onboarding-tooltip" role="dialog" aria-modal="false" aria-labelledby="onboarding-title" aria-describedby="onboarding-body" aria-hidden="true">
    <div class="onboarding-tooltip-header">
      <div><p class="section-kicker">PRODUCT TOUR</p><p class="onboarding-counter" id="onboarding-counter"></p></div>
      <button class="onboarding-skip" id="onboarding-skip" type="button">${t("onboarding.skip")}</button>
    </div>
    <span class="onboarding-step-label" id="onboarding-step-label"></span>
    <h2 id="onboarding-title"></h2>
    <p class="onboarding-body" id="onboarding-body"></p>
    <div class="onboarding-status" id="onboarding-status" hidden><strong id="onboarding-status-title"></strong><span id="onboarding-status-detail"></span></div>
    <div class="onboarding-actions">
      <button class="scan-button" id="onboarding-previous" type="button">${t("onboarding.previous")}</button>
      <button class="save-button" id="onboarding-next" type="button"></button>
    </div>
  </section>
  <div class="toast" id="toast" role="status" aria-live="polite"><i data-lucide="zap"></i><div><strong id="toast-title"></strong><span id="toast-detail"></span></div></div>`;
}

/** The whole main window, built once at startup; render functions fill the containers. */
export function appShellHtml(options: { releaseRows: string; minSharedKeyLength: number }): string {
  return `
  <div class="app-shell">
    <header class="toolbar">
      <span class="brand">MuxSU</span>
      <nav class="seg glass" aria-label="${t("nav.aria")}">
        <button type="button" class="seg-button is-active" data-page="dashboard">${t("nav.switch")}</button>
        <button type="button" class="seg-button" data-page="settings">${t("nav.settingsShort")}</button>
      </nav>
      <div class="toolbar-actions">
        <div class="agent-pill glass" id="agent-pill"><span class="status-dot"></span><span>${t("dashboard.agentMissing")}</span></div>
        <button class="icon-button glass" id="update-button" type="button" title="${t("action.checkUpdates")}" aria-label="${t("action.checkUpdates")}"><i data-lucide="download"></i></button>
        <button class="icon-button glass" id="refresh-button" type="button" title="${t("action.refresh")}" aria-label="${t("action.refresh")}"><i data-lucide="refresh-cw"></i></button>
      </div>
    </header>
    <main class="workspace">
      <section class="page is-active" id="dashboard-page">
        <div class="page-head">
          <div><h1 id="page-title">${t("nav.dashboard")}</h1><p id="dashboard-summary"></p></div>
          <div class="page-head-actions">
            <div class="switch-all-bar" id="switch-all-bar" hidden></div>
            <div class="view-toggle glass" id="view-toggle" role="group" aria-label="${t("dashboard.viewAria")}" hidden>
              <button type="button" data-switch-view="stage"><i data-lucide="monitor"></i>${t("dashboard.viewStage")}</button>
              <button type="button" data-switch-view="matrix"><i data-lucide="layout-grid"></i>${t("dashboard.viewMatrix")}</button>
            </div>
          </div>
        </div>
        <div class="group-bar glass" id="group-bar" role="group" aria-label="${t("dashboard.groupAria")}" hidden></div>
        <div id="switch-panel"></div>
      </section>
      <section class="page" id="settings-page">
        <div class="settings-layout">
          <nav class="settings-nav glass" aria-label="${t("settingsTab.aria")}">
            <span class="caption">${t("nav.settingsShort")}</span>
            ${upperTabs.map(tabButton).join("")}
            <span class="spacer"></span>
            ${lowerTabs.map(tabButton).join("")}
          </nav>
          <form id="settings-form" class="settings-content">
            ${displaysTab()}
            ${hostsTab(options.minSharedKeyLength)}
            ${groupsTab()}
            ${startupTab()}
            ${appearanceTab()}
            ${experimentalTab()}
            ${helpTab(options.releaseRows)}
            ${resetTab()}
            <div class="form-actions glass" id="form-actions" hidden>
              <p class="unsaved-note" aria-live="polite"><i data-lucide="alert-circle"></i>${t("settings.unsaved")}</p>
              <div class="form-actions-buttons">
                <button class="cancel-button" type="button" id="discard-button">${t("action.discard")}</button>
                <button class="save-button" type="submit"><i data-lucide="save"></i>${t("action.save")}</button>
              </div>
            </div>
          </form>
        </div>
      </section>
    </main>
  </div>
  ${overlaysHtml()}`;
}
