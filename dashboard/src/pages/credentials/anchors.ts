// Stable ids for the credentials, claude-cli entries and provider keys on
// the Credentials page, so other pages can link straight to one:
// `/credentials#credential-…` opens it, scrolls to it and focuses its
// heading once the list has loaded.

import { useEffect, useRef } from "react";
import { useLocation } from "react-router";

/**
 * `text` as a part of an HTML id and a URL fragment, one to one: letters,
 * digits, dots and dashes as they are, anything else as `_<hex>_`.
 */
function idPart(text: string): string {
  return text.replace(
    /[^A-Za-z0-9.-]/gu,
    (char) => `_${(char.codePointAt(0) ?? 0).toString(16)}_`,
  );
}

/** The anchor of a credential, by its `id`. */
export function credentialAnchor(id: string): string {
  return `credential-${idPart(id)}`;
}

/** The anchor of a claude-cli entry, by its name. */
export function claudeCliAnchor(name: string): string {
  return `claude-cli-${idPart(name)}`;
}

/** The anchor of a provider API key, by its provider and `auth-index`. */
export function providerKeyAnchor(provider: string, authIndex: string): string {
  return `key-${idPart(provider)}-${idPart(authIndex)}`;
}

/** The anchor the address points at, without the `#`; "" for none. */
export function useAddressAnchor(): string {
  const { hash } = useLocation();
  const raw = hash.startsWith("#") ? hash.slice(1) : hash;
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

/**
 * Once `anchor`'s element is shown, scrolls to it and focuses its heading
 * (the element marked `data-anchor-heading`), once for each anchor. Null
 * while there is nothing of this list's to show.
 */
export function useFocusAnchor(anchor: string | null) {
  const done = useRef<string | null>(null);
  useEffect(() => {
    if (anchor === null || done.current === anchor) {
      return;
    }
    const element = document.getElementById(anchor);
    if (element === null) {
      return;
    }
    done.current = anchor;
    element.scrollIntoView({ block: "start" });
    const heading = element.querySelector<HTMLElement>("[data-anchor-heading]") ?? element;
    heading.focus({ preventScroll: true });
  }, [anchor]);
}
