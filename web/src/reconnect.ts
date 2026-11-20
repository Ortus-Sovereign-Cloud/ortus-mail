// The page an owner opens from "your old mailbox needs your password":
// they give the old server's password themselves and copying goes on.
import { ApiError, LitElement, LocalizeController, call, controls, css, customElement, html, nothing, state } from "ortus-kit";
import { navigate } from "./events.js";
import { t } from "./i18n.js";
import { reasonKey } from "./migrations.js";

interface Mine {
  id: string;
  login: string;
  host: string;
  state: "queued" | "running" | "done" | "failed";
  error: string | null;
  copied: number;
}

@customElement("ortus-mail-reconnect")
export class OrtusMailReconnect extends LitElement {
  static override styles = [
    controls,
    css`
      :host {
        display: block;
        max-inline-size: 560px;
        margin-inline: auto;
        padding-block: 64px;
        padding-inline: 24px;
      }
      h1 {
        margin-block: 0 8px;
        margin-inline: 0;
        font-size: 26px;
        font-weight: 600;
      }
      p {
        color: var(--ortus-text-2);
        font-size: 15px;
        line-height: 1.5;
      }
      .why {
        color: var(--ortus-red);
      }
      form {
        display: flex;
        flex-direction: column;
        gap: 14px;
        margin-block-start: 24px;
      }
      label {
        display: flex;
        flex-direction: column;
        gap: 6px;
        font-size: 14px;
      }
      input {
        box-sizing: border-box;
        padding-block: 10px;
        padding-inline: 12px;
        border-radius: 10px;
        border: 1px solid var(--ortus-line-input);
        background: var(--ortus-surface-2);
        color: var(--ortus-text);
        font: inherit;
      }
    `,
  ];

  private readonly localize = new LocalizeController(this);

  @state() private account: Mine | null | undefined;
  @state() private error = "";

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private accountId(): string {
    return decodeURIComponent(location.pathname.split("/")[3] ?? "");
  }

  private async load(): Promise<void> {
    try {
      const r = await call<{ accounts: Mine[] }>("GET", "/mail/imports/mine");
      this.account = r.accounts.find((a) => a.id === this.accountId()) ?? null;
    } catch {
      this.account = null;
    }
    if (this.account && (this.account.state === "queued" || this.account.state === "running")) {
      window.setTimeout(() => void this.load(), 1500);
    }
  }

  private async submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const password = String(new FormData(event.target as HTMLFormElement).get("password") ?? "");
    this.error = "";
    try {
      await call("POST", `/mail/imports/mine/${encodeURIComponent(this.accountId())}/retry`, { password });
      await this.load();
    } catch (e) {
      this.error = e instanceof ApiError && e.status === 404 ? t(this.localize, "reconnectGone") : t(this.localize, "failed");
    }
  }

  override render() {
    const a = this.account;
    if (a === undefined) return nothing;
    if (a === null) {
      return html`<h1>${t(this.localize, "reconnectTitle")}</h1><p>${t(this.localize, "reconnectGone")}</p>`;
    }
    if (a.state !== "failed") {
      return html`<h1>${t(this.localize, "reconnectTitle")}</h1>
        <p role="status">${a.state === "done" ? t(this.localize, "reconnectDone", { count: a.copied }) : t(this.localize, "reconnectCopying", { count: a.copied })}</p>
        <button class="button" @click=${() => navigate("/mail")}>${t(this.localize, "inbox")}</button>`;
    }
    return html`
      <h1>${t(this.localize, "reconnectTitle")}</h1>
      <p>${t(this.localize, "reconnectLead", { host: a.host, login: a.login })}</p>
      <p class="why">${t(this.localize, reasonKey(a.error))}</p>
      <form @submit=${this.submit}>
        <label>${t(this.localize, "oldPassword", { host: a.host })}<input name="password" type="password" required autocomplete="off" /></label>
        ${this.error ? html`<p class="why" role="alert">${this.error}</p>` : nothing}
        <button class="button primary" type="submit">${t(this.localize, "continueCopying")}</button>
      </form>
    `;
  }
}
