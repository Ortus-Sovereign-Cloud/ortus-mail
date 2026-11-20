import { call } from "ortus-kit";
// Moving between the mail app's screens without reloading, and the toast
// that outlives the screen that raised it (sent, with undo).

export interface Toast {
  text: string;
  /** A sent message that can still be called back. */
  undo?: { submission: string; draft: string };
  seconds?: number;
}

export function navigate(url: string): void {
  history.pushState(null, "", url);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

export function toast(detail: Toast): void {
  window.dispatchEvent(new CustomEvent<Toast>("ortus-mail-toast", { detail }));
}

let appsCheck: Promise<string[]> | undefined;

/** The apps installed and on (asked once). */
function installedApps(): Promise<string[]> {
  appsCheck ??= call<{ apps: { id: string }[] }>("GET", "/apps")
    .then((r) => r.apps.map((a) => a.id))
    .catch(() => []);
  return appsCheck;
}

/** Whether the calendar app is installed and on. */
export function calendarInstalled(): Promise<boolean> {
  return installedApps().then((ids) => ids.includes("calendar"));
}

/** Whether the drive app is installed and on. */
export function driveInstalled(): Promise<boolean> {
  return installedApps().then((ids) => ids.includes("drive"));
}
