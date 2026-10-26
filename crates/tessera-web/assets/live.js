/* Status pages remain server rendered. Poll the same authenticated GET and
 * reconcile its content in place; forms still use their normal CSRF POSTs.
 *
 * The Speaker page's controls post in place as well (the same forms, the
 * same token, plus the X-Tessera-Live header, which makes the server answer
 * JSON instead of a redirect): sliders marked data-live-slider apply
 * themselves (volume: throttled previews while moving, one commit on
 * release; boost: on release only) and service rows act without leaving
 * the page, asking in the row first for an essential entry. Elements
 * marked data-live-local belong to this script: the background refresh
 * never writes or removes them. Without this script every form still has
 * its Apply button and the server's confirmation page. */
(() => {
  "use strict";
  // Lets the stylesheet hide the Apply buttons the sliders replace.
  document.documentElement.classList.add("js");

  const content = document.querySelector("#live-content[data-live-interval]");
  const interval = content ? Number(content.dataset.liveInterval) * 1000 : 0;
  const status = document.querySelector("[data-live-status]");
  const normalStatus = status?.textContent;
  const dirty = new WeakSet();
  let timer, request, stopped = false, submitting = false, pointerDown = false;
  let delay = interval;
  // In-place posts in flight: a read sampled meanwhile could show the value
  // from before the post, so patching waits for them.
  let busy = 0;
  // Counts posts started and finished: a read that spans a change is stale.
  let changes = 0;

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

  function local(node) {
    return node.nodeType === Node.ELEMENT_NODE && node.hasAttribute("data-live-local");
  }

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
    // This script's own nodes (result lines, confirmations) are not the
    // server's to update.
    if (local(current)) return;
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
      if (identity !== null && !identities.has(identity) && !protectedNode(node) && !local(node)) node.remove();
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
      if (!used.has(node) && !protectedNode(node) && !local(node)) node.remove();
    }
    // Attributes alone do not update a previously edited control's property.
    if (current.matches("input") && current.type !== "file") {
      current.value = next.value;
      current.checked = next.checked;
    } else if (current.matches("select, textarea")) current.value = next.value;
  }

  function schedule(ms = delay) {
    clearTimeout(timer);
    if (interval > 0 && !stopped && !document.hidden && !submitting) timer = setTimeout(poll, ms);
  }

  async function poll() {
    if (request || stopped || document.hidden || submitting) return;
    request = new AbortController();
    const timeout = setTimeout(() => request?.abort(), 15000);
    // A read that overlaps a post may carry the value from before it: drop it and read again.
    const seen = changes;
    let stale = false;
    try {
      const response = await fetch(location.pathname + location.search, {
        credentials: "same-origin", cache: "no-store", redirect: "manual",
        headers: { "X-Tessera-Live": "1" }, signal: request.signal
      });
      // Authentication is the only redirect from a live status route.
      // Navigate once to sign in; never silently display stale audio state.
      if (response.type === "opaqueredirect" || [401, 303].includes(response.status)) {
        expired();
        return;
      }
      if (!response.ok) throw new Error("Status unavailable");
      const page = new DOMParser().parseFromString(await response.text(), "text/html");
      const next = page.querySelector("#live-content[data-live-interval]");
      if (!next) throw new Error("Status unavailable");
      if (changes !== seen) {
        stale = true;
      } else if (!stopped && !document.hidden && !submitting && !pointerDown && busy === 0) {
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
      schedule(stale ? 150 : undefined);
    }
  }

  function expired() {
    stopped = true;
    if (status) status.textContent = "Session expired · sign in again";
    location.assign("/login");
  }

  // A change was just applied: read the page again soon, so the rows and
  // readouts show the server's view of it.
  function refreshSoon() {
    if (request) return;
    schedule(150);
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

  /* ---- posting a form in place --------------------------------------------- */

  // POST `form`'s action with its CSRF token and `fields`; the answer is the
  // server's JSON ({ok, text, ...}), or {ok: false, text} for anything else.
  async function post(form, fields) {
    const body = new URLSearchParams();
    const csrf = form.elements.namedItem("csrf");
    if (csrf) body.set("csrf", csrf.value);
    for (const [k, v] of Object.entries(fields)) body.set(k, String(v));
    busy++;
    changes++;
    try {
      const response = await fetch(form.getAttribute("action"), {
        method: "POST", credentials: "same-origin", cache: "no-store", redirect: "manual",
        headers: { "X-Tessera-Live": "1", "Content-Type": "application/x-www-form-urlencoded" },
        body
      });
      if (response.type === "opaqueredirect" || [401, 303].includes(response.status)) {
        expired();
        return { ok: false, text: "session expired" };
      }
      if ((response.headers.get("content-type") || "").includes("application/json")) {
        return await response.json();
      }
      const text = (await response.text()).trim();
      return { ok: false, text: text || `HTTP ${response.status}` };
    } catch (_) {
      return { ok: false, text: "no answer from the device" };
    } finally {
      busy--;
      changes++;
    }
  }

  const noteTimers = new WeakMap();

  // The form's or row's result line: "applying…", "applied", "failed: why".
  function note(scope, state, text) {
    const n = scope.querySelector(".note[data-live-local]");
    if (!n) return;
    clearTimeout(noteTimers.get(n));
    n.textContent = text;
    n.className = `note ${state}`;
    if (state === "ok") noteTimers.set(n, setTimeout(() => { n.textContent = ""; n.className = "note"; }, 4000));
  }

  /* ---- sliders ------------------------------------------------------------ */

  const PREVIEW_EVERY = 150;   // ms between volume previews while moving
  const COMMIT_SETTLE = 350;   // ms of quiet before a boost commit (keyboard steps coalesce)
  const BOOST_HOT = 12;        // dB: above this the boost is shown as a warning

  // Signed, one decimal, a real minus: as the server formats it.
  function signed(v) {
    const s = Math.abs(v).toFixed(1);
    return v > 0 ? `+${s}` : v < 0 ? `−${s}` : "0.0";
  }

  // The filled part of the track, in the stylesheet's 5 % classes.
  function fillClass(v, min, max) {
    const f = max > min ? Math.min(1, Math.max(0, (v - min) / (max - min))) : 0;
    return `fill-${Math.round(f * 20) * 5}`;
  }

  // Readout, fill and warning state follow the thumb.
  function paint(form, input, v) {
    const big = form.querySelector(".big");
    if (big && big.firstChild && big.firstChild.nodeType === Node.TEXT_NODE) big.firstChild.nodeValue = signed(v);
    input.className = fillClass(v, Number(input.min), Number(input.max));
    input.setAttribute("aria-valuetext", `${signed(v)} dB`);
    if (form.dataset.liveSlider === "boost") form.classList.toggle("hot", v > BOOST_HOT);
  }

  const sliders = new WeakMap();

  function slider(form) {
    let s = sliders.get(form);
    if (!s) {
      s = { pending: null, inflight: false, timer: null, last: 0 };
      sliders.set(form, s);
    }
    return s;
  }

  // One request at a time per form; the newest value wins and a commit is
  // never dropped (a preview that arrives while a commit waits becomes
  // part of that commit).
  function queue(form, value, commit) {
    const s = slider(form);
    s.pending = { value, commit: commit || Boolean(s.pending?.commit) };
    clearTimeout(s.timer);
    s.timer = null;
    const kind = form.dataset.liveSlider;
    if (s.pending.commit) {
      if (kind === "boost") s.timer = setTimeout(() => pump(form), COMMIT_SETTLE);
      else pump(form);
      return;
    }
    const wait = PREVIEW_EVERY - (Date.now() - s.last);
    if (wait <= 0) pump(form);
    else s.timer = setTimeout(() => pump(form), wait);
  }

  async function pump(form) {
    const s = slider(form);
    if (s.inflight || !s.pending) return;
    const { value, commit } = s.pending;
    s.pending = null;
    s.inflight = true;
    s.last = Date.now();
    const kind = form.dataset.liveSlider;
    if (commit) note(form, "busy", "applying…");
    const fields = { db: value.toFixed(1) };
    if (kind === "volume") fields.commit = commit ? "1" : "0";
    const r = await post(form, fields);
    s.inflight = false;
    if (!r.ok) {
      note(form, "bad", `failed: ${r.text}`);
    } else if (commit) {
      note(form, "ok", "applied");
      // The server's view is now the form's: let the refresh show other
      // operators' changes again, and show the snapped value.
      dirty.delete(form);
      const input = form.querySelector('input[type="range"]');
      if (input && !s.pending && typeof r.db === "number") {
        input.value = r.db.toFixed(1);
        paint(form, input, r.db);
      }
      refreshSoon();
    }
    if (s.pending) pump(form);
  }

  function sliderOf(e) {
    const input = e.target;
    const form = input?.form;
    if (!(input instanceof HTMLInputElement) || input.type !== "range") return null;
    if (!form?.hasAttribute("data-live-slider")) return null;
    return { form, input, value: Number(input.value) };
  }

  document.addEventListener("input", e => {
    const t = sliderOf(e);
    if (!t) return;
    paint(t.form, t.input, t.value);
    if (t.form.dataset.liveSlider === "volume") queue(t.form, t.value, false);
  });
  document.addEventListener("change", e => {
    const t = sliderOf(e);
    if (!t) return;
    paint(t.form, t.input, t.value);
    queue(t.form, t.value, true);
  });
  // Enter in the form (the hidden Apply button's implicit submission).
  document.addEventListener("submit", e => {
    const form = e.target;
    if (!(form instanceof HTMLFormElement) || !form.hasAttribute("data-live-slider")) return;
    const input = form.querySelector('input[type="range"]');
    if (!input) return;
    e.preventDefault();
    queue(form, Number(input.value), true);
  });

  /* ---- service rows ------------------------------------------------------- */

  function serviceOf(e) {
    const button = e.target instanceof Element ? e.target.closest("button") : null;
    const form = button?.form;
    const row = form?.closest(".row[data-live-key]");
    if (!button || !form || !row || button.type !== "submit" || button.disabled) return null;
    return { button, form, row };
  }

  async function act(row, form, button, fields) {
    const buttons = [...row.querySelectorAll("button")];
    for (const b of buttons) { b.disabled = true; b.setAttribute("aria-busy", "true"); }
    note(row, "busy", "working…");
    const r = await post(form, fields);
    for (const b of buttons) { b.disabled = false; b.removeAttribute("aria-busy"); }
    if (r.ok) {
      note(row, "ok", r.text);
      // Focus moves to the result (it is announced), not back to the
      // switch: a focused form is protected from the refresh, which has to
      // show the row's new state now.
      const n = row.querySelector(".note[data-live-local]");
      if (n) { n.tabIndex = -1; n.focus(); }
      refreshSoon();
    } else {
      note(row, "bad", `failed: ${r.text}`);
      if (document.contains(button)) button.focus();
    }
  }

  // The row asks before an essential action, with the server's words; only
  // the answer carries confirm=yes (the server refuses a post without it).
  function ask(row, form, button, text) {
    const n = row.querySelector(".note[data-live-local]");
    if (!n) return;
    clearTimeout(noteTimers.get(n));
    n.className = "note ask";
    n.textContent = "";
    const box = document.createElement("span");
    box.className = "confirm";
    const msg = document.createElement("span");
    msg.textContent = text;
    const yes = document.createElement("button");
    yes.type = "button";
    yes.className = "btn sm danger";
    yes.textContent = button.getAttribute("aria-label") || "Confirm";
    const no = document.createElement("button");
    no.type = "button";
    no.className = "btn sm";
    no.textContent = "Cancel";
    box.append(msg, yes, no);
    n.append(box);
    const close = () => { n.textContent = ""; n.className = "note"; };
    yes.addEventListener("click", () => {
      close();
      act(row, form, button, { unit: form.elements.namedItem("unit").value, action: form.elements.namedItem("action").value, confirm: "yes" });
    });
    no.addEventListener("click", () => { close(); button.focus(); });
    box.addEventListener("keydown", e => { if (e.key === "Escape") { close(); button.focus(); } });
    yes.focus();
  }

  document.addEventListener("click", e => {
    const t = serviceOf(e);
    if (!t) return;
    e.preventDefault();
    const { row, form, button } = t;
    const unit = form.elements.namedItem("unit")?.value;
    const action = form.elements.namedItem("action")?.value;
    if (!unit || !action) return;
    const confirm = form.getAttribute("data-confirm");
    if (confirm) ask(row, form, button, confirm);
    else act(row, form, button, { unit, action });
  });
})();
