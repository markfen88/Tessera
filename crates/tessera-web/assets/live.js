/* Status pages remain server rendered. Poll the same authenticated GET and
 * reconcile its content in place; forms still use their normal CSRF POSTs. */
(() => {
  "use strict";
  const content = document.querySelector("#live-content[data-live-interval]");
  if (!content) return;
  const interval = Number(content.dataset.liveInterval) * 1000;
  if (!(interval > 0)) return;
  const status = document.querySelector("[data-live-status]");
  const normalStatus = status?.textContent;
  const dirty = new WeakSet();
  let timer, request, stopped = false, submitting = false, pointerDown = false;
  let delay = interval;

  for (const event of ["input", "change"]) {
    document.addEventListener(event, e => {
      if (e.target.form) dirty.add(e.target.form);
    });
  }
  document.addEventListener("reset", e => {
    // The default reset happens after the event.
    setTimeout(() => dirty.delete(e.target), 0);
  });
  document.addEventListener("submit", e => {
    // A response from before a POST must never overwrite its result.
    submitting = true;
    request?.abort();
    setTimeout(() => {
      if (e.defaultPrevented) { submitting = false; schedule(); }
    }, 0);
  });
  document.addEventListener("pointerdown", () => { pointerDown = true; });
  for (const event of ["pointerup", "pointercancel"]) {
    document.addEventListener(event, () => { pointerDown = false; });
  }
  window.addEventListener("blur", () => { pointerDown = false; });

  function protectedNode(node) {
    return node.nodeType === Node.ELEMENT_NODE && (
      node === document.activeElement || node.contains(document.activeElement) ||
      (node.matches("form") && dirty.has(node)) ||
      [...node.querySelectorAll("form")].some(form => dirty.has(form))
    );
  }

  function key(node) {
    if (node.nodeType !== Node.ELEMENT_NODE) return null;
    if (node.id) return `id:${node.id}`;
    if (node.hasAttribute("data-live-key")) return `key:${node.dataset.liveKey}`;
    // Forms can appear/disappear when services or EQ configurations change.
    // Their action and hidden identity distinguish them, including uploads.
    if (node.matches("form")) return JSON.stringify([
      node.getAttribute("action"),
      [...node.querySelectorAll('input[type="hidden"]')]
        .filter(input => !["csrf", "action"].includes(input.name))
        .map(input => [input.name, input.value])
    ]);
    return null;
  }

  function compatible(a, b) {
    return a.nodeType === b.nodeType && a.nodeName === b.nodeName && key(a) === key(b);
  }

  function patch(current, next) {
    if (current.isEqualNode(next)) return;
    if (current.nodeType !== Node.ELEMENT_NODE) {
      if (current.nodeValue !== next.nodeValue) current.nodeValue = next.nodeValue;
      return;
    }
    // Keep the whole form coherent while the operator edits or focuses it.
    if (current.matches("form") && protectedNode(current)) return;
    for (const attr of [...current.attributes]) {
      if (current.matches("details") && attr.name === "open") continue;
      if (!next.hasAttribute(attr.name)) current.removeAttribute(attr.name);
    }
    for (const attr of next.attributes) {
      if (current.matches("details") && attr.name === "open") continue;
      if (current.getAttribute(attr.name) !== attr.value) current.setAttribute(attr.name, attr.value);
    }
    // Prune removed identities first: a deletion before a focused/dirty
    // sibling is not a reorder and must not leave stale action buttons.
    const identities = new Set([...next.childNodes].map(key).filter(k => k !== null));
    for (const node of [...current.childNodes]) {
      const identity = key(node);
      if (identity !== null && !identities.has(identity) && !protectedNode(node)) node.remove();
    }
    const old = [...current.childNodes];
    const used = new Set();
    let cursor = current.firstChild;
    for (const child of next.childNodes) {
      const identity = key(child);
      let match = identity === null
        ? (cursor && !used.has(cursor) && compatible(cursor, child) ? cursor : null)
        : old.find(node => !used.has(node) && compatible(node, child));
      if (!match) {
        match = child.cloneNode(true);
        current.insertBefore(match, cursor);
      } else {
        if (match !== cursor) {
          // Moving a focused subtree can blur it. Defer structural changes.
          if (protectedNode(current)) return;
          current.insertBefore(match, cursor);
        }
        patch(match, child);
      }
      used.add(match);
      cursor = match.nextSibling;
    }
    for (const node of old) {
      if (!used.has(node) && !protectedNode(node)) node.remove();
    }
    // Attributes alone do not update a previously edited control's property.
    if (current.matches("input") && current.type !== "file") {
      current.value = next.value;
      current.checked = next.checked;
    } else if (current.matches("select, textarea")) current.value = next.value;
  }

  function schedule(ms = delay) {
    clearTimeout(timer);
    if (!stopped && !document.hidden && !submitting) timer = setTimeout(poll, ms);
  }

  async function poll() {
    if (request || stopped || document.hidden || submitting) return;
    request = new AbortController();
    const timeout = setTimeout(() => request?.abort(), 15000);
    try {
      const response = await fetch(location.pathname + location.search, {
        credentials: "same-origin", cache: "no-store", redirect: "manual",
        headers: { "X-Tessera-Live": "1" }, signal: request.signal
      });
      // Authentication is the only redirect from a live status route.
      // Navigate once to sign in; never silently display stale audio state.
      if (response.type === "opaqueredirect" || [401, 303].includes(response.status)) {
        stopped = true;
        if (status) status.textContent = "Session expired · sign in again";
        location.assign("/login");
        return;
      }
      if (!response.ok) throw new Error("Status unavailable");
      const page = new DOMParser().parseFromString(await response.text(), "text/html");
      const next = page.querySelector("#live-content[data-live-interval]");
      if (!next) throw new Error("Status unavailable");
      if (!stopped && !document.hidden && !submitting && !pointerDown) {
        patch(content, next);
        // No navigation, scrolling, innerHTML replacement or focus changes.
        const clock = document.querySelector(".foot [data-live-clock]");
        const nextClock = page.querySelector(".foot [data-live-clock]");
        if (clock && nextClock) clock.textContent = nextClock.textContent;
      }
      delay = interval;
      if (status) status.textContent = normalStatus;
    } catch (_) {
      if (!stopped && !document.hidden && !submitting) {
        if (status) status.textContent = "Live updates paused · retrying";
        delay = Math.min(delay * 2, 60000);
      }
    } finally {
      clearTimeout(timeout);
      request = null;
      schedule();
    }
  }

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) { clearTimeout(timer); request?.abort(); }
    else schedule(0);
  });
  window.addEventListener("pagehide", () => {
    stopped = true;
    clearTimeout(timer);
    request?.abort();
  });
  window.addEventListener("pageshow", () => {
    stopped = false;
    submitting = false;
    schedule();
  });
  schedule();
})();
