// Admin › Deliverability (AdminDeliverability): a DMARC report arrives by
// mail and shows who sends as our domain; the numbers of mail sent; in six
// languages.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { expect, test } from "@playwright/test";
import ar from "../src/locales/ar.js";
import de from "../src/locales/de.js";
import en from "../src/locales/en.js";
import es from "../src/locales/es.js";
import fr from "../src/locales/fr.js";
import it from "../src/locales/it.js";
import pl from "../src/locales/pl.js";
import uk from "../src/locales/uk.js";
import { deliver } from "./smtp.js";

const smtpPort = Number(process.env.ORTUS_E2E_PORT ?? 7980) + 1;

function report(): string {
  const now = Math.floor(Date.now() / 1000);
  const row = (ip: string, count: number, dkim: string, spf: string) =>
    `<record><row><source_ip>${ip}</source_ip><count>${count}</count><policy_evaluated><disposition>none</disposition><dkim>${dkim}</dkim><spf>${spf}</spf></policy_evaluated></row><identifiers><header_from>ministry.example</header_from></identifiers></record>`;
  const xml = `<?xml version="1.0"?><feedback><report_metadata><org_name>google.com</org_name><report_id>e2e-${now}</report_id><date_range><begin>${now - 86400}</begin><end>${now}</end></date_range></report_metadata><policy_published><domain>ministry.example</domain><p>none</p></policy_published>${row("198.51.100.20", 512, "pass", "pass")}${row("203.0.113.9", 16, "fail", "pass")}${row("192.0.2.200", 231, "fail", "fail")}</feedback>`;
  const attachment = gzipSync(Buffer.from(xml)).toString("base64").replace(/.{76}/g, "$&\n");
  return `From: noreply-dmarc-support@google.com\nTo: dmarc-reports@ministry.example\nSubject: Report domain: ministry.example\nMessage-ID: <e2e-${now}@google.com>\nMIME-Version: 1.0\nContent-Type: multipart/mixed; boundary="b"\n\n--b\nContent-Type: text/plain\n\nAggregate report.\n--b\nContent-Type: application/gzip; name="google.com!ministry.example.xml.gz"\nContent-Disposition: attachment\nContent-Transfer-Encoding: base64\n\n${attachment}\n--b--\n`;
}

/** Adds records to the test DNS zone (the domains test wrote the rest). */
function publish(records: Record<string, unknown[]>): void {
  const file = new URL("./zone.json", import.meta.url);
  const zone = existsSync(file) ? JSON.parse(readFileSync(file, "utf8")) : {};
  writeFileSync(file, JSON.stringify({ ...zone, ...records }));
}

test("a DMARC report shows who sends as the domain", async ({ page }) => {
  // Reports are read only from who they say they are from.
  publish({ "google.com TXT": [["v=spf1 ip4:127.0.0.1 -all"]], "_dmarc.google.com TXT": [["v=DMARC1; p=reject"]] });
  const reply = await deliver(smtpPort, "noreply-dmarc-support@google.com", "dmarc-reports@ministry.example", report());
  expect(reply).toMatch(/^250/);
  // It is read after the reply; the page shows it once it is in.
  await expect.poll(async () => JSON.stringify(await (await page.request.get("/api/v1/mail/admin/deliverability?days=14")).json()), { timeout: 10_000 }).toContain("231");
  await page.goto("/admin/deliverability");
  const screen = page.locator("ortus-mail-deliverability");
  await expect(screen.getByRole("heading", { level: 1, name: en.deliverability })).toBeVisible();
  const senders = screen.getByRole("region", { name: "Who sends as ministry.example" });
  await expect(senders).toContainText("512");
  await expect(senders.getByRole("listitem").filter({ hasText: "231" })).toContainText(en.senderSpoofing);
  await expect(senders.getByRole("listitem").filter({ hasText: "16" })).toContainText(en.senderDkimFail);
  await screen.getByRole("button", { name: en.period90d }).click();
  await expect(screen.getByRole("button", { name: en.period90d })).toHaveAttribute("aria-pressed", "true");
  await expect(screen.getByRole("heading", { name: en.outboundPerDay })).toBeVisible();
  await screen.getByRole("button", { name: en.period24h }).click();
  await expect(screen.getByRole("heading", { name: en.outboundPerHour })).toBeVisible();
  await page.screenshot({ path: "test-results/deliverability.png" });
});

for (const [code, texts, dir] of [["en", en, "ltr"], ["pl", pl, "ltr"], ["uk", uk, "ltr"], ["de", de, "ltr"], ["fr", fr, "ltr"], ["es", es, "ltr"], ["it", it, "ltr"], ["ar", ar, "rtl"]] as const) {
  test(`the deliverability page is in ${code}`, async ({ page }) => {
    const errors: string[] = [];
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    await page.addInitScript((c) => localStorage.setItem("ortus.locale", c), code);
    await page.goto("/admin/deliverability");
    const screen = page.locator("ortus-mail-deliverability");
    await expect(screen.getByRole("heading", { level: 1, name: texts.deliverability as string })).toBeVisible();
    for (const key of ["cardDelivered", "cardSent", "cardBounced", "cardQueue"] as const) {
      await expect(screen.getByRole("heading", { name: texts[key] as string, exact: true })).toBeVisible();
    }
    await expect(page.locator("ortus-admin nav").getByRole("link", { name: texts.deliverability as string })).toHaveAttribute("aria-current", "page");
    await expect(page.locator("html")).toHaveAttribute("dir", dir);
    expect(errors).toEqual([]);
  });
}
