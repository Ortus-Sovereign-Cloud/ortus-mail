// Admin › Migrations: mailboxes copied over from the old server, each
// account on its own; failures say why, and are retried with a new
// password or by asking the owner.
import { ApiError, LitElement, LocalizeController, call, controls, css, customElement, html, nothing, state } from "ortus-kit";
import { t } from "./i18n.js";
import type { MailKey } from "./locales/en.js";

interface Account {
  id: string;
  login: string;
  target: string;
  state: "queued" | "running" | "done" | "failed";
  error: string | null;
  copied: number;
}

interface Import {
  id: string;
  host: string;
  port: number;
  security: string;
  created_at: number;
  accounts: Account[];
}

const STATES: Record<Account["state"], MailKey> = {
  queued: "importQueued",
  running: "importRunning",
  done: "importDone",
  failed: "importFailed",
};

/** A failure's reason, in words. */
export function reasonKey(error: string | null): MailKey {
  switch (error) {
    case "login_failed":
      return "errLogin";
    case "connect_failed":
    case "connect_timeout":
    case "bad_host":
      return "errConnect";
    case "tls_failed":
      return "errTls";
    case "not_imap":
      return "errNotImap";
    case "message_too_big":
      return "errTooBig";
    case "password_unreadable":
      return "errPassword";
    default:
      return "errOther";
  }
}

@customElement("ortus-mail-migrations")
export class OrtusMailMigrations extends LitElement {
  static override styles = [
    controls,
    css`
      :host {
        display: block;
        max-inline-size: 1100px;
        padding: 32px 24px;
        box-sizing: border-box;
      }
      .sr {
        position: absolute;
        inline-size: 1px;
        block-size: 1px;
        overflow: hidden;
        clip-path: inset(50%);
        white-space: nowrap;
      }
      .head {
        display: flex;
        justify-content: space-between;
        align-items: flex-start;
        gap: 16px;
        flex-wrap: wrap;
        margin-block-end: 24px;
      }
      h1 {
        margin-block: 0 4px;
        margin-inline: 0;
        font-size: 28px;
        font-weight: 600;
      }
      .lead {
        margin: 0;
        color: var(--ortus-text-2);
        font-size: 15px;
        max-inline-size: 640px;
      }
      .panel {
        margin-block-end: 20px;
        padding: 24px;
        border-radius: 18px;
        border: 1px solid var(--ortus-line);
        background: var(--ortus-surface);
      }
      .panel h2 {
        margin-block: 0 4px;
        margin-inline: 0;
        font-size: 17px;
        font-weight: 600;
      }
      .panel .sub {
        margin-block: 0 16px;
        margin-inline: 0;
        color: var(--ortus-text-3);
        font-size: 13px;
      }
      form {
        display: grid;
        grid-template-columns: minmax(0, 2fr) 120px minmax(0, 1fr);
        gap: 16px;
      }
      label {
        display: flex;
        flex-direction: column;
        gap: 6px;
        font-size: 14px;
        color: var(--ortus-text-2);
      }
      .wide {
        grid-column: 1 / -1;
      }
      input,
      select,
      textarea {
        box-sizing: border-box;
        inline-size: 100%;
        padding-block: 9px;
        padding-inline: 12px;
        border-radius: 10px;
        border: 1px solid var(--ortus-line-input);
        background: var(--ortus-surface-2);
        color: var(--ortus-text);
        font: inherit;
        font-size: 15px;
      }
      textarea {
        min-block-size: 110px;
        font-family: ui-monospace, monospace;
        font-size: 14px;
      }
      .hint {
        color: var(--ortus-text-3);
        font-size: 13px;
      }
      .actions {
        display: flex;
        gap: 10px;
        align-items: center;
      }
      table {
        inline-size: 100%;
        border-collapse: collapse;
        font-size: 14px;
      }
      th {
        text-align: start;
        color: var(--ortus-text-3);
        font-weight: 400;
        padding-block: 8px;
        border-block-end: 1px solid var(--ortus-line);
      }
      td {
        padding-block: 12px;
        padding-inline-end: 12px;
        border-block-end: 1px solid var(--ortus-line);
        vertical-align: top;
      }
      tr:last-child td {
        border-block-end: 0;
      }
      .chip {
        display: inline-block;
        padding: 2px 8px;
        border-radius: 6px;
        font-size: 13px;
        font-weight: 600;
        white-space: nowrap;
      }
      .chip.done {
        background: #163331;
        color: var(--ortus-teal);
      }
      .chip.failed {
        background: #3a1f22;
        color: var(--ortus-red);
      }
      .chip.queued,
      .chip.running {
        background: #232a4d;
        color: var(--ortus-accent);
      }
      .why {
        display: block;
        margin-block-start: 4px;
        color: var(--ortus-red);
        font-size: 13px;
      }
      .sub .why {
        display: inline;
      }
      .fix {
        display: flex;
        gap: 8px;
        align-items: center;
        flex-wrap: wrap;
      }
      .fix input {
        inline-size: 180px;
        padding-block: 6px;
      }
      .small {
        block-size: 34px;
        padding-inline: 12px;
        font-size: 13px;
      }
      .note {
        margin-block: 12px 0;
        margin-inline: 0;
        font-size: 14px;
      }
      .note.error {
        color: var(--ortus-red);
      }
      .empty {
        margin: 0;
        color: var(--ortus-text-3);
        font-size: 14px;
      }
      @media (max-width: 760px) {
        form {
          grid-template-columns: minmax(0, 1fr);
        }
      }
      /* A phone: a table scrolls sideways in its own box. */
      @media (max-width: 720px) {
        table {
          display: block;
          overflow-x: auto;
          /* Hidden labels in it stay in its box. */
          position: relative;
        }
      }
    `,
  ];

