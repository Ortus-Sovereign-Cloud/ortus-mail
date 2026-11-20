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
