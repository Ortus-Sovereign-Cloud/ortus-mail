// The mail app's client: the element the shell shows at /mail.
import { LitElement, css, customElement, html } from "ortus-kit";
import "./domains.js";
import "./inbox.js";
import { ready } from "./i18n.js";

await ready;

@customElement("ortus-mail-app")
export class OrtusMailApp extends LitElement {
  static override styles = css`
    :host {
      display: block;
    }
  `;

  override render() {
    return html`<ortus-mail-inbox></ortus-mail-inbox>`;
  }
}
