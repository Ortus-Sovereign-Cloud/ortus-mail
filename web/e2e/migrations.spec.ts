// Admin › Migrations: an import is started, its failure is shown with its
// reason and retried, the owner is asked and gives the password on the
// reconnect page; in six languages.
import { expect, test } from "@playwright/test";
import ar from "../src/locales/ar.js";
import de from "../src/locales/de.js";
import en from "../src/locales/en.js";
import fr from "../src/locales/fr.js";
import pl from "../src/locales/pl.js";
import uk from "../src/locales/uk.js";

test("an import fails with its reason, is retried, and the owner is asked", async ({ page }) => {
  await page.goto("/admin/migrations");
  const screen = page.locator("ortus-mail-migrations");
  await expect(screen.getByRole("heading", { level: 1, name: en.migrations })).toBeVisible();
  await screen.getByRole("button", { name: en.newImport }).click();
  // Nothing listens on port 1: the old server cannot be reached.
  await screen.getByLabel(en.oldServer).fill("localhost");
  await screen.getByLabel(en.port, { exact: true }).fill("1");
  await screen.getByLabel(en.accountsToCopy).fill("olena@old.example, old-secret, olena@ministry.example");
  await screen.getByRole("button", { name: en.startImport }).click();
  await expect(screen.getByRole("status")).toHaveText("Copying 1 account.");
  const row = screen.getByRole("row").filter({ hasText: "olena@old.example" });
  await expect(row).toContainText(en.importFailed, { timeout: 15_000 });
  await expect(row).toContainText(en.errConnect);
  await row.getByRole("button", { name: en.retry }).click();
  await expect(screen.getByRole("status")).toHaveText(en.retrying.replace("{login}", "olena@old.example"));
  await expect(row).toContainText(en.importFailed, { timeout: 15_000 });
  await page.screenshot({ path: "test-results/migrations.png" });

  // The owner is asked; the note in her inbox leads to the reconnect page.
  await screen.getByRole("button", { name: en.askOwners }).first().click();
  await expect(screen.getByRole("status")).toHaveText("Asked 1 owner for their password.");
  const mine = await (await page.request.get("/api/v1/mail/imports/mine")).json();
  const account = mine.accounts.find((a: { login: string }) => a.login === "olena@old.example");
  await page.goto(`/mail/reconnect/${account.id}`);
  const reconnect = page.locator("ortus-mail-reconnect");
  await expect(reconnect.getByRole("heading", { name: en.reconnectTitle })).toBeVisible();
  await expect(reconnect).toContainText(en.errConnect);
  await reconnect.getByLabel(en.oldPassword.replace("{host}", "localhost")).fill("new-secret");
  await reconnect.getByRole("button", { name: en.continueCopying }).click();
  // It is tried again (and fails again: there is still no server).
  await expect(reconnect.getByRole("button", { name: en.continueCopying })).toBeVisible({ timeout: 15_000 });
});

for (const [code, texts, dir] of [["en", en, "ltr"], ["pl", pl, "ltr"], ["uk", uk, "ltr"], ["de", de, "ltr"], ["fr", fr, "ltr"], ["ar", ar, "rtl"]] as const) {
  test(`the migrations page is in ${code}`, async ({ page }) => {
    const errors: string[] = [];
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    await page.addInitScript((c) => localStorage.setItem("ortus.locale", c), code);
    await page.goto("/admin/migrations");
    const screen = page.locator("ortus-mail-migrations");
    await expect(screen.getByRole("heading", { level: 1, name: texts.migrations as string })).toBeVisible();
    await screen.getByRole("button", { name: texts.newImport as string }).click();
    await expect(screen.getByLabel(texts.accountsToCopy as string)).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute("dir", dir);
    expect(errors).toEqual([]);
  });
}
