"use strict";
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
 * its Apply button and the server's confirmation page.
 *
 * The Device page's channel gain sliders work like the boost slider (one
 * commit on release); its mute toggles and a surface's enabled switch
 * (data-live-post) post in place. A refused gain (a surface fader owns it)
 * hands the slider back to the refresh, which shows the owner. Learning a
 * surface control shows a hands-off pause, then a countdown, while the
 * server waits for the control to move. In a mapping form, ticking a
 * channel chooses its RX/TX group, and a control form shows the options
 * that fit the kind chosen.
 *
 * The Mixer page's faders (data-live-slider="fader") post throttled values
 * while they move and one more on release, like the volume slider; its
 * momentary buttons (data-momentary) post on press and again on release.
 * An in-place post carries the form's own fields (its hidden inputs:
 * the output stage, router ports, crosspoints), repeated names included.
 *
 * The Router page's grid moves focus between crosspoints with the arrow
 * keys; its patches post in place and its editors open in a dialog on the
 * page. Pickers marked data-actions (the mixer editor) or data-what (a
 * surface mapping) show only the targets the chosen action needs; without
 * this script every picker is shown and the server uses the right one. A
 * surface's control filter acts in place (without the script it is a GET
 * the server applies). */
(() => {
    "use strict";
    // Lets the stylesheet hide the Apply buttons the sliders replace.
    document.documentElement.classList.add("js");
    const content = document.querySelector("#live-content[data-live-interval]");
    const interval = content ? Number(content.dataset.liveInterval) * 1000 : 0;
    const status = document.querySelector("[data-live-status]");
    const normalStatus = status?.textContent ?? "";
    const dirty = new WeakSet();
    // Forms the next read updates even while focused: a slider whose change
    // the server refused shows the server's value (and its owner) again.
    const forced = new WeakSet();
    let timer;
    let request = null;
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
            if (form) {
                dirty.add(form);
                forced.delete(form);
            }
        });
    }
    document.addEventListener("reset", e => {
        // The default reset happens after the event.
        const form = e.target;
        if (form instanceof HTMLFormElement)
            setTimeout(() => dirty.delete(form), 0);
    });
    document.addEventListener("submit", e => {
        // A response from before a POST must never overwrite its result.
        submitting = true;
        request?.abort();
        setTimeout(() => {
            if (e.defaultPrevented) {
                submitting = false;
                schedule();
            }
        }, 0);
    });
    document.addEventListener("pointerdown", () => { pointerDown = true; });
    for (const event of ["pointerup", "pointercancel"]) {
        document.addEventListener(event, () => { pointerDown = false; });
    }
    window.addEventListener("blur", () => { pointerDown = false; });
    function formOf(target) {
        return target instanceof HTMLInputElement || target instanceof HTMLSelectElement ||
            target instanceof HTMLTextAreaElement || target instanceof HTMLButtonElement
            ? target.form : null;
    }
    function local(node) {
        return node instanceof Element && node.hasAttribute("data-live-local");
    }
    function protectedNode(node) {
        return node instanceof Element && (node === document.activeElement || node.contains(document.activeElement) ||
            (node instanceof HTMLFormElement && dirty.has(node)) ||
            [...node.querySelectorAll("form")].some(form => dirty.has(form)));
    }
    function key(node) {
        if (!(node instanceof Element))
            return null;
        if (node.id)
            return `id:${node.id}`;
        if (node.hasAttribute("data-live-key"))
            return `key:${node.getAttribute("data-live-key")}`;
        // Forms can appear/disappear when services or EQ configurations change.
        // Their action and hidden identity distinguish them, including uploads.
        if (node.matches("form"))
            return JSON.stringify([
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
        if (current.isEqualNode(next))
            return;
        if (!(current instanceof Element) || !(next instanceof Element)) {
            if (current.nodeValue !== next.nodeValue)
                current.nodeValue = next.nodeValue;
            return;
        }
        // This script's own nodes (result lines, confirmations) are not the
        // server's to update.
        if (local(current))
            return;
        // Keep the whole form coherent while the operator edits or focuses it.
        if (current instanceof HTMLFormElement) {
            if (forced.has(current))
                forced.delete(current);
            else if (protectedNode(current))
                return;
        }
        for (const attr of [...current.attributes]) {
            if (current.matches("details") && attr.name === "open")
                continue;
            if (!next.hasAttribute(attr.name))
                current.removeAttribute(attr.name);
        }
        for (const attr of next.attributes) {
            if (current.matches("details") && attr.name === "open")
                continue;
            if (current.getAttribute(attr.name) !== attr.value)
                current.setAttribute(attr.name, attr.value);
        }
        // Prune removed identities first: a deletion before a focused/dirty
        // sibling is not a reorder and must not leave stale action buttons.
        const identities = new Set([...next.childNodes].map(key).filter(k => k !== null));
        for (const node of [...current.childNodes]) {
            const identity = key(node);
            if (identity !== null && !identities.has(identity) && !protectedNode(node) && !local(node))
                node.remove();
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
            }
            else {
                if (match !== cursor) {
                    // Moving a focused subtree can blur it. Defer structural changes.
                    if (protectedNode(current))
                        return;
                    current.insertBefore(match, cursor);
                }
                patch(match, child);
            }
            used.add(match);
            cursor = match.nextSibling;
        }
        for (const node of old) {
            if (!used.has(node) && !protectedNode(node) && !local(node))
                node.remove();
        }
        // Attributes alone do not update a previously edited control's property.
        if (current instanceof HTMLInputElement && next instanceof HTMLInputElement && current.type !== "file") {
            current.value = next.value;
            current.checked = next.checked;
        }
        else if ((current instanceof HTMLSelectElement && next instanceof HTMLSelectElement) ||
            (current instanceof HTMLTextAreaElement && next instanceof HTMLTextAreaElement))
            current.value = next.value;
    }
    function schedule(ms = delay) {
        clearTimeout(timer);
        if (interval > 0 && !stopped && !document.hidden && !submitting)
            timer = setTimeout(poll, ms);
    }
    async function poll() {
        if (request || stopped || document.hidden || submitting)
            return;
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
            if (!response.ok)
                throw new Error("Status unavailable");
            const page = new DOMParser().parseFromString(await response.text(), "text/html");
            const next = page.querySelector("#live-content[data-live-interval]");
            if (!next)
                throw new Error("Status unavailable");
            if (changes !== seen) {
                stale = true;
            }
            else if (!stopped && !document.hidden && !submitting && !pointerDown && busy === 0) {
                if (content)
                    patch(content, next);
                document.dispatchEvent(new Event("tessera:refreshed"));
                // No navigation, scrolling, innerHTML replacement or focus changes.
                const clock = document.querySelector(".foot [data-live-clock]");
                const nextClock = page.querySelector(".foot [data-live-clock]");
                if (clock && nextClock)
                    clock.textContent = nextClock.textContent;
            }
            delay = interval;
            if (status)
                status.textContent = normalStatus;
        }
        catch (_) {
            if (!stopped && !document.hidden && !submitting) {
                if (status)
                    status.textContent = "Live updates paused · retrying";
                document.dispatchEvent(new Event("tessera:stale"));
                delay = Math.min(delay * 2, 60000);
            }
        }
        finally {
            clearTimeout(timeout);
            request = null;
            schedule(stale ? 150 : undefined);
        }
    }
    function expired() {
        stopped = true;
        if (status)
            status.textContent = "Session expired · sign in again";
        location.assign("/login");
    }
    // A change was just applied: read the page again soon, so the rows and
    // readouts show the server's view of it.
    function refreshSoon() {
        if (request)
            return;
        schedule(150);
    }
    document.addEventListener("visibilitychange", () => {
        if (document.hidden) {
            clearTimeout(timer);
            request?.abort();
        }
        else
            schedule(0);
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
    // `entries` are the form's own fields (repeated names kept); `fields`
    // replace any of the same name.
    async function post(form, fields, entries = []) {
        const body = new URLSearchParams();
        const csrf = form.elements.namedItem("csrf");
        if (csrf)
            body.set("csrf", csrf.value);
        for (const [k, v] of entries)
            if (k !== "csrf" && !(k in fields))
                body.append(k, v);
        for (const [k, v] of Object.entries(fields))
            body.set(k, String(v));
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
        }
        catch (_) {
            return { ok: false, text: "no answer from the device" };
        }
        finally {
            busy--;
            changes++;
        }
    }
    // The form's hidden fields: what a slider's post needs besides its value.
    function hiddenEntries(form) {
        return [...form.querySelectorAll('input[type="hidden"]')]
            .filter(input => input.name && input.name !== "csrf")
            .map(input => [input.name, input.value]);
    }
    // Every string field of a form (and the button that submitted it).
    function formEntries(form, submitter) {
        const out = [];
        for (const [k, v] of new FormData(form, submitter ?? undefined))
            if (typeof v === "string" && k !== "csrf")
                out.push([k, v]);
        return out;
    }
    const noteTimers = new WeakMap();
    // The form's or row's result line: "applying…", "applied", "failed: why".
    function note(scope, state, text) {
        const n = scope.querySelector(".note[data-live-local]");
        if (!n)
            return;
        clearTimeout(noteTimers.get(n));
        n.textContent = text;
        n.className = `note ${state}`;
        if (state === "ok")
            noteTimers.set(n, setTimeout(() => { n.textContent = ""; n.className = "note"; }, 4000));
    }
    /* ---- sliders ------------------------------------------------------------ */
    const PREVIEW_EVERY = 150; // ms between volume previews while moving
    const COMMIT_SETTLE = 350; // ms of quiet before a boost commit (keyboard steps coalesce)
    const BOOST_HOT = 12; // dB: above this the boost is shown as a warning
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
    // Readout, fill and warning state follow the thumb. A channel gain
    // slider (data-live-slider="gain", or a mixer "fader") reads "off" at the
    // bottom of its travel and has its readout in an element marked
    // data-readout.
    function paint(form, input, v) {
        const gain = form.dataset.liveSlider === "gain" || form.dataset.liveSlider === "fader";
        const text = gain && v <= Number(input.min) ? "off" : `${signed(v)} dB`;
        const readout = form.querySelector("[data-readout]");
        if (readout)
            readout.textContent = text;
        const big = form.querySelector(".big");
        if (big && big.firstChild && big.firstChild.nodeType === Node.TEXT_NODE)
            big.firstChild.nodeValue = signed(v);
        input.className = fillClass(v, Number(input.min), Number(input.max));
        input.setAttribute("aria-valuetext", text);
        if (form.dataset.liveSlider === "boost")
            form.classList.toggle("hot", v > BOOST_HOT);
    }
    const sliders = new WeakMap();
    function slider(form) {
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
    function queue(form, value, commit) {
        const s = slider(form);
        s.pending = { value, commit: commit || Boolean(s.pending?.commit) };
        clearTimeout(s.timer);
        s.timer = undefined;
        const kind = form.dataset.liveSlider;
        if (s.pending.commit) {
            if (kind === "boost" || kind === "gain")
                s.timer = setTimeout(() => pump(form), COMMIT_SETTLE);
            else
                pump(form);
            return;
        }
        const wait = PREVIEW_EVERY - (Date.now() - s.last);
        if (wait <= 0)
            pump(form);
        else
            s.timer = setTimeout(() => pump(form), wait);
    }
    async function pump(form) {
        const s = slider(form);
        if (s.inflight || !s.pending)
            return;
        const { value, commit } = s.pending;
        s.pending = null;
        s.inflight = true;
        s.last = Date.now();
        const kind = form.dataset.liveSlider;
        if (commit)
            note(form, "busy", "applying…");
        const fields = { db: value.toFixed(1) };
        if (kind === "volume")
            fields.commit = commit ? "1" : "0";
        const r = await post(form, fields, hiddenEntries(form));
        s.inflight = false;
        if (!r.ok) {
            note(form, "bad", `failed: ${r.text}`);
            if (!s.pending) {
                // The server kept its own value (for example a gain a surface fader
                // owns, HTTP 409): the slider is no longer the operator's edit. The
                // next read shows the server's value, locked with its owner if so;
                // the answer takes the focus, so it is announced.
                dirty.delete(form);
                forced.add(form);
                const n = form.querySelector(".note[data-live-local]");
                if (n && form.contains(document.activeElement)) {
                    n.tabIndex = -1;
                    n.focus();
                }
                refreshSoon();
            }
        }
        else if (commit) {
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
        if (s.pending)
            pump(form);
    }
    function sliderOf(e) {
        const input = e.target;
        if (!(input instanceof HTMLInputElement) || input.type !== "range")
            return null;
        const form = input.form;
        if (!form?.hasAttribute("data-live-slider"))
            return null;
        return { form, input, value: Number(input.value) };
    }
    document.addEventListener("input", e => {
        const t = sliderOf(e);
        if (!t)
            return;
        paint(t.form, t.input, t.value);
        // Volume and mixer faders apply while they move (throttled previews).
        const kind = t.form.dataset.liveSlider;
        if (kind === "volume" || kind === "fader")
            queue(t.form, t.value, false);
    });
    document.addEventListener("change", e => {
        const t = sliderOf(e);
        if (!t)
            return;
        paint(t.form, t.input, t.value);
        queue(t.form, t.value, true);
    });
    // Enter in the form (the hidden Apply button's implicit submission).
    document.addEventListener("submit", e => {
        const form = e.target;
        if (!(form instanceof HTMLFormElement) || !form.hasAttribute("data-live-slider"))
            return;
        const input = form.querySelector('input[type="range"]');
        if (!input)
            return;
        e.preventDefault();
        queue(form, Number(input.value), true);
    });
    /* ---- service rows ------------------------------------------------------- */
    function serviceOf(e) {
        const button = e.target instanceof Element ? e.target.closest("button") : null;
        const form = button?.form;
        const row = form?.closest(".row[data-live-key]");
        if (!button || !form || !row || button.type !== "submit" || button.disabled)
            return null;
        return { button, form, row };
    }
    async function act(row, form, button, fields) {
        const buttons = [...row.querySelectorAll("button")];
        for (const b of buttons) {
            b.disabled = true;
            b.setAttribute("aria-busy", "true");
        }
        note(row, "busy", "working…");
        const r = await post(form, fields);
        for (const b of buttons) {
            b.disabled = false;
            b.removeAttribute("aria-busy");
        }
        if (r.ok) {
            note(row, "ok", r.text);
            // Focus moves to the result (it is announced), not back to the
            // switch: a focused form is protected from the refresh, which has to
            // show the row's new state now.
            const n = row.querySelector(".note[data-live-local]");
            if (n) {
                n.tabIndex = -1;
                n.focus();
            }
            refreshSoon();
        }
        else {
            note(row, "bad", `failed: ${r.text}`);
            if (document.contains(button))
                button.focus();
        }
    }
    // The row asks before an essential action, with the server's words; only
    // the answer carries confirm=yes (the server refuses a post without it).
    function ask(row, form, button, text) {
        const n = row.querySelector(".note[data-live-local]");
        if (!n)
            return;
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
                unit: form.elements.namedItem("unit").value,
                action: form.elements.namedItem("action").value,
                confirm: "yes"
            });
        });
        no.addEventListener("click", () => { close(); button.focus(); });
        box.addEventListener("keydown", e => { if (e.key === "Escape") {
            close();
            button.focus();
        } });
        yes.focus();
    }
    document.addEventListener("click", e => {
        const t = serviceOf(e);
        if (!t)
            return;
        e.preventDefault();
        const { row, form, button } = t;
        const unit = form.elements.namedItem("unit")?.value;
        const action = form.elements.namedItem("action")?.value;
        if (!unit || !action)
            return;
        const confirm = form.getAttribute("data-confirm");
        if (confirm)
            ask(row, form, button, confirm);
        else
            act(row, form, button, { unit, action });
    });
    /* ---- small forms posted in place ---------------------------------------- */
    // Forms marked data-live-post (channel mute toggles, a surface's enabled
    // switch) post their own fields in place. The answer shows in the note
    // beside the form, which takes the focus (it is announced), so the
    // refresh that follows may update the form itself. Without this script
    // they post normally and the server redirects back.
    document.addEventListener("submit", async (e) => {
        const form = e.target;
        if (!(form instanceof HTMLFormElement) || !form.hasAttribute("data-live-post"))
            return;
        e.preventDefault();
        // A momentary button acts on press and release, not on the click.
        if (form.hasAttribute("data-momentary"))
            return;
        const entries = formEntries(form, e.submitter);
        const scope = form.parentElement ?? form;
        const buttons = [...form.querySelectorAll("button")];
        for (const b of buttons) {
            b.disabled = true;
            b.setAttribute("aria-busy", "true");
        }
        note(scope, "busy", "applying…");
        const r = await post(form, {}, entries);
        for (const b of buttons) {
            b.disabled = false;
            b.removeAttribute("aria-busy");
        }
        dirty.delete(form);
        if (r.ok) {
            note(scope, "ok", r.text);
            const n = scope.querySelector(".note[data-live-local]");
            if (n && form.contains(document.activeElement)) {
                n.tabIndex = -1;
                n.focus();
            }
            refreshSoon();
        }
        else {
            note(scope, "bad", `failed: ${r.text}`);
        }
        // Others (the Router's editor dialog) may show the change too.
        document.dispatchEvent(new CustomEvent("tessera:posted", { detail: { form, ok: r.ok, text: r.text } }));
    });
    /* ---- momentary buttons -------------------------------------------------- */
    // A form marked data-momentary="<value>" (a mixer button in momentary mode)
    // posts its fields with muted=<value> while the button is pressed (pointer,
    // or Space/Enter held) and the opposite on release; the posts of one form
    // go in order. Without this script the form posts once, like a toggle.
    const chains = new WeakMap();
    let heldButton = null;
    function momentaryOf(target) {
        const button = target instanceof Element ? target.closest("button") : null;
        const form = button?.form;
        if (!button || !form || button.disabled || !form.hasAttribute("data-momentary"))
            return null;
        return { form, button };
    }
    function hold(form, button, press) {
        const pressed = form.dataset.momentary === "false" ? "false" : "true";
        const value = press ? pressed : (pressed === "true" ? "false" : "true");
        button.classList.toggle("held", press);
        const entries = formEntries(form);
        const fields = { muted: value };
        const scope = form.parentElement ?? form;
        if (press)
            dirty.add(form);
        const next = (chains.get(form) ?? Promise.resolve()).then(async () => {
            const r = await post(form, fields, entries);
            if (!r.ok)
                note(scope, "bad", `failed: ${r.text}`);
            else if (!press) {
                note(scope, "ok", r.text);
                dirty.delete(form);
                refreshSoon();
            }
        });
        chains.set(form, next);
    }
    function release() {
        if (!heldButton)
            return;
        const { form, button } = heldButton;
        heldButton = null;
        hold(form, button, false);
    }
    document.addEventListener("pointerdown", e => {
        const t = momentaryOf(e.target);
        if (!t || heldButton)
            return;
        e.preventDefault();
        heldButton = t;
        hold(t.form, t.button, true);
    });
    for (const event of ["pointerup", "pointercancel"])
        document.addEventListener(event, release);
    window.addEventListener("blur", release);
    document.addEventListener("keydown", e => {
        if ((e.key !== " " && e.key !== "Enter") || e.repeat)
            return;
        const t = momentaryOf(e.target);
        if (!t)
            return;
        e.preventDefault();
        if (heldButton)
            return;
        heldButton = t;
        hold(t.form, t.button, true);
    });
    document.addEventListener("keyup", e => {
        if (e.key !== " " && e.key !== "Enter")
            return;
        if (!momentaryOf(e.target))
            return;
        e.preventDefault();
        release();
    });
    document.addEventListener("focusout", e => {
        if (heldButton && e.target === heldButton.button)
            release();
    });
    /* ---- learning a surface control ----------------------------------------- */
    // The learn form (data-learn="<seconds>") posts normally: the server
    // answers once the control moved or the time ran out. Meanwhile its note
    // asks for hands off first (the daemon reads the first second as the
    // device at rest), then says what to do and counts down; a second click
    // does not start a second learn.
    const HANDS_OFF = 2; // seconds of "hands off" before "now"
    let learnTimer;
    document.addEventListener("submit", e => {
        const form = e.target;
        if (!(form instanceof HTMLFormElement) || !form.hasAttribute("data-learn"))
            return;
        if (form.dataset.learning) {
            e.preventDefault();
            return;
        }
        form.dataset.learning = "1";
        form.querySelector("button[type=submit]")?.setAttribute("aria-busy", "true");
        const hint = form.elements.namedItem("hint");
        const what = hint?.value === "fader"
            ? "move the fader from the bottom all the way up"
            : "press the button once";
        const n = form.querySelector(".note[data-live-local]");
        const total = Number(form.dataset.learn) || 15;
        let elapsed = 0;
        const tick = () => {
            if (n) {
                const left = total - elapsed;
                n.className = "note busy";
                n.textContent = elapsed < HANDS_OFF ? "Hands off for a second…"
                    : left > 0 ? `Now ${what}… ${left} s` : "Waiting for the answer…";
            }
            elapsed++;
        };
        tick();
        clearInterval(learnTimer);
        learnTimer = setInterval(tick, 1000);
    });
    window.addEventListener("pageshow", () => {
        // Back from the answer page (or a cached page): ready to learn again.
        clearInterval(learnTimer);
        for (const form of document.querySelectorAll("form[data-learning]")) {
            delete form.dataset.learning;
            form.querySelector("button[type=submit]")?.removeAttribute("aria-busy");
            note(form, "ok", "");
        }
    });
    /* ---- mapping and control forms ----------------------------------------- */
    // A mapping's channels are boxes under one RX/TX radio per device and
    // direction, and only the chosen group's ticks count. Ticking a box
    // chooses its group; choosing a group clears the ticks of the others, so
    // no tick is left out unseen. (Without this script the page fades the
    // other groups and the server refuses ticks outside the chosen one.)
    document.addEventListener("change", e => {
        const t = e.target;
        if (!(t instanceof HTMLInputElement) || !t.form)
            return;
        let group;
        if (t.type === "checkbox" && t.name.startsWith("ch:")) {
            if (!t.checked)
                return;
            group = t.name.slice(3);
            const radio = [...t.form.querySelectorAll('input[type="radio"][name="target"]')]
                .find(r => r.value === group);
            if (radio)
                radio.checked = true;
        }
        else if (t.type === "radio" && t.name === "target") {
            group = t.value;
        }
        else
            return;
        for (const box of t.form.querySelectorAll('input[type="checkbox"][name^="ch:"]'))
            if (box.name !== `ch:${group}`)
                box.checked = false;
    });
    // A control form shows the options (and mapping parts) that fit the kind
    // chosen: elements list their kinds in data-kinds.
    document.addEventListener("change", e => {
        const t = e.target;
        if (!(t instanceof HTMLSelectElement) || t.name !== "kind" || !t.form)
            return;
        for (const el of t.form.querySelectorAll("[data-kinds]"))
            el.hidden = !(el.dataset.kinds ?? "").split(" ").includes(t.value);
    });
    /* ---- routing crosspoints ------------------------------------------------ */
    // Enhance the same review GET and CSRF POST used without JavaScript. There
    // is no direct mutation on a crosspoint click and no mutation retry.
    // Transmitters are rows (a device band, then one row per channel);
    // receivers are columns (a vertical device band, then one column per
    // input). A collapsed band shows the server's read-only route counts.
    const collapsedTx = new Set(), collapsedRx = new Set();
    const selections = new Map();
    const ownRouteUpdates = new Map();
    let routingView = "auto", routingStale = false;
    const externalRouteChanges = new Set();
    let routingFreshness;
    let routeOrigin = null;
    let routingRequest = false;
    const board = () => document.querySelector("[data-routing-board]");
    const columnKey = (el) => `${el.dataset.rx ?? ""}\u0000${el.dataset.rxChannel ?? ""}`;
    const includes = (el, filter) => (el.dataset.search ?? "").toLocaleLowerCase().includes(filter);
    /** The receiver input header (data-route-key) above a crosspoint. */
    function columnOf(el) {
        const cell = el?.closest("[data-rx][data-rx-channel]");
        if (!cell)
            return null;
        const key = columnKey(cell);
        return [...(board()?.querySelectorAll("th[data-route-key]") ?? [])].find(h => columnKey(h) === key) ?? null;
    }
    function routingLayout() {
        const root = board();
        if (!root)
            return;
        root.dataset.routingView = routingView;
        for (const b of root.querySelectorAll("[data-routing-view]"))
            b.setAttribute("aria-pressed", String(b.dataset.routingView === (routingView === "auto" ? (window.innerWidth <= 700 ? "list" : "matrix") : routingView)));
        const txFilter = root.querySelector('[data-routing-filter="tx"]')?.value.toLocaleLowerCase() ?? "";
        const rxFilter = root.querySelector('[data-routing-filter="rx"]')?.value.toLocaleLowerCase() ?? "";
        for (const article of root.querySelectorAll("[data-list-search]"))
            article.hidden = !(article.dataset.listSearch ?? "").toLocaleLowerCase().includes(rxFilter);
        for (const device of root.querySelectorAll("[data-list-device]")) {
            const rows = [...device.querySelectorAll("[data-list-search]")];
            device.hidden = rows.length > 0 && rows.every(r => r.hidden);
        }
        for (const form of root.querySelectorAll(".routing-list [data-source-search]"))
            form.hidden = !(form.dataset.sourceSearch ?? "").toLocaleLowerCase().includes(txFilter);
        // Receiver columns: every header and cell of one input shares a key.
        const byColumn = new Map();
        for (const el of root.querySelectorAll(".routing-matrix [data-rx][data-rx-channel]")) {
            const key = columnKey(el);
            byColumn.set(key, [...(byColumn.get(key) ?? []), el]);
        }
        const headers = [...root.querySelectorAll("th.routing-rx-channel[data-rx]")];
        for (const heading of root.querySelectorAll("[data-rx-heading]")) {
            const id = heading.dataset.rxHeading, collapsed = collapsedRx.has(id);
            const columns = headers.filter(h => h.dataset.rx === id);
            const matching = includes(heading, rxFilter) ? columns : columns.filter(c => includes(c, rxFilter));
            const summary = [...root.querySelectorAll("[data-rx-summary]")].filter(s => s.dataset.rxSummary === id);
            const summarised = collapsed && summary.length > 0 && matching.length > 0;
            const visible = summarised ? [] : collapsed ? matching.slice(0, 1) : matching;
            heading.hidden = !matching.length;
            heading.colSpan = Math.max(1, visible.length + (summarised ? 1 : 0));
            heading.classList.toggle("routing-collapsed", collapsed);
            heading.querySelector("button")?.setAttribute("aria-expanded", String(!collapsed));
            for (const column of columns) {
                const show = visible.includes(column);
                for (const el of byColumn.get(columnKey(column)) ?? [])
                    el.hidden = !show;
            }
            for (const el of summary)
                el.hidden = !summarised;
        }
        // Transmitter rows.
        for (const heading of root.querySelectorAll("[data-tx-heading]")) {
            const id = heading.dataset.txHeading, collapsed = collapsedTx.has(id);
            const rows = [...root.querySelectorAll("tr[data-tx]")].filter(r => r.dataset.tx === id);
            const matching = includes(heading, txFilter) ? rows : rows.filter(r => includes(r, txFilter));
            heading.hidden = !matching.length;
            heading.classList.toggle("routing-collapsed", collapsed);
            heading.querySelector("button")?.setAttribute("aria-expanded", String(!collapsed));
            for (const row of rows)
                row.hidden = collapsed || !matching.includes(row);
        }
        const stale = root.querySelector("[data-routing-stale]");
        if (stale)
            stale.hidden = !routingStale;
        const pending = new Set(headers.filter(h => h.classList.contains("routing-pending")).map(columnKey));
        for (const button of root.querySelectorAll(".route-cell")) {
            const cell = button.closest("[data-rx][data-rx-channel]");
            button.disabled = button.dataset.allowed !== "true" || routingStale || (cell !== null && pending.has(columnKey(cell)));
            button.tabIndex = -1;
        }
        const focusable = routeButtons(root);
        const focused = focusable.find(b => b === document.activeElement) ?? focusable[0];
        if (focused)
            focused.tabIndex = 0;
    }
    function routeButtons(root) {
        return [...root.querySelectorAll(".route-cell")]
            .filter(b => !b.disabled && !b.closest("[hidden]"));
    }
    function routingFresh() {
        const root = board();
        if (!root)
            return;
        routingStale = false;
        clearTimeout(routingFreshness);
        routingFreshness = setTimeout(() => { routingStale = true; routingLayout(); }, 30000);
        for (const column of root.querySelectorAll("[data-route-key]")) {
            const key = column.dataset.routeKey, current = column.dataset.selection ?? "";
            if (selections.has(key) && selections.get(key) !== current && ownRouteUpdates.get(key) !== current) {
                externalRouteChanges.add(key);
            }
            if (selections.get(key) !== current)
                ownRouteUpdates.delete(key);
            selections.set(key, current);
            column.classList.toggle("routing-changed", externalRouteChanges.has(key));
            const text = column.querySelector(".routing-external");
            if (text)
                text.hidden = !externalRouteChanges.has(key);
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
            for (const input of board()?.querySelectorAll("[data-routing-filter]") ?? [])
                if (input.dataset.routingFilter === filter)
                    input.value = value;
            routingLayout();
        }
    });
    document.addEventListener("click", e => {
        const target = e.target instanceof Element ? e.target.closest("button") : null;
        const root = board();
        if (!target || !root || !root.contains(target))
            return;
        if (target.hasAttribute("data-routing-toggle")) {
            const set = target.dataset.routingToggle === "tx" ? collapsedTx : collapsedRx;
            const id = target.dataset.group;
            if (set.has(id))
                set.delete(id);
            else
                set.add(id);
            routingLayout();
        }
        else if (target.hasAttribute("data-routing-expand")) {
            const expand = target.dataset.routingExpand === "true", axis = target.dataset.routingAxis;
            for (const b of root.querySelectorAll("[data-routing-toggle]")) {
                if (axis && b.dataset.routingToggle !== axis)
                    continue;
                const set = b.dataset.routingToggle === "tx" ? collapsedTx : collapsedRx;
                if (expand)
                    set.delete(b.dataset.group);
                else
                    set.add(b.dataset.group);
            }
            routingLayout();
        }
        else if (target.dataset.routingView) {
            routingView = target.dataset.routingView;
            routingLayout();
        }
        else if (target.hasAttribute("data-routing-close")) {
            root.querySelector("#routing-dialog")?.close();
            routeOrigin?.focus();
        }
    });
    // Crosshair: the column header of the hovered or focused crosspoint.
    for (const [event, on] of [["pointerover", true], ["pointerout", false], ["focusin", true], ["focusout", false]]) {
        document.addEventListener(event, e => {
            const cell = e.target instanceof Element ? e.target.closest(".route-cell") : null;
            if (cell)
                columnOf(cell)?.classList.toggle("routing-hover", on);
        });
    }
    document.addEventListener("keydown", e => {
        const target = e.target;
        if (!(target instanceof HTMLButtonElement) || !target.matches(".route-cell"))
            return;
        const root = board();
        if (!root)
            return;
        const buttons = routeButtons(root), index = buttons.indexOf(target);
        const row = target.closest("tr"), cells = buttons.filter(b => b.closest("tr") === row);
        let next;
        if (e.key === "ArrowLeft")
            next = cells[cells.indexOf(target) - 1];
        if (e.key === "ArrowRight")
            next = cells[cells.indexOf(target) + 1];
        if (["ArrowUp", "ArrowDown"].includes(e.key)) {
            const key = columnKey(target.closest("td"));
            const sameColumn = buttons.filter(b => columnKey(b.closest("td")) === key);
            next = sameColumn[sameColumn.indexOf(target) + (e.key === "ArrowUp" ? -1 : 1)];
        }
        if (e.key === "Home")
            next = cells[0];
        if (e.key === "End")
            next = cells[cells.length - 1];
        if (next && index >= 0) {
            e.preventDefault();
            target.tabIndex = -1;
            next.tabIndex = 0;
            next.focus();
        }
    });
    async function routingDocument(url, options = {}) {
        const abort = new AbortController();
        const deadline = setTimeout(() => abort.abort(), 12000);
        try {
            const response = await fetch(url, { ...options, credentials: "same-origin", redirect: "manual", cache: "no-store", signal: abort.signal });
            if (!response.ok)
                throw new Error(`Request failed (${response.status}); refresh before trying again.`);
            return new DOMParser().parseFromString(await response.text(), "text/html");
        }
        finally {
            clearTimeout(deadline);
        }
    }
    document.addEventListener("submit", async (e) => {
        const form = e.target;
        if (!(form instanceof HTMLFormElement))
            return;
        const root = board(), dialog = root?.querySelector("#routing-dialog");
        const review = form.matches("[data-routing-choice]");
        const apply = form.matches('#routing-dialog form[action="/routing/apply"]');
        if ((!review && !apply) || !dialog || typeof dialog.showModal !== "function")
            return;
        e.preventDefault();
        if (routingRequest || (review && routingStale))
            return;
        const slot = dialog.querySelector("[data-routing-dialog-content]");
        routingRequest = true;
        // The receiver input being changed: its column header carries the key.
        const column = columnOf(review ? form : routeOrigin);
        if (review) {
            routeOrigin = form.querySelector("button");
            slot.textContent = "Reading current receiver selection…";
            dialog.showModal();
        }
        else {
            busy++;
            changes++;
            request?.abort();
            column?.classList.add("routing-pending");
            for (const b of dialog.querySelectorAll("button[type=submit]"))
                b.disabled = true;
            routingLayout();
        }
        try {
            const data = new URLSearchParams();
            for (const [key, value] of new FormData(form))
                if (typeof value === "string")
                    data.append(key, value);
            const response = await routingDocument(review ? `${form.action}?${data}` : form.action, review ? {} : { method: "POST", body: data });
            const result = response.querySelector(review ? "#routing-confirmation" : "#routing-result");
            if (!result)
                throw new Error("Unexpected response; inspect the receiver before continuing.");
            // Only server-rendered, authenticated same-origin markup is adopted.
            // Names in it have already passed through the HTML escaper.
            slot.replaceChildren(document.importNode(result, true));
            const expected = routeOrigin?.form?.dataset.expectedSelection;
            if (apply && column?.dataset.routeKey && expected && result.dataset.routingOutcome === "receiver_confirmed")
                ownRouteUpdates.set(column.dataset.routeKey, expected);
            const submit = slot.querySelector("button[type=submit]");
            submit?.focus();
        }
        catch (error) {
            slot.textContent = apply
                ? "Outcome unknown. No retry was sent. Refresh and inspect the receiver before making another change."
                : error instanceof Error ? error.message : "Could not prepare this change.";
            if (apply)
                routingStale = true;
        }
        finally {
            routingRequest = false;
            if (apply) {
                busy--;
                changes++;
                column?.classList.remove("routing-pending");
                refreshSoon();
            }
            routingLayout();
        }
    });
    /* ---- target pickers ----------------------------------------------------- */
    // The mixer editor's "Add a strip" (data-strip-form: a select named
    // action, pickers marked data-actions="<action> …") and a surface mapping
    // (radios named what, pickers marked data-what="<what>") show only the
    // pickers the choice needs. The page's refresh writes the server's markup
    // (every picker shown) back, so the choice is applied again after it.
    function pickers(form) {
        const action = form.elements.namedItem("action");
        if (action instanceof HTMLSelectElement) {
            for (const el of form.querySelectorAll("[data-actions]"))
                el.hidden = !(el.dataset.actions ?? "").split(" ").includes(action.value);
        }
        const what = form.querySelector('input[type="radio"][name="what"]:checked');
        if (what) {
            for (const el of form.querySelectorAll("[data-what]"))
                el.hidden = el.dataset.what !== what.value;
        }
    }
    function allPickers() {
        for (const form of document.querySelectorAll("form")) {
            if (form.querySelector("[data-actions], [data-what]"))
                pickers(form);
        }
    }
    document.addEventListener("change", e => {
        const t = e.target;
        if (!(t instanceof HTMLSelectElement || t instanceof HTMLInputElement) || !t.form)
            return;
        if (t.name === "action" || t.name === "what")
            pickers(t.form);
    });
    document.addEventListener("tessera:refreshed", allPickers);
    allPickers();
    /* ---- surface control filters ------------------------------------------- */
    // A surface's filter (form[data-controls-filter]: words and "mapped only
    // / all") hides the controls that do not match, in place; the refresh
    // writes the server's markup back, so the filter is applied again after
    // it. Without this script the form is a GET and the server filters.
    function controlsFilter(form) {
        const card = form.closest("section");
        if (!card)
            return;
        const q = form.elements.namedItem("q")?.value.trim().toLocaleLowerCase() ?? "";
        const mapped = (form.querySelector('input[name="show"]:checked')?.value ?? "all") === "mapped";
        const controls = [...card.querySelectorAll(".controls > article.control")];
        let shown = 0;
        for (const c of controls) {
            const ok = (!mapped || c.dataset.mapped === "true") && (q === "" || (c.dataset.search ?? "").toLocaleLowerCase().includes(q));
            // A control whose form is open stays: hiding it would lose the edit.
            const keep = c.querySelector("details[open]") !== null || c.contains(document.activeElement);
            c.hidden = !ok && !keep;
            if (!c.hidden)
                shown++;
        }
        const count = form.querySelector("[data-controls-count]");
        if (count)
            count.textContent = `Showing ${shown} of ${controls.length} controls`;
        const none = card.querySelector("[data-controls-none]");
        if (none)
            none.hidden = shown > 0;
    }
    function allControlsFilters() {
        for (const form of document.querySelectorAll("form[data-controls-filter]"))
            controlsFilter(form);
    }
    for (const event of ["input", "change"]) {
        document.addEventListener(event, e => {
            const form = e.target instanceof Element ? e.target.closest("form[data-controls-filter]") : null;
            if (form)
                controlsFilter(form);
        });
    }
    document.addEventListener("submit", e => {
        const form = e.target;
        if (!(form instanceof HTMLFormElement) || !form.hasAttribute("data-controls-filter"))
            return;
        e.preventDefault();
        controlsFilter(form);
    });
    document.addEventListener("tessera:refreshed", allControlsFilters);
    /* ---- router: patches and editors in place ------------------------------- */
    // An empty crosspoint (and a phone row's Add source and Remove) is a form
    // marked data-router-patch: posted in place, its answer shown in the
    // page's note (or the editor dialog's), and the grid read again; the
    // focus returns to the same cell. A patched crosspoint, a port header or
    // a phone row's Edit (links marked data-router-open) opens its editor in
    // the page's dialog: the same server-rendered editor, whose forms post in
    // place as they do on its own page. Without this script the forms post
    // and the links open the editor pages.
    const routerDialog = () => document.querySelector("#router-dialog");
    let routerOrigin = null;
    let routerFocus = null;
    let routerUrl = null;
    function routerNote(scope, state, text) {
        const n = scope?.closest("#router-dialog")
            ? scope.closest("#router-dialog").querySelector(".dialog-note[data-live-local]")
            : document.querySelector(".router-note[data-live-local]");
        if (!n)
            return null;
        const base = n.classList.contains("dialog-note") ? "dialog-note" : "router-note";
        clearTimeout(noteTimers.get(n));
        n.textContent = text;
        n.className = `note ${base} ${state}`;
        if (state === "ok")
            noteTimers.set(n, setTimeout(() => { n.textContent = ""; n.className = `note ${base}`; }, 4000));
        return n;
    }
    /** The id of the cell, header or phone section an element is in. */
    function anchorOf(el) {
        return el.closest("td[id], th[id], section[id]")?.id ?? "";
    }
    document.addEventListener("submit", async (e) => {
        const form = e.target;
        if (!(form instanceof HTMLFormElement) || !form.hasAttribute("data-router-patch"))
            return;
        e.preventDefault();
        const inDialog = Boolean(form.closest("#router-dialog"));
        const entries = formEntries(form, e.submitter);
        const buttons = [...form.querySelectorAll("button")];
        for (const b of buttons) {
            b.disabled = true;
            b.setAttribute("aria-busy", "true");
        }
        routerFocus = inDialog ? null : anchorOf(form);
        const n = routerNote(form, "busy", "applying…");
        // The cell is replaced by the next read: the answer takes the focus
        // (it is announced) and gives it back to the cell afterwards.
        if (n && form.contains(document.activeElement)) {
            n.tabIndex = -1;
            n.focus();
        }
        const r = await post(form, {}, entries);
        for (const b of buttons) {
            b.disabled = false;
            b.removeAttribute("aria-busy");
        }
        if (r.ok) {
            routerNote(inDialog ? null : form, "ok", r.text);
            if (inDialog && form.dataset.routerPatch === "remove")
                closeRouterDialog();
            refreshSoon();
        }
        else {
            routerFocus = null;
            routerNote(form, "bad", `failed: ${r.text}`);
        }
    });
    document.addEventListener("tessera:refreshed", () => {
        if (!routerFocus)
            return;
        const target = document.getElementById(routerFocus)?.querySelector("a, button:not([disabled]), select");
        routerFocus = null;
        const at = document.activeElement;
        if (target && (!at || at === document.body || at.matches(".router-note")))
            target.focus();
    });
    async function openRouterEditor(url) {
        const dialog = routerDialog();
        const slot = dialog?.querySelector("[data-router-dialog-content]");
        const title = dialog?.querySelector("#router-dialog-title");
        if (!dialog || !slot || !title)
            return;
        routerUrl = url;
        const reload = dialog.hasAttribute("open");
        if (!reload) {
            slot.textContent = "Reading…";
            dialog.showModal();
        }
        try {
            const page = await routingDocument(url, { headers: { "X-Tessera-Live": "1" } });
            const editor = page.querySelector("section.editor");
            if (!editor)
                throw new Error("The editor did not load; open it on its own page.");
            // Only server-rendered, authenticated same-origin markup is adopted;
            // names in it have passed the HTML escaper.
            const node = document.importNode(editor, true);
            // The heading's own nodes move, so a place under a name keeps its
            // muted look and its separator (`Speaker R — Host output 2`).
            const heading = node.querySelector("h2");
            if (heading)
                title.replaceChildren(...heading.childNodes);
            else
                title.textContent = "Edit";
            heading?.remove();
            // The dialog has its own Close: the page's way back (for its own
            // page and without this script) would be a second one.
            node.querySelectorAll("a[data-router-back]").forEach(back => (back.closest("p") ?? back).remove());
            slot.replaceChildren(...node.childNodes);
            allPickers();
            // A reload after a change keeps the focus on its answer.
            const answer = dialog.querySelector(".dialog-note");
            if (reload && answer?.textContent) {
                answer.tabIndex = -1;
                answer.focus();
            }
            else
                slot.querySelector("input, button, a")?.focus();
        }
        catch (error) {
            slot.textContent = error instanceof Error ? error.message : "The editor did not load.";
        }
    }
    function closeRouterDialog() {
        const dialog = routerDialog();
        if (dialog?.hasAttribute("open"))
            dialog.close();
    }
    document.addEventListener("click", e => {
        if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey)
            return;
        const t = e.target instanceof Element ? e.target : null;
        const dialog = routerDialog();
        if (!t || !dialog || typeof dialog.showModal !== "function")
            return;
        const link = t.closest("a[data-router-open]");
        if (link) {
            e.preventDefault();
            if (!dialog.hasAttribute("open"))
                routerOrigin = { id: anchorOf(link), el: link };
            openRouterEditor(link.href);
            return;
        }
        if (t.closest("[data-router-close]")) {
            e.preventDefault();
            closeRouterDialog();
        }
    });
    routerDialog()?.addEventListener("close", () => {
        routerUrl = null;
        const back = routerOrigin;
        routerOrigin = null;
        refreshSoon();
        if (!back)
            return;
        const el = document.contains(back.el) ? back.el
            : document.getElementById(back.id)?.querySelector("a, button");
        el?.focus();
    });
    // A mute or label change in the dialog: show the editor's new state.
    document.addEventListener("tessera:posted", e => {
        const { form, ok, text } = e.detail;
        if (!routerUrl || !form.closest("#router-dialog"))
            return;
        const dialog = routerDialog();
        routerNote(dialog, ok ? "ok" : "bad", ok ? text : `failed: ${text}`);
        if (ok)
            openRouterEditor(routerUrl);
    });
    /* ---- router grid -------------------------------------------------------- */
    // Arrow keys move between the crosspoints of the grid (data-router-grid:
    // cells carry data-row and data-col), Home and End to the row's ends.
    document.addEventListener("keydown", e => {
        const t = e.target;
        if (!(t instanceof HTMLElement))
            return;
        const cell = t.closest("td[data-row][data-col]");
        const grid = cell?.closest("[data-router-grid]");
        if (!cell || !grid || !["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(e.key))
            return;
        const row = Number(cell.dataset.row), col = Number(cell.dataset.col);
        const target = (el) => el.querySelector("a, button:not([disabled])");
        const cells = [...grid.querySelectorAll("td[data-row][data-col]")].filter(c => target(c));
        const inRow = cells.filter(x => Number(x.dataset.row) === row).sort((a, b) => Number(a.dataset.col) - Number(b.dataset.col));
        const inCol = cells.filter(x => Number(x.dataset.col) === col).sort((a, b) => Number(a.dataset.row) - Number(b.dataset.row));
        let next;
        if (e.key === "ArrowLeft")
            next = inRow.filter(x => Number(x.dataset.col) < col).at(-1);
        if (e.key === "ArrowRight")
            next = inRow.find(x => Number(x.dataset.col) > col);
        if (e.key === "ArrowUp")
            next = inCol.filter(x => Number(x.dataset.row) < row).at(-1);
        if (e.key === "ArrowDown")
            next = inCol.find(x => Number(x.dataset.row) > row);
        if (e.key === "Home")
            next = inRow[0];
        if (e.key === "End")
            next = inRow.at(-1);
        const focus = next && target(next);
        if (focus) {
            e.preventDefault();
            focus.focus();
        }
    });
})();
