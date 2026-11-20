// Admin › Deliverability (AdminDeliverability): is our mail getting
// through, and is anyone sending as our domains?
import { LitElement, LocalizeController, call, controls, css, customElement, html, nothing, state, svg } from "ortus-kit";
import { t } from "./i18n.js";
import type { MailKey } from "./locales/en.js";

interface Summary {
  days: number;
  sent: number;
  per_day_average: number;
  delivered: number | null;
  delivered_before: number | null;
  bounced: number | null;
  hard_bounces: number;
  queue: { count: number; oldest: number | null; retrying: number };
  per_day: { at: number; delivered: number; deferred: number; bounced: number }[];
  bucket: "hour" | "day";
  providers: { name: "gmail" | "microsoft" | "yahoo" | "corporate"; delivered: number | null; total: number }[];
  senders: { domain: string; groups: { ours: boolean; name: string; count: number; status: "pass" | "dkim_fail" | "spoofing" }[] }[];
}

const PERIODS = [
  { days: 1, key: "period24h" },
  { days: 14, key: "period14d" },
  { days: 90, key: "period90d" },
] as const;

const PROVIDERS: Record<Summary["providers"][number]["name"], MailKey> = {
  gmail: "providerGmail",
  microsoft: "providerMicrosoft",
  yahoo: "providerYahoo",
  corporate: "providerCorporate",
};

const STATUS: Record<Summary["senders"][number]["groups"][number]["status"], MailKey> = {
  pass: "senderPass",
  dkim_fail: "senderDkimFail",
  spoofing: "senderSpoofing",
};

