// Admin › Domains (AdminDomains): mail domains, the records to publish,
// and what DNS says about each.
import { ApiError, LitElement, LocalizeController, call, controls, css, customElement, html, nothing, property, state, svg, type Me } from "ortus-kit";

const COPY = svg`<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/></svg>`;
import { t } from "./i18n.js";
import type { MailKey } from "./locales/en.js";

type Status = "ok" | "missing" | "different" | "lenient";

interface DomainRow {
  domain: string;
  state: "verified" | "needs_fix" | "waiting";
  checked_at: number | null;
  fixes: number;
}

interface RecordRow {
  purpose: string;
  kind: string;
  name: string;
  value: string;
  required: boolean;
  status: Status;
  found: string[];
}

interface Domain {
  domain: string;
  state: DomainRow["state"];
  checked_at: number | null;
  records: RecordRow[];
  hosts: { mail: string; web: string };
}

@customElement("ortus-mail-domains")
export class OrtusMailDomains extends LitElement {
  static override styles = [
    controls,
    css`
      :host {
        display: block;
        max-inline-size: 1240px;
        padding: 32px 24px;
        box-sizing: border-box;
      }
      .head {
        display: flex;
        align-items: flex-start;
        justify-content: space-between;
        gap: 16px;
        flex-wrap: wrap;
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
      }
      .tabs {
        display: flex;
        gap: 8px;
        flex-wrap: wrap;
        margin-block: 24px 16px;
      }
      .tab {
        display: flex;
        align-items: center;
        gap: 10px;
        padding: 10px 16px;
        border-radius: 12px;
        border: 1px solid var(--ortus-line);
        background: var(--ortus-surface);
        color: var(--ortus-text);
        font: inherit;
        font-size: 15px;
        font-weight: 600;
        cursor: pointer;
      }
      .tab[aria-selected="true"] {
        border-color: var(--ortus-accent);
      }
      .chip {
        padding: 2px 8px;
        border-radius: 6px;
        font-size: 13px;
        font-weight: 500;
      }
      .verified,
      .ok {
        background: rgba(79, 209, 197, 0.14);
        color: var(--ortus-teal);
      }
      .needs_fix,
      .lenient,
      .different {
        background: rgba(245, 163, 92, 0.16);
        color: var(--ortus-orange);
      }
      .waiting,
      .missing {
        background: rgba(255, 255, 255, 0.08);
        color: var(--ortus-text-2);
      }
      .cards {
        display: grid;
        grid-template-columns: repeat(4, minmax(0, 1fr));
        gap: 16px;
      }
      .card {
        padding: 20px 24px;
        border-radius: 18px;
        border: 1px solid var(--ortus-line);
        background: var(--ortus-surface);
      }
      .card small {
        display: block;
        color: var(--ortus-text-2);
        font-size: 13px;
      }
      .card strong {
        display: block;
        margin-block: 6px;
        font-size: 18px;
        font-weight: 600;
      }
      .card strong.good {
        color: var(--ortus-text);
      }
      .card strong.warn {
        color: var(--ortus-orange);
      }
      .records {
        margin-block-start: 16px;
        border-radius: 18px;
        border: 1px solid var(--ortus-line);
        background: var(--ortus-surface);
        overflow: hidden;
      }
      .records header {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
        flex-wrap: wrap;
        padding: 20px 24px;
      }
      .records h2 {
        margin: 0;
        font-size: 17px;
      }
      .records header div {
        display: flex;
        align-items: center;
        gap: 8px;
      }
      table {
        inline-size: 100%;
        border-collapse: collapse;
      }
      th {
        padding: 12px 24px;
        border-block-start: 1px solid var(--ortus-line);
        color: var(--ortus-text-3);
        font-size: 13px;
        font-weight: 500;
        text-align: start;
      }
      td:first-child {
        white-space: nowrap;
      }
      td {
        padding: 14px 24px;
        border-block-start: 1px solid var(--ortus-line);
        font-size: 15px;
        vertical-align: middle;
      }
      code {
        font-family: "Geist Mono Variable", ui-monospace, monospace;
        font-size: 13px;
      }
      td.value code {
        display: block;
        max-inline-size: 420px;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      .copy {
        display: inline-flex;
        align-items: center;
        justify-content: center;
        inline-size: 36px;
        block-size: 36px;
        padding: 0;
      }
      .advice td {
        border-block-start: 0;
        padding-block: 0 14px;
      }
      .advice p {
        margin: 0;
        padding: 12px 16px;
        border-radius: 12px;
        border: 1px solid rgba(245, 163, 92, 0.5);
        background: rgba(245, 163, 92, 0.08);
        color: var(--ortus-orange);
        font-size: 14px;
      }
      .muted {
        color: var(--ortus-text-2);
        font-size: 13px;
      }
      .empty {
        margin-block-start: 24px;
        padding: 48px 24px;
        border-radius: 18px;
        border: 1px dashed var(--ortus-line-strong);
        text-align: center;
        color: var(--ortus-text-2);
      }
      dialog {
        inline-size: min(420px, calc(100vw - 32px));
        padding: 24px;
        border-radius: 18px;
        border: 1px solid var(--ortus-line-strong);
        background: var(--ortus-surface);
        color: var(--ortus-text);
      }
      dialog::backdrop {
        background: rgba(0, 0, 0, 0.6);
      }
      .row {
        display: flex;
        justify-content: flex-end;
        gap: 8px;
        margin-block-start: 20px;
      }
      .status {
        margin-block: 12px 0;
        color: var(--ortus-teal);
        font-size: 14px;
      }
      @media (max-width: 1100px) {
        .cards {
          grid-template-columns: repeat(2, minmax(0, 1fr));
        }
        .kind,
        .name {
          display: none;
        }
      }
      /* A phone: one card a row, and the records table scrolls in its own box. */
      @media (max-width: 720px) {
        .cards {
          grid-template-columns: minmax(0, 1fr);
        }
        .cards * {
          overflow-wrap: anywhere;
        }
        table {
          display: block;
          overflow-x: auto;
          /* Hidden labels in it stay in its box. */
          position: relative;
        }
        .records header div {
          flex-wrap: wrap;
        }
        .records header button {
          white-space: nowrap;
        }
      }
    `,
  ];

