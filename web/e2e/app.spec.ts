// The mail app's client loads at /mail inside the shell, in every language.
import { expect, test } from "@playwright/test";
import ar from "../src/locales/ar.js";
import de from "../src/locales/de.js";
import en from "../src/locales/en.js";
import es from "../src/locales/es.js";
import fr from "../src/locales/fr.js";
import it from "../src/locales/it.js";
import pl from "../src/locales/pl.js";
import uk from "../src/locales/uk.js";

const LOCALES = [
  { code: "en", dir: "ltr", texts: en },
  { code: "pl", dir: "ltr", texts: pl },
  { code: "uk", dir: "ltr", texts: uk },
  { code: "de", dir: "ltr", texts: de },
  { code: "fr", dir: "ltr", texts: fr },
  { code: "es", dir: "ltr", texts: es },
  { code: "it", dir: "ltr", texts: it },
  { code: "ar", dir: "rtl", texts: ar },
] as const;

/** Fails the test on anything the browser complains about (a blocked script, a 404). */
function watchForErrors(page: import("@playwright/test").Page): string[] {
  const errors: string[] = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));
  return errors;
}

for (const locale of LOCALES) {
  test(`the inbox opens at /mail in ${locale.code}`, async ({ page }) => {
    const errors = watchForErrors(page);
    await page.addInitScript((code) => localStorage.setItem("ortus.locale", code), locale.code);
    await page.goto("/mail");
    const inbox = page.locator("ortus-mail-inbox");
    await expect(inbox.getByRole("heading", { level: 1, name: locale.texts.inbox as string })).toBeAttached();
    await expect(inbox.getByRole("navigation").getByRole("button", { name: locale.texts.inbox as string })).toHaveAttribute("aria-current", "page");
    await expect(inbox.getByRole("tab", { name: locale.texts.tabImportant as string })).toHaveAttribute("aria-selected", "true");
    await expect(inbox.getByRole("button", { name: /Anna Kowalska/ })).toBeVisible();
    await expect(inbox.getByRole("searchbox", { name: locale.texts.searchMail as string })).toBeVisible();
    await expect(inbox.getByRole("link", { name: locale.texts.newMessage as string })).toBeVisible();
    await expect(inbox.getByRole("heading", { name: locale.texts.agendaToday as string })).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute("dir", locale.dir);
    await expect(page.locator("ortus-shell nav a[href=\"/mail\"]")).toHaveAttribute("aria-current", "page");
    if (locale.code === "ar") await page.screenshot({ path: "test-results/inbox-ar.png" });
    expect(errors).toEqual([]);
  });
}
