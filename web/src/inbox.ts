// The inbox (board Main): folders and labels, the list with its tabs and
// catch-up card, the thread being read with a quick reply, and the day's
// agenda column.
import { LitElement, LocalizeController, controls, css, customElement, html, me, nothing, repeat, state, svg, type Me } from "ortus-kit";
import { t } from "./i18n.js";
import { LIST_PROPERTIES, MethodError, READ_PROPERTIES, jmap, labelsOf, type Email, type EmailAddress, type Mailbox } from "./jmap.js";
import type { MailKey } from "./locales/en.js";
import { navigate } from "./events.js";

const ICON_ARCHIVE = svg`<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><rect x="3" y="4" width="18" height="5" rx="1"/><path d="M5 9v10a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V9M10 13h4"/></svg>`;
const ICON_TRASH = svg`<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/></svg>`;
const ICON_SEARCH = svg`<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/></svg>`;
const ICON_CLOCK = svg`<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><circle cx="12" cy="12" r="8"/><path d="M12 8v4l3 2"/></svg>`;
const ICON_SEND = svg`<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M5 12h14M13 6l6 6-6 6"/></svg>`;

const FOLDERS: { role: string; key: MailKey; count: "unread" | "total" | null }[] = [
  { role: "inbox", key: "inbox", count: "unread" },
  { role: "today", key: "today", count: null },
  { role: "snoozed", key: "snoozed", count: "total" },
  { role: "scheduled", key: "scheduled", count: "total" },
  { role: "sent", key: "sent", count: null },
  { role: "drafts", key: "drafts", count: "total" },
  { role: "archive", key: "archive", count: null },
  { role: "junk", key: "junk", count: "unread" },
  { role: "trash", key: "trash", count: null },
];

const TABS = [
  { id: "important", key: "tabImportant" },
  { id: "other", key: "tabOther" },
  { id: "feeds", key: "tabFeeds" },
] as const;

type Tab = (typeof TABS)[number]["id"];

/** Where the person is: folder (a role or a mailbox id), label, tab, search, thread. */
interface Place {
  box: string;
  label: string | null;
  tab: Tab;
  q: string;
  thread: string | null;
  only: string[] | null;
}

function readPlace(): Place {
  const p = new URLSearchParams(location.search);
  const tab = p.get("tab");
  return {
    box: p.get("box") ?? "inbox",
    label: p.get("label"),
    tab: TABS.some((x) => x.id === tab) ? (tab as Tab) : "important",
    q: p.get("q") ?? "",
    thread: p.get("thread"),
    only: null,
  };
}

function initials(name: string): string {
  const words = name.replace(/[^\p{L}\p{N} ]/gu, " ").split(/\s+/).filter(Boolean);
  return ((words[0]?.[0] ?? "?") + (words.length > 1 ? (words[words.length - 1]?.[0] ?? "") : "")).toUpperCase();
}

/** A stable colour for a name (one of six). */
function tone(text: string): string {
  let h = 0;
  for (const c of text) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return `tone${h % 6}`;
}

function who(a: EmailAddress | undefined | null): string {
  return a?.name || a?.email || "";
}

/** Snooze choices: later today, tomorrow morning, next Monday morning. */
function snoozeTimes(now = new Date()): { key: MailKey; at: Date }[] {
  const later = new Date(now);
  if (now.getHours() < 17) later.setHours(18, 0, 0, 0);
  else later.setTime(now.getTime() + 3 * 3_600_000);
  const tomorrow = new Date(now);
  tomorrow.setDate(now.getDate() + 1);
  tomorrow.setHours(8, 0, 0, 0);
  const week = new Date(now);
  week.setDate(now.getDate() + ((8 - now.getDay()) % 7 || 7));
  week.setHours(8, 0, 0, 0);
  return [
    { key: "laterToday", at: later },
    { key: "tomorrow", at: tomorrow },
    { key: "nextWeek", at: week },
  ];
}

