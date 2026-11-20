// JMAP from the web client: the same methods mail apps call, through the
// web client's own door (its session, /mail/web/jmap).
import { call } from "ortus-kit";

const USING = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail", "urn:ietf:params:jmap:submission"];

export type Invocation = [string, Record<string, unknown>, string];

export class MethodError extends Error {
  constructor(
    readonly type: string,
    readonly detail: Record<string, unknown>,
  ) {
    super(type);
  }
}

/** Runs method calls; answers each response's arguments by call id. */
export async function jmap(calls: Invocation[]): Promise<Record<string, Record<string, unknown>>> {
  const answer = await call<{ methodResponses: Invocation[] }>("POST", "/mail/web/jmap", { using: USING, methodCalls: calls });
  const out: Record<string, Record<string, unknown>> = {};
  for (const [name, args, id] of answer.methodResponses) {
    if (name === "error") throw new MethodError(String(args.type), args);
    // A follow-up (the draft removed after sending) shares its call's id.
    if (!(id in out)) out[id] = args;
  }
  return out;
}

export interface EmailAddress {
  name: string | null;
  email: string | null;
}

export interface Mailbox {
  id: string;
  name: string;
  role: string | null;
  totalEmails: number;
  unreadEmails: number;
}

export interface BodyPart {
  partId: string;
  blobId: string;
  size: number;
  type: string;
  name: string | null;
}

export interface Email {
  id: string;
  threadId: string;
  subject: string;
  from: EmailAddress[] | null;
  to?: EmailAddress[] | null;
  cc?: EmailAddress[] | null;
  replyTo?: EmailAddress[] | null;
  receivedAt: string;
  preview: string;
  keywords: Record<string, boolean>;
  mailboxIds: Record<string, boolean>;
  hasAttachment: boolean;
  messageId?: string[] | null;
  references?: string[] | null;
  attachments?: BodyPart[];
  ortusText?: string;
  ortusVerified?: boolean;
  ortusCategory?: string;
}

export const LIST_PROPERTIES = ["threadId", "subject", "from", "to", "receivedAt", "preview", "keywords", "mailboxIds", "hasAttachment", "ortusCategory"];
export const READ_PROPERTIES = [...LIST_PROPERTIES, "cc", "replyTo", "messageId", "references", "attachments", "ortusText", "ortusVerified"];

/** Labels are keywords of one's own (not the `$` system ones). */
export function labelsOf(email: Pick<Email, "keywords">): string[] {
  return Object.keys(email.keywords).filter((k) => !k.startsWith("$"));
}