  private readonly localize = new LocalizeController(this);
  @property({ attribute: false }) me: Me | undefined;
  @state() private domains: DomainRow[] = [];
  @state() private current: Domain | undefined;
  @state() private message: { key: MailKey; error: boolean } | undefined;
  @state() private loaded = false;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(select?: string): Promise<void> {
    this.domains = (await call<{ domains: DomainRow[] }>("GET", "/mail/admin/domains")).domains;
    this.loaded = true;
    const name = select ?? this.current?.domain ?? this.domains[0]?.domain;
    this.current = name ? await call<Domain>("GET", `/mail/admin/domains/${name}`) : undefined;
  }

  private async act(work: () => Promise<unknown>, done: MailKey, select?: string): Promise<void> {
    this.message = undefined;
    try {
      await work();
      this.message = { key: done, error: false };
      await this.load(select);
    } catch (error) {
      const code = error instanceof ApiError ? error.code : "";
      this.message = { key: code === "exists" ? "domainExists" : error instanceof ApiError && error.status === 400 ? "domainInvalid" : "failed", error: true };
    }
  }

  private add(event: SubmitEvent): void {
    event.preventDefault();
    const form = event.target as HTMLFormElement;
    const domain = String(new FormData(form).get("domain") ?? "").trim().toLowerCase();
    void this.act(async () => {
      await call("POST", "/mail/admin/domains", { domain });
      form.reset();
      this.renderRoot.querySelector<HTMLDialogElement>("dialog")?.close();
    }, "domainAdded", domain);
  }

  private ago(ms: number | null): string {
    if (!ms) return t(this.localize, "notChecked");
    const minutes = Math.max(0, Math.round((Date.now() - ms) / 60_000));
    return t(this.localize, "checkedAgo", { time: new Intl.RelativeTimeFormat(this.localize.lang, { numeric: "auto" }).format(-minutes, "minute") });
  }

  private status(record: RecordRow) {
    const key: MailKey = record.status === "lenient" ? "statusLenient" : record.status === "ok" ? "statusOk" : record.status === "different" ? "statusDifferent" : "statusMissing";
    return html`<span class="chip ${record.status}">${t(this.localize, key)}</span>`;
  }

  private cards(d: Domain) {
    const of = (purpose: string) => d.records.find((r) => r.purpose === purpose);
    const ok = (purpose: string) => of(purpose)?.status === "ok";
    const receiving = ok("receive");
    const signing = ok("spf") && ok("dkim_rsa");
    const dmarc = of("dmarc")?.status;
    const sts = ok("mta_sts");
    const kinds = ["imap", "submission", "jmap", "autoconfig", "caldav", "carddav"].filter((p) => of(p));
    const apps = kinds.filter(ok).length;
    return html`<div class="cards">
      <div class="card">
        <small>${t(this.localize, "cardReceiving")}</small>
        <strong class=${receiving ? "good" : "warn"}>${t(this.localize, receiving ? "working" : "notYet")}</strong>
        <small>${t(this.localize, "mxPoints", { host: d.hosts.mail })}</small>
      </div>
      <div class="card">
        <small>${t(this.localize, "cardSending")}</small>
        <strong class=${signing && dmarc === "ok" ? "good" : "warn"}>${t(this.localize, signing ? "spfDkimOk" : "spfDkimMissing")}</strong>
        <small>${t(this.localize, dmarc === "ok" ? "dmarcEnforced" : dmarc === "lenient" ? "dmarcNone" : "dmarcMissing")}</small>
      </div>
      <div class="card">
        <small>${t(this.localize, "cardTransport")}</small>
        <strong class=${sts ? "good" : "warn"}>${t(this.localize, sts ? "stsOn" : "stsOff")}</strong>
        <small>${t(this.localize, "tlsAlways")}</small>
      </div>
      <div class="card">
        <small>${t(this.localize, "cardApps")}</small>
        <strong class=${apps === kinds.length ? "good" : "warn"}>${t(this.localize, "appsFound", { count: apps, total: kinds.length })}</strong>
        <small>${t(this.localize, "appsLead")}</small>
      </div>
    </div>`;
  }

