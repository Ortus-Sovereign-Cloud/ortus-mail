// Admin › Domains: add a domain, publish its records (in the test DNS
// server's zone), watch the checks turn green; in every language.
import { writeFileSync } from "node:fs";
import { expect, test, type Page } from "@playwright/test";
import ar from "../src/locales/ar.js";
import de from "../src/locales/de.js";
import en from "../src/locales/en.js";
import es from "../src/locales/es.js";
import fr from "../src/locales/fr.js";
import it from "../src/locales/it.js";
import pl from "../src/locales/pl.js";
import uk from "../src/locales/uk.js";

test.describe.configure({ mode: "serial" });

const zone: Record<string, unknown[]> = {};
function publish(name: string, type: string, data: unknown[]): void {
  zone[`${name} ${type}`] = data;
  writeFileSync(new URL("./zone.json", import.meta.url), JSON.stringify(zone));
}

async function recordValue(page: Page, domain: string, purpose: string): Promise<{ name: string; value: string }> {
  const shown = await (await page.request.get(`/api/v1/mail/admin/domains/${domain}`)).json();
  return shown.records.find((r: { purpose: string }) => r.purpose === purpose);
}

test("the owner adds a domain, publishes its records and gets it verified", async ({ page }) => {
  await page.goto("/admin/domains");
  await expect(page.getByRole("heading", { level: 1, name: en.domains })).toBeVisible();
  await expect(page.getByText(en.noDomains)).toBeVisible();
  await page.getByRole("button", { name: en.addDomain }).click();
  await page.getByLabel(en.domainName, { exact: true }).fill("ministry.example");
  await page.getByRole("button", { name: en.add, exact: true }).click();
  await expect(page.getByRole("status")).toHaveText(en.domainAdded);
  const tab = page.getByRole("tab", { name: /ministry\.example/ });
  await expect(tab).toContainText(en.stateWaiting);
  await expect(page.getByRole("heading", { name: en.recordsFor.replace("{domain}", "ministry.example") })).toBeVisible();

  // The records, published as shown (DMARC still at p=none).
  const dkim = await recordValue(page, "ministry.example", "dkim_rsa");
  publish("ministry.example", "MX", [{ preference: 10, exchange: "mail.ministry.example" }]);
  publish("ministry.example", "TXT", [["v=spf1 mx -all"]]);
  publish(`${dkim.name}.ministry.example`, "TXT", [dkim.value.match(/.{1,255}/g)]);
  publish("_dmarc.ministry.example", "TXT", [["v=DMARC1; p=none"]]);
  await page.getByRole("button", { name: en.checkNow }).click();
  await expect(page.getByRole("status")).toHaveText(en.checked);
  await expect(tab).toContainText("Needs 1 fix");
  await expect(page.getByRole("note")).toHaveText(en.dmarcAdvice);
  await expect(page.getByRole("row", { name: new RegExp(en.purpose_receive) })).toContainText(en.statusOk);

  // DMARC to quarantine: verified.
  publish("_dmarc.ministry.example", "TXT", [["v=DMARC1; p=quarantine"]]);
  await page.getByRole("button", { name: en.checkNow }).click();
  await expect(tab).toContainText(en.stateVerified);
  await expect(page.getByRole("note")).toHaveCount(0);

  const download = page.waitForEvent("download");
  await page.getByRole("link", { name: en.exportZone }).click();
  expect((await download).suggestedFilename()).toBe("ministry.example.zone");
});

for (const [code, texts, dir] of [["en", en, "ltr"], ["pl", pl, "ltr"], ["uk", uk, "ltr"], ["de", de, "ltr"], ["fr", fr, "ltr"], ["es", es, "ltr"], ["it", it, "ltr"], ["ar", ar, "rtl"]] as const) {
  test(`the domains page is in ${code}`, async ({ page }) => {
    await page.addInitScript((c) => localStorage.setItem("ortus.locale", c), code);
    await page.goto("/admin/domains");
    await expect(page.getByRole("heading", { level: 1, name: texts.domains as string })).toBeVisible();
    await expect(page.getByRole("link", { name: texts.domains as string })).toHaveAttribute("aria-current", "page");
    await expect(page.locator("html")).toHaveAttribute("dir", dir);
  });
}

test("screenshot of the domains page for comparing with the board", async ({ page }) => {
  await page.goto("/admin/domains");
  await expect(page.getByRole("tab", { name: /ministry\.example/ })).toBeVisible();
  await page.screenshot({ path: "test-results/domains.png" });
});
