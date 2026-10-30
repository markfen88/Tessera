/* assets/generated/live.js is generated from this TypeScript source.
 * Edit live.ts and run cargo xtask web-assets; do not edit the emitted script.
 *
 * Status pages remain server rendered. Poll the same authenticated GET and
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

  const content = document.querySelector<HTMLElement>("#live-content[data-live-interval]");
  const interval = content ? Number(content.dataset.liveInterval) * 1000 : 0;
  const status = document.querySelector("[data-live-status]");
  const normalStatus = status?.textContent ?? "";
  const dirty = new WeakSet<HTMLFormElement>();
  let timer: number | undefined;
  let request: AbortController | null = null;
  let stopped = false, submitting = false, pointerDown = false;
  let delay = interval;
  // In-place posts in flight: a read sampled meanwhile could show the value
  // from before the post, so patching waits for them.
  let busy = 0;
  // Counts posts started and finished: a read that spans a change is stale.
  let changes = 0;

  for (const event of ["input", "change"]) {
    document.addEventListener(event, e => {
      const form = formOf(e.target);
      if (form) dirty.add(form);
    });
  }
  document.addEventListener("reset", e => {
    // The default reset happens after the event.
    const form = e.target;
    if (form instanceof HTMLFormElement) setTimeout(() => dirty.delete(form), 0);
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

  function formOf(target: EventTarget | null): HTMLFormElement | null {
    return target instanceof HTMLInputElement || target instanceof HTMLSelectElement ||
      target instanceof HTMLTextAreaElement || target instanceof HTMLButtonElement
      ? target.form : null;
  }

  function local(node: Node): boolean {
    return node instanceof Element && node.hasAttribute("data-live-local");
  }

  function protectedNode(node: Node): boolean {
    return node instanceof Element && (
      node === document.activeElement || node.contains(document.activeElement) ||
      (node instanceof HTMLFormElement && dirty.has(node)) ||
      [...node.querySelectorAll("form")].some(form => dirty.has(form))
    );
  }

  function key(node: Node): string | null {
    if (!(node instanceof Element)) return null;
    if (node.id) return `id:${node.id}`;
    if (node.hasAttribute("data-live-key")) return `key:${node.getAttribute("data-live-key")}`;
    // Forms can appear/disappear when services or EQ configurations change.
    // Their action and hidden identity distinguish them, including uploads.
    if (node.matches("form")) return JSON.stringify([
      node.getAttribute("action"),
      [...node.querySelectorAll<HTMLInputElement>('input[type="hidden"]')]
        .filter(input => !["csrf", "action"].includes(input.name))
        .map(input => [input.name, input.value])
    ]);
    return null;
  }

  function compatible(a: Node, b: Node): boolean {
    return a.nodeType === b.nodeType && a.nodeName === b.nodeName && key(a) === key(b);
  }

  function patch(current: Node, next: Node): void {
    if (current.isEqualNode(next)) return;
    if (!(current instanceof Element) || !(next instanceof Element)) {
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
    const used = new Set<Node>();
    let cursor = current.firstChild;
    for (const child of next.childNodes) {
      const identity = key(child);
      let match: Node | null | undefined = identity === null
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
    if (current instanceof HTMLInputElement && next instanceof HTMLInputElement && current.type !== "file") {
      current.value = next.value;
      current.checked = next.checked;
    } else if (
      (current instanceof HTMLSelectElement && next instanceof HTMLSelectElement) ||
      (current instanceof HTMLTextAreaElement && next instanceof HTMLTextAreaElement)
    ) current.value = next.value;
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
        if (content) patch(content, next);
        document.dispatchEvent(new Event("tessera:refreshed"));
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
        document.dispatchEvent(new Event("tessera:stale"));
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
  type FormFields = Record<string, string>;
  type PostResult = { ok: boolean; text: string; db?: number };

  async function post(form: HTMLFormElement, fields: FormFields): Promise<PostResult> {
    const body = new URLSearchParams();
    const csrf = form.elements.namedItem("csrf") as HTMLInputElement | null;
    if (csrf) body.set("csrf", csrf.value);
    for (const [k, v] of Object.entries(fields)) body.set(k, String(v));
    busy++;
    changes++;
    try {
      const response = await fetch(form.getAttribute("action")!, {
        method: "POST", credentials: "same-origin", cache: "no-store", redirect: "manual",
        headers: { "X-Tessera-Live": "1", "Content-Type": "application/x-www-form-urlencoded" },
        body
      });
      if (response.type === "opaqueredirect" || [401, 303].includes(response.status)) {
        expired();
        return { ok: false, text: "session expired" };
      }
      if ((response.headers.get("content-type") || "").includes("application/json")) {
        return await response.json() as PostResult;
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

  const noteTimers = new WeakMap<HTMLElement, number>();

  // The form's or row's result line: "applying…", "applied", "failed: why".
  function note(scope: Element, state: "ok" | "bad" | "busy", text: string): void {
    const n = scope.querySelector<HTMLElement>(".note[data-live-local]");
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
  function signed(v: number): string {
    const s = Math.abs(v).toFixed(1);
    return v > 0 ? `+${s}` : v < 0 ? `−${s}` : "0.0";
  }

  // The filled part of the track, in the stylesheet's 5 % classes.
  function fillClass(v: number, min: number, max: number): string {
    const f = max > min ? Math.min(1, Math.max(0, (v - min) / (max - min))) : 0;
    return `fill-${Math.round(f * 20) * 5}`;
  }

  // Readout, fill and warning state follow the thumb.
  function paint(form: HTMLFormElement, input: HTMLInputElement, v: number): void {
    const big = form.querySelector(".big");
    if (big && big.firstChild && big.firstChild.nodeType === Node.TEXT_NODE) big.firstChild.nodeValue = signed(v);
    input.className = fillClass(v, Number(input.min), Number(input.max));
    input.setAttribute("aria-valuetext", `${signed(v)} dB`);
    if (form.dataset.liveSlider === "boost") form.classList.toggle("hot", v > BOOST_HOT);
  }

  type SliderState = {
    pending: { value: number; commit: boolean } | null;
    inflight: boolean;
    timer: number | undefined;
    last: number;
  };
  const sliders = new WeakMap<HTMLFormElement, SliderState>();

  function slider(form: HTMLFormElement): SliderState {
    let s = sliders.get(form);
    if (!s) {
      s = { pending: null, inflight: false, timer: undefined, last: 0 };
      sliders.set(form, s);
    }
    return s;
  }

  // One request at a time per form; the newest value wins and a commit is
  // never dropped (a preview that arrives while a commit waits becomes
  // part of that commit).
  function queue(form: HTMLFormElement, value: number, commit: boolean): void {
    const s = slider(form);
    s.pending = { value, commit: commit || Boolean(s.pending?.commit) };
    clearTimeout(s.timer);
    s.timer = undefined;
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

  async function pump(form: HTMLFormElement): Promise<void> {
    const s = slider(form);
    if (s.inflight || !s.pending) return;
    const { value, commit } = s.pending;
    s.pending = null;
    s.inflight = true;
    s.last = Date.now();
    const kind = form.dataset.liveSlider;
    if (commit) note(form, "busy", "applying…");
    const fields: FormFields = { db: value.toFixed(1) };
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
      const input = form.querySelector<HTMLInputElement>('input[type="range"]');
      if (input && !s.pending && typeof r.db === "number") {
        input.value = r.db.toFixed(1);
        paint(form, input, r.db);
      }
      refreshSoon();
    }
    if (s.pending) pump(form);
  }

  function sliderOf(e: Event) {
    const input = e.target;
    if (!(input instanceof HTMLInputElement) || input.type !== "range") return null;
    const form = input.form;
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
    const input = form.querySelector<HTMLInputElement>('input[type="range"]');
    if (!input) return;
    e.preventDefault();
    queue(form, Number(input.value), true);
  });

  /* ---- service rows ------------------------------------------------------- */

  function serviceOf(e: Event) {
    const button = e.target instanceof Element ? e.target.closest("button") : null;
    const form = button?.form;
    const row = form?.closest<HTMLElement>(".row[data-live-key]");
    if (!button || !form || !row || button.type !== "submit" || button.disabled) return null;
    return { button, form, row };
  }

  async function act(row: HTMLElement, form: HTMLFormElement, button: HTMLButtonElement, fields: FormFields): Promise<void> {
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
      const n = row.querySelector<HTMLElement>(".note[data-live-local]");
      if (n) { n.tabIndex = -1; n.focus(); }
      refreshSoon();
    } else {
      note(row, "bad", `failed: ${r.text}`);
      if (document.contains(button)) button.focus();
    }
  }

  // The row asks before an essential action, with the server's words; only
  // the answer carries confirm=yes (the server refuses a post without it).
  function ask(row: HTMLElement, form: HTMLFormElement, button: HTMLButtonElement, text: string): void {
    const n = row.querySelector<HTMLElement>(".note[data-live-local]");
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
      act(row, form, button, {
        unit: (form.elements.namedItem("unit") as HTMLInputElement).value,
        action: (form.elements.namedItem("action") as HTMLInputElement).value,
        confirm: "yes"
      });
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
    const unit = (form.elements.namedItem("unit") as HTMLInputElement | null)?.value;
    const action = (form.elements.namedItem("action") as HTMLInputElement | null)?.value;
    if (!unit || !action) return;
    const confirm = form.getAttribute("data-confirm");
    if (confirm) ask(row, form, button, confirm);
    else act(row, form, button, { unit, action });
  });

  /* ---- routing crosspoints ------------------------------------------------ */
  // Enhance the same review GET and CSRF POST used without JavaScript. There
  // is no direct mutation on a crosspoint click and no mutation retry.
  // Transmitters are rows (a device band, then one row per channel);
  // receivers are columns (a vertical device band, then one column per
  // input). A collapsed band shows the server's read-only route counts.
  const collapsedTx = new Set<string>(), collapsedRx = new Set<string>();
  const selections = new Map<string, string>();
  const ownRouteUpdates = new Map<string, string>();
  let routingView = "auto", routingStale = false;
  const externalRouteChanges = new Set<string>();
  let routingFreshness: number | undefined;
  let routeOrigin: HTMLButtonElement | null = null;
  let routingRequest = false;
  const board = () => document.querySelector<HTMLElement>("[data-routing-board]");
  const columnKey = (el: HTMLElement) => `${el.dataset.rx ?? ""}\u0000${el.dataset.rxChannel ?? ""}`;
  const includes = (el: HTMLElement, filter: string) => (el.dataset.search ?? "").toLocaleLowerCase().includes(filter);

  /** The receiver input header (data-route-key) above a crosspoint. */
  function columnOf(el: Element | null | undefined): HTMLElement | null {
    const cell = el?.closest<HTMLElement>("[data-rx][data-rx-channel]");
    if (!cell) return null;
    const key = columnKey(cell);
    return [...(board()?.querySelectorAll<HTMLElement>("th[data-route-key]") ?? [])].find(h => columnKey(h) === key) ?? null;
  }

  function routingLayout(): void {
    const root = board();
    if (!root) return;
    root.dataset.routingView = routingView;
    for (const b of root.querySelectorAll<HTMLButtonElement>("[data-routing-view]"))
      b.setAttribute("aria-pressed", String(b.dataset.routingView === (routingView === "auto" ? (window.innerWidth <= 700 ? "list" : "matrix") : routingView)));
    const txFilter = root.querySelector<HTMLInputElement>('[data-routing-filter="tx"]')?.value.toLocaleLowerCase() ?? "";
    const rxFilter = root.querySelector<HTMLInputElement>('[data-routing-filter="rx"]')?.value.toLocaleLowerCase() ?? "";
    for (const article of root.querySelectorAll<HTMLElement>("[data-list-search]")) article.hidden = !(article.dataset.listSearch ?? "").toLocaleLowerCase().includes(rxFilter);
    for (const device of root.querySelectorAll<HTMLElement>("[data-list-device]")) {
      const rows = [...device.querySelectorAll<HTMLElement>("[data-list-search]")];
      device.hidden = rows.length > 0 && rows.every(r => r.hidden);
    }
    for (const form of root.querySelectorAll<HTMLElement>(".routing-list [data-source-search]")) form.hidden = !(form.dataset.sourceSearch ?? "").toLocaleLowerCase().includes(txFilter);
    // Receiver columns: every header and cell of one input shares a key.
    const byColumn = new Map<string, HTMLElement[]>();
    for (const el of root.querySelectorAll<HTMLElement>(".routing-matrix [data-rx][data-rx-channel]")) {
      const key = columnKey(el);
      byColumn.set(key, [...(byColumn.get(key) ?? []), el]);
    }
    const headers = [...root.querySelectorAll<HTMLElement>("th.routing-rx-channel[data-rx]")];
    for (const heading of root.querySelectorAll<HTMLTableCellElement>("[data-rx-heading]")) {
      const id = heading.dataset.rxHeading!, collapsed = collapsedRx.has(id);
      const columns = headers.filter(h => h.dataset.rx === id);
      const matching = includes(heading, rxFilter) ? columns : columns.filter(c => includes(c, rxFilter));
      const summary = [...root.querySelectorAll<HTMLElement>("[data-rx-summary]")].filter(s => s.dataset.rxSummary === id);
      const summarised = collapsed && summary.length > 0 && matching.length > 0;
      const visible = summarised ? [] : collapsed ? matching.slice(0, 1) : matching;
      heading.hidden = !matching.length;
      heading.colSpan = Math.max(1, visible.length + (summarised ? 1 : 0));
      heading.classList.toggle("routing-collapsed", collapsed);
      heading.querySelector("button")?.setAttribute("aria-expanded", String(!collapsed));
      for (const column of columns) {
        const show = visible.includes(column);
        for (const el of byColumn.get(columnKey(column)) ?? []) el.hidden = !show;
      }
      for (const el of summary) el.hidden = !summarised;
    }
    // Transmitter rows.
    for (const heading of root.querySelectorAll<HTMLElement>("[data-tx-heading]")) {
      const id = heading.dataset.txHeading!, collapsed = collapsedTx.has(id);
      const rows = [...root.querySelectorAll<HTMLElement>("tr[data-tx]")].filter(r => r.dataset.tx === id);
      const matching = includes(heading, txFilter) ? rows : rows.filter(r => includes(r, txFilter));
      heading.hidden = !matching.length;
      heading.classList.toggle("routing-collapsed", collapsed);
      heading.querySelector("button")?.setAttribute("aria-expanded", String(!collapsed));
      for (const row of rows) row.hidden = collapsed || !matching.includes(row);
    }
    const stale = root.querySelector<HTMLElement>("[data-routing-stale]");
    if (stale) stale.hidden = !routingStale;
    const pending = new Set(headers.filter(h => h.classList.contains("routing-pending")).map(columnKey));
    for (const button of root.querySelectorAll<HTMLButtonElement>(".route-cell")) {
      const cell = button.closest<HTMLElement>("[data-rx][data-rx-channel]");
      button.disabled = button.dataset.allowed !== "true" || routingStale || (cell !== null && pending.has(columnKey(cell)));
      button.tabIndex = -1;
    }
    const focusable = routeButtons(root);
    const focused = focusable.find(b => b === document.activeElement) ?? focusable[0];
    if (focused) focused.tabIndex = 0;
  }

  function routeButtons(root: HTMLElement): HTMLButtonElement[] {
    return [...root.querySelectorAll<HTMLButtonElement>(".route-cell")]
      .filter(b => !b.disabled && !b.closest("[hidden]"));
  }

  function routingFresh(): void {
    const root = board();
    if (!root) return;
    routingStale = false;
    clearTimeout(routingFreshness);
    routingFreshness = setTimeout(() => { routingStale = true; routingLayout(); }, 30000);
    for (const column of root.querySelectorAll<HTMLElement>("[data-route-key]")) {
      const key = column.dataset.routeKey!, current = column.dataset.selection ?? "";
      if (selections.has(key) && selections.get(key) !== current && ownRouteUpdates.get(key) !== current) {
        externalRouteChanges.add(key);
      }
      if (selections.get(key) !== current) ownRouteUpdates.delete(key);
      selections.set(key, current);
      column.classList.toggle("routing-changed", externalRouteChanges.has(key));
      const text = column.querySelector<HTMLElement>(".routing-external");
      if (text) text.hidden = !externalRouteChanges.has(key);
    }
    routingLayout();
  }
  document.addEventListener("tessera:refreshed", routingFresh);
  window.addEventListener("resize", routingLayout);
  document.addEventListener("tessera:stale", () => { routingStale = true; routingLayout(); });
  routingFresh();

  document.addEventListener("input", e => {
    if (e.target instanceof HTMLInputElement && e.target.hasAttribute("data-routing-filter")) {
      const filter = e.target.dataset.routingFilter, value = e.target.value;
      for (const input of board()?.querySelectorAll<HTMLInputElement>("[data-routing-filter]") ?? [])
        if (input.dataset.routingFilter === filter) input.value = value;
      routingLayout();
    }
  });
  document.addEventListener("click", e => {
    const target = e.target instanceof Element ? e.target.closest<HTMLButtonElement>("button") : null;
    const root = board();
    if (!target || !root || !root.contains(target)) return;
    if (target.hasAttribute("data-routing-toggle")) {
      const set = target.dataset.routingToggle === "tx" ? collapsedTx : collapsedRx;
      const id = target.dataset.group!;
      if (set.has(id)) set.delete(id); else set.add(id);
      routingLayout();
    } else if (target.hasAttribute("data-routing-expand")) {
      const expand = target.dataset.routingExpand === "true", axis = target.dataset.routingAxis;
      for (const b of root.querySelectorAll<HTMLElement>("[data-routing-toggle]")) {
        if (axis && b.dataset.routingToggle !== axis) continue;
        const set = b.dataset.routingToggle === "tx" ? collapsedTx : collapsedRx;
        if (expand) set.delete(b.dataset.group!); else set.add(b.dataset.group!);
      }
      routingLayout();
    } else if (target.dataset.routingView) {
      routingView = target.dataset.routingView;
      routingLayout();
    } else if (target.hasAttribute("data-routing-close")) {
      root.querySelector<HTMLDialogElement>("#routing-dialog")?.close();
      routeOrigin?.focus();
    }
  });
  // Crosshair: the column header of the hovered or focused crosspoint.
  for (const [event, on] of [["pointerover", true], ["pointerout", false], ["focusin", true], ["focusout", false]] as const) {
    document.addEventListener(event, e => {
      const cell = e.target instanceof Element ? e.target.closest<HTMLButtonElement>(".route-cell") : null;
      if (cell) columnOf(cell)?.classList.toggle("routing-hover", on);
    });
  }
  document.addEventListener("keydown", e => {
    const target = e.target;
    if (!(target instanceof HTMLButtonElement) || !target.matches(".route-cell")) return;
    const root = board();
    if (!root) return;
    const buttons = routeButtons(root), index = buttons.indexOf(target);
    const row = target.closest("tr"), cells = buttons.filter(b => b.closest("tr") === row);
    let next: HTMLButtonElement | undefined;
    if (e.key === "ArrowLeft") next = cells[cells.indexOf(target) - 1];
    if (e.key === "ArrowRight") next = cells[cells.indexOf(target) + 1];
    if (["ArrowUp", "ArrowDown"].includes(e.key)) {
      const key = columnKey(target.closest<HTMLElement>("td")!);
      const sameColumn = buttons.filter(b => columnKey(b.closest<HTMLElement>("td")!) === key);
      next = sameColumn[sameColumn.indexOf(target) + (e.key === "ArrowUp" ? -1 : 1)];
    }
    if (e.key === "Home") next = cells[0];
    if (e.key === "End") next = cells[cells.length - 1];
    if (next && index >= 0) { e.preventDefault(); target.tabIndex = -1; next.tabIndex = 0; next.focus(); }
  });

  async function routingDocument(url: string, options: RequestInit = {}): Promise<Document> {
    const abort = new AbortController();
    const deadline = setTimeout(() => abort.abort(), 12000);
    try {
      const response = await fetch(url, { ...options, credentials: "same-origin", redirect: "manual", cache: "no-store", signal: abort.signal });
      if (!response.ok) throw new Error(`Request failed (${response.status}); refresh before trying again.`);
      return new DOMParser().parseFromString(await response.text(), "text/html");
    } finally { clearTimeout(deadline); }
  }

  document.addEventListener("submit", async e => {
    const form = e.target;
    if (!(form instanceof HTMLFormElement)) return;
    const root = board(), dialog = root?.querySelector<HTMLDialogElement>("#routing-dialog");
    const review = form.matches("[data-routing-choice]");
    const apply = form.matches('#routing-dialog form[action="/routing/apply"]');
    if ((!review && !apply) || !dialog || typeof dialog.showModal !== "function") return;
    e.preventDefault();
    if (routingRequest || (review && routingStale)) return;
    const slot = dialog.querySelector<HTMLElement>("[data-routing-dialog-content]")!;
    routingRequest = true;
    // The receiver input being changed: its column header carries the key.
    const column = columnOf(review ? form : routeOrigin);
    if (review) {
      routeOrigin = form.querySelector<HTMLButtonElement>("button");
      slot.textContent = "Reading current receiver selection…";
      dialog.showModal();
    } else {
      busy++; changes++;
      request?.abort();
      column?.classList.add("routing-pending");
      for (const b of dialog.querySelectorAll<HTMLButtonElement>("button[type=submit]")) b.disabled = true;
      routingLayout();
    }
    try {
      const data = new URLSearchParams();
      for (const [key, value] of new FormData(form)) if (typeof value === "string") data.append(key, value);
      const response = await routingDocument(review ? `${form.action}?${data}` : form.action,
        review ? {} : { method: "POST", body: data });
      const result = response.querySelector<HTMLElement>(review ? "#routing-confirmation" : "#routing-result");
      if (!result) throw new Error("Unexpected response; inspect the receiver before continuing.");
      // Only server-rendered, authenticated same-origin markup is adopted.
      // Names in it have already passed through the HTML escaper.
      slot.replaceChildren(document.importNode(result, true));
      const expected = routeOrigin?.form?.dataset.expectedSelection;
      if (apply && column?.dataset.routeKey && expected && result.dataset.routingOutcome === "receiver_confirmed") ownRouteUpdates.set(column.dataset.routeKey, expected);
      const submit = slot.querySelector<HTMLButtonElement>("button[type=submit]");
      submit?.focus();
    } catch (error) {
      slot.textContent = apply
        ? "Outcome unknown. No retry was sent. Refresh and inspect the receiver before making another change."
        : error instanceof Error ? error.message : "Could not prepare this change.";
      if (apply) routingStale = true;
    } finally {
      routingRequest = false;
      if (apply) {
        busy--; changes++; column?.classList.remove("routing-pending");
        refreshSoon();
      }
      routingLayout();
    }
  });
})();
