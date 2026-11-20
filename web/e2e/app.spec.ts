// The mail app's client loads at /mail inside the shell, in six languages.
import { expect, test } from "@playwright/test";
import ar from "../src/locales/ar.js";
import de from "../src/locales/de.js";
import en from "../src/locales/en.js";
import fr from "../src/locales/fr.js";
import pl from "../src/locales/pl.js";
import uk from "../src/locales/uk.js";

const LOCALES = [
  { code: "en", dir: "ltr", texts: en },
  { code: "pl", dir: "ltr", texts: pl },
  { code: "uk", dir: "ltr", texts: uk },
  { code: "de", dir: "ltr", texts: de },
  { code: "fr", dir: "ltr", texts: fr },
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
    await expect(page.locator("ortus-mail-app").getByRole("heading", { level: 1, name: locale.texts.inbox })).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute("dir", locale.dir);
    await expect(page.locator("ortus-shell nav a[href=\"/mail\"]")).toHaveAttribute("aria-current", "page");
    expect(errors).toEqual([]);
  });
}