  private readonly localize = new LocalizeController(this);

  @state() private imports: Import[] | undefined;
  @state() private formOpen = false;
  @state() private note: { error: boolean; text: string } | null = null;
  @state() private passwords: Record<string, string> = {};
  private timer: number | undefined;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.clearTimeout(this.timer);
  }

  private async load(): Promise<void> {
    window.clearTimeout(this.timer);
    try {
      this.imports = (await call<{ imports: Import[] }>("GET", "/mail/admin/imports")).imports;
    } catch {
      this.imports ??= [];
    }
    // While something is being copied, keep looking.
    if (this.imports.some((i) => i.accounts.some((a) => a.state === "queued" || a.state === "running"))) {
      this.timer = window.setTimeout(() => void this.load(), 1500);
    }
  }

  private async start(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const form = new FormData(event.target as HTMLFormElement);
    const lines = String(form.get("accounts") ?? "")
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean);
    const accounts = lines.map((l) => {
      const [login = "", password = "", to = ""] = l.split(",").map((p) => p.trim());
      return { login, password, to };
    });
    if (!accounts.length || accounts.some((a) => !a.login || !a.password || !a.to)) {
      this.note = { error: true, text: t(this.localize, "accountsFormat") };
      return;
    }
    const security = String(form.get("security"));
    try {
      await call("POST", "/mail/admin/imports", { host: String(form.get("host")).trim(), port: Number(form.get("port")) || undefined, security, accounts });
      this.formOpen = false;
      this.note = { error: false, text: t(this.localize, "importStarted", { count: accounts.length }) };
      await this.load();
    } catch (e) {
      this.note = { error: true, text: e instanceof ApiError && e.status === 400 ? e.message : t(this.localize, "failed") };
    }
  }

  private async retry(account: Account): Promise<void> {
    const password = this.passwords[account.id] ?? "";
    try {
      await call("POST", `/mail/admin/imports/accounts/${encodeURIComponent(account.id)}/retry`, password ? { password } : {});
      this.passwords = { ...this.passwords, [account.id]: "" };
      this.note = { error: false, text: t(this.localize, "retrying", { login: account.login }) };
      await this.load();
    } catch {
      this.note = { error: true, text: t(this.localize, "failed") };
    }
  }

  private async askOwners(item: Import): Promise<void> {
    const ids = item.accounts.filter((a) => a.state === "failed").map((a) => a.id);
    try {
      const r = await call<{ asked: number }>("POST", "/mail/admin/imports/ask-owners", { accounts: ids });
      this.note = { error: false, text: t(this.localize, "ownersAsked", { count: r.asked }) };
    } catch {
      this.note = { error: true, text: t(this.localize, "failed") };
    }
  }

  override render() {
    return html`
      <div class="head">
        <div>
          <h1>${t(this.localize, "migrations")}</h1>
          <p class="lead">${t(this.localize, "migrationsLead")}</p>
        </div>
        <button class="button primary" aria-expanded=${this.formOpen ? "true" : "false"} @click=${() => (this.formOpen = !this.formOpen)}>${t(this.localize, "newImport")}</button>
      </div>
      ${this.note ? html`<p class="note ${this.note.error ? "error" : ""}" role=${this.note.error ? "alert" : "status"}>${this.note.text}</p>` : nothing}
      ${this.formOpen ? this.form() : nothing}
      ${this.imports === undefined
        ? nothing
        : this.imports.length
          ? this.imports.map((i) => this.item(i))
          : html`<section class="panel"><p class="empty">${t(this.localize, "noImports")}</p></section>`}
    `;
  }

  private form() {
    return html`<section class="panel" aria-labelledby="new-import">
      <h2 id="new-import">${t(this.localize, "newImport")}</h2>
      <p class="sub">${t(this.localize, "newImportLead")}</p>
      <form @submit=${this.start}>
        <label>${t(this.localize, "oldServer")}<input name="host" required autocomplete="off" placeholder="imap.old-provider.example" /></label>
        <label>${t(this.localize, "port")}<input name="port" inputmode="numeric" placeholder="993" /></label>
        <label>${t(this.localize, "security")}
          <select name="security">
            <option value="tls">${t(this.localize, "securityTls")}</option>
            <option value="starttls">${t(this.localize, "securityStarttls")}</option>
          </select>
        </label>
        <label class="wide">${t(this.localize, "accountsToCopy")}
          <textarea name="accounts" required spellcheck="false" placeholder="olena@old.example, ••••••••, olena@ministry.example"></textarea>
          <span class="hint">${t(this.localize, "accountsFormat")}</span>
        </label>
        <div class="actions wide">
          <button class="button primary" type="submit">${t(this.localize, "startImport")}</button>
          <button class="button" type="button" @click=${() => (this.formOpen = false)}>${t(this.localize, "cancel")}</button>
        </div>
      </form>
    </section>`;
  }

  private item(item: Import) {
    const failed = item.accounts.filter((a) => a.state === "failed").length;
    const done = item.accounts.filter((a) => a.state === "done").length;
    const date = new Intl.DateTimeFormat(document.documentElement.lang || "en", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" }).format(new Date(item.created_at));
    return html`<section class="panel" aria-labelledby="import-${item.id}">
      <div class="head">
        <div>
          <h2 id="import-${item.id}">${item.host}</h2>
          <p class="sub">${date} · ${t(this.localize, "importSummary", { done, total: item.accounts.length })}${failed ? html` · <span class="why">${t(this.localize, "failedCount", { count: failed })}</span>` : nothing}</p>
        </div>
        ${failed ? html`<button class="button" @click=${() => this.askOwners(item)}>${t(this.localize, "askOwners")}</button>` : nothing}
      </div>
      <table>
        <thead>
          <tr>
            <th scope="col">${t(this.localize, "oldAccount")}</th>
            <th scope="col">${t(this.localize, "goesTo")}</th>
            <th scope="col">${t(this.localize, "progress")}</th>
            <th scope="col"><span class="sr">${t(this.localize, "actions")}</span></th>
          </tr>
        </thead>
        <tbody>
          ${item.accounts.map(
            (a) => html`<tr>
              <td dir="auto">${a.login}</td>
              <td dir="auto">${a.target}</td>
              <td>
                <span class="chip ${a.state}">${t(this.localize, STATES[a.state])}</span>
                ${t(this.localize, "copiedCount", { count: a.copied })}
                ${a.state === "failed" ? html`<span class="why">${t(this.localize, reasonKey(a.error))}</span>` : nothing}
              </td>
              <td>
                ${a.state === "failed"
                  ? html`<div class="fix">
                      ${a.error === "login_failed" || a.error === "password_unreadable"
                        ? html`<input type="password" autocomplete="off" aria-label=${t(this.localize, "newPasswordFor", { login: a.login })} placeholder=${t(this.localize, "newPassword")} .value=${this.passwords[a.id] ?? ""} @input=${(e: InputEvent) => (this.passwords = { ...this.passwords, [a.id]: (e.target as HTMLInputElement).value })} />`
                        : nothing}
                      <button class="button small" @click=${() => this.retry(a)}>${t(this.localize, "retry")}</button>
                    </div>`
                  : nothing}
              </td>
            </tr>`,
          )}
        </tbody>
      </table>
    </section>`;
  }
}
