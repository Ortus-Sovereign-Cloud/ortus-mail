// Mail on a phone (390 px): one pane at a time. The folders are a strip
// above the list, a thread fills the screen with a way back, and nothing
// is wider than the screen.
import { expect, test, type Page } from "@playwright/test";
import ar from "../src/locales/ar.js";
import en from "../src/locales/en.js";

test.use({ viewport: { width: 390, height: 844 } });

const inbox = (page: Page) => page.locator("ortus-mail-inbox");

async function fitsTheScreen(page: Page): Promise<void> {
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);
}

test("on a phone the inbox shows the list, then a thread full width with a way back", async ({ page }) => {
  await page.goto("/mail");
  const list = inbox(page).locator("section.list");
  // Town hall is read already, so opening it changes nothing other tests count.
  const item = inbox(page).getByRole("button", { name: /Ana Ferreira/ });
  await expect(item).toBeVisible();
  await expect(inbox(page).getByRole("navigation", { name: en.folders }).getByRole("link", { name: en.newMessage })).toBeVisible();
  await expect(inbox(page).getByRole("region", { name: en.reading })).toBeHidden();
  await fitsTheScreen(page);
  await page.screenshot({ path: "test-results/phone-inbox.png" });

  await item.click();
  const reader = inbox(page).getByRole("region", { name: "Town hall tonight: talking points" });
  await expect(reader).toContainText("Three points for tonight.");
  await expect(list).toBeHidden();
  // The thread takes the app's whole width (less its margins).
  const host = await inbox(page).boundingBox();
  const box = await reader.boundingBox();
  expect(box!.width).toBeGreaterThan(host!.width - 40);
  const back = reader.getByRole("button", { name: en.back });
  await expect(back).toBeVisible();
  expect((await back.boundingBox())!.height).toBeGreaterThanOrEqual(44);
  await fitsTheScreen(page);
  await page.screenshot({ path: "test-results/phone-thread.png" });

  await back.click();
  await expect(item).toBeVisible();
  await expect(reader).toBeHidden();
  await expect(page).toHaveURL(/\/mail$/);
});

test("on a phone a thread with an attachment and the Arabic inbox fit the screen", async ({ page }) => {
  await page.goto("/mail");
  // Opened by address, so it stays unread for the inbox tests.
  const id = await page.evaluate(async () => {
    const r = await fetch("/api/v1/mail/web/jmap", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Requested-With": "ortus" },
      body: JSON.stringify({ using: ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], methodCalls: [["Email/query", { filter: { text: "clause 4" } }, "q"], ["Email/get", { "#ids": { resultOf: "q", name: "Email/query", path: "/ids" }, properties: ["threadId"] }, "g"]] }),
    });
    return (await r.json()).methodResponses[1][1].list[0].threadId as string;
  });
  expect(id).toBeTruthy();
  await page.route("**/api/v1/mail/web/jmap", async (route) => {
    // Reading is not marking: the seen flag would change what other tests see.
    const body = route.request().postData() ?? "";
    if (body.includes("keywords/$seen")) await route.fulfill({ json: { methodResponses: [["Email/set", { updated: {} }, "s"]], sessionState: "0" } });
    else await route.continue();
  });
  await page.goto(`/mail?thread=${encodeURIComponent(id)}`);
  const reader = inbox(page).getByRole("region", { name: "Re: Gauge report and updated shelter agreement" });
  await expect(reader.getByRole("link", { name: /gauge-report\.pdf/ })).toBeVisible();
  await fitsTheScreen(page);
  await page.screenshot({ path: "test-results/phone-thread-files.png", fullPage: true });

  await page.unroute("**/api/v1/mail/web/jmap");
  await page.addInitScript(() => localStorage.setItem("ortus.locale", "ar"));
  await page.goto("/mail");
  await expect(inbox(page).getByRole("navigation", { name: ar.folders as string })).toBeVisible();
  await expect(inbox(page).getByRole("button", { name: /Ana Ferreira/ })).toBeVisible();
  await fitsTheScreen(page);
  await page.screenshot({ path: "test-results/phone-inbox-ar.png" });
});

test("on a phone Compose and the mail admin screens fit the screen", async ({ page }) => {
  await page.goto("/mail/compose");
  await expect(page.locator("ortus-mail-compose").getByRole("button", { name: en.send, exact: true })).toBeVisible();
  await fitsTheScreen(page);
  await page.screenshot({ path: "test-results/phone-compose.png" });
  for (const path of ["/admin/addresses", "/admin/domains", "/admin/deliverability", "/admin/migrations"]) {
    await page.goto(path);
    await page.waitForLoadState("networkidle");
    await fitsTheScreen(page);
    // The page scrolls inside the shell, so a tall window shows all of it.
    await page.setViewportSize({ width: 390, height: 2000 });
    await page.screenshot({ path: `test-results/phone${path.replaceAll("/", "-")}.png` });
    await page.setViewportSize({ width: 390, height: 844 });
  }
});
