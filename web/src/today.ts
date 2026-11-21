// The mail app's card on the start page (ortus-mail-today, loaded by the
// core's Today page from /assets/apps/mail/today.js): unread mail in the
// inbox, newest first, and what Catch up has for the person.
import { LitElement, LocalizeController, css, customElement, html, nothing, state } from "ortus-kit";
import { ready, t } from "./i18n.js";
import { jmap, type Mailbox } from "./jmap.js";

await ready;

interface Unread {
  id: string;
  threadId: string;
  subject: string | null;
  from: { name: string | null; email: string | null }[] | null;
  receivedAt: string;
}

@customElement("ortus-mail-today")
export class OrtusMailToday extends LitElement {
  static override styles = css`
    :host {
      display: block;
    }
    section {
      padding: 20px;
      border-radius: 18px;
      border: 1px solid var(--ortus-line);
      background: var(--ortus-surface);
    }
    header {
      display: flex;
      align-items: baseline;
      justify-content: space-between;
      gap: 12px;
      margin-block-end: 12px;
    }
    h2 {
      margin: 0;
      font-size: 17px;
      font-weight: 600;
    }
    .count {
      color: var(--ortus-accent);
      font-size: 14px;
      font-weight: 600;
    }
    ul {
      margin: 0;
      padding: 0;
      list-style: none;
    }
    li a {
      display: flex;
      align-items: center;
      gap: 10px;
      padding: 8px;
      margin-inline: -8px;
      border-radius: 10px;
      color: inherit;
      text-decoration: none;
    }
    li a:hover,
    li a:focus-visible {
      background: var(--ortus-hover);
    }
    .text {
      display: flex;
      flex-direction: column;
      min-inline-size: 0;
    }
    .text strong,
    .text span {
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .text strong {
      font-size: 14px;
    }
    .text span {
      color: var(--ortus-text-2);
      font-size: 13px;
    }
    .quiet {
      margin: 0;
      color: var(--ortus-text-2);
      font-size: 14px;
    }
    footer {
      display: flex;
      gap: 16px;
      margin-block-start: 12px;
      font-size: 14px;
    }
    footer a {
      color: var(--ortus-accent);
    }
  `;

  private readonly localize = new LocalizeController(this);
  @state() private unread: Unread[] = [];
  @state() private total = 0;
  @state() private loaded = false;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.load();
  }

  private async load(): Promise<void> {
    try {
      const boxes = (await jmap([["Mailbox/get", {}, "b"]])).b?.list as Mailbox[] | undefined;
      const inbox = boxes?.find((b) => b.role === "inbox");
      if (inbox) {
        this.total = inbox.unreadEmails;
        const r = await jmap([
          ["Email/query", { filter: { inMailbox: inbox.id, notKeyword: "$seen" }, sort: [{ property: "receivedAt", isAscending: false }], limit: 4 }, "q"],
          ["Email/get", { "#ids": { resultOf: "q", name: "Email/query", path: "/ids" }, properties: ["threadId", "subject", "from", "receivedAt"] }, "g"],
        ]);
        this.unread = (r.g?.list as Unread[] | undefined) ?? [];
      }
    } catch {
      // No mailbox here (or mail off): the card says nothing is waiting.
    }
    this.loaded = true;
  }

  override render() {
    if (!this.loaded) return nothing;
    return html`
      <section aria-labelledby="mail-today">
        <header>
          <h2 id="mail-today">${t(this.localize, "inbox")}</h2>
          ${this.total ? html`<span class="count">${t(this.localize, "todayUnread", { count: this.total })}</span>` : nothing}
        </header>
        ${this.unread.length
          ? html`<ul>
              ${this.unread.map((m) => {
                const from = m.from?.[0];
                const name = from?.name || from?.email || "";
                return html`<li>
                  <a href=${`/mail?thread=${encodeURIComponent(m.threadId)}`}>
                    <ortus-avatar size="32" radius="9" .name=${name} .email=${from?.email ?? ""}></ortus-avatar>
                    <span class="text"><strong>${name}</strong><span>${m.subject || t(this.localize, "todayNoSubject")}</span></span>
                  </a>
                </li>`;
              })}
            </ul>`
          : html`<p class="quiet">${t(this.localize, "todayAllRead")}</p>`}
        <footer>
          <a href="/mail">${t(this.localize, "todayOpen")}</a>
          <a href="/mail/compose">${t(this.localize, "todayWrite")}</a>
        </footer>
      </section>
    `;
  }
}

declare global {
  interface HTMLElementTagNameMap {
    "ortus-mail-today": OrtusMailToday;
  }
}
