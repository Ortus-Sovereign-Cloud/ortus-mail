// The mail app's client: the element the shell shows at /mail. It shows
// the inbox or Compose by address, and the toast both can raise.
import { LitElement, LocalizeController, css, customElement, html, nothing, state } from "ortus-kit";
import "./addresses.js";
import "./compose.js";
import "./deliverability.js";
import "./domains.js";
import "./inbox.js";
import "./migrations.js";
import "./reconnect.js";
import { navigate, type Toast } from "./events.js";
import { ready, t } from "./i18n.js";
import { jmap } from "./jmap.js";

await ready;

@customElement("ortus-mail-app")
export class OrtusMailApp extends LitElement {
  static override styles = css`
    :host {
      display: block;
    }
    .toast {
      position: fixed;
      inset-block-end: 24px;
      inset-inline-start: 50%;
      translate: -50% 0;
      display: flex;
      align-items: center;
      gap: 14px;
      padding-block: 8px;
      padding-inline: 18px 8px;
      border-radius: 14px;
      background: var(--ortus-text);
      color: var(--ortus-bg);
      font-size: 15px;
      font-weight: 600;
      z-index: 10;
    }
    :host(:dir(rtl)) .toast {
      translate: 50% 0;
    }
    .toast.plain {
      padding-inline: 18px;
      padding-block: 14px;
    }
    .toast button {
      block-size: 36px;
      padding-inline: 14px;
      border-radius: 10px;
      border: 0;
      background: var(--ortus-bg);
      color: var(--ortus-text);
      font: inherit;
      font-size: 14px;
      cursor: pointer;
    }
  `;

  private readonly localize = new LocalizeController(this);

  @state() private path = location.pathname;
  @state() private toast: (Toast & { remaining: number }) | null = null;
  private timer: number | undefined;

  override connectedCallback(): void {
    super.connectedCallback();
    window.addEventListener("popstate", this.onPop);
    window.addEventListener("ortus-mail-toast", this.onToast as EventListener);
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.removeEventListener("popstate", this.onPop);
    window.removeEventListener("ortus-mail-toast", this.onToast as EventListener);
    window.clearInterval(this.timer);
  }

  private readonly onPop = (): void => {
    this.path = location.pathname;
  };

  private readonly onToast = (event: CustomEvent<Toast>): void => {
    window.clearInterval(this.timer);
    const seconds = event.detail.seconds ?? 6;
    this.toast = { ...event.detail, remaining: seconds };
    this.timer = window.setInterval(() => {
      if (!this.toast) return;
      const remaining = this.toast.remaining - 1;
      if (remaining <= 0) {
        window.clearInterval(this.timer);
        this.toast = null;
      } else {
        this.toast = { ...this.toast, remaining };
      }
    }, 1000);
  };

  private async undo(): Promise<void> {
    const undo = this.toast?.undo;
    if (!undo) return;
    window.clearInterval(this.timer);
    this.toast = null;
    const r = await jmap([["EmailSubmission/set", { update: { [undo.submission]: { undoStatus: "canceled" } } }, "u"]]);
    if ((r.u!.updated as Record<string, unknown> | undefined)?.[undo.submission] !== undefined) {
      navigate(`/mail/compose?draft=${encodeURIComponent(undo.draft)}`);
    }
  }

  override render() {
    const screen = this.path === "/mail/compose" ? html`<ortus-mail-compose></ortus-mail-compose>` : this.path.startsWith("/mail/reconnect/") ? html`<ortus-mail-reconnect></ortus-mail-reconnect>` : html`<ortus-mail-inbox></ortus-mail-inbox>`;
    return html`
      ${screen}
      ${this.toast
        ? html`<div class="toast ${this.toast.undo ? "" : "plain"}" role="status">
            <span>${this.toast.text}</span>
            ${this.toast.undo ? html`<button @click=${() => this.undo()}>${t(this.localize, "undoIn", { seconds: this.toast.remaining })}</button>` : nothing}
          </div>`
        : nothing}
    `;
  }
}
