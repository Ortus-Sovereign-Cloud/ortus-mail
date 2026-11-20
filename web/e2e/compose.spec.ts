// Compose (board WebCompose) in six languages, and its flows: suggestions,
// a draft saved as one writes, an attachment, undo send, send later and
// calling it back, the label policy asking for a reason; snooze.
import { expect, test, type Page } from "@playwright/test";
import ar from "../src/locales/ar.js";
import de from "../src/locales/de.js";
import en from "../src/locales/en.js";
import fr from "../src/locales/fr.js";
import pl from "../src/locales/pl.js";
import uk from "../src/locales/uk.js";

const compose = (page: Page) => page.locator("ortus-mail-compose");
const inbox = (page: Page) => page.locator("ortus-mail-inbox");

for (const [code, texts, dir] of [["en", en, "ltr"], ["pl", pl, "ltr"], ["uk", uk, "ltr"], ["de", de, "ltr"], ["fr", fr, "ltr"], ["ar", ar, "rtl"]] as const) {
  test(`Compose opens in ${code}`, async ({ page }) => {
    const errors: string[] = [];
    page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
    await page.addInitScript((c) => localStorage.setItem("ortus.locale", c), code);
    await page.goto("/mail/compose");
    await expect(compose(page).getByRole("combobox", { name: texts.to as string })).toBeVisible();
    await expect(compose(page).getByRole("textbox", { name: texts.subject as string })).toBeVisible();
    await expect(compose(page).getByRole("button", { name: texts.send as string, exact: true })).toBeDisabled();
    await expect(compose(page).getByRole("heading", { name: texts.people as string })).toBeVisible();
    await expect(compose(page).getByRole("heading", { name: texts.delivery as string })).toBeVisible();
    await expect(compose(page).getByRole("combobox", { name: texts.label as string })).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute("dir", dir);
    expect(errors).toEqual([]);
  });
}

test.describe("flows", () => {
  test.describe.configure({ mode: "serial" });

  test("write to someone suggested, attach, send, and undo", async ({ page }) => {
    await page.goto("/mail");
    await inbox(page).getByRole("link", { name: en.newMessage }).click();
    const to = compose(page).getByRole("combobox", { name: en.to });
    await to.fill("anna");
    await compose(page).getByRole("option", { name: /Anna Kowalska/ }).click();
    await expect(compose(page).getByRole("region", { name: en.people })).toContainText("outside · regional.example");
    await compose(page).getByRole("textbox", { name: en.subject }).fill("Shelter agreement review: pick a time");
    await compose(page).getByRole("textbox", { name: en.message }).fill("Hi both,\n\nLet's do a 30-minute review of the shelter agreement changes.");
    await expect(compose(page).getByText(en.draftSaved)).toBeVisible();
    await compose(page).locator('input[type="file"]').setInputFiles({ name: "agenda.txt", mimeType: "text/plain", buffer: Buffer.from("1. Clause 4\n") });
    await expect(compose(page).getByLabel(en.attachments)).toContainText("agenda.txt");
    await expect(compose(page).getByLabel(en.attachments)).not.toContainText(en.uploading);
    await page.screenshot({ path: "test-results/compose.png" });
    await compose(page).getByRole("button", { name: en.send, exact: true }).click();
    const toast = page.locator("ortus-mail-app").getByRole("status").filter({ hasText: "Sent to 1 person" });
    await expect(toast).toBeVisible();
    await toast.getByRole("button", { name: /Undo/ }).click();
    await expect(compose(page).getByRole("textbox", { name: en.subject })).toHaveValue("Shelter agreement review: pick a time");
    await expect(compose(page).getByLabel(en.attachments)).toContainText("agenda.txt");
  });

  test("send later waits in Scheduled and can be called back", async ({ page }) => {
    await page.goto("/mail/compose");
    await compose(page).getByRole("combobox", { name: en.to }).fill("daniel@hydro.example");
    await compose(page).getByRole("combobox", { name: en.to }).press("Enter");
    await compose(page).getByRole("textbox", { name: en.subject }).fill("Briefing notes");
    await compose(page).getByRole("textbox", { name: en.message }).fill("Notes for Thursday.");
    await compose(page).getByRole("button", { name: /Send tomorrow/ }).click();
    await expect(page.locator("ortus-mail-app").getByRole("status").filter({ hasText: "Scheduled for" })).toBeVisible();
    await inbox(page).getByRole("navigation").getByRole("button", { name: en.scheduled }).click();
    await inbox(page).getByRole("button", { name: /Briefing notes/ }).click();
    const reader = inbox(page).getByRole("region", { name: "Briefing notes" });
    await expect(reader.getByRole("status")).toContainText("Goes");
    await reader.getByRole("button", { name: en.cancelSending }).click();
    await expect(compose(page).getByRole("textbox", { name: en.subject })).toHaveValue("Briefing notes");
  });

  test("a label that needs a reason asks for one before sending", async ({ page }) => {
    await page.goto("/mail/compose");
    await compose(page).getByRole("combobox", { name: en.to }).fill("anna@regional.example");
    await compose(page).getByRole("combobox", { name: en.to }).press("Enter");
    await compose(page).getByRole("textbox", { name: en.subject }).fill("Clause 4");
    await compose(page).getByRole("combobox", { name: en.label }).selectOption("internal");
    const warning = compose(page).getByRole("status").filter({ hasText: "needs a reason" });
    await expect(warning).toContainText("anna@regional.example");
    await expect(compose(page).getByRole("button", { name: en.send, exact: true })).toBeDisabled();
    await compose(page).getByRole("textbox", { name: en.reason }).fill("Anna drafted the agreement with us");
    await expect(compose(page).getByRole("button", { name: en.send, exact: true })).toBeEnabled();
  });

  test("snooze takes a thread away until later, and it can come back now", async ({ page }) => {
    await page.goto("/mail?tab=other");
    await inbox(page).getByRole("button", { name: /River Authority/ }).click();
    const reader = inbox(page).getByRole("region", { name: "Gauge at the bridge passed 4.2 m" });
    await reader.getByRole("button", { name: en.snooze }).click();
    await reader.getByRole("menuitem", { name: new RegExp(en.tomorrow) }).click();
    await expect(inbox(page).getByRole("button", { name: /River Authority/ })).toHaveCount(0);
    await inbox(page).getByRole("navigation").getByRole("button", { name: en.snoozed }).click();
    await inbox(page).getByRole("button", { name: /River Authority/ }).click();
    await expect(reader.getByRole("status")).toContainText("Snoozed until");
    await reader.getByRole("button", { name: en.unsnooze }).click();
    await inbox(page).getByRole("navigation").getByRole("button", { name: en.inbox }).click();
    await inbox(page).getByRole("tab", { name: en.tabOther }).click();
    await expect(inbox(page).getByRole("button", { name: /River Authority/ })).toBeVisible();
  });

  test("a draft opens in Compose from Drafts", async ({ page }) => {
    await page.goto("/mail?box=drafts");
    await inbox(page).getByRole("button", { name: /Briefing notes/ }).click();
    await expect(compose(page).getByRole("textbox", { name: en.subject })).toHaveValue("Briefing notes");
  });
});

test("without the calendar app, Compose offers no times", async ({ page }) => {
  const errors: string[] = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  await page.goto("/mail/compose");
  const to = compose(page).getByRole("combobox", { name: en.to });
  await to.fill("anna@regional.example");
  await to.press("Enter");
  await expect(compose(page).getByRole("button", { name: en.timesThatWork })).toHaveCount(0);
  expect(errors).toEqual([]);
});
