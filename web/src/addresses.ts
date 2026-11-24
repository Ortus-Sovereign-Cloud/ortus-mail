// Admin › Addresses: addresses people have besides their own, in this
// installation's mail domains (an owner who signs in with a personal
// address can still have mail here; a role address like info@).
import { ApiError, LitElement, LocalizeController, call, controls, css, customElement, html, nothing, property, state, type Me } from "ortus-kit";
import { t } from "./i18n.js";
import type { MailKey } from "./locales/en.js";

interface Row {
  address: string;
  user_id: string;
  name: string | null;
  email: string | null;
}

interface Person {
  id: string;
  name: string;
  email: string;
}

@customElement("ortus-mail-addresses")
export class OrtusMailAddresses extends LitElement {
  static override styles = [
    controls,
    css`
      :host {
        display: block;
        max-inline-size: 1040px;
        padding: 32px 24px;
        box-sizing: border-box;
      }
      .head {
        display: flex;
        align-items: flex-start;
        justify-content: space-between;
        gap: 16px;
        flex-wrap: wrap;
        margin-block-end: 24px;
      }
      h1 {
        margin: 0 0 4px;
        font-size: 28px;
        font-weight: 600;
      }
      .lead {
        margin: 0;
        color: var(--ortus-text-2);
        font-size: 15px;
        max-inline-size: 640px;
      }
      .card {
        border: 1px solid var(--ortus-line);
        border-radius: 16px;
        background: var(--ortus-surface);
        overflow: hidden;
      }
      table {
        inline-size: 100%;
        border-collapse: collapse;
      }
      th {
        padding: 12px 24px;
        color: var(--ortus-text-3);
        font-size: 13px;
        font-weight: 500;
        text-align: start;
      }
      td {
        padding: 14px 24px;
        border-block-start: 1px solid var(--ortus-line);
        font-size: 15px;
        vertical-align: middle;
      }
      td small {
        display: block;
        color: var(--ortus-text-3);
        font-size: 13px;
      }
      td:last-child {
        text-align: end;
      }
      .empty {
        padding: 24px;
        margin: 0;
        color: var(--ortus-text-2);
      }
      dialog {
        inline-size: min(440px, calc(100vw - 32px));
        padding: 24px;
        border-radius: 18px;
        border: 1px solid var(--ortus-line-strong);
        background: var(--ortus-surface);
        color: var(--ortus-text);
      }
      dialog::backdrop {
        background: rgba(0, 0, 0, 0.6);
      }
      dialog h2 {
        margin: 0 0 16px;
        font-size: 20px;
      }
      label {
        display: block;
        margin-block: 12px 6px;
        color: var(--ortus-text-2);
        font-size: 14px;
      }
      input,
      select {
        box-sizing: border-box;
        inline-size: 100%;
        block-size: 44px;
        padding-inline: 12px;
        border-radius: 10px;
        border: 1px solid var(--ortus-line-input);
        background-color: var(--ortus-surface-2);
        color: var(--ortus-text);
        font: inherit;
      }
      .split {
        display: grid;
        grid-template-columns: 1fr auto 1fr;
        align-items: center;
        gap: 8px;
      }
      .row {
        display: flex;
        justify-content: flex-end;
        gap: 8px;
        margin-block-start: 20px;
      }
      .hint {
        color: var(--ortus-text-3);
        font-size: 13px;
        margin: 8px 0 0;
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
  @property({ attribute: false }) me: Me | undefined;
  @state() private rows: Row[] = [];
  @state() private domains: string[] = [];
  @state() private people: Person[] = [];
  @state() private loaded = false;
  @state() private message: { text: string; error: boolean } | undefined;
  @state() private dialogError = "";

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    const [list, people] = await Promise.all([
      call<{ addresses: Row[]; domains: string[] }>("GET", "/mail/admin/addresses"),
      call<{ users: Person[] }>("GET", "/admin/users").catch(() => ({ users: [] as Person[] })),
    ]);
    this.rows = list.addresses;
    this.domains = list.domains;
    this.people = people.users;
    this.loaded = true;
  }

  private say(key: MailKey, args: Record<string, string> = {}, error = false): void {
    this.message = { text: t(this.localize, key, args), error };
  }

  private open = (): void => {
    this.dialogError = "";
    this.renderRoot.querySelector<HTMLDialogElement>("dialog")?.showModal();
  };

  private add = async (event: SubmitEvent): Promise<void> => {
    event.preventDefault();
    const form = event.target as HTMLFormElement;
    const data = new FormData(form);
    const address = `${String(data.get("local") ?? "").trim()}@${String(data.get("domain") ?? "")}`;
    const user = String(data.get("person") ?? "");
    try {
      await call("POST", "/mail/admin/addresses", { address, user_id: user });
    } catch (e) {
      // The server says why (taken, not a domain here, …).
      this.dialogError = e instanceof ApiError ? e.message : t(this.localize, "addressFailed");
      return;
    }
    this.renderRoot.querySelector<HTMLDialogElement>("dialog")?.close();
    form.reset();
    const who = this.people.find((p) => p.id === user)?.name ?? "";
    this.say("addressAdded", { address: address.toLowerCase(), name: who });
    await this.load();
  };

  private async takeAway(row: Row): Promise<void> {
    if (!confirm(t(this.localize, "addressRemoveConfirm", { address: row.address, name: row.name ?? "" }))) return;
    try {
      await call("DELETE", `/mail/admin/addresses/${encodeURIComponent(row.address)}`);
    } catch (e) {
      this.message = { text: e instanceof ApiError ? e.message : t(this.localize, "addressFailed"), error: true };
      return;
    }
    this.say("addressRemoved", { address: row.address });
    await this.load();
  }

  override render() {
    if (!this.loaded) return nothing;
    return html`
      <div class="head">
        <div>
          <h1>${t(this.localize, "addresses")}</h1>
          <p class="lead">${t(this.localize, "addressesLead")}</p>
        </div>
        ${this.domains.length ? html`<button class="button primary" @click=${this.open}>${t(this.localize, "addAddress")}</button>` : nothing}
      </div>
      ${this.message ? html`<p class=${this.message.error ? "error" : "status"} role=${this.message.error ? "alert" : "status"}>${this.message.text}</p>` : nothing}
      <div class="card">
        ${!this.domains.length
          ? html`<p class="empty">${t(this.localize, "addressesNoDomain")}</p>`
          : this.rows.length
            ? html`<table>
                <thead>
                  <tr>
                    <th>${t(this.localize, "address")}</th>
                    <th>${t(this.localize, "addressPerson")}</th>
                    <th></th>
                  </tr>
                </thead>
                <tbody>
                  ${this.rows.map(
                    (r) => html`<tr>
                      <td>${r.address}</td>
                      <td>${r.name ?? ""}<small>${r.email ?? ""}</small></td>
                      <td><button class="button" @click=${() => this.takeAway(r)} aria-label=${t(this.localize, "addressRemoveNamed", { address: r.address })}>${t(this.localize, "remove")}</button></td>
                    </tr>`,
                  )}
                </tbody>
              </table>`
            : html`<p class="empty">${t(this.localize, "addressesEmpty")}</p>`}
      </div>
      <dialog aria-labelledby="address-title">
        <form @submit=${this.add}>
          <h2 id="address-title">${t(this.localize, "addAddress")}</h2>
          <label for="person">${t(this.localize, "addressPerson")}</label>
          <select id="person" name="person" required>
            ${this.people.map((p) => html`<option value=${p.id}>${p.name} · ${p.email}</option>`)}
          </select>
          <label for="local">${t(this.localize, "address")}</label>
          <div class="split">
            <input id="local" name="local" required autocomplete="off" placeholder="bohdan" aria-describedby="address-hint" />
            <span aria-hidden="true">@</span>
            <select name="domain" aria-label=${t(this.localize, "domainName")}>
              ${this.domains.map((d) => html`<option value=${d}>${d}</option>`)}
            </select>
          </div>
          <p class="hint" id="address-hint">${t(this.localize, "addressHint")}</p>
          ${this.dialogError ? html`<p class="error" role="alert">${this.dialogError}</p>` : nothing}
          <div class="row">
            <button type="button" class="button" @click=${() => this.renderRoot.querySelector<HTMLDialogElement>("dialog")?.close()}>${t(this.localize, "cancel")}</button>
            <button class="button primary">${t(this.localize, "add")}</button>
          </div>
        </form>
      </dialog>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "ortus-mail-addresses": OrtusMailAddresses;
  }
}
