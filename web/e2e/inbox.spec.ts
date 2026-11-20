// The inbox (board Main): catch-up, tabs, reading a thread, labels,
// search, archiving, a quick reply, and new mail arriving while it is open.
import { expect, test, type Page } from "@playwright/test";
import en from "../src/locales/en.js";
import { importMessage } from "./seed.js";

test.describe.configure({ mode: "serial" });

const inbox = (page: Page) => page.locator("ortus-mail-inbox");

test("catch-up names the mail waiting for a reply and shows it", async ({ page }) => {
  await page.goto("/mail");
  const card = inbox(page).getByRole("region", { name: en.catchUp });
  await expect(card).toContainText("2 need a reply (Anna, Daniel).");
  await card.getByRole("button", { name: "Show the 2" }).click();
  await expect(inbox(page).locator(".item")).toHaveCount(2);
});

test("the tabs part mail from people, machines and lists", async ({ page }) => {
  await page.goto("/mail");
  const list = inbox(page).locator("ul.list");
  await expect(list).toContainText("Anna Kowalska");
  await expect(list).not.toContainText("River Authority");
  await inbox(page).getByRole("tab", { name: en.tabOther }).click();
  await expect(list).toContainText("River Authority");
  await expect(list).not.toContainText("Anna Kowalska");
  await inbox(page).getByRole("tab", { name: en.tabFeeds }).click();
  await expect(list).toContainText("City News");
});

test("a thread opens with its messages, attachment and label, and is read", async ({ page }) => {
  await page.goto("/mail");
  const anna = inbox(page).getByRole("button", { name: /Anna Kowalska/ });
  await expect(anna).toHaveClass(/unread/);
  await anna.click();
  const reader = inbox(page).getByRole("region", { name: "Re: Gauge report and updated shelter agreement" });
  await expect(reader.getByRole("heading", { level: 2 })).toBeVisible();
  await expect(reader).toContainText("2 messages · 3 people");
  await expect(reader).toContainText("Legal should look at clause 4.");
  await expect(reader.locator(".chip")).toHaveText("Flood");
  await expect(anna).not.toHaveClass(/unread/);
  const download = page.waitForEvent("download");
  await reader.getByRole("link", { name: /gauge-report\.pdf/ }).click();
  expect((await download).suggestedFilename()).toBe("gauge-report.pdf");
  // The earlier message is folded; a click opens it.
  await reader.getByText("the gauge report follows within the hour").click();
  await expect(reader).toContainText("Anna");
  await page.screenshot({ path: "test-results/inbox.png" });
});

test("labels and search find mail", async ({ page }) => {
  await page.goto("/mail");
  await inbox(page).getByRole("navigation").getByRole("button", { name: "Flood" }).click();
  await expect(inbox(page).locator(".item")).toHaveCount(1);
  await expect(inbox(page).getByRole("heading", { level: 1 })).toHaveText("Flood");
  await inbox(page).getByRole("searchbox", { name: en.searchMail }).fill("hydrology");
  await inbox(page).getByRole("searchbox", { name: en.searchMail }).press("Enter");
  await expect(inbox(page).locator(".item")).toHaveCount(1);
  await expect(inbox(page).locator(".item")).toContainText("Daniel Price");
  await inbox(page).getByRole("searchbox", { name: en.searchMail }).fill("nothing like this");
  await inbox(page).getByRole("searchbox", { name: en.searchMail }).press("Enter");
  await expect(inbox(page).getByRole("status")).toContainText(en.noResults.replace("{q}", "nothing like this"));
});

test("archiving takes a thread out of the inbox and into Archive", async ({ page }) => {
  await page.goto("/mail");
  await inbox(page).getByRole("button", { name: /Jonas Weber/ }).click();
  await inbox(page).getByRole("region", { name: "Police escort schedule" }).getByRole("button", { name: en.archiveIt }).click();
  await expect(inbox(page).getByRole("button", { name: /Jonas Weber/ })).toHaveCount(0);
  await inbox(page).getByRole("navigation").getByRole("button", { name: en.archive }).click();
  await expect(inbox(page).getByRole("button", { name: /Jonas Weber/ })).toBeVisible();
});

test("a quick reply goes out and is kept in Sent", async ({ page }) => {
  await page.goto("/mail");
  await inbox(page).getByRole("button", { name: /Daniel Price/ }).click();
  await inbox(page).getByRole("textbox", { name: "Reply to Daniel Price…" }).fill("Yes, I will join on Thursday.");
  await inbox(page).getByRole("button", { name: en.send }).click();
  await expect(inbox(page).getByRole("status")).toHaveText(en.replySent);
  await inbox(page).getByRole("navigation").getByRole("button", { name: en.sent }).click();
  await expect(inbox(page).getByRole("heading", { level: 1 })).toHaveText(en.sent);
  await expect(inbox(page).locator(".item")).toHaveCount(1);
  await expect(inbox(page).locator(".item")).toContainText("Re: Invitation: hydrology briefing");
});

test("new mail appears while the inbox is open", async ({ page }) => {
  await page.goto("/mail");
  await expect(inbox(page).getByRole("button", { name: /Anna Kowalska/ })).toBeVisible();
  const me = await (await page.request.get("/api/v1/me")).json();
  const token = (await page.context().cookies()).find((c) => c.name === "ortus")?.value ?? "";
  const boxes = await (await page.request.post("/api/v1/mail/web/jmap", { headers: { "X-Requested-With": "ortus" }, data: { using: ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], methodCalls: [["Mailbox/get", {}, "0"]] } })).json();
  const id = boxes.methodResponses[0][1].list.find((b: { role: string }) => b.role === "inbox").id;
  await importMessage(page.request, token, me.id, id, "From: Marek Nowak <marek@ministry.example>\nTo: olena@ministry.example\nSubject: Shelter capacity sheet\nMessage-ID: <cap@ministry.example>\n\nUpdated numbers.\n", {});
  await expect(inbox(page).getByRole("button", { name: /Marek Nowak/ })).toBeVisible({ timeout: 10_000 });
});
