// The mail card of the start page (ortus-mail-today, from today.js): the
// unread mail in the inbox, each a link to its thread, and the way in.
import { expect, test } from "@playwright/test";
import en from "../src/locales/en.js";

test("the start page's mail card lists unread mail and leads to it", async ({ page }) => {
  await page.goto("/mail");
  await page.evaluate(async () => {
    const card = "/assets/apps/mail/today.js";
    await import(/* @vite-ignore */ card);
    document.body.append(document.createElement("ortus-mail-today"));
  });
  const card = page.locator("ortus-mail-today").getByRole("region", { name: en.inbox });
  await expect(card.getByRole("link", { name: en.todayOpen })).toHaveAttribute("href", "/mail");
  await expect(card.getByRole("link", { name: en.todayWrite })).toHaveAttribute("href", "/mail/compose");
  // What the inbox says is unread, the card says too.
  const unread = await page.request
    .post("/api/v1/mail/web/jmap", { headers: { "X-Requested-With": "ortus" }, data: { using: ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], methodCalls: [["Mailbox/get", {}, "b"]] } })
    .then(async (r) => ((await r.json()).methodResponses[0][1].list as { role: string; unreadEmails: number }[]).find((b) => b.role === "inbox")!.unreadEmails);
  if (unread) {
    // Other tests read and receive mail meanwhile: the count, not its value.
    await expect(card).toContainText(new RegExp(en.todayUnread.replace("{count}", "\\d+")));
    const first = card.getByRole("link").first();
    await expect(first).toHaveAttribute("href", /^\/mail\?thread=/);
  } else {
    await expect(card).toContainText(en.todayAllRead);
  }
});