@customElement("ortus-mail-deliverability")
export class OrtusMailDeliverability extends LitElement {
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
        justify-content: space-between;
        align-items: flex-start;
        gap: 16px;
        flex-wrap: wrap;
        margin-block-end: 20px;
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
      }
      .periods {
        display: flex;
        gap: 4px;
        padding: 4px;
        border-radius: 14px;
        border: 1px solid var(--ortus-line-strong);
      }
      .periods button {
        block-size: 34px;
        padding-inline: 14px;
        border: 0;
        border-radius: 10px;
        background: none;
        color: var(--ortus-text-3);
        font: inherit;
        font-size: 14px;
        cursor: pointer;
      }
      .periods button[aria-pressed="true"] {
        background: var(--ortus-surface-2);
        color: var(--ortus-text);
        font-weight: 600;
      }
      .cards {
        display: grid;
        grid-template-columns: repeat(4, minmax(0, 1fr));
        gap: 16px;
        margin-block-end: 20px;
      }
      .card,
      .panel {
        border-radius: 18px;
        border: 1px solid var(--ortus-line);
        background: var(--ortus-surface);
      }
      .card {
        padding: 22px 24px;
      }
      .card h2 {
        margin-block: 0 8px;
        margin-inline: 0;
        font-size: 13px;
        font-weight: 400;
        color: var(--ortus-text-3);
      }
      .card .value {
        margin: 0;
        font-size: 24px;
        font-weight: 600;
      }
      .card .note {
        margin-block: 8px 0;
        margin-inline: 0;
        font-size: 13px;
      }
      .good {
        color: var(--ortus-teal);
      }
      .warn {
        color: var(--ortus-orange);
      }
      .bad {
        color: var(--ortus-red);
      }
      .muted {
        color: var(--ortus-text-3);
      }
      .panel {
        padding: 24px;
        margin-block-end: 20px;
      }
      .panel h2 {
        margin: 0;
        font-size: 16px;
        font-weight: 600;
      }
      .panel-head {
        display: flex;
        justify-content: space-between;
        align-items: center;
        gap: 16px;
        flex-wrap: wrap;
        margin-block-end: 20px;
      }
      .legend {
        display: flex;
        gap: 16px;
        font-size: 13px;
        color: var(--ortus-text-2);
      }
      .legend span::before {
        content: "";
        display: inline-block;
        inline-size: 10px;
        block-size: 10px;
        border-radius: 3px;
        margin-inline-end: 6px;
        vertical-align: -1px;
      }
      .legend .l-delivered::before,
      .d {
        background: var(--ortus-accent);
        fill: var(--ortus-accent);
      }
      .legend .l-deferred::before,
      .f {
        background: var(--ortus-orange);
        fill: var(--ortus-orange);
      }
      .legend .l-bounced::before,
      .b {
        background: var(--ortus-red);
        fill: var(--ortus-red);
      }
      svg.chart {
        display: block;
        inline-size: 100%;
        block-size: 140px;
      }
      :host(:dir(rtl)) svg.chart {
        transform: scaleX(-1);
      }
      .axis {
        display: flex;
        justify-content: space-between;
        margin-block-start: 14px;
        font-size: 13px;
        color: var(--ortus-text-3);
      }
      .two {
        display: grid;
        grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
        gap: 16px;
      }
      .provider {
        display: grid;
        grid-template-columns: 180px minmax(0, 1fr) 64px;
        gap: 16px;
        align-items: center;
        block-size: 52px;
        font-size: 15px;
      }
      .provider .pct {
        text-align: end;
        color: var(--ortus-text-2);
        font-size: 14px;
      }
      svg.bar {
        display: block;
        inline-size: 100%;
        block-size: 6px;
      }
      :host(:dir(rtl)) svg.bar {
        transform: scaleX(-1);
      }
      .track {
        fill: var(--ortus-line-strong);
      }
      ul {
        list-style: none;
        margin: 0;
        padding: 0;
        display: flex;
        flex-direction: column;
        gap: 8px;
      }
      li.sender {
        display: grid;
        grid-template-columns: minmax(0, 1fr) auto auto;
        gap: 16px;
        align-items: center;
        padding: 10px 16px;
        border-radius: 10px;
        background: var(--ortus-surface-2);
        font-size: 15px;
        font-weight: 600;
      }
      li.sender .count {
        color: var(--ortus-text-2);
        font-size: 14px;
        font-weight: 400;
      }
      .chip {
        padding: 2px 8px;
        border-radius: 6px;
        font-size: 13px;
        font-weight: 600;
      }
      .chip.pass {
        background: #163331;
        color: var(--ortus-teal);
      }
      .chip.dkim_fail,
      .chip.spoofing {
        background: #3a1f22;
        color: var(--ortus-red);
      }
      .empty {
        margin: 0;
        color: var(--ortus-text-3);
        font-size: 14px;
        line-height: 1.5;
      }
      .skeleton {
        block-size: 120px;
        border-radius: 18px;
        background: var(--ortus-surface);
      }
      @media (max-width: 1100px) {
        .cards {
          grid-template-columns: repeat(2, minmax(0, 1fr));
        }
        .two {
          grid-template-columns: minmax(0, 1fr);
        }
      }
    `,
  ];

  private readonly localize = new LocalizeController(this);

  @state() private days = 14;
  @state() private summary: Summary | undefined;
  @state() private failed = false;
  private loads = 0;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    const ticket = ++this.loads;
    this.failed = false;
    try {
      const summary = await call<Summary>("GET", `/mail/admin/deliverability?days=${this.days}`);
      if (ticket === this.loads) this.summary = summary;
    } catch {
      if (ticket === this.loads) this.failed = true;
    }
  }

  private locale(): string {
    return document.documentElement.lang || "en";
  }

  private pct(value: number | null): string {
    return value === null ? "–" : new Intl.NumberFormat(this.locale(), { style: "percent", maximumFractionDigits: 1 }).format(value / 100);
  }

  private num(value: number): string {
    return new Intl.NumberFormat(this.locale()).format(value);
  }

  private age(since: number): string {
    const minutes = Math.max(1, Math.round((Date.now() - since) / 60_000));
    const [value, unit] = minutes < 120 ? [minutes, "minute"] : minutes < 2880 ? [Math.round(minutes / 60), "hour"] : [Math.round(minutes / 1440), "day"];
    return new Intl.NumberFormat(this.locale(), { style: "unit", unit, unitDisplay: "short" }).format(value);
  }

  override render() {
    return html`
      <div class="head">
        <div>
          <h1>${t(this.localize, "deliverability")}</h1>
          <p class="lead">${t(this.localize, "deliverabilityLead")}</p>
        </div>
        <div class="periods" role="group" aria-label=${t(this.localize, "period")}>
          ${PERIODS.map(
            (p) => html`<button aria-pressed=${this.days === p.days ? "true" : "false"} @click=${() => {
              this.days = p.days;
              void this.load();
            }}>${t(this.localize, p.key)}</button>`,
          )}
        </div>
      </div>
      ${this.failed
        ? html`<p class="empty" role="alert">${t(this.localize, "failed")} <button class="button" @click=${() => this.load()}>${t(this.localize, "tryAgain")}</button></p>`
        : this.summary
          ? this.body(this.summary)
          : html`<div class="skeleton"></div>`}
    `;
  }

  private body(s: Summary) {
    const change = s.delivered !== null && s.delivered_before !== null ? Math.round((s.delivered - s.delivered_before) * 10) / 10 : null;
    return html`
      <div class="cards">
        <section class="card" aria-labelledby="c-delivered">
          <h2 id="c-delivered">${t(this.localize, "cardDelivered")}</h2>
          <p class="value">${this.pct(s.delivered)}</p>
          <p class="note ${change === null ? "muted" : change >= 0 ? "good" : "warn"}">
            ${change === null ? t(this.localize, "nothingFinished") : t(this.localize, "versusBefore", { change: `${change >= 0 ? "+" : ""}${new Intl.NumberFormat(this.locale(), { maximumFractionDigits: 1 }).format(change)}`, days: s.days })}
          </p>
        </section>
        <section class="card" aria-labelledby="c-sent">
          <h2 id="c-sent">${t(this.localize, "cardSent")}</h2>
          <p class="value">${this.num(s.sent)}</p>
          <p class="note good">${t(this.localize, "perDay", { count: this.num(s.per_day_average) })}</p>
        </section>
        <section class="card" aria-labelledby="c-bounced">
          <h2 id="c-bounced">${t(this.localize, "cardBounced")}</h2>
          <p class="value">${this.pct(s.bounced)}</p>
          <p class="note ${s.hard_bounces ? "warn" : "good"}">${t(this.localize, "hardBounces", { count: s.hard_bounces })}</p>
        </section>
        <section class="card" aria-labelledby="c-queue">
          <h2 id="c-queue">${t(this.localize, "cardQueue")}</h2>
          <p class="value">${this.num(s.queue.count)}</p>
          <p class="note ${s.queue.count ? "warn" : "good"}">
            ${s.queue.oldest ? t(this.localize, s.queue.retrying ? "oldestRetrying" : "oldest", { age: this.age(s.queue.oldest) }) : t(this.localize, "queueEmpty")}
          </p>
        </section>
      </div>
      ${this.chart(s)}
      <div class="two">
        <section class="panel" aria-labelledby="providers-title">
          <div class="panel-head"><h2 id="providers-title">${t(this.localize, "byProvider")}</h2></div>
          ${s.providers.length
            ? s.providers.map(
                (p) => html`<div class="provider">
                  <span>${t(this.localize, PROVIDERS[p.name])}</span>
                  <svg class="bar" viewBox="0 0 100 6" preserveAspectRatio="none" aria-hidden="true"><rect class="track" x="0" y="0" width="100" height="6" rx="3"/><rect class=${(p.delivered ?? 0) >= 99 ? "d" : "f"} x="0" y="0" width=${p.delivered ?? 0} height="6" rx="3"/></svg>
                  <span class="pct">${this.pct(p.delivered)}</span>
                </div>`,
              )
            : html`<p class="empty">${t(this.localize, "noOutbound")}</p>`}
        </section>
        ${s.senders.length
          ? s.senders.map(
              (d) => html`<section class="panel" aria-labelledby="senders-${d.domain}">
                <div class="panel-head"><h2 id="senders-${d.domain}">${t(this.localize, "whoSends", { domain: d.domain })}</h2></div>
                <ul>
                  ${d.groups.map(
                    (g) => html`<li class="sender">
                      <span dir="auto">${g.ours ? t(this.localize, "ourServers") : g.name || t(this.localize, "unknownSender")}</span>
                      <span class="count">${this.num(g.count)}</span>
                      <span class="chip ${g.status}">${t(this.localize, STATUS[g.status])}</span>
                    </li>`,
                  )}
                </ul>
              </section>`,
            )
          : html`<section class="panel" aria-labelledby="senders-none">
              <div class="panel-head"><h2 id="senders-none">${t(this.localize, "whoSendsAny")}</h2></div>
              <p class="empty">${t(this.localize, "noReports")}</p>
            </section>`}
      </div>
    `;
  }

  private chart(s: Summary) {
    const max = Math.max(1, ...s.per_day.map((d) => d.delivered + d.deferred + d.bounced));
    const n = s.per_day.length;
    const slot = 1000 / n;
    const width = slot * 0.82;
    const bars = s.per_day.map((d, i) => {
      const x = i * slot + (slot - width) / 2;
      const h = (v: number) => (v / max) * 136;
      const [hd, hf, hb] = [h(d.delivered), h(d.deferred), h(d.bounced)];
      return svg`<g>
        <rect class="d" x=${x} y=${140 - hd} width=${width} height=${hd}/>
        <rect class="f" x=${x} y=${140 - hd - hf} width=${width} height=${hf}/>
        <rect class="b" x=${x} y=${140 - hd - hf - hb} width=${width} height=${hb}/>
      </g>`;
    });
    const first = s.per_day[0];
    const format = new Intl.DateTimeFormat(this.locale(), s.bucket === "hour" ? { hour: "2-digit", minute: "2-digit" } : { day: "numeric", month: "short" });
    return html`<section class="panel" aria-labelledby="chart-title">
      <div class="panel-head">
        <h2 id="chart-title">${t(this.localize, s.bucket === "hour" ? "outboundPerHour" : "outboundPerDay")}</h2>
        <div class="legend" aria-hidden="true">
          <span class="l-delivered">${t(this.localize, "legendDelivered")}</span>
          <span class="l-deferred">${t(this.localize, "legendDeferred")}</span>
          <span class="l-bounced">${t(this.localize, "legendBounced")}</span>
        </div>
      </div>
      ${s.sent
        ? html`<svg class="chart" viewBox="0 0 1000 140" preserveAspectRatio="none" role="img" aria-label=${t(this.localize, "chartSummary", { sent: s.sent, days: s.days })}>${bars}</svg>
            <div class="axis"><span>${first ? format.format(new Date(first.at)) : nothing}</span><span>${t(this.localize, s.bucket === "hour" ? "now" : "today")}</span></div>`
        : html`<p class="empty">${t(this.localize, "noOutbound")}</p>`}
    </section>`;
  }
}