  private table(d: Domain) {
    return html`<section class="records" aria-labelledby="records">
      <header>
        <h2 id="records">${t(this.localize, "recordsFor", { domain: d.domain })}</h2>
        <div>
          <span class="muted">${this.ago(d.checked_at)}</span>
          <button class="button" @click=${() => this.act(() => call("POST", `/mail/admin/domains/${d.domain}/check`), "checked")}>${t(this.localize, "checkNow")}</button>
          <a class="button" href=${`/api/v1/mail/admin/domains/${d.domain}/zone`} download>${t(this.localize, "exportZone")}</a>
        </div>
      </header>
      <table>
        <thead>
          <tr>
            <th>${t(this.localize, "colPurpose")}</th>
            <th class="kind">${t(this.localize, "colType")}</th>
            <th class="name">${t(this.localize, "colName")}</th>
            <th>${t(this.localize, "colValue")}</th>
            <th><span hidden>${t(this.localize, "copy")}</span></th>
            <th>${t(this.localize, "colStatus")}</th>
          </tr>
        </thead>
        <tbody>
          ${d.records.map(
            (r) => html`<tr>
                <td>${t(this.localize, `purpose_${r.purpose}` as MailKey)}</td>
                <td class="kind"><code>${r.kind}</code></td>
                <td class="name"><code>${r.name}</code></td>
                <td class="value"><code title=${r.value}>${r.value}</code></td>
                <td>
                  <button class="button copy" aria-label=${t(this.localize, "copyValue", { purpose: t(this.localize, `purpose_${r.purpose}` as MailKey) })} @click=${() => navigator.clipboard?.writeText(r.value)}>${COPY}</button>
                </td>
                <td>${this.status(r)}</td>
              </tr>
              ${r.status === "lenient"
                ? html`<tr class="advice">
                    <td colspan="6"><p role="note">${t(this.localize, "dmarcAdvice")}</p></td>
                  </tr>`
                : nothing}`,
          )}
        </tbody>
      </table>
    </section>`;
  }

  override render() {
    if (!this.loaded) return nothing;
    const owner = this.me?.role === "owner";
    const stateKey = (s: DomainRow["state"]): MailKey => (s === "verified" ? "stateVerified" : s === "waiting" ? "stateWaiting" : "stateNeedsFix");
    return html`
      <div class="head">
        <div>
          <h1>${t(this.localize, "domains")}</h1>
          <p class="lead">${t(this.localize, "domainsLead")}</p>
        </div>
        ${owner ? html`<button class="button primary" @click=${() => this.renderRoot.querySelector<HTMLDialogElement>("dialog")?.showModal()}>${t(this.localize, "addDomain")}</button>` : nothing}
      </div>
      ${this.message ? html`<p class=${this.message.error ? "error" : "status"} role=${this.message.error ? "alert" : "status"}>${t(this.localize, this.message.key)}</p>` : nothing}
      ${this.domains.length
        ? html`<div class="tabs" role="tablist" aria-label=${t(this.localize, "domains")}>
              ${this.domains.map(
                (d) => html`<button class="tab" role="tab" aria-selected=${this.current?.domain === d.domain ? "true" : "false"} @click=${() => this.load(d.domain)}>
                  ${d.domain}
                  <span class="chip ${d.state}">${d.state === "needs_fix" ? t(this.localize, "needsFixes", { count: d.fixes }) : t(this.localize, stateKey(d.state))}</span>
                </button>`,
              )}
            </div>
            ${this.current ? html`${this.cards(this.current)} ${this.table(this.current)}` : nothing}`
        : html`<p class="empty">${t(this.localize, "noDomains")}</p>`}
      <dialog aria-labelledby="add-title">
        <form @submit=${this.add}>
          <h2 id="add-title">${t(this.localize, "addDomain")}</h2>
          <label for="new-domain">${t(this.localize, "domainName")}</label>
          <input id="new-domain" name="domain" required placeholder="ministry.example" autocomplete="off" />
          <div class="row">
            <button type="button" class="button" @click=${() => this.renderRoot.querySelector<HTMLDialogElement>("dialog")?.close()}>${t(this.localize, "cancel")}</button>
            <button class="button primary">${t(this.localize, "add")}</button>
          </div>
        </form>
      </dialog>
    `;
  }
}