function startOfToday(): number {
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

@customElement("ortus-mail-inbox")
export class OrtusMailInbox extends LitElement {
  static override styles = [
    controls,
    css`
      :host {
        display: block;
        box-sizing: border-box;
        block-size: 100%;
        min-block-size: 0;
      }
      .sr {
        position: absolute;
        inline-size: 1px;
        block-size: 1px;
        overflow: hidden;
        clip-path: inset(50%);
        white-space: nowrap;
      }
      .top {
        display: grid;
        grid-template-columns: 196px minmax(0, 1fr);
        gap: 20px;
        align-items: center;
        padding-block: 14px;
        padding-inline: 12px 24px;
        border-block-end: 1px solid var(--ortus-line);
      }
      .search {
        display: flex;
        align-items: center;
        gap: 10px;
        box-sizing: border-box;
        max-inline-size: 446px;
        block-size: 42px;
        padding-inline: 14px;
        border-radius: 12px;
        border: 1px solid var(--ortus-line-strong);
        background: var(--ortus-surface);
        color: var(--ortus-text-3);
      }
      .search input {
        flex: 1;
        min-inline-size: 0;
        border: 0;
        background: none;
        color: var(--ortus-text);
        font: inherit;
        font-size: 15px;
      }
      .search input:focus {
        outline: none;
      }
      .search:focus-within {
        border-color: var(--ortus-accent);
      }
      .columns {
        display: grid;
        grid-template-columns: 196px 344px minmax(0, 1fr) 228px;
        gap: 16px 20px;
        padding-block: 20px;
        padding-inline: 12px 24px;
        box-sizing: border-box;
        align-items: start;
      }
      nav.folders {
        display: flex;
        flex-direction: column;
        gap: 4px;
      }
      .new {
        display: flex;
        align-items: center;
        block-size: 44px;
        padding-inline: 14px;
        margin-block-end: 20px;
        border-radius: 12px;
        background: var(--ortus-accent);
        color: var(--ortus-bg);
        font-weight: 600;
        font-size: 15px;
        text-decoration: none;
      }
      .folder {
        display: flex;
        align-items: center;
        justify-content: space-between;
        block-size: 40px;
        padding-inline: 12px;
        border-radius: 10px;
        border: 1px solid transparent;
        background: none;
        color: var(--ortus-text);
        font: inherit;
        font-size: 15px;
        text-align: start;
        cursor: pointer;
        text-decoration: none;
      }
      .folder:hover {
        background: var(--ortus-hover);
      }
      .folder[aria-current="page"] {
        background: var(--ortus-surface-2);
        border-color: var(--ortus-line-strong);
        font-weight: 600;
      }
      .folder .count {
        color: var(--ortus-text-3);
        font-size: 13px;
        font-weight: 400;
      }
      .labels-head {
        margin-block: 20px 6px;
        padding-inline: 12px;
        color: var(--ortus-text-3);
        font-size: 13px;
        font-weight: 400;
      }
      .dot {
        inline-size: 7px;
        block-size: 7px;
        border-radius: 50%;
        margin-inline-end: 10px;
        flex: none;
      }
      .folder .name {
        display: flex;
        align-items: center;
      }
      .list {
        display: flex;
        flex-direction: column;
        gap: 4px;
        min-inline-size: 0;
      }
      ul.list {
        list-style: none;
        margin: 0;
        padding: 0;
      }
      .catchup {
        padding: 18px 20px;
        margin-block-end: 16px;
        border-radius: 18px;
        border: 1px solid var(--ortus-line-strong);
        background: var(--ortus-surface);
      }
      .catchup .head {
        margin-block: 0 8px;
        margin-inline: 0;
        font-size: 13px;
        color: var(--ortus-text-3);
      }
      .catchup .head strong {
        color: var(--ortus-accent);
        font-weight: 600;
      }
      .catchup p {
        margin-block: 0 14px;
        margin-inline: 0;
        font-size: 15px;
        line-height: 1.5;
      }
      .tabs {
        display: flex;
        gap: 4px;
        margin-block-end: 12px;
      }
      .tab {
        block-size: 36px;
        padding-inline: 14px;
        border-radius: 10px;
        border: 0;
        background: none;
        color: var(--ortus-text-3);
        font: inherit;
        font-size: 14px;
        cursor: pointer;
      }
      .tab[aria-selected="true"] {
        background: var(--ortus-surface-2);
        color: var(--ortus-text);
        font-weight: 600;
      }
      .item {
        display: grid;
        grid-template-columns: 40px minmax(0, 1fr) auto;
        gap: 2px 14px;
        align-items: center;
        padding: 12px 16px;
        border-radius: 14px;
        border: 1px solid transparent;
        background: none;
        color: var(--ortus-text);
        font: inherit;
        text-align: start;
        cursor: pointer;
        inline-size: 100%;
        box-sizing: border-box;
      }
      .item:hover {
        background: var(--ortus-hover);
      }
      .item[aria-current="true"] {
        background: var(--ortus-surface-2);
        border-color: var(--ortus-line-strong);
      }
      .avatar {
        grid-row: span 2;
        display: grid;
        place-items: center;
        inline-size: 40px;
        block-size: 40px;
        border-radius: 50%;
        font-size: 13px;
        font-weight: 600;
        color: var(--ortus-bg);
      }
      .tone0 { background: var(--ortus-orange); }
      .tone1 { background: #9aa3b5; }
      .tone2 { background: var(--ortus-accent); }
      .tone3 { background: var(--ortus-teal); }
      .tone4 { background: var(--ortus-lime); }
      .tone5 { background: #c9a7ff; }
      .item .from {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        font-size: 15px;
      }
      .item .time {
        color: var(--ortus-text-3);
        font-size: 13px;
      }
      .item .subject {
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
        font-size: 14px;
        color: var(--ortus-text-2);
      }
      .item.unread .from,
      .item.unread .subject {
        font-weight: 600;
        color: var(--ortus-text);
      }
      .chip {
        display: inline-block;
        padding: 2px 8px;
        border-radius: 6px;
        font-size: 13px;
        font-weight: 600;
        white-space: nowrap;
      }
      .chip.tone0 { background: #3a2a1c; color: var(--ortus-orange); }
      .chip.tone1 { background: #252a35; color: var(--ortus-text-2); }
      .chip.tone2 { background: #232a4d; color: var(--ortus-accent); }
      .chip.tone3 { background: #163331; color: var(--ortus-teal); }
      .chip.tone4 { background: #2c3519; color: var(--ortus-lime); }
      .chip.tone5 { background: #30264a; color: #c9a7ff; }
      .dot.tone0 { background: var(--ortus-orange); }
      .dot.tone1 { background: #9aa3b5; }
      .dot.tone2 { background: var(--ortus-accent); }
      .dot.tone3 { background: var(--ortus-teal); }
      .dot.tone4 { background: var(--ortus-lime); }
      .dot.tone5 { background: #c9a7ff; }
      .reader {
        padding: 28px 32px;
        border-radius: 18px;
        border: 1px solid var(--ortus-line);
        background: var(--ortus-surface);
        min-inline-size: 0;
      }
      .reader .title {
        display: flex;
        justify-content: space-between;
        gap: 16px;
      }
      .reader h2 {
        margin-block: 0 8px;
        margin-inline: 0;
        font-size: 24px;
        font-weight: 600;
        line-height: 1.2;
        overflow-wrap: anywhere;
      }
      .actions {
        display: flex;
        gap: 8px;
        flex: none;
      }
      .icon {
        display: grid;
        place-items: center;
        inline-size: 40px;
        block-size: 40px;
        border-radius: 10px;
        border: 1px solid var(--ortus-line-strong);
        background: none;
        color: var(--ortus-text);
        cursor: pointer;
      }
      .icon:hover {
        background: var(--ortus-hover);
      }
      .snooze {
        position: relative;
      }
      .menu {
        position: absolute;
        inset-block-start: 46px;
        inset-inline-end: 0;
        z-index: 3;
        display: flex;
        flex-direction: column;
        min-inline-size: 230px;
        padding: 6px;
        border-radius: 12px;
        border: 1px solid var(--ortus-line-strong);
        background: var(--ortus-surface-2);
      }
      .menu button {
        display: flex;
        justify-content: space-between;
        gap: 12px;
        padding: 10px 12px;
        border: 0;
        border-radius: 8px;
        background: none;
        color: var(--ortus-text);
        font: inherit;
        font-size: 14px;
        text-align: start;
        cursor: pointer;
      }
      .menu button:hover {
        background: var(--ortus-hover);
      }
      .menu small {
        color: var(--ortus-text-3);
      }
      .banner {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 12px;
        margin-block-end: 20px;
        padding: 12px 16px;
        border-radius: 12px;
        background: #1c2240;
        font-size: 14px;
      }
      .banner.bad {
        background: #2a1617;
      }
      .meta {
        display: flex;
        gap: 8px;
        align-items: center;
        margin-block-end: 24px;
        font-size: 13px;
        color: var(--ortus-text-3);
      }
      article {
        border-block-start: 1px solid var(--ortus-line);
        padding-block: 18px;
      }
      article:first-of-type {
        border-block-start: 0;
        padding-block-start: 0;
      }
      .sender {
        display: grid;
        grid-template-columns: 40px minmax(0, 1fr) auto;
        gap: 0 12px;
        align-items: center;
        margin-block-end: 18px;
      }
      .sender .avatar {
        border-radius: 10px;
      }
      .sender .name {
        font-weight: 600;
        font-size: 15px;
        overflow-wrap: anywhere;
      }
      .sender .address,
      .sender .to,
      .sender .time {
        color: var(--ortus-text-3);
        font-size: 13px;
        font-weight: 400;
      }
      .verified {
        color: var(--ortus-teal);
      }
      .body {
        margin: 0;
        font: inherit;
        font-size: 15px;
        line-height: 1.6;
        white-space: pre-wrap;
        overflow-wrap: anywhere;
      }
      .collapsed {
        cursor: pointer;
      }
      .collapsed .preview {
        color: var(--ortus-text-3);
        font-size: 14px;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
      }
      .files {
        display: flex;
        flex-wrap: wrap;
        gap: 10px;
        margin-block-start: 20px;
      }
      .file {
        display: flex;
        align-items: center;
        gap: 12px;
        padding: 10px 12px;
        border-radius: 12px;
        border: 1px solid var(--ortus-line-strong);
        color: var(--ortus-text);
        text-decoration: none;
        font-size: 14px;
        font-weight: 600;
      }
      .file small {
        display: block;
        color: var(--ortus-text-3);
        font-weight: 400;
        font-size: 12px;
      }
      .badge {
        display: grid;
        place-items: center;
        inline-size: 34px;
        block-size: 34px;
        border-radius: 8px;
        background: #3a1f22;
        color: var(--ortus-red);
        font-size: 11px;
        font-weight: 700;
      }
      .badge.doc {
        background: #232a4d;
        color: var(--ortus-accent);
      }
      .reply {
        display: flex;
        gap: 10px;
        align-items: center;
        margin-block-start: 24px;
        padding-block: 10px;
        padding-inline: 20px 10px;
        border-radius: 16px;
        border: 1px solid var(--ortus-line-strong);
      }
      .reply textarea {
        flex: 1;
        min-inline-size: 0;
        min-block-size: 22px;
        max-block-size: 200px;
        border: 0;
        background: none;
        color: var(--ortus-text);
        font: inherit;
        font-size: 15px;
        resize: none;
        field-sizing: content;
      }
      .reply textarea:focus {
        outline: none;
      }
      .reply:focus-within {
        border-color: var(--ortus-accent);
      }
      .send {
        display: grid;
        place-items: center;
        inline-size: 40px;
        block-size: 40px;
        border-radius: 12px;
        border: 0;
        background: var(--ortus-accent);
        color: var(--ortus-bg);
        cursor: pointer;
      }
      .send:disabled {
        opacity: 0.5;
        cursor: default;
      }
      .note {
        margin-block: 12px 0;
        margin-inline: 0;
        font-size: 14px;
        color: var(--ortus-text-2);
      }
      .note.error {
        color: var(--ortus-red);
      }
      .agenda .head {
        display: flex;
        justify-content: space-between;
        align-items: baseline;
        margin-block-end: 12px;
      }
      .agenda h2 {
        margin: 0;
        font-size: 15px;
        font-weight: 600;
      }
      .agenda .date {
        color: var(--ortus-text-3);
        font-size: 13px;
      }
      .quiet {
        padding: 18px;
        border-radius: 14px;
        background: var(--ortus-surface);
        color: var(--ortus-text-3);
        font-size: 14px;
        line-height: 1.5;
      }
      .empty {
        padding: 40px 24px;
        border-radius: 18px;
        border: 1px dashed var(--ortus-line-strong);
        text-align: center;
      }
      .empty h3 {
        margin-block: 0 6px;
        margin-inline: 0;
        font-size: 17px;
      }
      .empty p {
        margin: 0;
        color: var(--ortus-text-2);
        font-size: 14px;
      }
      .skeleton {
        block-size: 64px;
        border-radius: 14px;
        background: var(--ortus-surface);
      }
      @media (max-width: 1280px) {
        .columns {
          grid-template-columns: 196px 320px minmax(0, 1fr);
        }
        .agenda {
          display: none;
        }
      }
      @media (max-width: 960px) {
        .top,
        .columns {
          grid-template-columns: minmax(0, 1fr);
        }
        .top > .brand,
        nav.folders {
          display: none;
        }
        .reader {
          padding: 20px;
        }
      }
    `,
  ];

  private readonly localize = new LocalizeController(this);

  @state() private place: Place = readPlace();
  @state() private boxes: Mailbox[] = [];
  @state() private emails: Email[] | undefined;
  @state() private labels: string[] = [];
  @state() private catchUp: Email[] = [];
  @state() private thread: Email[] | undefined;
  @state() private expanded = new Set<string>();
  @state() private failed = false;
  @state() private replyText = "";
  @state() private replyNote: { error: boolean; text: string } | null = null;
  @state() private sending = false;
  @state() private scheduled: { id: string; emailId: string; sendAt: string; ortusState: string; ortusError: string | null }[] = [];
  @state() private snoozeOpen = false;
  private person: Me | undefined;
  private events: EventSource | undefined;
  private loads = 0;
  private reloadTimer: number | undefined;

  override connectedCallback(): void {
    super.connectedCallback();
    window.addEventListener("popstate", this.onPop);
    void this.start();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.removeEventListener("popstate", this.onPop);
    this.events?.close();
    this.events = undefined;
  }

  private readonly onPop = (): void => {
    this.place = readPlace();
    void this.load();
  };

  private async start(): Promise<void> {
    this.person = await me().catch(() => undefined);
    await this.load();
    // Changes (new mail, another device) arrive as they happen.
    this.events = new EventSource("/api/v1/mail/web/events");
    let first = true;
    this.events.addEventListener("state", () => {
      if (first) {
        first = false;
        return;
      }
      window.clearTimeout(this.reloadTimer);
      this.reloadTimer = window.setTimeout(() => void this.load(true), 200);
    });
  }

  private go(change: Partial<Place>): void {
    this.place = { ...this.place, ...change };
    const p = new URLSearchParams();
    if (this.place.box !== "inbox") p.set("box", this.place.box);
    if (this.place.label) p.set("label", this.place.label);
    if (this.place.tab !== "important") p.set("tab", this.place.tab);
    if (this.place.q) p.set("q", this.place.q);
    if (this.place.thread) p.set("thread", this.place.thread);
    const query = p.toString();
    history.pushState(null, "", `/mail${query ? `?${query}` : ""}`);
    void this.load();
  }

  private boxId(role: string): string | undefined {
    return this.boxes.find((b) => b.role === role)?.id;
  }

  /** The filter for the list as the person has it now. */
  private filter(): Record<string, unknown> {
    const { box, label, tab, q } = this.place;
    const all: Record<string, unknown>[] = [];
    if (q) all.push({ text: q });
    if (label) {
      all.push({ hasKeyword: label });
    } else if (box === "today") {
      all.push({ inMailbox: this.boxId("inbox"), after: new Date(startOfToday()).toISOString().replace(/\.\d+Z$/, "Z") });
    } else if (box === "scheduled") {
      all.push({ inMailbox: this.boxId("drafts") });
    } else if (!q) {
      const id = this.boxes.find((b) => b.id === box || b.role === box)?.id;
      if (id) all.push({ inMailbox: id });
      if (box === "inbox") all.push({ ortusCategory: tab });
    }
    return all.length === 1 ? all[0]! : { operator: "AND", conditions: all };
  }

  private async load(quiet = false): Promise<void> {
    const ticket = ++this.loads;
    try {
      if (!quiet) this.failed = false;
      const first = await jmap([
        ["Mailbox/get", {}, "b"],
        ["EmailSubmission/get", { ids: null }, "sub"],
        ["Email/query", { filter: { notKeyword: "$deleted" }, limit: 500 }, "k"],
        ["Email/get", { "#ids": { resultOf: "k", name: "Email/query", path: "/ids" }, properties: ["keywords"] }, "kw"],
      ]);
      if (ticket !== this.loads) return;
      this.boxes = (first.b!.list as Mailbox[]) ?? [];
      const seen = new Set<string>();
      for (const e of (first.kw!.list as Email[]) ?? []) for (const l of labelsOf(e)) seen.add(l);
      this.labels = [...seen].sort();
      this.scheduled = (first.sub!.list as OrtusMailInbox["scheduled"]) ?? [];
      const inbox = this.boxId("inbox");
      const answers = await jmap([
        ["Email/query", { filter: this.filter(), sort: [{ property: "receivedAt", isAscending: false }], collapseThreads: true, limit: 100 }, "q"],
        ["Email/get", { "#ids": { resultOf: "q", name: "Email/query", path: "/ids" }, properties: LIST_PROPERTIES }, "l"],
        ["Email/query", { filter: { operator: "AND", conditions: [{ inMailbox: inbox }, { ortusCategory: "important" }, { notKeyword: "$seen" }] }, limit: 20 }, "c"],
        ["Email/get", { "#ids": { resultOf: "c", name: "Email/query", path: "/ids" }, properties: ["from", "to", "receivedAt", "subject"] }, "cl"],
      ]);
      if (ticket !== this.loads) return;
      let emails = (answers.l!.list as Email[]) ?? [];
      if (this.place.box === "scheduled" && !this.place.q && !this.place.label) {
        const ids = this.scheduled.map((x) => x.emailId);
        const got = ids.length ? await jmap([["Email/get", { ids, properties: LIST_PROPERTIES }, "s"]]) : { s: { list: [] } };
        if (ticket !== this.loads) return;
        emails = (got.s!.list as Email[]) ?? [];
      }
      if (this.place.only) emails = emails.filter((e) => this.place.only!.includes(e.id));
      this.emails = emails;
      const mine = this.person?.email.toLowerCase();
      this.catchUp = ((answers.cl!.list as Email[]) ?? []).filter((e) => (e.to ?? []).some((a) => a.email?.toLowerCase() === mine));
      if (this.place.thread) await this.openThread(this.place.thread, ticket);
      else this.thread = undefined;
    } catch (e) {
      if (ticket === this.loads) this.failed = true;
      console.warn("mail: could not load", e);
    }
  }

  private async openThread(threadId: string, ticket: number): Promise<void> {
    const r = await jmap([
      ["Thread/get", { ids: [threadId] }, "t"],
      ["Email/get", { "#ids": { resultOf: "t", name: "Thread/get", path: "/list/*/emailIds" }, properties: READ_PROPERTIES, fetchTextBodyValues: false }, "e"],
    ]);
    if (ticket !== this.loads) return;
    const messages = ((r.e!.list as Email[]) ?? []).filter((m) => !m.keywords.$deleted);
    if (this.thread?.[0]?.threadId !== threadId) {
      // The newest and the unread are open; the rest folded.
      this.expanded = new Set(messages.filter((m, i) => i === messages.length - 1 || !m.keywords.$seen).map((m) => m.id));
      this.replyText = "";
      this.replyNote = null;
    }
    this.thread = messages;
    const unread = messages.filter((m) => !m.keywords.$seen);
    if (unread.length) {
      const update: Record<string, unknown> = {};
      for (const m of unread) update[m.id] = { "keywords/$seen": true };
      await jmap([["Email/set", { update }, "s"]]);
    }
  }

  private when(iso: string): string {
    const date = new Date(iso);
    const lang = document.documentElement.lang || "en";
    if (date.getTime() >= startOfToday()) return new Intl.DateTimeFormat(lang, { hour: "2-digit", minute: "2-digit" }).format(date);
    if (date.getTime() >= startOfToday() - 6 * 86_400_000) return new Intl.DateTimeFormat(lang, { weekday: "short" }).format(date);
    return new Intl.DateTimeFormat(lang, { day: "numeric", month: "short" }).format(date);
  }

  /** The time shown in the list: when it comes or goes, for Scheduled. */
  private timeFor(e: Email): string {
    const pending = this.scheduled.find((x) => x.emailId === e.id);
    return pending && this.place.box === "scheduled" ? this.when(pending.sendAt) : this.when(e.receivedAt);
  }

  private size(bytes: number): string {
    const lang = document.documentElement.lang || "en";
    const [value, unit] = bytes >= 1_000_000 ? [bytes / 1_000_000, "megabyte"] : [Math.max(1, Math.round(bytes / 1000)), "kilobyte"];
    return new Intl.NumberFormat(lang, { style: "unit", unit, unitDisplay: "short", maximumFractionDigits: 1 }).format(value);
  }

  private label(name: string): string {
    return name.charAt(0).toUpperCase() + name.slice(1);
  }

  private folderTitle(): string {
    if (this.place.q) return t(this.localize, "searchResults");
    if (this.place.label) return this.label(this.place.label);
    const folder = FOLDERS.find((f) => f.role === this.place.box);
    if (folder) return t(this.localize, folder.key);
    return this.boxes.find((b) => b.id === this.place.box)?.name ?? t(this.localize, "inbox");
  }

  override render() {
    return html`
      <div class="top">
        <span class="brand"></span>
        <form class="search" role="search" @submit=${this.search}>
          ${ICON_SEARCH}
          <label class="sr" for="q">${t(this.localize, "searchMail")}</label>
          <input id="q" name="q" type="search" .value=${this.place.q} placeholder=${t(this.localize, "searchMail")} autocomplete="off" />
        </form>
      </div>
      <div class="columns">
        ${this.folders()}
        <section class="list" aria-labelledby="list-title">
          <h1 id="list-title" class="sr">${this.folderTitle()}</h1>
          ${this.place.box === "inbox" && !this.place.q && !this.place.label ? this.catchUpCard() : nothing}
          ${this.place.box === "inbox" && !this.place.q && !this.place.label ? this.tabs() : nothing}
          ${this.items()}
        </section>
        ${this.reader()}
        <aside class="agenda" aria-labelledby="agenda-title">
          <div class="head">
            <h2 id="agenda-title">${t(this.localize, "agendaToday")}</h2>
            <span class="date">${new Intl.DateTimeFormat(document.documentElement.lang || "en", { weekday: "short", day: "numeric", month: "short" }).format(new Date())}</span>
          </div>
          <p class="quiet">${t(this.localize, "agendaEmpty")}</p>
        </aside>
      </div>
    `;
  }

  private search(event: SubmitEvent): void {
    event.preventDefault();
    const q = String(new FormData(event.target as HTMLFormElement).get("q") ?? "").trim();
    this.go({ q, label: null, thread: null, only: null });
  }

  private folders() {
    const count = (role: string, kind: "unread" | "total" | null) => {
      const b = this.boxes.find((x) => x.role === role);
      const n = role === "scheduled" ? this.scheduled.length : !b || !kind ? 0 : kind === "unread" ? b.unreadEmails : b.totalEmails;
      return n > 0 ? html`<span class="count">${n}</span>` : nothing;
    };
    const current = (role: string) => (!this.place.q && !this.place.label && this.place.box === role ? "page" : nothing);
    const own = this.boxes.filter((b) => !b.role);
    return html`
      <nav class="folders" aria-label=${t(this.localize, "folders")}>
        <a class="new" href="/mail/compose" @click=${(e: Event) => {
          e.preventDefault();
          navigate("/mail/compose");
        }}>${t(this.localize, "newMessage")}</a>
        ${FOLDERS.map(
          (f) => html`<button class="folder" aria-current=${current(f.role)} @click=${() => this.go({ box: f.role, label: null, q: "", thread: null, only: null, tab: "important" })}>
            <span>${t(this.localize, f.key)}</span>${count(f.role, f.count)}
          </button>`,
        )}
        ${own.map(
          (b) => html`<button class="folder" aria-current=${current(b.id)} @click=${() => this.go({ box: b.id, label: null, q: "", thread: null, only: null })}>
            <span>${b.name}</span>${b.unreadEmails ? html`<span class="count">${b.unreadEmails}</span>` : nothing}
          </button>`,
        )}
        ${this.labels.length
          ? html`<h2 class="labels-head">${t(this.localize, "labels")}</h2>
              ${this.labels.map(
                (l) => html`<button class="folder" aria-current=${this.place.label === l ? "page" : nothing} @click=${() => this.go({ label: l, q: "", thread: null, only: null })}>
                  <span class="name"><span class="dot ${tone(l)}" aria-hidden="true"></span>${this.label(l)}</span>
                </button>`,
              )}`
          : nothing}
      </nav>
    `;
  }

  private catchUpCard() {
    if (!this.catchUp.length) return nothing;
    const oldest = this.catchUp.reduce((a, b) => (a.receivedAt < b.receivedAt ? a : b));
    const names = [...new Set(this.catchUp.map((e) => (who(e.from?.[0]) || "").split(" ")[0]))].slice(0, 3).join(", ");
    const lang = document.documentElement.lang || "en";
    const since = new Intl.DateTimeFormat(lang, { weekday: "short", hour: "2-digit", minute: "2-digit" }).format(new Date(oldest.receivedAt));
    return html`
      <section class="catchup" aria-labelledby="catchup-title">
        <p class="head"><strong id="catchup-title">${t(this.localize, "catchUp")}</strong> · ${t(this.localize, "catchUpSince", { time: since })}</p>
        <p>${t(this.localize, "needReply", { count: this.catchUp.length, names })}</p>
        <button class="button" @click=${() => this.go({ only: this.catchUp.map((e) => e.id), thread: null })}>${t(this.localize, "showThe", { count: this.catchUp.length })}</button>
      </section>
    `;
  }

  private tabs() {
    return html`
      <div class="tabs" role="tablist" aria-label=${t(this.localize, "inbox")}>
        ${TABS.map(
          (tab) => html`<button class="tab" role="tab" aria-selected=${this.place.tab === tab.id ? "true" : "false"} @click=${() => this.go({ tab: tab.id, thread: null, only: null })}>
            ${t(this.localize, tab.key)}
          </button>`,
        )}
      </div>
    `;
  }

  private items() {
    if (this.failed) return html`<div class="empty" role="alert"><h3>${t(this.localize, "loadFailed")}</h3><p><button class="button" @click=${() => this.load()}>${t(this.localize, "tryAgain")}</button></p></div>`;
    if (!this.emails) return html`<div class="skeleton"></div><div class="skeleton"></div><div class="skeleton"></div>`;
    if (!this.emails.length) {
      return html`<div class="empty" role="status">
        <h3>${this.place.q ? t(this.localize, "noResults", { q: this.place.q }) : this.place.box === "inbox" ? t(this.localize, "empty") : t(this.localize, "emptyFolder")}</h3>
        <p>${this.place.q ? t(this.localize, "noResultsLead") : t(this.localize, "emptyLead")}</p>
      </div>`;
    }
    const sent = ["sent", "drafts", "scheduled"].includes(this.place.box);
    return html`<ul class="list" role="list">
      ${repeat(
        this.emails,
        (e) => e.id,
        (e) => {
          const person = sent ? who(e.to?.[0]) : who(e.from?.[0]);
          const labels = labelsOf(e);
          return html`<li>
            <button class="item ${e.keywords.$seen ? "" : "unread"}" aria-current=${this.place.thread === e.threadId ? "true" : nothing} @click=${() => (this.place.box === "drafts" ? navigate(`/mail/compose?draft=${encodeURIComponent(e.id)}`) : this.go({ thread: e.threadId }))}>
              <span class="avatar ${tone(person)}" aria-hidden="true">${initials(person)}</span>
              <span class="from" dir="auto">${e.keywords.$seen ? nothing : html`<span class="sr">${t(this.localize, "unread")}</span>`}${person || t(this.localize, "noSender")}</span>
              <span class="time">${this.timeFor(e)}</span>
              <span class="subject" dir="auto">${e.subject || t(this.localize, "noSubject")}</span>
              <span>${labels[0] ? html`<span class="chip ${tone(labels[0])}">${this.label(labels[0])}</span>` : nothing}</span>
            </button>
          </li>`;
        },
      )}
    </ul>`;
  }

  private reader() {
    const thread = this.thread;
    if (!thread?.length) {
      return html`<section class="reader" aria-label=${t(this.localize, "reading")}>
        <p class="quiet">${t(this.localize, "nothingSelected")}</p>
      </section>`;
    }
    const last = thread[thread.length - 1]!;
    const people = new Set(thread.flatMap((m) => [...(m.from ?? []), ...(m.to ?? [])].map((a) => a.email?.toLowerCase())).filter(Boolean));
    const labels = [...new Set(thread.flatMap(labelsOf))];
    const inTrash = Boolean(this.boxId("trash") && last.mailboxIds[this.boxId("trash")!]);
    return html`
      <section class="reader" aria-labelledby="thread-title">
        <div class="title">
          <h2 id="thread-title" dir="auto">${last.subject || t(this.localize, "noSubject")}</h2>
          <div class="actions">
            <div class="snooze">
              <button class="icon" aria-label=${t(this.localize, "snooze")} title=${t(this.localize, "snooze")} aria-haspopup="menu" aria-expanded=${this.snoozeOpen ? "true" : "false"} @click=${() => (this.snoozeOpen = !this.snoozeOpen)}>${ICON_CLOCK}</button>
              ${this.snoozeOpen
                ? html`<div class="menu" role="menu" aria-label=${t(this.localize, "snooze")}>
                    ${snoozeTimes().map((s) => html`<button role="menuitem" @click=${() => this.snooze(s.at)}>${t(this.localize, s.key)}<small>${this.when(s.at.toISOString())}</small></button>`)}
                  </div>`
                : nothing}
            </div>
            <button class="icon" aria-label=${t(this.localize, "archiveIt")} title=${t(this.localize, "archiveIt")} @click=${() => this.move("archive")}>${ICON_ARCHIVE}</button>
            <button class="icon" aria-label=${t(this.localize, inTrash ? "deleteForever" : "moveToTrash")} title=${t(this.localize, inTrash ? "deleteForever" : "moveToTrash")} @click=${() => (inTrash ? this.destroy() : this.move("trash"))}>${ICON_TRASH}</button>
          </div>
        </div>
        <p class="meta">
          ${labels.map((l) => html`<span class="chip ${tone(l)}">${this.label(l)}</span>`)}
          <span>${t(this.localize, "messagesCount", { count: thread.length })} · ${t(this.localize, "peopleCount", { count: people.size })}</span>
        </p>
        ${this.banner(last)}
        ${thread.map((m) => this.message(m))}
        <form class="reply" @submit=${this.reply}>
          <label class="sr" for="reply">${t(this.localize, "replyTo", { name: who(last.replyTo?.[0] ?? last.from?.[0]) })}</label>
          <textarea id="reply" rows="1" .value=${this.replyText} placeholder=${t(this.localize, "replyTo", { name: who(last.replyTo?.[0] ?? last.from?.[0]).split(" ")[0] ?? "" })} @input=${(e: InputEvent) => (this.replyText = (e.target as HTMLTextAreaElement).value)}></textarea>
          <button class="send" type="submit" aria-label=${t(this.localize, "send")} ?disabled=${this.sending || !this.replyText.trim()}>${ICON_SEND}</button>
        </form>
        ${this.replyNote ? html`<p class="note ${this.replyNote.error ? "error" : ""}" role=${this.replyNote.error ? "alert" : "status"}>${this.replyNote.text}</p>` : nothing}
      </section>
    `;
  }

  private message(m: Email) {
    const from = m.from?.[0];
    const name = who(from);
    const mine = this.person?.email.toLowerCase();
    const to = [...(m.to ?? []), ...(m.cc ?? [])].map((a) => (a.email?.toLowerCase() === mine ? t(this.localize, "me") : (a.name || a.email || "").split(" ")[0])).join(", ");
    if (!this.expanded.has(m.id)) {
      return html`<article class="collapsed" @click=${() => (this.expanded = new Set([...this.expanded, m.id]))}>
        <div class="sender">
          <span class="avatar ${tone(name)}" aria-hidden="true">${initials(name)}</span>
          <span><span class="name">${name}</span><div class="preview" dir="auto">${m.preview}</div></span>
          <span class="time">${this.when(m.receivedAt)}</span>
        </div>
      </article>`;
    }
    return html`<article>
      <div class="sender">
        <span class="avatar ${tone(name)}" aria-hidden="true">${initials(name)}</span>
        <span>
          <span class="name">${name}</span> <span class="address">${from?.email ?? ""}</span>
          <div class="to">${to ? t(this.localize, "toNames", { names: to }) : nothing}${m.ortusVerified ? html` · <span class="verified">${t(this.localize, "verifiedSender")}</span>` : nothing}</div>
        </span>
        <span class="time">${this.when(m.receivedAt)}</span>
      </div>
      <p class="body" dir="auto">${m.ortusText ?? m.preview}</p>
      ${m.attachments?.length
        ? html`<div class="files">
            ${m.attachments.map((a) => {
              const ext = (a.name ?? "").split(".").pop()?.toUpperCase().slice(0, 4) || "FILE";
              return html`<a class="file" href="/api/v1/mail/web/download/${encodeURIComponent(a.blobId)}/${encodeURIComponent(a.name ?? "file")}" download=${a.name ?? "file"}>
                <span class="badge ${ext === "PDF" ? "" : "doc"}" aria-hidden="true">${ext}</span>
                <span>${a.name ?? t(this.localize, "attachment")}<small>${this.size(a.size)}</small></span>
              </a>`;
            })}
          </div>`
        : nothing}
    </article>`;
  }

  private banner(last: Email) {
    const pending = this.scheduled.find((x) => this.thread?.some((m) => m.id === x.emailId));
    if (pending) {
      const failed = pending.ortusState === "failed";
      return html`<div class="banner ${failed ? "bad" : ""}" role="status">
        <span>${failed ? t(this.localize, "couldNotSend") : t(this.localize, "sendsAt", { time: this.when(pending.sendAt) })}</span>
        ${failed
          ? html`<button class="button" @click=${() => navigate(`/mail/compose?draft=${encodeURIComponent(pending.emailId)}`)}>${t(this.localize, "editDraft")}</button>`
          : html`<button class="button" @click=${() => this.cancelScheduled(pending.id, pending.emailId)}>${t(this.localize, "cancelSending")}</button>`}
      </div>`;
    }
    const snoozed = this.boxId("snoozed");
    if (snoozed && last.mailboxIds[snoozed]) {
      return html`<div class="banner" role="status">
        <span>${t(this.localize, "snoozedTill", { time: last.ortusSnoozedUntil ? this.when(last.ortusSnoozedUntil) : "" })}</span>
        <button class="button" @click=${() => this.wake()}>${t(this.localize, "unsnooze")}</button>
      </div>`;
    }
    return nothing;
  }

  private async cancelScheduled(id: string, draft: string): Promise<void> {
    await jmap([["EmailSubmission/set", { update: { [id]: { undoStatus: "canceled" } } }, "c"]]);
    navigate(`/mail/compose?draft=${encodeURIComponent(draft)}`);
  }

  private async snooze(at: Date): Promise<void> {
    this.snoozeOpen = false;
    if (!this.thread) return;
    const update: Record<string, unknown> = {};
    const until = at.toISOString().replace(/\.\d+Z$/, "Z");
    for (const m of this.thread) update[m.id] = { ortusSnoozedUntil: until };
    await jmap([["Email/set", { update }, "z"]]);
    this.go({ thread: null });
  }

  private async wake(): Promise<void> {
    if (!this.thread) return;
    const update: Record<string, unknown> = {};
    for (const m of this.thread) update[m.id] = { ortusSnoozedUntil: null };
    await jmap([["Email/set", { update }, "w"]]);
    this.go({ thread: null });
  }

  /** Moves the open thread's messages here out of the folder shown. */
  private async move(role: string): Promise<void> {
    const target = this.boxId(role);
    if (!target || !this.thread) return;
    const update: Record<string, unknown> = {};
    for (const m of this.thread) update[m.id] = { mailboxIds: { [target]: true } };
    await jmap([["Email/set", { update }, "m"]]);
    this.go({ thread: null });
  }

  private async destroy(): Promise<void> {
    if (!this.thread) return;
    await jmap([["Email/set", { destroy: this.thread.map((m) => m.id) }, "d"]]);
    this.go({ thread: null });
  }

  private async reply(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const thread = this.thread;
    const text = this.replyText.trim();
    if (!thread?.length || !text || this.sending) return;
    const last = thread[thread.length - 1]!;
    const drafts = this.boxId("drafts");
    this.sending = true;
    this.replyNote = null;
    try {
      const ids = await jmap([["Identity/get", {}, "i"]]);
      const identity = (ids.i!.list as { id: string; email: string; name: string }[])[0];
      if (!identity || !drafts) throw new MethodError("forbiddenFrom", {});
      const to = last.replyTo?.length ? last.replyTo : last.from ?? [];
      const subject = /^re:/i.test(last.subject) ? last.subject : `Re: ${last.subject}`;
      const references = [...(last.references ?? []), ...(last.messageId ?? [])];
      const r = await jmap([
        ["Email/set", { create: { r: { mailboxIds: { [drafts]: true }, keywords: { $draft: true, $seen: true }, from: [{ name: identity.name, email: identity.email }], to, subject, inReplyTo: last.messageId ?? null, references, textBody: [{ partId: "1", type: "text/plain" }], bodyValues: { "1": { value: text } } } } }, "c"],
        ["EmailSubmission/set", { create: { s: { identityId: identity.id, emailId: "#r" } }, onSuccessDestroyEmail: ["#s"] }, "s"],
      ]);
      const failed = (r.s!.notCreated as Record<string, { type: string; description?: string }> | undefined)?.s;
      if (failed) {
        const address = String((failed as { ortusAddress?: string }).ortusAddress ?? "");
        const needsReason = Boolean((failed as { ortusNeedsReason?: boolean }).ortusNeedsReason);
        this.replyNote = { error: true, text: failed.type === "forbiddenToSend" ? t(this.localize, needsReason ? "needsReason" : "notAllowed", { address }) : t(this.localize, "sendFailed") };
        // The draft stays in Drafts to finish in Compose.
      } else {
        this.replyText = "";
        this.replyNote = { error: false, text: t(this.localize, "replySent") };
        void this.load(true);
      }
    } catch {
      this.replyNote = { error: true, text: t(this.localize, "sendFailed") };
    } finally {
      this.sending = false;
    }
  }
}
