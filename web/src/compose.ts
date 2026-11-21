// Compose (board WebCompose): recipients with suggestions, subject and
// text, attachments, a label and the policy's answer before sending,
// drafts saved as one writes, send now (with time to undo) or later.
import { ApiError, LitElement, LocalizeController, call, controls, css, customElement, html, nothing, state, svg } from "ortus-kit";
import { t } from "./i18n.js";
import { jmap, type Email, type EmailAddress } from "./jmap.js";
import { calendarInstalled, navigate, toast } from "./events.js";

const ICON_BACK = svg`<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="m15 18-6-6 6-6"/></svg>`;
const ICON_CALENDAR = svg`<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><rect x="3" y="5" width="18" height="16" rx="2"/><path d="M3 10h18M8 3v4M16 3v4"/></svg>`;
const ICON_CLIP = svg`<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><path d="m21 11-8.5 8.5a5 5 0 0 1-7-7L14 4a3.5 3.5 0 0 1 5 5l-8.5 8.5a2 2 0 0 1-3-3L15 7"/></svg>`;

/** Undo send: how long a sent message waits before it goes. */
export const UNDO_SECONDS = 10;

interface Person {
  name: string | null;
  email: string;
  here?: boolean;
}

interface Attachment {
  blobId: string | null;
  name: string;
  size: number;
  type: string;
  failed?: boolean;
}

interface Identity {
  id: string;
  email: string;
  name: string;
  ortusSigned?: boolean;
}

type Problem = { ok: true } | { ok: false; problem: "needs_reason" | "policy" | "bad_recipient" | "not_your_address"; address?: string; label?: string };

