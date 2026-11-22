// Admin › Addresses: an administrator gives a person another address in
// one of the mail domains; the person can send from it; it can be taken
// away again.
import { expect, test } from "@playwright/test";
import en from "../src/locales/en.js";

test("an address is given to a person, used in From, and taken away", async ({ page }) => {
  const errors: string[] = [];
  // The refused duplicate below answers 409 on purpose.
  page.on("console", (m) => m.type() === "error" && !m.text().includes("409") && errors.push(m.text()));
  await page.goto("/admin/addresses");
  const admin = page.locator("ortus-mail-addresses");
  await expect(admin.getByRole("heading", { level: 1, name: en.addresses })).toBeVisible();
  await admin.getByRole("button", { name: en.addAddress }).click();
  const dialog = admin.getByRole("dialog", { name: en.addAddress });
  await dialog.getByLabel(en.addressPerson).selectOption({ label: await dialog.getByLabel(en.addressPerson).locator("option", { hasText: "olena@ministry.example" }).innerText() });
  await dialog.getByLabel(en.address, { exact: true }).fill("press");
  await dialog.getByLabel(en.domainName, { exact: true }).selectOption("ministry.example");
  await page.screenshot({ path: "test-results/addresses-dialog.png" });
  await dialog.getByRole("button", { name: en.add, exact: true }).click();
  await expect(admin.getByRole("status")).toContainText("press@ministry.example");
  await expect(admin.getByRole("row", { name: /press@ministry\.example/ })).toBeVisible();

  // The same address again: refused, and the dialog says why.
  await admin.getByRole("button", { name: en.addAddress }).click();
  await dialog.getByLabel(en.address, { exact: true }).fill("press");
  await dialog.getByRole("button", { name: en.add, exact: true }).click();
  await expect(dialog.getByRole("alert")).toContainText("already belongs");
  await dialog.getByRole("button", { name: en.cancel }).click();

  // Olena can send from it.
  await page.goto("/mail/compose");
  await expect(page.locator("ortus-mail-compose").locator("#from option", { hasText: "press@ministry.example" })).toHaveCount(1);

  // Taken away again.
  await page.goto("/admin/addresses");
  page.once("dialog", (d) => void d.accept());
  await admin.getByRole("button", { name: en.addressRemoveNamed.replace("{address}", "press@ministry.example") }).click();
  await expect(admin.getByRole("status")).toContainText(en.addressRemoved.replace("{address}", "press@ministry.example"));
  await expect(admin.getByText(en.addressesEmpty)).toBeVisible();
  expect(errors).toEqual([]);
});
