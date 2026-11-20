// The mail app's client: the element the shell shows at /mail.
import { LitElement, LocalizeController, controls, css, customElement, html } from "ortus-kit";
import { ready, t } from "./i18n.js";

await ready;

@customElement("ortus-mail-app")
export class OrtusMailApp extends LitElement {
  static override styles = [
    controls,
    css`
      :host {
        display: block;
        padding: 32px 24px;
      }
      h1 {
        margin: 0 0 24px;
        font-size: 28px;
        font-weight: 600;
      }
      .empty {
        padding: 48px 24px;
        border-radius: 18px;
        border: 1px dashed var(--ortus-line-strong);
        text-align: center;
      }
      .empty h2 {
        margin: 0 0 8px;
        font-size: 17px;
      }
      .empty p {
        margin: 0;
        color: var(--ortus-text-2);
      }
    `,
  ];

  private readonly localize = new LocalizeController(this);

  override render() {
    return html`
      <h1>${t(this.localize, "inbox")}</h1>
      <section class="empty" role="status">
        <h2>${t(this.localize, "empty")}</h2>
        <p>${t(this.localize, "emptyLead")}</p>
      </section>
    `;
  }
}
