// By Oskar Wickström
// Licensed under the MIT License (https://github.com/owickstrom/the-proportional-web/blob/main/LICENSE.md)

window.addEventListener("load", () => {
  const asides = [...document.querySelectorAll("aside")];
  asides.forEach((aside, i) => {
    const anchor = aside.previousElementSibling;
    const name = `--paragraph-before-aside-${i}`;
    anchor.style.anchorName = name;
    aside.style.positionAnchor = name;
  });
});

// Rargo: give every <cmd>…</cmd> a copy-to-clipboard button.
window.addEventListener("load", () => {
  const copyIcon =
    '<svg class="copy" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="9" y="9" width="12" height="12" rx="1"/><path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1"/></svg>';
  const checkIcon =
    '<svg class="check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6 9 17l-5-5"/></svg>';

  [...document.querySelectorAll("cmd")].forEach((el) => {
    const text = el.textContent.trim();
    el.textContent = "";

    const label = document.createElement("span");
    label.className = "cmd-text";
    label.textContent = text;

    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy-cmd";
    btn.title = "Copy";
    btn.setAttribute("aria-label", "Copy command to clipboard");
    btn.innerHTML = copyIcon + checkIcon;

    btn.addEventListener("click", async () => {
      try {
        if (navigator.clipboard && window.isSecureContext) {
          await navigator.clipboard.writeText(text);
        } else {
          const ta = document.createElement("textarea");
          ta.value = text;
          ta.style.position = "fixed";
          ta.style.opacity = "0";
          document.body.appendChild(ta);
          ta.select();
          document.execCommand("copy");
          ta.remove();
        }
        btn.classList.add("copied");
        btn.title = "Copied!";
        setTimeout(() => {
          btn.classList.remove("copied");
          btn.title = "Copy";
        }, 1400);
      } catch (e) {
        /* clipboard unavailable; the text stays selectable */
      }
    });

    el.append(label, btn);
  });
});
