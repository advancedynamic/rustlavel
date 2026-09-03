/* The starter kit's JavaScript. Vanilla, no build step, no dependencies.
 *
 * Everything here is bound by data attribute rather than by inline handler,
 * because the pages are served under a Content-Security-Policy with no
 * `unsafe-inline`: an `onclick=` would silently never fire. Markup says what
 * a thing is; this file says what that means.
 *
 * Nothing here is a security control. The confirmations, the disabled submit
 * buttons and the password meter are conveniences, and every one of them is
 * checked again on the server — a person who turns JavaScript off gets a site
 * that is less pleasant and exactly as safe.
 */
(function () {
  "use strict";

  const on = (event, selector, handler) => {
    document.addEventListener(event, (e) => {
      const target = e.target.closest(selector);
      if (target) handler(e, target);
    });
  };

  /* ---- Flash messages ---------------------------------------------- */

  on("click", "[data-dismiss]", (_e, button) => {
    const box = button.closest("[role=status], [role=alert]");
    if (box) box.remove();
  });

  /* ---- The sidebar, below `lg` -------------------------------------- */

  const sidebar = document.getElementById("sidebar");
  const backdrop = document.getElementById("sidebar-backdrop");

  const setSidebar = (open) => {
    if (!sidebar) return;
    // `hidden` rather than a style, so the `lg:block` class still wins on a
    // wide screen and the sidebar cannot get stuck closed on resize.
    sidebar.hidden = !open;
    if (backdrop) backdrop.hidden = !open;
    document.querySelectorAll("[data-sidebar-toggle]").forEach((b) => {
      b.setAttribute("aria-expanded", String(open));
    });
  };

  const wide = window.matchMedia("(min-width: 1024px)");
  const syncSidebar = () => setSidebar(wide.matches);
  syncSidebar();
  wide.addEventListener("change", syncSidebar);

  on("click", "[data-sidebar-toggle]", () => setSidebar(sidebar && sidebar.hidden));
  if (backdrop) backdrop.addEventListener("click", () => setSidebar(false));

  /* ---- Dropdown menus ----------------------------------------------- */

  const closeMenus = (except) => {
    document.querySelectorAll("[data-menu]").forEach((menu) => {
      if (menu === except) return;
      menu.hidden = true;
      const toggle = document.querySelector(`[data-menu-toggle="${menu.id}"]`);
      if (toggle) toggle.setAttribute("aria-expanded", "false");
    });
  };

  on("click", "[data-menu-toggle]", (e, button) => {
    e.preventDefault();
    const menu = document.getElementById(button.dataset.menuToggle);
    if (!menu) return;
    const open = menu.hidden;
    closeMenus(menu);
    menu.hidden = !open;
    button.setAttribute("aria-expanded", String(open));
  });

  document.addEventListener("click", (e) => {
    if (!e.target.closest("[data-menu], [data-menu-toggle]")) closeMenus();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeMenus();
      if (!wide.matches) setSidebar(false);
    }
  });

  /* ---- Destructive actions ------------------------------------------ */

  on("submit", "form[data-confirm]", (e, form) => {
    // `confirm` is a blocking dialog and deliberately so: the alternative is a
    // custom modal that has to re-implement focus trapping, and getting that
    // subtly wrong is worse than a plain browser dialog.
    if (!window.confirm(form.dataset.confirm)) {
      e.preventDefault();
      return;
    }
    lockSubmit(form);
  });

  /* ---- Double submits ------------------------------------------------ */

  const lockSubmit = (form) => {
    form.querySelectorAll("button[type=submit]").forEach((button) => {
      button.disabled = true;
      // A class rather than `button.style.minWidth`, so the button does not
      // collapse as the label changes. Setting a style property from script is
      // allowed under this CSP, but not writing any inline style at all leaves
      // nothing to argue about later.
      button.classList.add("min-w-24");
      button.textContent = button.dataset.busyLabel || "Working…";
    });
  };

  on("submit", "form:not([data-confirm]):not([data-allow-resubmit])", (_e, form) => {
    // Only once the browser is satisfied; a form that failed validation must
    // stay submittable.
    if (form.checkValidity()) lockSubmit(form);
  });

  /* ---- Show/hide a password ------------------------------------------ */

  on("click", "[data-toggle-password]", (_e, button) => {
    const input = document.getElementById(button.dataset.togglePassword);
    if (!input) return;
    const showing = input.type === "text";
    input.type = showing ? "password" : "text";
    button.setAttribute("aria-label", showing ? "Show the password" : "Hide the password");
    const eye = button.querySelector("[data-eye]");
    if (eye) eye.classList.toggle("opacity-50", !showing);
    input.focus();
  });

  /* ---- Password strength --------------------------------------------- */

  /* A rough estimate of how much guessing a password would take, in bits.
   * Deliberately rough: the server enforces the length rule, and this exists
   * only to talk somebody out of "Password1". It is not a security control
   * and does not pretend to be one. */
  const strengthBits = (password) => {
    if (!password) return 0;
    let alphabet = 0;
    if (/[a-z]/.test(password)) alphabet += 26;
    if (/[A-Z]/.test(password)) alphabet += 26;
    if (/[0-9]/.test(password)) alphabet += 10;
    if (/[^a-zA-Z0-9]/.test(password)) alphabet += 33;

    const unique = new Set(password).size;
    // A long run of one character is not long at all.
    const effective = Math.min(password.length, unique * 2);
    let bits = effective * Math.log2(alphabet || 1);

    if (/^[0-9]+$/.test(password)) bits *= 0.5;
    if (/(.)\1{2,}/.test(password)) bits *= 0.7;
    if (/^(?:012|123|234|345|456|567|678|789|abc|qwe|password|admin|letmein)/i.test(password)) bits *= 0.35;
    return bits;
  };

  const RUNGS = [
    { max: 28, label: "Too easy to guess", bar: "w-1/4", colour: "bg-red-500" },
    { max: 40, label: "Weak", bar: "w-2/4", colour: "bg-amber-500" },
    { max: 60, label: "Reasonable", bar: "w-3/4", colour: "bg-brand-500" },
    { max: Infinity, label: "Strong", bar: "w-full", colour: "bg-green-500" },
  ];

  document.querySelectorAll("[data-strength-input]").forEach((input) => {
    const form = input.closest("form");
    const meter = form && form.querySelector("[data-strength-meter]");
    if (!meter) return;
    const bar = meter.querySelector("[data-strength-bar]");
    const text = meter.querySelector("[data-strength-text]");
    const confirm = form.querySelector("[data-strength-confirm]");
    const min = parseInt(input.dataset.strengthMin || "12", 10);

    const paint = () => {
      const value = input.value;
      meter.hidden = value.length === 0;
      if (!value) return;

      const rung = RUNGS.find((r) => strengthBits(value) < r.max);
      bar.className = `h-full rounded-full transition-all ${rung.bar} ${rung.colour}`;
      text.textContent =
        value.length < min
          ? `${min - value.length} more character${min - value.length === 1 ? "" : "s"} needed`
          : rung.label;
    };

    const compare = () => {
      if (!confirm) return;
      // Only complain once there is something to compare against, so the
      // message does not appear while somebody is still typing.
      const mismatch = confirm.value.length > 0 && confirm.value !== input.value;
      confirm.setCustomValidity(mismatch ? "The two passwords do not match." : "");
      confirm.classList.toggle("field-input-invalid", mismatch);
    };

    input.addEventListener("input", () => { paint(); compare(); });
    if (confirm) confirm.addEventListener("input", compare);
  });

  /* ---- One-time-code inputs ------------------------------------------ */

  document.querySelectorAll("[data-otp-input]").forEach((input) => {
    input.addEventListener("input", () => {
      const digits = input.value.replace(/\D/g, "").slice(0, input.maxLength);
      if (digits !== input.value) input.value = digits;
      // Submit as soon as it is complete: the code is short-lived and the
      // extra click is the most common way to miss the window.
      if (digits.length === input.maxLength) {
        const form = input.closest("form");
        if (form && form.checkValidity()) form.requestSubmit();
      }
    });
    input.addEventListener("paste", (e) => {
      const pasted = (e.clipboardData || window.clipboardData).getData("text");
      const digits = pasted.replace(/\D/g, "").slice(0, input.maxLength);
      if (digits) {
        e.preventDefault();
        input.value = digits;
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
    });
  });

  /* ---- Copy, and download ------------------------------------------- */

  on("click", "[data-copy]", async (_e, button) => {
    const text = button.dataset.copy;
    const original = button.textContent;
    try {
      await navigator.clipboard.writeText(text);
      button.textContent = "Copied";
    } catch {
      // Clipboard access is refused outside a secure context, and on plain
      // HTTP in development that is the normal case rather than an error.
      button.textContent = "Press ⌘/Ctrl+C";
    }
    setTimeout(() => { button.textContent = original; }, 1800);
  });

  on("click", "a[data-download-text]", (e, link) => {
    e.preventDefault();
    const blob = new Blob([link.dataset.downloadText], { type: "text/plain" });
    const url = URL.createObjectURL(blob);
    const temporary = document.createElement("a");
    temporary.href = url;
    temporary.download = link.getAttribute("download") || "download.txt";
    temporary.click();
    URL.revokeObjectURL(url);
  });

  /* ---- Bulk checkbox controls ---------------------------------------- */

  const setGroup = (name, checked) => {
    const group = document.querySelector(`[data-check-group="${name}"]`);
    if (!group) return;
    group.querySelectorAll("input[type=checkbox]").forEach((box) => { box.checked = checked; });
  };
  on("click", "[data-check-all]", (_e, b) => setGroup(b.dataset.checkAll, true));
  on("click", "[data-uncheck-all]", (_e, b) => setGroup(b.dataset.uncheckAll, false));

  /* ---- Passkeys ------------------------------------------------------ */

  /* WebAuthn speaks ArrayBuffers; the server speaks base64url. These two are
   * the whole translation layer, and getting the padding wrong is the classic
   * way to make a ceremony fail with an error that names the wrong thing. */
  const fromBase64Url = (value) => {
    const padded = value.replace(/-/g, "+").replace(/_/g, "/");
    const binary = atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));
    return Uint8Array.from(binary, (c) => c.charCodeAt(0));
  };

  const toBase64Url = (buffer) =>
    btoa(String.fromCharCode(...new Uint8Array(buffer)))
      .replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

  const csrfToken = () => {
    const field = document.querySelector("input[name=_token]");
    return field ? field.value : "";
  };

  const postJson = (url, body) =>
    fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json", "x-csrf-token": csrfToken() },
      credentials: "same-origin",
      body: JSON.stringify(body),
    });

  const showPasskeyError = (message) => {
    const box = document.querySelector("[data-passkey-error]");
    if (!box) return;
    box.textContent = message;
    box.hidden = false;
  };

  if (!window.PublicKeyCredential) {
    const notice = document.querySelector("[data-passkey-unsupported]");
    if (notice) notice.hidden = false;
    document.querySelectorAll("[data-passkey-register], [data-passkey-authenticate]")
      .forEach((b) => { b.disabled = true; });
  }

  on("click", "[data-passkey-register]", async (_e, button) => {
    button.disabled = true;
    try {
      const options = await (await fetch(button.dataset.optionsUrl, {
        method: "POST",
        headers: { "x-csrf-token": csrfToken() },
        credentials: "same-origin",
      })).json();

      options.challenge = fromBase64Url(options.challenge);
      options.user.id = fromBase64Url(options.user.id);
      (options.excludeCredentials || []).forEach((c) => { c.id = fromBase64Url(c.id); });

      const credential = await navigator.credentials.create({ publicKey: options });
      // The shape `PublicKeyCredential.toJSON()` produces, spelled out rather
      // than called: `toJSON` is recent enough that some browsers in use do
      // not have it. The names are the specification's, not ours — the server
      // reads what a browser sends, and a snake_case invention of our own
      // means every credential arrives with no `rawId`.
      const response = await postJson(button.dataset.verifyUrl, {
        id: credential.id,
        rawId: toBase64Url(credential.rawId),
        type: credential.type,
        response: {
          clientDataJSON: toBase64Url(credential.response.clientDataJSON),
          attestationObject: toBase64Url(credential.response.attestationObject),
        },
        label: navigator.userAgent.slice(0, 60),
      });

      if (response.ok) window.location.reload();
      else showPasskeyError((await response.json()).message || "That passkey could not be registered.");
    } catch (error) {
      // A user who dismisses the browser prompt lands here, and that is not a
      // failure worth shouting about.
      if (error && error.name !== "NotAllowedError") showPasskeyError(error.message || String(error));
    } finally {
      button.disabled = false;
    }
  });

  on("click", "[data-passkey-authenticate]", async (_e, button) => {
    button.disabled = true;
    try {
      const options = await (await fetch(button.dataset.optionsUrl, {
        method: "POST",
        headers: { "x-csrf-token": csrfToken() },
        credentials: "same-origin",
      })).json();

      options.challenge = fromBase64Url(options.challenge);
      (options.allowCredentials || []).forEach((c) => { c.id = fromBase64Url(c.id); });

      const assertion = await navigator.credentials.get({ publicKey: options });
      const response = await postJson(button.dataset.verifyUrl, {
        id: assertion.id,
        rawId: toBase64Url(assertion.rawId),
        type: assertion.type,
        response: {
          clientDataJSON: toBase64Url(assertion.response.clientDataJSON),
          authenticatorData: toBase64Url(assertion.response.authenticatorData),
          signature: toBase64Url(assertion.response.signature),
          userHandle: assertion.response.userHandle ? toBase64Url(assertion.response.userHandle) : null,
        },
      });

      if (response.ok) window.location.assign((await response.json()).redirect || "/dashboard");
      else showPasskeyError((await response.json()).message || "That passkey was not accepted.");
    } catch (error) {
      if (error && error.name !== "NotAllowedError") showPasskeyError(error.message || String(error));
    } finally {
      button.disabled = false;
    }
  });
})();

