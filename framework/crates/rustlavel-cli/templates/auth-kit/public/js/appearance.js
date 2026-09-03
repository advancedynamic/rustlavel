/* Settings → Appearance. Loaded by that tab alone, not from the layout.
 *
 * The whole reason this file exists is the Content-Security-Policy. The page
 * has no `unsafe-inline`, so a preview cannot be coloured by a `style=`
 * attribute in the template and a swatch cannot be coloured by a `<style>`
 * block — a browser drops both silently, which is the worst way to find out.
 * `element.style.setProperty(...)` is CSSOM called from a script the policy
 * already allowed to load, and it is not inline style, so that is how every
 * colour below reaches the screen.
 *
 * The script knows no setting keys. A card names the keys it holds in
 * `data-keys`, a preset lists values positional against that, and a preview
 * names the keys it reads. Adding a colour is a change to the template.
 *
 * Nothing here is a security control. The hex check, the file-size check and
 * the accepted file types are all conveniences; the controller validates every
 * colour again and decides what a file is by looking at its bytes.
 */
(function () {
  "use strict";

  const HEX = /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i;

  /* ---- The colour fields --------------------------------------------- */

  /* Every colour box on the page, by setting key. */
  const fields = new Map();
  document.querySelectorAll("[data-colour-field]").forEach((field) => {
    const text = field.querySelector("[data-colour-text]");
    if (!text) return;
    fields.set(field.dataset.colourField, { text, picker: field.querySelector("[data-colour-picker]") });
  });

  /* The last value that was actually a colour, per key. A preview repaints on
   * every keystroke, and halfway through typing "#3b82f6" the box says "#3b8"
   * — using that would flash the preview through colours nobody chose. */
  const known = new Map();

  const expand = (value) =>
    value.length === 4
      ? "#" + value.slice(1).split("").map((c) => c + c).join("").toLowerCase()
      : value.toLowerCase();

  const valueOf = (key) => known.get(key) || "#000000";

  const remember = (key, value) => {
    if (HEX.test(value)) known.set(key, value);
  };

  const setColour = (key, value) => {
    const field = fields.get(key);
    if (!field || !HEX.test(value)) return;
    field.text.value = value;
    // `<input type="color">` accepts only the six-digit lowercase form, and
    // silently keeps its old value for anything else.
    if (field.picker) field.picker.value = expand(value);
    known.set(key, value);
  };

  fields.forEach(({ text }, key) => remember(key, text.value.trim()));

  /* ---- The previews --------------------------------------------------- */

  const paint = () => {
    document.querySelectorAll("[data-preview-login]").forEach((preview) => {
      const [from, to] = preview.dataset.previewLogin.split(",");
      preview.style.setProperty(
        "background-image",
        "linear-gradient(135deg, " + valueOf(from) + ", " + valueOf(to) + ")"
      );
    });

    document.querySelectorAll("[data-preview-sidebar]").forEach((preview) => {
      const [background, text, activeBackground, activeText] = preview.dataset.previewSidebar.split(",");
      preview.style.setProperty("background-color", valueOf(background));

      preview.querySelectorAll("[data-preview-row]").forEach((row) => {
        row.style.setProperty("background-color", "transparent");
        row.style.setProperty("color", valueOf(text));
      });
      preview.querySelectorAll("[data-preview-row-active]").forEach((row) => {
        row.style.setProperty("background-color", valueOf(activeBackground));
        row.style.setProperty("color", valueOf(activeText));
      });
    });

    /* The brand swatch. The server derives eleven shades from this colour and
     * the preview shows three of them, so mixing towards white here is a
     * stand-in rather than the real ramp — close enough to answer "is that the
     * blue I meant", which is the only question a preview is for. */
    document.querySelectorAll("[data-brand-preview]").forEach((preview) => {
      const brand = valueOf(preview.dataset.brandPreview);

      preview.querySelectorAll("[data-brand-fill]").forEach((el) => {
        el.style.setProperty("background-color", brand);
      });
      preview.querySelectorAll("[data-brand-tint]").forEach((el) => {
        el.style.setProperty("background-color", mix(brand, "#ffffff", 0.88));
        el.style.setProperty("color", brand);
      });
      preview.querySelectorAll("[data-brand-text]").forEach((el) => {
        el.style.setProperty("color", brand);
      });
    });
  };

  /* `amount` of white, the rest of `colour`. Enough for a tint swatch. */
  const mix = (colour, towards, amount) => {
    const channels = (value) => {
      const full = expand(value);
      return [1, 3, 5].map((i) => parseInt(full.slice(i, i + 2), 16));
    };
    const a = channels(colour);
    const b = channels(towards);
    const out = a.map((v, i) => Math.round(v + (b[i] - v) * amount));
    return "#" + out.map((v) => v.toString(16).padStart(2, "0")).join("");
  };

  fields.forEach(({ text, picker }, key) => {
    text.addEventListener("input", () => {
      const value = text.value.trim();
      if (!HEX.test(value)) return;
      if (picker) picker.value = expand(value);
      known.set(key, value);
      paint();
    });
    if (picker) {
      picker.addEventListener("input", () => {
        setColour(key, picker.value);
        paint();
      });
    }
  });

  /* ---- Presets --------------------------------------------------------- */

  document.querySelectorAll("[data-preset]").forEach((button) => {
    const group = button.closest("[data-colour-group]");
    if (!group) return;
    const keys = (group.dataset.keys || "").split(",").map((key) => key.trim());
    const values = button.dataset.preset.split(",").map((value) => value.trim());

    button.addEventListener("click", () => {
      keys.forEach((key, index) => setColour(key, values[index] || ""));
      paint();
    });
  });

  /* The little coloured dot on each preset. The span is in the template so
   * Tailwind can see its classes; only the one thing a stylesheet cannot know
   * — which colour this preset is — is set here. */
  document.querySelectorAll("[data-swatch]").forEach((dot) => {
    if (HEX.test(dot.dataset.swatch)) dot.style.setProperty("background-color", dot.dataset.swatch);
  });

  paint();

  /* ---- The logo boxes -------------------------------------------------- */

  /* Matches the controller. Checked again there, on the bytes rather than on
   * what the browser reported. */
  const MAX_LOGO_BYTES = 2 * 1024 * 1024;

  const csrfToken = () => {
    const field = document.querySelector("input[name=_token]");
    return field ? field.value : "";
  };

  const say = (element, message) => {
    if (!element) return;
    element.textContent = message;
    element.hidden = message === "";
  };

  document.querySelectorAll("[data-logo-box]").forEach((box) => {
    const input = box.querySelector("[data-logo-input]");
    const choose = box.querySelector("[data-logo-choose]");
    const path = box.querySelector("[data-logo-path]");
    if (!input || !choose || !path) return;

    const current = box.querySelector("[data-logo-current]");
    const image = box.querySelector("[data-logo-image]");
    const status = box.querySelector("[data-logo-status]");
    const problem = box.querySelector("[data-logo-error]");

    const show = (value) => {
      path.value = value;
      // Never `image.src = ""`: an empty src resolves to the page itself and
      // the browser fetches this whole document again to draw an image.
      if (image && value) image.src = value;
      if (current) current.hidden = value === "";
    };

    choose.addEventListener("click", () => input.click());

    box.querySelectorAll("[data-logo-remove]").forEach((remove) => {
      remove.addEventListener("click", () => {
        // Only the field is cleared. The stored file goes when the form is
        // saved, because until somebody presses Save nothing has changed.
        show("");
        say(problem, "");
        say(status, "Removed. Press Save Logos to confirm.");
      });
    });

    input.addEventListener("change", async () => {
      const file = input.files && input.files[0];
      if (!file) return;

      say(problem, "");
      if (file.size > MAX_LOGO_BYTES) {
        say(status, "");
        say(problem, "That file is larger than 2 MB. Please use a smaller one.");
        input.value = "";
        return;
      }

      say(status, "Uploading…");
      choose.disabled = true;
      try {
        /* The file is the entire request body. `rustlavel-http` has no
         * multipart parser — `Request` offers `input`, `form` and `json` and
         * nothing else — so sending one part on its own is what the framework
         * can actually read. The content type below is what the operating
         * system told the browser; the server ignores it and sniffs the
         * bytes, which is the only claim about a file worth believing. */
        const response = await fetch(box.dataset.uploadUrl, {
          method: "POST",
          headers: {
            "content-type": file.type || "application/octet-stream",
            "x-csrf-token": csrfToken(),
          },
          credentials: "same-origin",
          body: file,
        });

        const payload = await response.json().catch(() => ({}));
        if (response.ok && payload.path) {
          show(payload.path);
          say(status, "Uploaded. Press Save Logos to keep it.");
        } else {
          say(status, "");
          say(problem, payload.message || "That file was not accepted.");
        }
      } catch (error) {
        say(status, "");
        say(problem, "The upload could not be sent. Check the connection and try again.");
      } finally {
        choose.disabled = false;
        // Cleared so that choosing the same file twice fires `change` again,
        // which it otherwise would not.
        input.value = "";
      }
    });
  });
})();
