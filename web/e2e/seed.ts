// Mail for the browser tests: imported through the same JMAP the apps
// use (upload, then Email/import), oldest first.
import type { APIRequestContext } from "@playwright/test";

const ANNA_1 = `From: Anna Kowalska <anna@regional.example>
To: Olena Hrytsenko <olena@ministry.example>, finance@ministry.example
Subject: Gauge report and updated shelter agreement
Message-ID: <gauge-1@regional.example>
Date: Wed, 18 Nov 2026 09:40:00 +0000

Hi Olena, the gauge report follows within the hour.

Anna
`;

const ANNA_2 = `From: Anna Kowalska <anna@regional.example>
To: Olena Hrytsenko <olena@ministry.example>, finance@ministry.example
Subject: Re: Gauge report and updated shelter agreement
Message-ID: <gauge-2@regional.example>
In-Reply-To: <gauge-1@regional.example>
References: <gauge-1@regional.example>
Date: Wed, 18 Nov 2026 10:42:00 +0000
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary="b1"

--b1
Content-Type: text/plain; charset=utf-8

Hi,

Attached is today's gauge report and the updated shelter agreement. The only change is the school on Park Street as the second site. Legal should look at clause 4. I sent a slot for Thursday.

Anna
--b1
Content-Type: application/pdf; name="gauge-report.pdf"
Content-Disposition: attachment; filename="gauge-report.pdf"
Content-Transfer-Encoding: base64

JVBERi0xLjQKJcOkw7zDtsOfCjEgMCBvYmoKPDwvVHlwZS9DYXRhbG9nPj4KZW5kb2JqCnRyYWlsZXIKPDwvUm9vdCAxIDAgUj4+CiUlRU9GCg==
--b1--
`;

const MESSAGES: { raw: string; keywords?: Record<string, boolean> }[] = [
  { raw: "From: Ana Ferreira <ana@ministry.example>\nTo: olena@ministry.example\nSubject: Town hall tonight: talking points\nMessage-ID: <town@ministry.example>\n\nThree points for tonight.\n", keywords: { $seen: true, team: true } },
  { raw: "From: Jonas Weber <jonas@police.example>\nTo: olena@ministry.example\nSubject: Police escort schedule\nMessage-ID: <escort@police.example>\n\nEscort leaves at 07:00.\n", keywords: { $seen: true } },
  { raw: "From: Daniel Price <daniel@hydro.example>\nTo: olena@ministry.example\nSubject: Invitation: hydrology briefing\nMessage-ID: <brief@hydro.example>\n\nCan you join the hydrology briefing on Thursday?\n" },
  { raw: "From: City News <news@city.example>\nTo: olena@ministry.example\nSubject: Weekly digest\nList-Id: <news.city.example>\nMessage-ID: <digest@city.example>\n\nRoad works this week.\n" },
  { raw: "From: River Authority <alerts@river.example>\nTo: olena@ministry.example\nSubject: Gauge at the bridge passed 4.2 m\nAuto-Submitted: auto-generated\nMessage-ID: <alert@river.example>\n\nLevel 4.2 m and rising.\n" },
  { raw: ANNA_1, keywords: { $seen: true, flood: true } },
  { raw: ANNA_2, keywords: { flood: true } },
];

export async function seedMail(request: APIRequestContext, token: string): Promise<void> {
  const me = await (await request.get("/api/v1/me")).json();
  const boxes = await (await request.post("/api/v1/mail/web/jmap", { headers: { "X-Requested-With": "ortus" }, data: { using: ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], methodCalls: [["Mailbox/get", {}, "0"]] } })).json();
  const inbox = boxes.methodResponses[0][1].list.find((b: { role: string }) => b.role === "inbox").id;
  for (const message of MESSAGES) {
    await importMessage(request, token, me.id, inbox, message.raw, message.keywords ?? {});
  }
}

export async function importMessage(request: APIRequestContext, token: string, account: string, mailbox: string, raw: string, keywords: Record<string, boolean>): Promise<void> {
  const uploaded = await (await request.post(`/api/v1/mail/jmap/upload/${account}`, { headers: { Authorization: `Bearer ${token}`, "Content-Type": "message/rfc822" }, data: raw.replace(/\r?\n/g, "\r\n") })).json();
  const r = await request.post("/api/v1/mail/web/jmap", {
    headers: { "X-Requested-With": "ortus" },
    data: { using: ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], methodCalls: [["Email/import", { emails: { m: { blobId: uploaded.blobId, mailboxIds: { [mailbox]: true }, keywords } } }, "0"]] },
  });
  const body = await r.json();
  if (!body.methodResponses[0][1].created?.m) throw new Error(`import failed: ${JSON.stringify(body)}`);
}