/* ---- Reordering a menu -------------------------------------------------
 *
 * Native HTML drag-and-drop rather than a library, and a handle rather than a
 * draggable row: the row holds links and buttons, and making all of it
 * draggable takes the text selection and the click targets with it.
 *
 * The whole order is submitted, not the moved row: dragging one item changes
 * the position of every item after it, so a single number would already be
 * stale by the time it arrived. */
(function () {
  "use strict";

  const form = document.querySelector("[data-menu-reorder]");
  if (!form) return;

  const field = form.querySelector("[data-menu-order]");
  const rowOf = (element) => element.closest("[data-menu-row]");
  let dragging = null;

  form.addEventListener("dragstart", (event) => {
    const handle = event.target.closest(".menu-handle");
    if (!handle) return;
    dragging = rowOf(handle);
    if (!dragging) return;
    dragging.classList.add("opacity-50");
    // Firefox will not start a drag without data on the transfer.
    event.dataTransfer.effectAllowed = "move";
    event.dataTransfer.setData("text/plain", dragging.dataset.menuId);
  });

  form.addEventListener("dragover", (event) => {
    if (!dragging) return;
    event.preventDefault();
    const over = rowOf(event.target);
    if (!over || over === dragging) return;

    // Above the midpoint means before, below means after. Comparing against
    // the midpoint rather than the edge is what stops the row flickering
    // between two positions while the pointer sits on a boundary.
    const box = over.getBoundingClientRect();
    const before = event.clientY < box.top + box.height / 2;
    over.parentNode.insertBefore(dragging, before ? over : over.nextSibling);
  });

  form.addEventListener("dragend", () => {
    if (!dragging) return;
    dragging.classList.remove("opacity-50");
    dragging = null;

    field.value = Array.from(form.querySelectorAll("[data-menu-row]"))
      .map((row) => row.dataset.menuId)
      .join(",");
    form.submit();
  });
})();