const ADDRESS = /^[^\s@<>,;"]+@[^\s@<>,;"]+\.[^\s@<>,;"]+$/;


function tone(text: string): string {
  let h = 0;
  for (const c of text) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return `tone${h % 6}`;
}

/** The next morning at 08:00, local time. */
/** How long a meeting offered by "Times that work" is. */
const SLOT_MINUTES = 30;

/**
 * A few times to offer from everyone's free half hours: working hours
 * (08:00 to 18:00 here), at most two a day, two hours apart.
 */
export function pickSlots(times: number[], count: number): number[] {
  const out: number[] = [];
  for (const time of times) {
    const d = new Date(time);
    const minutes = d.getHours() * 60 + d.getMinutes();
    if (d.getDay() === 0 || d.getDay() === 6 || minutes < 8 * 60 || minutes + SLOT_MINUTES > 18 * 60) continue;
    const day = new Date(d).setHours(0, 0, 0, 0);
    const same = out.filter((o) => new Date(o).setHours(0, 0, 0, 0) === day);
    if (same.length >= 2 || same.some((o) => Math.abs(o - time) < 2 * 3_600_000)) continue;
    out.push(time);
    if (out.length === count) break;
  }
  return out;
}

export function tomorrowMorning(now = new Date()): Date {
  const d = new Date(now);
  d.setDate(d.getDate() + 1);
  d.setHours(8, 0, 0, 0);
  return d;
}

function utc(d: Date): string {
  return d.toISOString().replace(/\.\d+Z$/, "Z");
}

@customElement("ortus-mail-compose")
export class OrtusMailCompose extends LitElement {
  static override styles = [
    controls,
    css`
      :host {
        display: block;
        box-sizing: border-box;
        padding-block: 40px;
        padding-inline: 24px;
      }
      .sr {
        position: absolute;
        inline-size: 1px;
        block-size: 1px;
        overflow: hidden;
        clip-path: inset(50%);
        white-space: nowrap;
      }
      .page {
        display: grid;
        grid-template-columns: minmax(0, 800px) 260px;
        gap: 20px;
        max-inline-size: 1080px;
        margin-inline: auto;
      }
      .bar {
        grid-column: 1 / -1;
        display: flex;
        justify-content: space-between;
        align-items: center;
        margin-block-end: 8px;
      }
      .back {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        color: var(--ortus-text-2);
        font-size: 15px;
        text-decoration: none;
      }
      :host(:dir(rtl)) .back svg {
        transform: scaleX(-1);
      }
      .saved {
        color: var(--ortus-text-3);
        font-size: 13px;
      }
      h1 {
        margin: 0;
        font-size: 15px;
        font-weight: 600;
      }
      .card {
        border-radius: 18px;
        border: 1px solid var(--ortus-line);
        background: var(--ortus-surface);
      }
      .row {
        display: grid;
        grid-template-columns: 62px minmax(0, 1fr);
        align-items: center;
        gap: 12px;
        min-block-size: 72px;
        padding-inline: 28px;
        border-block-end: 1px solid var(--ortus-line);
        box-sizing: border-box;
      }
      .row > label {
        color: var(--ortus-text-3);
        font-size: 15px;
      }
      .recipients {
        position: relative;
        display: flex;
        flex-wrap: wrap;
        gap: 8px;
        align-items: center;
        padding-block: 12px;
      }
      .person {
        display: inline-flex;
        align-items: center;
        gap: 8px;
        block-size: 36px;
        padding-inline: 4px 10px;
        border-radius: 10px;
        background: var(--ortus-surface-2);
        border: 1px solid var(--ortus-line-strong);
        font-size: 14px;
      }
      .person.outside {
        border-color: #4a3a22;
      }
      .person button {
        border: 0;
        background: none;
        color: var(--ortus-text-3);
        cursor: pointer;
        font-size: 16px;
        padding-inline: 4px;
      }
      .mini {
        display: grid;
        place-items: center;
        inline-size: 24px;
        block-size: 24px;
        border-radius: 6px;
        font-size: 11px;
        font-weight: 700;
        color: var(--ortus-bg);
      }
      .tone0 { background: var(--ortus-orange); }
      .tone1 { background: #9aa3b5; }
      .tone2 { background: var(--ortus-accent); }
      .tone3 { background: var(--ortus-teal); }
      .tone4 { background: var(--ortus-lime); }
      .tone5 { background: #c9a7ff; }
      input.plain,
      textarea.plain {
        border: 0;
        background: none;
        color: var(--ortus-text);
        font: inherit;
        font-size: 16px;
        min-inline-size: 0;
      }
      input.plain:focus,
      textarea.plain:focus {
        outline: none;
      }
      .recipients input {
        flex: 1;
        min-inline-size: 160px;
        block-size: 36px;
      }
      #subject {
        font-weight: 600;
        inline-size: 100%;
      }
      [role="listbox"] {
        position: absolute;
        inset-block-start: 100%;
        inset-inline-start: 0;
        z-index: 2;
        min-inline-size: 320px;
        margin: 0;
        padding: 6px;
        list-style: none;
        border-radius: 12px;
        border: 1px solid var(--ortus-line-strong);
        background: var(--ortus-surface-2);
      }
      [role="option"] {
        display: grid;
        grid-template-columns: 28px minmax(0, 1fr);
        gap: 0 10px;
        align-items: center;
        padding: 8px 10px;
        border-radius: 8px;
        cursor: pointer;
        font-size: 14px;
      }
      [role="option"] small {
        grid-column: 2;
        color: var(--ortus-text-3);
      }
      [role="option"][aria-selected="true"] {
        background: var(--ortus-hover);
      }
      .text {
        display: block;
        box-sizing: border-box;
        inline-size: 100%;
        min-block-size: 180px;
        padding: 28px;
        line-height: 1.6;
        resize: vertical;
        field-sizing: content;
      }
      .files {
        display: flex;
        flex-wrap: wrap;
        gap: 10px;
        padding-inline: 28px;
        padding-block-end: 20px;
      }
      .file {
        display: inline-flex;
        align-items: center;
        gap: 10px;
        padding-block: 8px;
        padding-inline: 12px 6px;
        border-radius: 10px;
        border: 1px solid var(--ortus-line-strong);
        font-size: 14px;
      }
      .file small {
        color: var(--ortus-text-3);
      }
      .file.failed {
        border-color: var(--ortus-red);
      }
      .file button {
        border: 0;
        background: none;
        color: var(--ortus-text-3);
        cursor: pointer;
        font-size: 16px;
      }
      .warning {
        margin-block: 0 20px;
        margin-inline: 28px;
        padding: 16px 18px;
        border-radius: 14px;
        border: 1px solid #5a4421;
        background: #241c10;
        font-size: 14px;
        line-height: 1.5;
      }
      .warning.stop {
        border-color: #5a2626;
        background: #241314;
      }
      .warning p {
        margin: 0;
      }
      .warning label {
        display: block;
        margin-block: 12px 6px;
        color: var(--ortus-text-2);
      }
      .warning textarea {
        box-sizing: border-box;
        inline-size: 100%;
        min-block-size: 60px;
        padding: 10px 12px;
        border-radius: 10px;
        border: 1px solid var(--ortus-line-input);
        background: var(--ortus-surface);
        color: var(--ortus-text);
        font: inherit;
      }
      .foot {
        display: flex;
        align-items: center;
        gap: 10px;
        padding: 20px 28px;
        border-block-start: 1px solid var(--ortus-line);
      }
      .times {
        margin-block: 0 20px;
        margin-inline: 28px;
        padding: 20px;
        border-radius: 16px;
        border: 1px solid var(--ortus-line-strong);
      }
      .times p {
        margin: 0;
        font-size: 13px;
        color: var(--ortus-text-2);
      }
      .times p strong {
        color: var(--ortus-accent);
      }
      .times ul {
        display: grid;
        grid-template-columns: repeat(4, 1fr);
        gap: 12px;
        margin-block: 14px;
        margin-inline: 0;
        padding: 0;
        list-style: none;
      }
      .times li {
        display: grid;
        gap: 4px;
        padding-block: 12px;
        padding-inline: 16px;
        border-radius: 10px;
        border: 1px solid var(--ortus-line-strong);
        background: var(--ortus-surface-2);
      }
      .times li small {
        font-size: 13px;
        color: var(--ortus-text-2);
      }
      .times li strong {
        font-size: 16px;
      }
      .icon[aria-pressed="true"] {
        border-color: var(--ortus-accent);
        color: var(--ortus-accent);
      }
      .icon:disabled {
        opacity: 0.5;
        cursor: default;
      }
      .foot .spacer {
        flex: 1;
      }
      .primary {
        block-size: 44px;
        padding-inline: 20px;
        border-radius: 12px;
        border: 0;
        background: var(--ortus-accent);
        color: var(--ortus-bg);
        font: inherit;
        font-weight: 600;
        font-size: 15px;
        cursor: pointer;
      }
      .secondary {
        block-size: 44px;
        padding-inline: 18px;
        border-radius: 12px;
        border: 1px solid var(--ortus-line-strong);
        background: none;
        color: var(--ortus-text);
        font: inherit;
        font-weight: 600;
        font-size: 15px;
        cursor: pointer;
      }
      .primary:disabled,
      .secondary:disabled {
        opacity: 0.5;
        cursor: default;
      }
      .icon {
        display: grid;
        place-items: center;
        inline-size: 44px;
        block-size: 44px;
        border-radius: 12px;
        border: 1px solid var(--ortus-line-strong);
        background: var(--ortus-surface-2);
        color: var(--ortus-text);
        cursor: pointer;
      }
      aside {
        display: flex;
        flex-direction: column;
        gap: 16px;
      }
      aside section {
        padding: 20px;
        border-radius: 18px;
        border: 1px solid var(--ortus-line);
        background: var(--ortus-surface);
      }
      aside h2 {
        margin-block: 0 14px;
        margin-inline: 0;
        color: var(--ortus-text-3);
        font-size: 13px;
        font-weight: 400;
      }
      .who {
        display: grid;
        grid-template-columns: 40px minmax(0, 1fr);
        gap: 0 12px;
        align-items: center;
        margin-block-end: 14px;
      }
      .who:last-child {
        margin-block-end: 0;
      }
      .who .mini {
        inline-size: 40px;
        block-size: 40px;
        border-radius: 10px;
        font-size: 13px;
      }
      .who strong {
        display: block;
        font-size: 15px;
        overflow: hidden;
        text-overflow: ellipsis;
      }
      .who small {
        color: var(--ortus-text-3);
        font-size: 13px;
      }
      .delivery p {
        margin: 0 0 6px;
        font-size: 14px;
      }
      .ok {
        color: var(--ortus-teal);
      }
      .delivery label {
        display: block;
        margin-block: 14px 6px;
        color: var(--ortus-text-3);
        font-size: 13px;
      }
      select {
        inline-size: 100%;
        block-size: 38px;
        padding-inline: 10px;
        border-radius: 10px;
        border: 1px solid var(--ortus-line-input);
        background: var(--ortus-surface-2);
        color: var(--ortus-text);
        font: inherit;
      }
      .quiet {
        color: var(--ortus-text-3);
        font-size: 14px;
        margin: 0;
      }
      .error {
        color: var(--ortus-red);
        font-size: 14px;
        margin-block: 0;
        margin-inline: 28px;
        padding-block-end: 16px;
      }
      @media (max-width: 960px) {
        .page {
          grid-template-columns: minmax(0, 1fr);
        }
      }
    `,
  ];

  private readonly localize = new LocalizeController(this);

  @state() private to: Person[] = [];
  @state() private typing = "";
  @state() private suggestions: Person[] = [];
  @state() private active = 0;
  @state() private subject = "";
  @state() private text = "";
  @state() private attachments: Attachment[] = [];
  @state() private identities: Identity[] = [];
  @state() private from = "";
  @state() private labels: { id: string; name: string }[] = [];
  @state() private label = "";
  @state() private reason = "";
  @state() private problem: Problem = { ok: true };
  @state() private saved: "no" | "saving" | "saved" = "no";
  @state() private busy = false;
  @state() private error = "";
  /** "Times that work": the times offered, or null when the card is closed. */
  @state() private slots: number[] | null = null;
  @state() private slotsBusy = false;
  @state() private calendarsFree = 1;
  @state() private noCalendar = false;
  private draftId: string | null = null;
  private replyTo: Email | null = null;
  private saveTimer: number | undefined;
  private checkTimer: number | undefined;
  private suggestTimer: number | undefined;
  private changed = false;

  override connectedCallback(): void {
    super.connectedCallback();
    void this.start();
  }

  override disconnectedCallback(): void {
    super.disconnectedCallback();
    window.clearTimeout(this.saveTimer);
    window.clearTimeout(this.checkTimer);
    // Leaving with unsaved changes keeps them as a draft.
    if (this.changed && !this.busy) void this.save();
  }

  private async start(): Promise<void> {
    void calendarInstalled().then((on) => (this.noCalendar = !on));
    const p = new URLSearchParams(location.search);
    const [ids, labels] = await Promise.all([jmap([["Identity/get", {}, "i"]]), call<{ labels: { id: string; name: string }[] }>("GET", "/mail/labels").catch(() => ({ labels: [] }))]);
    this.identities = (ids.i!.list as Identity[]) ?? [];
    this.from = this.identities[0]?.id ?? "";
    this.labels = labels.labels;
    const draft = p.get("draft");
    const reply = p.get("reply");
    const to = p.get("to");
    if (draft) await this.loadDraft(draft);
    else if (reply) await this.loadReply(reply);
    if (to && ADDRESS.test(to)) this.to = [{ name: null, email: to }];
    // Another app's "send by email": a subject and text to start from.
    if (!draft && !reply) {
      const subject = p.get("subject");
      const text = p.get("text");
      if (subject) this.subject = subject.slice(0, 500);
      if (text) this.text = text.slice(0, 10_000);
    }
    void this.check();
  }

  private async loadDraft(id: string): Promise<void> {
    const r = await jmap([["Email/get", { ids: [id], properties: ["to", "subject", "ortusText", "attachments", "from", "inReplyTo", "references"], fetchTextBodyValues: false }, "d"]]);
    const draft = (r.d!.list as (Email & { inReplyTo?: string[] | null })[])[0];
    if (!draft) return;
    this.draftId = id;
    this.to = (draft.to ?? []).filter((a): a is EmailAddress & { email: string } => Boolean(a.email)).map((a) => ({ name: a.name, email: a.email }));
    this.subject = draft.subject;
    this.text = draft.ortusText ?? "";
    // The draft's own attachments are reused as they are.
    this.attachments = (draft.attachments ?? []).map((a) => ({ blobId: a.blobId, name: a.name ?? "file", size: a.size, type: a.type }));
    const sender = draft.from?.[0]?.email;
    if (sender && this.identities.some((i) => i.id === sender)) this.from = sender;
    this.saved = "saved";
  }

  private async loadReply(id: string): Promise<void> {
    const r = await jmap([["Email/get", { ids: [id], properties: ["from", "replyTo", "subject", "messageId", "references"] }, "e"]]);
    const original = (r.e!.list as Email[])[0];
    if (!original) return;
    this.replyTo = original;
    const to = original.replyTo?.length ? original.replyTo : (original.from ?? []);
    this.to = to.filter((a) => a.email).map((a) => ({ name: a.name, email: a.email! }));
    this.subject = /^re:/i.test(original.subject) ? original.subject : `Re: ${original.subject}`;
  }

  /** Something the person changed: save the draft soon and ask the policy. */
  private edited(): void {
    this.changed = true;
    this.saved = "no";
    window.clearTimeout(this.saveTimer);
    this.saveTimer = window.setTimeout(() => void this.save(), 1500);
    window.clearTimeout(this.checkTimer);
    this.checkTimer = window.setTimeout(() => void this.check(), 300);
  }

  private body(): Record<string, unknown> {
    const identity = this.identities.find((i) => i.id === this.from);
    const drafts: Record<string, boolean> = {};
    const email: Record<string, unknown> = {
      keywords: { $draft: true, $seen: true },
      from: identity ? [{ name: identity.name, email: identity.email }] : undefined,
      to: this.to.map((p) => ({ name: p.name, email: p.email })),
      subject: this.subject,
      textBody: [{ partId: "1", type: "text/plain" }],
      bodyValues: { "1": { value: this.text } },
      attachments: this.attachments.filter((a) => a.blobId).map((a) => ({ blobId: a.blobId, type: a.type, name: a.name })),
      mailboxIds: drafts,
    };
    if (this.label) email["header:X-Ortus-Label:asText"] = this.label;
    if (this.replyTo) {
      email.inReplyTo = this.replyTo.messageId ?? null;
      email.references = [...(this.replyTo.references ?? []), ...(this.replyTo.messageId ?? [])];
    }
    return email;
  }

  /** Keeps what is written as a draft (a new one replaces the old). */
  private async save(): Promise<string | null> {
    window.clearTimeout(this.saveTimer);
    if (!this.changed && this.draftId) return this.draftId;
    this.saved = "saving";
    try {
      const boxes = await jmap([["Mailbox/get", { properties: ["role"] }, "b"]]);
      const drafts = (boxes.b!.list as { id: string; role: string | null }[]).find((b) => b.role === "drafts")?.id;
      if (!drafts) throw new Error("no drafts folder");
      const email = this.body();
      email.mailboxIds = { [drafts]: true };
      const old = this.draftId;
      const r = await jmap([["Email/set", { create: { d: email }, destroy: old ? [old] : [] }, "s"]]);
      const made = (r.s!.created as Record<string, { id: string }> | undefined)?.d;
      if (!made) throw new Error(JSON.stringify(r.s!.notCreated));
      this.draftId = made.id;
      this.changed = false;
      this.saved = "saved";
      return made.id;
    } catch (e) {
      console.warn("mail: draft not saved", e);
      this.saved = "no";
      return null;
    }
  }

  private async check(): Promise<void> {
    if (!this.to.length || !this.from) {
      this.problem = { ok: true };
      return;
    }
    try {
      this.problem = await call<Problem>("POST", "/mail/check", { from: this.from, to: this.to.map((p) => p.email), label: this.label || null, reason: this.reason || null });
    } catch {
      this.problem = { ok: true };
    }
  }

  private async suggest(): Promise<void> {
    const q = this.typing.trim();
    if (q.length < 2) {
      this.suggestions = [];
      return;
    }
    try {
      const r = await call<{ people: Person[] }>("GET", `/mail/people?q=${encodeURIComponent(q)}`);
      if (this.typing.trim() !== q) return;
      this.suggestions = r.people.filter((p) => !this.to.some((x) => x.email === p.email));
      this.active = 0;
    } catch {
      this.suggestions = [];
    }
  }

  private add(person: Person): void {
    if (!this.to.some((p) => p.email.toLowerCase() === person.email.toLowerCase())) this.to = [...this.to, person];
    this.typing = "";
    this.suggestions = [];
    this.edited();
  }

  private onKey(event: KeyboardEvent): void {
    const typed = this.typing.trim().replace(/[,;]$/, "");
    if (event.key === "ArrowDown" && this.suggestions.length) {
      event.preventDefault();
      this.active = (this.active + 1) % this.suggestions.length;
    } else if (event.key === "ArrowUp" && this.suggestions.length) {
      event.preventDefault();
      this.active = (this.active - 1 + this.suggestions.length) % this.suggestions.length;
    } else if (event.key === "Enter" || event.key === "," || event.key === ";" || event.key === "Tab") {
      const pick = this.suggestions[this.active];
      if (pick && this.typing.trim()) {
        event.preventDefault();
        this.add(pick);
      } else if (ADDRESS.test(typed)) {
        event.preventDefault();
        this.add({ name: null, email: typed });
      } else if (event.key !== "Tab" && typed) {
        event.preventDefault();
      }
    } else if (event.key === "Escape") {
      this.suggestions = [];
    } else if (event.key === "Backspace" && !this.typing && this.to.length) {
      this.to = this.to.slice(0, -1);
      this.edited();
    }
  }

  private async attach(event: Event): Promise<void> {
    const input = event.target as HTMLInputElement;
    for (const file of Array.from(input.files ?? [])) {
      const entry: Attachment = { blobId: null, name: file.name, size: file.size, type: file.type || "application/octet-stream" };
      this.attachments = [...this.attachments, entry];
      try {
        const response = await fetch("/api/v1/mail/web/upload", { method: "POST", headers: { "X-Requested-With": "ortus", "Content-Type": "application/octet-stream" }, body: file });
        if (!response.ok) throw new Error(String(response.status));
        const uploaded = (await response.json()) as { blobId: string };
        this.attachments = this.attachments.map((a) => (a === entry ? { ...a, blobId: uploaded.blobId } : a));
      } catch {
        this.attachments = this.attachments.map((a) => (a === entry ? { ...a, failed: true } : a));
      }
      this.edited();
    }
    input.value = "";
  }

  private size(bytes: number): string {
    const lang = document.documentElement.lang || "en";
    const [value, unit] = bytes >= 1_000_000 ? [bytes / 1_000_000, "megabyte"] : [Math.max(1, Math.round(bytes / 1000)), "kilobyte"];
    return new Intl.NumberFormat(lang, { style: "unit", unit, unitDisplay: "short", maximumFractionDigits: 1 }).format(value);
  }

  private blocked(): boolean {
    if (!this.to.length || this.busy || this.attachments.some((a) => !a.blobId)) return true;
    if (!this.problem.ok && this.problem.problem !== "needs_reason") return true;
    return !this.problem.ok && this.problem.problem === "needs_reason" && !this.reason.trim();
  }

  /** Sends at `when`, or now with a few seconds to undo. */
  private async send(when: Date | null): Promise<void> {
    if (this.blocked()) return;
    this.busy = true;
    this.error = "";
    try {
      if (this.slots?.length) {
        // The times go as a link anyone may pick from; once.
        const offer = await call<{ url: string }>("POST", "/calendar/polls", { summary: this.subject.trim() || t(this.localize, "meeting"), minutes: SLOT_MINUTES, times: this.slots, people: this.to.map((p) => p.email) });
        const list = this.slots.map((time) => `- ${this.slotText(time)}`).join("\n");
        this.text = `${this.text.replace(/\s+$/, "")}\n\n${t(this.localize, "pickLine", { url: offer.url })}\n${list}\n`;
        this.slots = null;
      }
      this.changed = true;
      const id = await this.save();
      if (!id) throw new Error("draft");
      const at = when ?? new Date(Date.now() + UNDO_SECONDS * 1000);
      const r = await jmap([["EmailSubmission/set", { create: { s: { identityId: this.from, emailId: id, ortusSendAt: utc(at), ortusReason: this.reason.trim() || undefined } } }, "x"]]);
      const made = (r.x!.created as Record<string, { id: string }> | undefined)?.s;
      if (!made) {
        const failed = (r.x!.notCreated as Record<string, { type: string; ortusAddress?: string; ortusNeedsReason?: boolean }>).s;
        this.error = failed?.type === "forbiddenToSend" ? t(this.localize, failed.ortusNeedsReason ? "needsReason" : "notAllowed", { address: failed.ortusAddress ?? "" }) : t(this.localize, "sendFailed");
        void this.check();
        return;
      }
      this.changed = false;
      const count = this.to.length;
      if (when) toast({ text: t(this.localize, "scheduledFor", { time: this.when(when) }) });
      else toast({ text: t(this.localize, "sentTo", { count }), undo: { submission: made.id, draft: id }, seconds: UNDO_SECONDS });
      navigate("/mail");
    } catch (e) {
      this.error = e instanceof ApiError ? e.message : t(this.localize, "sendFailed");
    } finally {
      this.busy = false;
    }
  }

  /** Opens or closes "Times that work": free times for everyone, this week. */
  private async toggleTimes(): Promise<void> {
    if (this.slots) {
      this.slots = null;
      return;
    }
    this.slotsBusy = true;
    this.error = "";
    try {
      const from = Math.ceil((Date.now() + 1) / 1_800_000) * 1_800_000;
      const to = from + 7 * 86_400_000;
      const people = this.to.map((p) => p.email).join(",");
      const r = await call<{ times: number[]; people: { here: boolean }[] }>("GET", `/calendar/free?people=${encodeURIComponent(people)}&from=${from}&to=${to}&minutes=${SLOT_MINUTES}&limit=200`);
      this.slots = pickSlots(r.times, 4);
      this.calendarsFree = 1 + r.people.filter((p) => p.here).length;
    } catch (e) {
      if (e instanceof ApiError && (e.status === 404 || e.status === 501)) this.noCalendar = true;
      else this.error = t(this.localize, "timesFailed");
    } finally {
      this.slotsBusy = false;
    }
  }

  private slotText(time: number): string {
    return new Intl.DateTimeFormat(document.documentElement.lang || "en", { weekday: "short", day: "numeric", month: "short", hour: "2-digit", minute: "2-digit", hourCycle: "h23" }).format(new Date(time));
  }

  private renderTimes() {
    if (!this.slots) return nothing;
    const lang = document.documentElement.lang || "en";
    return html`<section class="times" aria-labelledby="times-title">
      <p><strong id="times-title">${t(this.localize, "timesThatWork")}</strong> <span>· ${t(this.localize, "timesLead", { minutes: SLOT_MINUTES, count: this.calendarsFree })}</span></p>
      ${this.slots.length
        ? html`<ul>
            ${this.slots.map(
              (time) => html`<li>
                <small>${new Intl.DateTimeFormat(lang, { weekday: "short", day: "numeric" }).format(new Date(time))}</small>
                <strong>${new Intl.DateTimeFormat(lang, { hour: "2-digit", minute: "2-digit", hourCycle: "h23" }).format(new Date(time))}</strong>
              </li>`,
            )}
          </ul>`
        : html`<p class="quiet">${t(this.localize, "noFreeTimes")}</p>`}
      <p class="quiet">${t(this.localize, "timesHint")}</p>
    </section>`;
  }

  private when(d: Date): string {
    return new Intl.DateTimeFormat(document.documentElement.lang || "en", { weekday: "short", hour: "2-digit", minute: "2-digit" }).format(d);
  }

  override render() {
    const identity = this.identities.find((i) => i.id === this.from);
    const allHere = this.to.length > 0 && this.to.every((p) => p.here);
    const tomorrow = tomorrowMorning();
    return html`
      <div class="page">
        <div class="bar">
          <a class="back" href="/mail" @click=${(e: Event) => {
            e.preventDefault();
            navigate("/mail");
          }}>${ICON_BACK}${t(this.localize, "inbox")}</a>
          <span class="saved" role="status">${this.saved === "saved" ? t(this.localize, "draftSaved") : this.saved === "saving" ? t(this.localize, "draftSaving") : ""}</span>
        </div>
        <form class="card" @submit=${(e: Event) => {
          e.preventDefault();
          void this.send(null);
        }}>
          <h1 class="sr">${t(this.localize, "newMessage")}</h1>
          <div class="row">
            <label for="to">${t(this.localize, "to")}</label>
            <div class="recipients">
              ${this.to.map(
                (p) => html`<span class="person ${p.here === false ? "outside" : ""}">
                  <ortus-avatar class="mini ${tone(p.name || p.email)}" aria-hidden="true" .name=${p.name || p.email} .email=${p.email ?? ""}></ortus-avatar>
                  <span dir="auto">${p.name || p.email}</span>
                  <button type="button" aria-label=${t(this.localize, "removeRecipient", { name: p.name || p.email })} @click=${() => {
                    this.to = this.to.filter((x) => x !== p);
                    this.edited();
                  }}>×</button>
                </span>`,
              )}
              <input
                id="to"
                class="plain"
                role="combobox"
                aria-autocomplete="list"
                aria-expanded=${this.suggestions.length ? "true" : "false"}
                aria-controls="people"
                aria-activedescendant=${this.suggestions.length ? `person-${this.active}` : nothing}
                autocomplete="off"
                .value=${this.typing}
                @input=${(e: InputEvent) => {
                  this.typing = (e.target as HTMLInputElement).value;
                  window.clearTimeout(this.suggestTimer);
                  this.suggestTimer = window.setTimeout(() => void this.suggest(), 150);
                }}
                @keydown=${this.onKey}
                @blur=${() => {
                  const typed = this.typing.trim();
                  if (ADDRESS.test(typed)) this.add({ name: null, email: typed });
                  window.setTimeout(() => (this.suggestions = []), 150);
                }}
              />
              ${this.suggestions.length
                ? html`<ul id="people" role="listbox" aria-label=${t(this.localize, "suggestions")}>
                    ${this.suggestions.map(
                      (p, i) => html`<li id="person-${i}" role="option" aria-selected=${i === this.active ? "true" : "false"} @mousedown=${(e: Event) => {
                        e.preventDefault();
                        this.add(p);
                      }}>
                        <ortus-avatar class="mini ${tone(p.name || p.email)}" aria-hidden="true" .name=${p.name || p.email} .email=${p.email ?? ""}></ortus-avatar>
                        <span dir="auto">${p.name || p.email}</span>
                        <small dir="auto">${p.email} · ${p.here ? t(this.localize, "colleague") : t(this.localize, "outside")}</small>
                      </li>`,
                    )}
                  </ul>`
                : nothing}
            </div>
          </div>
          <div class="row">
            <label for="subject">${t(this.localize, "subject")}</label>
            <input id="subject" class="plain" dir="auto" .value=${this.subject} @input=${(e: InputEvent) => {
              this.subject = (e.target as HTMLInputElement).value;
              this.edited();
            }} />
          </div>
          <label class="sr" for="text">${t(this.localize, "message")}</label>
          <textarea id="text" class="plain text" dir="auto" .value=${this.text} @input=${(e: InputEvent) => {
            this.text = (e.target as HTMLTextAreaElement).value;
            this.edited();
          }}></textarea>
          ${this.attachments.length
            ? html`<div class="files" aria-label=${t(this.localize, "attachments")}>
                ${this.attachments.map(
                  (a) => html`<span class="file ${a.failed ? "failed" : ""}">
                    <span dir="auto">${a.name}</span>
                    <small>${a.failed ? t(this.localize, "uploadFailed") : a.blobId ? this.size(a.size) : t(this.localize, "uploading")}</small>
                    <button type="button" aria-label=${t(this.localize, "removeAttachment", { name: a.name })} @click=${() => {
                      this.attachments = this.attachments.filter((x) => x !== a);
                      this.edited();
                    }}>×</button>
                  </span>`,
                )}
              </div>`
            : nothing}
          ${this.renderTimes()}
          ${this.warning()}
          ${this.error ? html`<p class="error" role="alert">${this.error}</p>` : nothing}
          <div class="foot">
            <button class="primary" type="submit" ?disabled=${this.blocked()}>${t(this.localize, "send")}</button>
            <button class="secondary" type="button" ?disabled=${this.blocked()} @click=${() => this.send(tomorrow)}>${t(this.localize, "sendAt", { time: new Intl.DateTimeFormat(document.documentElement.lang || "en", { hour: "2-digit", minute: "2-digit" }).format(tomorrow) })}</button>
            <span class="spacer"></span>
            ${this.noCalendar
              ? nothing
              : html`<button type="button" class="icon" aria-pressed=${this.slots ? "true" : "false"} title=${t(this.localize, "timesThatWork")} ?disabled=${!this.to.length || this.slotsBusy} @click=${this.toggleTimes}>
                  ${ICON_CALENDAR}<span class="sr">${t(this.localize, "timesThatWork")}</span>
                </button>`}
            <label class="icon" title=${t(this.localize, "attach")}>
              ${ICON_CLIP}<span class="sr">${t(this.localize, "attach")}</span>
              <input class="sr" type="file" multiple @change=${this.attach} />
            </label>
          </div>
        </form>
        <aside>
          <section aria-labelledby="people-title">
            <h2 id="people-title">${t(this.localize, "people")}</h2>
            ${this.to.length
              ? this.to.map(
                  (p) => html`<div class="who">
                    <ortus-avatar class="mini ${tone(p.name || p.email)}" aria-hidden="true" .name=${p.name || p.email} .email=${p.email ?? ""}></ortus-avatar>
                    <span><strong dir="auto">${p.name || p.email}</strong><small dir="auto">${p.here === true ? t(this.localize, "colleague") : p.here === false ? `${t(this.localize, "outside")} · ${p.email.split("@")[1] ?? ""}` : p.email}</small></span>
                  </div>`,
                )
              : html`<p class="quiet">${t(this.localize, "noRecipients")}</p>`}
          </section>
          <section class="delivery" aria-labelledby="delivery-title">
            <h2 id="delivery-title">${t(this.localize, "delivery")}</h2>
            ${this.identities.length > 1
              ? html`<label for="from">${t(this.localize, "from")}</label>
                  <select id="from" @change=${(e: Event) => {
                    this.from = (e.target as HTMLSelectElement).value;
                    this.edited();
                  }}>
                    ${this.identities.map((i) => html`<option value=${i.id} ?selected=${i.id === this.from}>${i.email}</option>`)}
                  </select>`
              : html`<p>${t(this.localize, "fromAddress", { address: identity?.email ?? "" })}</p>`}
            ${identity ? html`<p class=${identity.ortusSigned ? "ok" : "quiet"}>${identity.ortusSigned ? t(this.localize, "signed") : t(this.localize, "notSigned")} · ${allHere ? t(this.localize, "staysInside") : t(this.localize, "tlsWhenOffered")}</p>` : html`<p class="quiet">${t(this.localize, "noIdentity")}</p>`}
            <label for="label">${t(this.localize, "label")}</label>
            <select id="label" @change=${(e: Event) => {
              this.label = (e.target as HTMLSelectElement).value;
              this.edited();
            }}>
              <option value="" ?selected=${!this.label}>${t(this.localize, "noLabel")}</option>
              ${this.labels.map((l) => html`<option value=${l.id} ?selected=${l.id === this.label}>${l.name}</option>`)}
            </select>
          </section>
        </aside>
      </div>
    `;
  }

  private warning() {
    const p = this.problem;
    if (p.ok) return nothing;
    const label = this.labels.find((l) => l.id === (p.label ?? this.label))?.name ?? p.label ?? this.label;
    if (p.problem === "needs_reason") {
      return html`<div class="warning" role="status">
        <p>${t(this.localize, "reasonNeeded", { label, address: p.address ?? "" })}</p>
        <label for="reason">${t(this.localize, "reason")}</label>
        <textarea id="reason" .value=${this.reason} @input=${(e: InputEvent) => {
          this.reason = (e.target as HTMLTextAreaElement).value;
        }}></textarea>
      </div>`;
    }
    const text =
      p.problem === "policy"
        ? t(this.localize, "labelForbids", { label, address: p.address ?? "" })
        : p.problem === "bad_recipient"
          ? t(this.localize, "badAddress", { address: p.address ?? "" })
          : t(this.localize, "noIdentity");
    return html`<div class="warning stop" role="alert"><p>${text}</p></div>`;
  }
}
